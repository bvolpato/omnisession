use std::{
    cmp::Reverse,
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
    sync::{Condvar, Mutex, MutexGuard, PoisonError},
    thread,
    time::{Duration, Instant},
};

use anyhow::Result;
use chrono::{DateTime, Utc};
use crossterm::{
    cursor::MoveToColumn,
    queue,
    style::Print,
    terminal::{Clear, ClearType},
};
use omnis_adapters::{
    AdapterRegistry, IndexRead, IndexSessionReader, NativeSession, NativeSessionName, RollbackTitle,
};
use omnis_core::{
    SearchDocument, search_document_version, session_message_prefix_is_known,
    session_message_search_title, trajectory_search_document, workspace_paths_match,
};
use omnis_ir::{CanonicalSnapshot, Provider, SessionRef, SourceRevision};
use omnis_store::{
    IndexedSession, IndexedTrajectoryTitle, LocalOpenerEvidence, MAX_TRAJECTORY_WRITE_BATCH_BYTES,
    MAX_TRAJECTORY_WRITE_BATCH_DOCUMENTS, SelectedSessionObservation, SessionTrajectoryOrigin,
    Store, TrajectoryDocument, TrajectoryIndexFailure, TrajectoryIndexState, TrajectorySourceGuard,
};

use crate::read_session;

// Larger sources index only their sampled head and tail.
pub(crate) const FULL_READ_SOURCE_BYTES: u64 = 16 * 1024 * 1024;
const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);
/// Most threads that read and redact sessions while the calling thread writes the index.
const MAX_PREPARE_WORKERS: usize = 8;
/// Prepared sessions each worker may hold ahead of the writer. This bounds memory, because one
/// prepared session holds its whole redacted text.
const PREPARED_AHEAD_PER_WORKER: usize = 2;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct IndexCandidate {
    pub(crate) session: SessionRef,
    pub(crate) updated_at: Option<DateTime<Utc>>,
    pub(crate) source_path: Option<PathBuf>,
    source_revision: SourceRevision,
    metadata: Option<IndexedSession>,
}

impl IndexCandidate {
    fn selected_codex(
        &self,
        registry: &AdapterRegistry,
        revision: SourceRevision,
    ) -> Option<NativeSession> {
        let session = registry
            .find_native_session(Provider::Codex, &self.session.id)
            .ok()??;
        let selected =
            omnis_adapters::CodexAdapter::index_file_revision(session.source_path.as_deref()?)?;
        (revision.file.is_some()
            && selected == revision
            && session.source_revision == Some(revision))
        .then_some(session)
    }

    fn record_failure(
        &self,
        registry: &AdapterRegistry,
        store: &Store,
        guard: TrajectorySourceGuard,
        error: &anyhow::Error,
    ) -> Option<IndexedTrajectoryTitle> {
        let revision = self.source_revision;
        if self.session.provider == Provider::Codex
            && omnis_adapters::CodexAdapter::is_current_rollout_unavailable(error)
        {
            if let Err(error) = crate::remove_unavailable_session(registry, store, &self.session) {
                eprintln!("warning: local trajectory index write: {error}");
            }
            return None;
        }
        if retryable_read_failure(error)
            || (self.session.provider == Provider::Codex
                && self.selected_codex(registry, revision).is_none())
        {
            return None;
        }
        let failure = TrajectoryIndexFailure {
            source_updated_at: self.updated_at,
            source_file: revision.file,
            source_contents: revision.contents,
            document_version: search_document_version(self.session.provider),
        };
        store
            .record_observed_trajectory_index_failure(&self.session, &failure, guard)
            .ok()?;
        (self.session.provider == Provider::Codex)
            .then(|| store.indexed_trajectory_title(&self.session).ok().flatten())
            .flatten()
    }

    // Sessions without a discovered source file are skipped so oversized files are never read
    // whole. Reading an OpenCode session spawns its CLI, so those index when opened instead.
    pub(crate) fn from_session(session: &NativeSession) -> Option<Self> {
        let provider = session.session.provider;
        let readable = provider == Provider::Imported
            || (provider != Provider::OpenCode && session.source_path.is_some());
        readable.then(|| Self {
            session: session.session.clone(),
            updated_at: session.updated_at,
            source_path: session.source_path.clone(),
            source_revision: session.source_revision.unwrap_or_default(),
            metadata: (provider == Provider::Codex)
                .then(|| crate::indexed_session_metadata(session)),
        })
    }

    fn can_prepare_in_worker(&self, guard: TrajectorySourceGuard) -> bool {
        // Legacy projection checks need the store. Read a previously full, oversized rollout on the calling thread.
        reads_transcript_file(self.session.provider)
            && !needs_legacy_projection_check(guard, self.updated_at)
            && (projection_read_budget(guard, self.updated_at, self.source_revision)
                == FULL_READ_SOURCE_BYTES
                || self.can_upgrade_preview())
    }

    fn can_upgrade_preview(&self) -> bool {
        if matches!(self.session.provider, Provider::Hermes | Provider::Codex) {
            // The physical size does not measure the selected session's decoded history.
            return false;
        }
        !self
            .source_path
            .as_deref()
            .and_then(|path| fs::metadata(path).ok())
            .is_some_and(|metadata| metadata.is_file() && metadata.len() > FULL_READ_SOURCE_BYTES)
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct IndexProgress {
    pub(crate) indexed: usize,
    pub(crate) total: usize,
    pub(crate) failed: usize,
    pub(crate) titles: Vec<IndexedTrajectoryTitle>,
    pub(crate) workspaces: Vec<(SessionRef, omnis_store::CodexWorkspaceUpdate)>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct IndexSummary {
    pub(crate) candidates: usize,
    pub(crate) stale: usize,
    pub(crate) indexed: usize,
    pub(crate) failed: usize,
    /// Stale sessions not read because they failed before and have not changed since.
    pub(crate) failed_skipped: usize,
    pub(crate) stopped: bool,
}

// Codex reports the rollout's modification time for both discovery and reads. Restoring a
// backup can move it backwards. Other adapters can use different clocks for the two paths.
pub(crate) fn source_time_changed(
    provider: Provider,
    current: DateTime<Utc>,
    cached: DateTime<Utc>,
) -> bool {
    if provider == Provider::Codex {
        current.timestamp_millis() != cached.timestamp_millis()
    } else {
        current.timestamp_millis() > cached.timestamp_millis()
    }
}

pub(crate) fn source_guard(
    provider: Provider,
    prior: Option<&TrajectoryIndexState>,
) -> TrajectorySourceGuard {
    if provider == Provider::Codex {
        TrajectorySourceGuard::Observed(prior.copied())
    } else {
        TrajectorySourceGuard::NewerSource
    }
}

/// Keeps a full search document when the same rollout needs a projection upgrade.
pub(crate) fn projection_read_budget(
    guard: TrajectorySourceGuard,
    updated_at: Option<DateTime<Utc>>,
    revision: SourceRevision,
) -> u64 {
    let TrajectorySourceGuard::Observed(Some(prior)) = guard else {
        return FULL_READ_SOURCE_BYTES;
    };
    if prior.source_complete
        && prior.document_version < search_document_version(Provider::Codex)
        && updated_at.is_some_and(|timestamp| {
            !source_time_changed(Provider::Codex, timestamp, prior.source_updated_at)
        })
        && revision.contents.is_some()
        && revision.matches_file_contents(prior.source_revision)
    {
        u64::MAX
    } else {
        FULL_READ_SOURCE_BYTES
    }
}

fn needs_legacy_projection_check(
    guard: TrajectorySourceGuard,
    updated_at: Option<DateTime<Utc>>,
) -> bool {
    let TrajectorySourceGuard::Observed(Some(prior)) = guard else {
        return false;
    };
    matches!(prior.document_version, 4 | 5)
        && prior.source_complete
        && prior.source_revision == SourceRevision::default()
        && updated_at
            .is_none_or(|time| !source_time_changed(Provider::Codex, time, prior.source_updated_at))
}

pub(crate) fn verify_legacy_search_read(
    registry: &AdapterRegistry,
    store: &Store,
    guard: TrajectorySourceGuard,
    read: IndexRead,
) -> Result<IndexRead> {
    let session = &read.snapshot.session;
    let time = read.snapshot.captured_at;
    if read.source_complete
        || session.provider != Provider::Codex
        || read.source_revision.file.is_none()
        || read.source_revision.contents.is_none()
        || !needs_legacy_projection_check(guard, Some(time))
    {
        return Ok(read);
    }
    let Some(hash) = store.legacy_codex_projection_hash(session, time)? else {
        return Ok(read);
    };
    let selected_path = || -> Result<PathBuf> {
        registry
            .find_native_session(Provider::Codex, &session.id)?
            .filter(|native| {
                native
                    .source_revision
                    .is_some_and(|revision| revision.matches_file_contents(read.source_revision))
            })
            .and_then(|native| native.source_path)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "selected Codex rollout changed during legacy projection verification"
                )
            })
    };
    let path = selected_path()?;
    let verified =
        omnis_adapters::CodexAdapter::verify_legacy_search_projection(session, &path, &read, &hash);
    anyhow::ensure!(
        selected_path()? == path,
        "selected Codex rollout changed during legacy projection verification"
    );
    // Keep the ordinary preview when the historical projection cannot be reproduced.
    Ok(verified.ok().flatten().unwrap_or(read))
}

#[cfg(test)]
fn needs_index(candidate: &IndexCandidate, state: Option<&TrajectoryIndexState>) -> bool {
    needs_index_at(candidate, candidate.source_revision, state)
}

fn needs_index_at(
    candidate: &IndexCandidate,
    revision: SourceRevision,
    state: Option<&TrajectoryIndexState>,
) -> bool {
    let Some(state) = state else {
        return true;
    };
    revision.file != state.source_revision.file
        || revision.contents != state.source_revision.contents
        || state.document_version < search_document_version(candidate.session.provider)
        || candidate.updated_at.is_some_and(|updated_at| {
            source_time_changed(
                candidate.session.provider,
                updated_at,
                state.source_updated_at,
            )
        })
        || (!state.source_complete && candidate.can_upgrade_preview())
}

/// Whether a recorded failure still covers the candidate, so reading it again would repeat it.
///
/// Retry when the source time or document version changes. Without a source time, only the
/// document version can show that a retry is due.
fn failure_is_current(
    candidate: &IndexCandidate,
    revision: SourceRevision,
    failure: &TrajectoryIndexFailure,
) -> bool {
    revision.file == failure.source_file
        && revision.contents == failure.source_contents
        && failure.document_version >= search_document_version(candidate.session.provider)
        && candidate.updated_at.is_none_or(|updated_at| {
            failure.source_updated_at.is_some_and(|failed_source| {
                updated_at.timestamp_millis() == failed_source.timestamp_millis()
            })
        })
}

/// Whether a read failed only because a database was busy or a source changed while it was read,
/// so a later pass can succeed even when the source time does not move.
fn retryable_read_failure(error: &anyhow::Error) -> bool {
    if omnis_adapters::CodexAdapter::is_current_rollout_unavailable(error) {
        return true;
    }
    error.chain().any(|cause| {
        cause
            .downcast_ref::<rusqlite::Error>()
            .and_then(rusqlite::Error::sqlite_error_code)
            .is_some_and(|code| {
                matches!(
                    code,
                    rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                )
            })
            // Adapters report sources rewritten during a snapshot or read with this wording.
            || cause.to_string().contains("changed during")
            // A full temporary volume says nothing about the session itself.
            || cause
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::StorageFull)
    })
}

/// Builds index candidates with the current workspace first, then newest sessions first.
pub(crate) fn ordered_candidates(
    sessions: &[NativeSession],
    current_project: Option<&Path>,
) -> Vec<IndexCandidate> {
    let mut workspace_matches = HashMap::<&Path, bool>::new();
    let mut ordered = sessions
        .iter()
        .filter_map(|session| {
            let candidate = IndexCandidate::from_session(session)?;
            let current = if let (Some(project), Some(path)) =
                (current_project, session.project_path.as_deref())
            {
                *workspace_matches
                    .entry(path)
                    .or_insert_with(|| workspace_paths_match(path, project))
            } else {
                false
            };
            Some((Reverse(current), Reverse(candidate.updated_at), candidate))
        })
        .collect::<Vec<_>>();
    ordered.sort_by_key(|(current, updated_at, _)| (*current, *updated_at));
    ordered
        .into_iter()
        .map(|(_, _, candidate)| candidate)
        .collect()
}

/// Stale candidates in order, the failures they were checked against, and their counts.
struct Pending {
    stale: Vec<(IndexCandidate, TrajectorySourceGuard)>,
    failures: HashMap<SessionRef, TrajectoryIndexFailure>,
    recovered: Vec<(
        IndexCandidate,
        TrajectoryIndexState,
        Option<TrajectoryIndexFailure>,
    )>,
    hidden: Vec<IndexedTrajectoryTitle>,
    summary: IndexSummary,
}

fn pending(
    registry: &AdapterRegistry,
    store: &Store,
    candidates: Vec<IndexCandidate>,
    retry_failed: bool,
) -> Result<Pending> {
    let states = store.trajectory_index_states()?;
    let (conflicts, deferred_names) = if candidates
        .iter()
        .any(|candidate| candidate.session.provider == Provider::Codex)
    {
        (
            store.conflicting_codex_catalog_rows()?,
            store.legacy_codex_name_retries()?,
        )
    } else {
        (HashSet::new(), HashMap::new())
    };
    let failures = store.trajectory_index_failures()?;
    let mut summary = IndexSummary {
        candidates: candidates.len(),
        ..IndexSummary::default()
    };
    let mut stale = Vec::new();
    let mut recovered = Vec::new();
    let mut hidden = Vec::new();
    let missing: HashSet<_> = candidates
        .iter()
        .filter(|candidate| candidate.session.provider == Provider::Codex)
        .filter_map(|candidate| {
            states
                .get(&candidate.session)?
                .source_revision
                .missing_ancestor
        })
        .collect();
    let restored = registry
        .get(Provider::Codex)
        .filter(|_| !missing.is_empty())
        .map_or_else(HashSet::new, |adapter| {
            adapter.available_index_sources(&missing)
        });
    for candidate in candidates {
        let prior = states.get(&candidate.session);
        let revision = candidate.source_revision;
        let failure = failures.get(&candidate.session);
        let ancestor_restored = candidate.session.provider == Provider::Codex
            && prior
                .and_then(|s| s.source_revision.missing_ancestor)
                .is_some_and(|id| restored.contains(&id));
        let retry_name = deferred_names.get(&candidate.session) == Some(&revision)
            && native_codex_name(registry, &candidate.session)? == Some(NativeSessionName::Absent);
        if !ancestor_restored && !retry_name && !needs_index_at(&candidate, revision, prior) {
            if candidate.session.provider == Provider::Codex
                && let Some(prior) = prior
                && (conflicts.contains(&candidate.session)
                    || failure
                        .is_some_and(|failure| !failure_is_current(&candidate, revision, failure)))
            {
                recovered.push((candidate, *prior, failure.copied()));
            }
            continue;
        }
        if !retry_failed
            && failure.is_some_and(|failure| failure_is_current(&candidate, revision, failure))
        {
            summary.failed_skipped += 1;
            if candidate.session.provider == Provider::Codex
                && let Some(title) = store.indexed_trajectory_title(&candidate.session)?
            {
                hidden.push(title);
            }
        } else {
            let guard = source_guard(candidate.session.provider, prior);
            stale.push((candidate, guard));
        }
    }
    summary.stale = stale.len();
    Ok(Pending {
        stale,
        failures,
        recovered,
        hidden,
        summary,
    })
}

fn restore_cached_sources(
    registry: &AdapterRegistry,
    store: &Store,
    recovered: Vec<(
        IndexCandidate,
        TrajectoryIndexState,
        Option<TrajectoryIndexFailure>,
    )>,
) -> Result<Vec<IndexedTrajectoryTitle>> {
    let mut titles = Vec::new();
    for (candidate, prior, failure) in recovered {
        let observed = store.indexed_session(&candidate.session)?;
        let revision = SourceRevision {
            missing_ancestor: None,
            ..prior.source_revision
        };
        if let Some(selected) = candidate.selected_codex(registry, revision)
            && store
                .restore_indexed_source(
                    &crate::indexed_session_metadata(&selected),
                    prior,
                    observed.as_ref(),
                    failure,
                )
                .is_ok()
        {
            titles.extend(store.indexed_trajectory_title(&candidate.session)?);
        }
    }
    Ok(titles)
}

fn observe_replacements(
    store: &Store,
    stale: &[(IndexCandidate, TrajectorySourceGuard)],
    failures: &HashMap<SessionRef, TrajectoryIndexFailure>,
) -> Result<HashMap<SessionRef, SelectedSessionObservation>> {
    let mut observations = HashMap::new();
    if !stale
        .iter()
        .any(|(candidate, _)| candidate.metadata.is_some())
    {
        return Ok(observations);
    }
    let inventory = store.observe_codex_catalog_inventory()?;
    let catalog = inventory
        .observed
        .iter()
        .map(|row| (&row.session, row))
        .collect::<HashMap<_, _>>();
    for (candidate, guard) in stale {
        let Some(metadata) = &candidate.metadata else {
            continue;
        };
        if inventory.legacy_codex_source(&candidate.session).is_some()
            || failures.contains_key(&candidate.session)
            || matches!(guard, TrajectorySourceGuard::Observed(Some(prior)) if prior.source_revision.file != candidate.source_revision.file)
            || catalog
                .get(&candidate.session)
                .is_some_and(|stored| *stored != metadata)
        {
            observations.insert(
                candidate.session.clone(),
                store.observe_selected_session(&candidate.session)?,
            );
        }
    }
    Ok(observations)
}

/// Counts candidates whose search document is missing or out of date. Unchanged sessions that
/// failed before are counted as skipped instead.
///
/// # Errors
///
/// Returns an error when the index state cannot be read.
pub(crate) fn pending_summary(
    registry: &AdapterRegistry,
    store: &Store,
    candidates: Vec<IndexCandidate>,
) -> Result<IndexSummary> {
    let pending = pending(registry, store, candidates, false)?;
    restore_cached_sources(registry, store, pending.recovered)?;
    Ok(pending.summary)
}

fn batch_session_reader<'a>(
    registry: &'a AdapterRegistry,
    stale: &[(IndexCandidate, TrajectorySourceGuard)],
) -> impl Fn(&SessionRef, Option<&Path>, u64) -> Result<IndexRead> + Sync + 'a {
    let ids: Vec<_> = stale
        .iter()
        .filter(|(candidate, _)| candidate.session.provider == Provider::Codex)
        .map(|(candidate, _)| candidate.session.id.as_str())
        .collect();
    let codex = if ids.is_empty() {
        None
    } else {
        registry
            .get(Provider::Codex)
            .and_then(|adapter| adapter.index_batch(&ids))
    };
    move |session, path, budget| {
        if session.provider == Provider::Codex
            && let Some(reader) = &codex
        {
            reader(session, path, budget)
        } else {
            registry.index_session(session, path, budget)
        }
    }
}

/// Indexes stale sessions in candidate order, reporting progress and derived titles in batches.
///
/// A session that cannot be read is recorded, and later passes skip it until its source time or
/// the search document version changes. `retry_failed` reads recorded sessions again anyway.
///
/// # Errors
///
/// Returns an error only when the index state cannot be read. Unreadable sessions are counted.
pub(crate) fn index_candidates(
    registry: &AdapterRegistry,
    store: &Store,
    candidates: Vec<IndexCandidate>,
    retry_failed: bool,
    stop: &dyn Fn() -> bool,
    report: &mut dyn FnMut(IndexProgress),
) -> Result<IndexSummary> {
    let Pending {
        stale,
        failures,
        recovered,
        mut hidden,
        mut summary,
    } = pending(registry, store, candidates, retry_failed)?;
    let observations = observe_replacements(store, &stale, &failures)?;
    hidden.extend(restore_cached_sources(registry, store, recovered)?);
    report(IndexProgress {
        total: stale.len(),
        titles: hidden,
        ..IndexProgress::default()
    });
    let mut updates = IndexProgress::default();
    let mut last_report = Instant::now();
    let mut batch = Vec::new();
    let mut batch_bytes = 0usize;
    let read_session = batch_session_reader(registry, &stale);
    let prepare_read = |candidate: &IndexCandidate, guard, store: Option<&Store>| {
        prepare_session(registry, candidate, guard, Some(&read_session), store)
    };
    // `None` leaves a session to the calling thread, which reads it when its turn comes.
    prepare_in_order(
        &stale,
        prepare_worker_count(stale.len()),
        &|(candidate, guard)| {
            candidate
                .can_prepare_in_worker(*guard)
                .then(|| prepare_read(candidate, *guard, None))
        },
        |item, prepared| {
            let (candidate, guard) = item;
            if stop() {
                flush_batch(registry, store, &mut batch, &mut summary, &mut updates);
                summary.stopped = true;
                return false;
            }
            let prepared = prepared
                .unwrap_or_else(|| prepare_read(candidate, *guard, Some(store)))
                .and_then(|mut read| {
                    read.title.check_withdrawal(
                        registry,
                        store,
                        &candidate.session,
                        read.source_revision,
                    )?;
                    Ok(read)
                });
            match prepared {
                Ok(prepared) if observations.contains_key(&candidate.session) => {
                    publish_replacement(
                        registry,
                        store,
                        &prepared,
                        &observations[&candidate.session],
                        &mut summary,
                        &mut updates,
                    );
                }
                Ok(prepared) => {
                    if let Some(failure) = failures.get(&candidate.session) {
                        let _ = store
                            .clear_observed_trajectory_index_failure(&candidate.session, failure);
                    }
                    if !batch.is_empty()
                        && (batch.len() >= MAX_TRAJECTORY_WRITE_BATCH_DOCUMENTS
                            || batch_bytes.saturating_add(prepared.document.indexed_byte_count)
                                > MAX_TRAJECTORY_WRITE_BATCH_BYTES)
                    {
                        flush_batch(registry, store, &mut batch, &mut summary, &mut updates);
                        batch_bytes = 0;
                    }
                    batch_bytes = batch_bytes.saturating_add(prepared.document.indexed_byte_count);
                    batch.push(prepared);
                }
                Err(error) => {
                    summary.failed += 1;
                    updates
                        .titles
                        .extend(candidate.record_failure(registry, store, *guard, &error));
                }
            }
            if batch.len() >= MAX_TRAJECTORY_WRITE_BATCH_DOCUMENTS
                || batch_bytes >= MAX_TRAJECTORY_WRITE_BATCH_BYTES
                || last_report.elapsed() >= PROGRESS_INTERVAL
            {
                flush_batch(registry, store, &mut batch, &mut summary, &mut updates);
                batch_bytes = 0;
            }
            if last_report.elapsed() >= PROGRESS_INTERVAL {
                report(progress(&summary, &mut updates));
                last_report = Instant::now();
            }
            true
        },
    );
    flush_batch(registry, store, &mut batch, &mut summary, &mut updates);
    report(progress(&summary, &mut updates));
    Ok(summary)
}

fn publish_replacement(
    registry: &AdapterRegistry,
    store: &Store,
    prepared: &PreparedIndex,
    observation: &SelectedSessionObservation,
    summary: &mut IndexSummary,
    updates: &mut IndexProgress,
) {
    let session = &prepared.candidate.session;
    match publish_selected_read(registry, store, prepared, observation) {
        Ok(title) => {
            summary.indexed += 1;
            updates.titles.extend(title);
            updates.record_workspace(prepared);
        }
        Err(error) => {
            summary.failed += 1;
            if let Some(failure) = observation.failure
                && !matches!(
                    error.downcast_ref::<omnis_store::StoreError>(),
                    Some(omnis_store::StoreError::TrajectorySourceChanged)
                )
            {
                if observation.state.is_none() {
                    let _ = store.clear_observed_trajectory_index_failure(session, &failure);
                } else {
                    let _ = store.retry_observed_trajectory_index_failure(session, &failure);
                }
            }
        }
    }
}

fn publish_selected_read(
    registry: &AdapterRegistry,
    store: &Store,
    read: &PreparedIndex,
    observation: &SelectedSessionObservation,
) -> Result<Option<IndexedTrajectoryTitle>> {
    let revision = SourceRevision {
        missing_ancestor: None,
        ..read.source_revision
    };
    anyhow::ensure!(
        TrajectorySourceGuard::Observed(observation.state) == read.guard,
        "cached source changed during indexing"
    );
    let selected = read
        .candidate
        .selected_codex(registry, revision)
        .ok_or(omnis_store::StoreError::TrajectorySourceChanged)?;
    let name_observed = verified_name_coverage(read.native_name.as_ref(), Some(&selected))?;
    let mut metadata = crate::indexed_session_metadata(&selected);
    let history = corroborated_codex_name(
        registry,
        &selected,
        revision,
        &metadata,
        observation,
        read.native_name.as_ref(),
    );
    let mut document = read.store_document();
    let retained;
    if let Some(current) = &observation.metadata
        && current.source_revision == metadata.source_revision
        && current.updated_at == metadata.updated_at
        && current.title != metadata.title
    {
        if name_observed {
            return Err(omnis_store::StoreError::TrajectorySourceChanged.into());
        }
        metadata.title.clone_from(&current.title);
        retained = metadata
            .title
            .as_deref()
            .and_then(omnis_core::compact_search_title);
        document.derived_title = retained.as_deref();
        document.title_authoritative = retained.is_some();
        document.title_is_verified = retained.is_some();
        document.local_opener = LocalOpenerEvidence::Unknown;
        document.withdrawn_title = None;
    }
    store.upsert_selected_session_document_with_name_history(
        &metadata,
        observation,
        document,
        read.workspace.as_ref(),
        name_observed,
        history.as_ref().map(|prior| prior.assignment.as_str()),
    )?;
    Ok(store.indexed_trajectory_title(&read.candidate.session)?)
}

/// Whether each session of `provider` is one transcript file, which worker threads may read
/// ahead of the writer.
///
/// The other providers copy a `SQLite` database to private temporary storage for a read, or open
/// the `OmniSession` store. The calling thread reads those sessions, one at a time, when their
/// turn comes. Concurrent copies of a large database cannot fill the temporary volume that way,
/// and a stopped run starts no further copy.
const fn reads_transcript_file(provider: Provider) -> bool {
    match provider {
        Provider::Claude | Provider::Codex | Provider::Grok | Provider::Pi | Provider::OhMyPi => {
            true
        }
        Provider::OpenCode
        | Provider::Hermes
        | Provider::Antigravity
        | Provider::AntigravityIde
        | Provider::AntigravityDesktop
        | Provider::CursorCli
        | Provider::CursorIde
        | Provider::GenericAcp
        | Provider::Imported => false,
    }
}

/// One core stays free for the calling thread, which writes the index.
fn prepare_worker_count(sessions: usize) -> usize {
    thread::available_parallelism()
        .map_or(1, |cores| cores.get().saturating_sub(1).max(1))
        .min(MAX_PREPARE_WORKERS)
        .min(sessions)
}

struct PrepareQueue<T> {
    /// Next item a worker takes.
    next_claim: usize,
    /// Next item the consumer handles. Workers stay within a fixed distance of it.
    next_consume: usize,
    ready: BTreeMap<usize, T>,
    /// Set when the consumer stops early or a worker panics.
    closed: bool,
}

/// Closes the queue when a worker unwinds, so the consumer does not wait for its result forever.
struct CloseOnPanic<'a, T>(&'a Mutex<PrepareQueue<T>>, &'a Condvar);

impl<T> Drop for CloseOnPanic<'_, T> {
    fn drop(&mut self) {
        if thread::panicking() {
            lock_queue(self.0).closed = true;
            self.1.notify_all();
        }
    }
}

fn lock_queue<T>(queue: &Mutex<PrepareQueue<T>>) -> MutexGuard<'_, PrepareQueue<T>> {
    queue.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Runs `prepare` for every item on `workers` threads and passes each result to `consume` on the
/// calling thread, in item order. `consume` returns `false` to stop early.
///
/// Sessions are read concurrently because reading and redacting them takes longer than one core
/// can feed the index writer. The writer keeps candidate order, so interrupted runs still index
/// the current workspace and the newest sessions first.
fn prepare_in_order<I: Sync, T: Send>(
    items: &[I],
    workers: usize,
    prepare: &(dyn Fn(&I) -> T + Sync),
    mut consume: impl FnMut(&I, T) -> bool,
) {
    let queue = Mutex::new(PrepareQueue {
        next_claim: 0,
        next_consume: 0,
        ready: BTreeMap::new(),
        closed: false,
    });
    let (claimable, prepared) = (Condvar::new(), Condvar::new());
    let ahead = workers * PREPARED_AHEAD_PER_WORKER;
    thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                let _close_on_panic = CloseOnPanic(&queue, &prepared);
                loop {
                    let index = {
                        let mut state = claimable
                            .wait_while(lock_queue(&queue), |state| {
                                !state.closed
                                    && state.next_claim < items.len()
                                    && state.next_claim >= state.next_consume + ahead
                            })
                            .unwrap_or_else(PoisonError::into_inner);
                        if state.closed || state.next_claim == items.len() {
                            return;
                        }
                        state.next_claim += 1;
                        state.next_claim - 1
                    };
                    let result = prepare(&items[index]);
                    lock_queue(&queue).ready.insert(index, result);
                    prepared.notify_one();
                }
            });
        }
        for (index, item) in items.iter().enumerate() {
            let result = {
                let mut state = prepared
                    .wait_while(lock_queue(&queue), |state| {
                        !state.closed && !state.ready.contains_key(&index)
                    })
                    .unwrap_or_else(PoisonError::into_inner);
                state.next_consume = index + 1;
                state.ready.remove(&index)
            };
            claimable.notify_all();
            // A missing result means a worker panicked. The scope reports that panic on exit.
            if !result.is_some_and(|result| consume(item, result)) {
                break;
            }
        }
        lock_queue(&queue).closed = true;
        claimable.notify_all();
    });
}

fn flush_batch(
    registry: &AdapterRegistry,
    store: &Store,
    batch: &mut Vec<PreparedIndex>,
    summary: &mut IndexSummary,
    updates: &mut IndexProgress,
) {
    if batch.is_empty() {
        return;
    }
    let documents = batch
        .iter()
        .map(|prepared| {
            (
                &prepared.candidate.session,
                prepared.store_document(),
                prepared.guard,
                prepared.workspace.as_ref(),
            )
        })
        .collect::<Vec<_>>();
    let names = batch
        .iter()
        .filter_map(|read| read.candidate.metadata.as_ref())
        .collect::<Vec<_>>();
    let stored = store.upsert_discovered_trajectory_documents(&documents, &names);
    if stored.is_ok() {
        summary.indexed += batch.len();
        for prepared in batch.drain(..) {
            updates.record_workspace(&prepared);
            prepared.report_title(store, &mut updates.titles);
        }
    } else {
        for mut prepared in batch.drain(..) {
            let mut stored = prepared.store(store);
            if matches!(
                stored,
                Err(omnis_store::StoreError::TrajectorySourceChanged)
            ) {
                // Re-read the live rollout once. The competing writer may have indexed either
                // the restored file or the file it replaced.
                let session = &prepared.candidate.session;
                let fresh = (|| -> Result<_> {
                    let observation = store.observe_selected_session(session)?;
                    let selected = registry
                        .find_native_session(session.provider, &session.id)?
                        .and_then(|native| IndexCandidate::from_session(&native))
                        .ok_or(omnis_store::StoreError::TrajectorySourceChanged)?;
                    let mut read = prepare_session(
                        registry,
                        &selected,
                        TrajectorySourceGuard::Observed(observation.state),
                        None,
                        Some(store),
                    )?;
                    read.title
                        .check_withdrawal(registry, store, session, read.source_revision)?;
                    publish_selected_read(registry, store, &read, &observation)?;
                    Ok(read)
                })();
                if let Ok(read) = fresh {
                    prepared = read;
                    stored = Ok(());
                }
            }
            if stored.is_ok() {
                summary.indexed += 1;
                updates.record_workspace(&prepared);
                prepared.report_title(store, &mut updates.titles);
            } else {
                summary.failed += 1;
            }
        }
    }
}

impl IndexProgress {
    pub(crate) fn processed(&self) -> usize {
        self.indexed + self.failed
    }

    fn record_workspace(&mut self, prepared: &PreparedIndex) {
        if let Some(workspace) = &prepared.workspace {
            self.workspaces
                .push((prepared.candidate.session.clone(), workspace.clone()));
        }
    }
}

fn progress(summary: &IndexSummary, updates: &mut IndexProgress) -> IndexProgress {
    IndexProgress {
        indexed: summary.indexed,
        total: summary.stale,
        failed: summary.failed,
        ..std::mem::take(updates)
    }
}

pub(crate) fn native_codex_name(
    registry: &AdapterRegistry,
    session: &SessionRef,
) -> Result<Option<NativeSessionName>> {
    (session.provider == Provider::Codex)
        .then(|| {
            registry
                .adapter(Provider::Codex)
                .map(|adapter| adapter.native_session_name(&session.id))
        })
        .transpose()
}

pub(crate) struct CorroboratedCodexName<'a> {
    pub(crate) stored: &'a str,
    pub(crate) assignment: String,
}

pub(crate) fn corroborated_codex_name<'a>(
    registry: &AdapterRegistry,
    native: &NativeSession,
    revision: SourceRevision,
    selected: &IndexedSession,
    observation: &'a SelectedSessionObservation,
    name: Option<&NativeSessionName>,
) -> Option<CorroboratedCodexName<'a>> {
    if name != Some(&NativeSessionName::Unknown)
        || native.title.is_some()
        || !native
            .source_revision
            .is_some_and(|source| source.matches_file_contents(revision))
        || !observation.legacy_codex_name_candidate(selected)
    {
        return None;
    }
    let assignment = registry
        .adapter(Provider::Codex)
        .ok()?
        .prior_session_names(&[native])
        .remove(&native.session.id)?;
    let stored = observation.corroborated_legacy_codex_name(selected, &assignment)?;
    Some(CorroboratedCodexName { stored, assignment })
}

pub(crate) fn verified_name_coverage(
    name: Option<&NativeSessionName>,
    native: Option<&NativeSession>,
) -> Result<bool> {
    let (Some(name), Some(native)) = (name, native) else {
        return Ok(false);
    };
    if name.confirms(native.title.as_deref()) {
        Ok(true)
    } else if *name == NativeSessionName::Unknown && native.title.is_none() {
        Ok(false)
    } else {
        Err(omnis_store::StoreError::TrajectorySourceChanged.into())
    }
}

fn legacy_opener_name(
    registry: &AdapterRegistry,
    session: &SessionRef,
    revision: SourceRevision,
    prior: &omnis_store::LegacyCodexOpenerCandidate,
    selected_path: impl Fn() -> Result<PathBuf>,
) -> Result<Option<NativeSessionName>> {
    let Some(name @ (NativeSessionName::Absent | NativeSessionName::Unknown)) =
        native_codex_name(registry, session)?
    else {
        return Ok(None);
    };
    if !omnis_adapters::CodexAdapter::verify_legacy_local_opener(
        &selected_path()?,
        session,
        revision,
        &prior.title,
        prior.requires_tool_catalog,
    )? {
        return Ok(None);
    }
    selected_path()?;
    anyhow::ensure!(
        native_codex_name(registry, session)?.as_ref() == Some(&name),
        omnis_store::StoreError::TrajectorySourceChanged
    );
    Ok(Some(name))
}

#[derive(Default)]
pub(crate) struct IndexedTitle {
    pub(crate) current: Option<String>,
    pub(crate) authoritative: bool,
    pub(crate) withdrawn: Option<String>,
    pub(crate) local_opener: LocalOpenerEvidence,
    uncertain_rollback: RollbackTitle,
    check_local_rollback: bool,
}

impl IndexedTitle {
    pub(crate) fn from_read(
        snapshot: &CanonicalSnapshot,
        full_read: bool,
        document: &SearchDocument,
        rollback_title: &RollbackTitle,
        native_name: Option<&NativeSessionName>,
    ) -> Self {
        let provider_name = match native_name {
            Some(NativeSessionName::Named(name)) => Some(name.as_str()),
            Some(_) => None,
            None => snapshot.title.as_deref(),
        };
        if let Some(name) = provider_name.and_then(omnis_core::compact_search_title) {
            return Self {
                current: Some(name),
                authoritative: true,
                ..Self::default()
            };
        }
        let authoritative = session_message_prefix_is_known(snapshot, full_read);
        let local = match rollback_title {
            RollbackTitle::Known(current) => Some(current.clone()),
            RollbackTitle::UnknownLocal | RollbackTitle::UnknownAfterWithdrawal(_)
                if authoritative =>
            {
                Some(session_message_search_title(snapshot))
            }
            _ => None,
        };
        if let Some(current) = local
            && !current
                .as_deref()
                .is_some_and(|title| rollback_title.withdraws(title))
        {
            // A readable local opener can replace a provider name only after verified catalog removal.
            let name_absent = matches!(native_name, Some(NativeSessionName::Absent));
            return Self {
                current,
                authoritative: name_absent,
                local_opener: LocalOpenerEvidence::Current,
                ..Self::default()
            };
        }
        if matches!(rollback_title, RollbackTitle::UnknownAfterWithdrawal(_)) {
            return Self {
                uncertain_rollback: rollback_title.clone(),
                ..Self::default()
            };
        }
        let check_local_rollback = !document.source_complete
            && !authoritative
            && snapshot.session.provider == Provider::Codex
            && snapshot.events.iter().any(|event| {
                event.source.raw_record_type.as_deref() == Some("omnisession.codex_history_base")
                    && event.source.provider == Provider::Codex
                    && event.source.native_session_id == snapshot.session.id
                    && event.payload["native_history_unverified"].as_bool() == Some(true)
            });
        let unresolved_prefix = authoritative
            && native_name.is_some_and(|name| !matches!(name, NativeSessionName::Absent));
        Self {
            current: if snapshot.session.provider == Provider::Codex && !authoritative {
                None
            } else {
                session_message_search_title(snapshot)
            },
            authoritative: authoritative && !unresolved_prefix,
            local_opener: if unresolved_prefix {
                LocalOpenerEvidence::VisiblePrefix
            } else {
                LocalOpenerEvidence::Unknown
            },
            uncertain_rollback: RollbackTitle::Unknown,
            check_local_rollback,
            withdrawn: None,
        }
    }

    pub(crate) fn is_verified(&self) -> bool {
        self.current.is_some()
            && (self.authoritative
                || matches!(
                    self.local_opener,
                    LocalOpenerEvidence::Current
                        | LocalOpenerEvidence::RecheckedCurrent
                        | LocalOpenerEvidence::VisiblePrefix
                ))
    }

    pub(crate) fn check_withdrawal(
        &mut self,
        registry: &AdapterRegistry,
        store: &Store,
        session: &SessionRef,
        revision: SourceRevision,
    ) -> Result<()> {
        let uncertain = matches!(
            self.uncertain_rollback,
            RollbackTitle::UnknownAfterWithdrawal(_)
        );
        let Some(file) = revision.file else {
            return Ok(());
        };
        let selected_path = |require_unnamed: bool| -> Result<PathBuf> {
            registry
                .find_native_session(Provider::Codex, &session.id)?
                .filter(|selected| {
                    (!require_unnamed || selected.title.is_none())
                        && selected
                            .source_revision
                            .is_some_and(|selected| selected.matches_file_contents(revision))
                })
                .and_then(|selected| selected.source_path)
                .ok_or_else(|| omnis_store::StoreError::TrajectorySourceChanged.into())
        };
        if (uncertain || self.local_opener == LocalOpenerEvidence::Current)
            && let Some(prior) = store.legacy_codex_opener_candidate(session, file)?
            && let Some(name) =
                legacy_opener_name(registry, session, revision, &prior, || selected_path(true))?
        {
            self.withdrawn = Some(prior.title);
            self.local_opener = if name == NativeSessionName::Unknown {
                self.current = None;
                self.authoritative = false;
                LocalOpenerEvidence::DeferredLegacyName
            } else if uncertain {
                LocalOpenerEvidence::RecheckedWithdrawn
            } else {
                LocalOpenerEvidence::RecheckedCurrent
            };
            return Ok(());
        }
        if !self.check_local_rollback && !uncertain {
            return Ok(());
        }
        let Some(prior) = store.withdrawable_trajectory_title(session, file, uncertain)? else {
            return Ok(());
        };
        if uncertain {
            if self.uncertain_rollback.withdraws(&prior)
                || (self.uncertain_rollback.needs_withdrawal_check(&prior)
                    && omnis_adapters::CodexAdapter::verify_earlier_opener_withdrawal(
                        &selected_path(false)?,
                        session,
                        revision,
                        &prior,
                    )?)
            {
                selected_path(false)?;
                self.withdrawn = Some(prior);
                self.local_opener = LocalOpenerEvidence::Withdrawn;
            }
            return Ok(());
        }
        if omnis_adapters::CodexAdapter::rollout_withdrew_search_title(
            &selected_path(false)?,
            session,
            revision,
            &prior,
        )? {
            selected_path(false)?;
            self.withdrawn = Some(prior);
        }
        Ok(())
    }
}

struct PreparedIndex {
    workspace: Option<omnis_store::CodexWorkspaceUpdate>,
    guard: TrajectorySourceGuard,
    candidate: IndexCandidate,
    document: SearchDocument,
    source_updated_at: DateTime<Utc>,
    source_complete: bool,
    source_revision: SourceRevision,
    origin: SessionTrajectoryOrigin,
    title: IndexedTitle,
    native_name: Option<NativeSessionName>,
}

impl PreparedIndex {
    fn report_title(&self, store: &Store, titles: &mut Vec<IndexedTrajectoryTitle>) {
        if let Ok(Some(title)) = store.indexed_trajectory_title(&self.candidate.session) {
            titles.push(title);
        }
    }

    fn store(&self, store: &Store) -> omnis_store::Result<()> {
        let names = self.candidate.metadata.iter().collect::<Vec<_>>();
        store.upsert_discovered_trajectory_documents(
            &[(
                &self.candidate.session,
                self.store_document(),
                self.guard,
                self.workspace.as_ref(),
            )],
            &names,
        )
    }

    fn store_document(&self) -> TrajectoryDocument<'_> {
        TrajectoryDocument {
            redacted_text: &self.document.text,
            source_updated_at: self.source_updated_at,
            source_byte_count: self.document.source_byte_count,
            indexed_byte_count: self.document.indexed_byte_count,
            truncation_strategy: self.document.truncation_strategy.as_str(),
            source_complete: self.source_complete,
            source_revision: self.source_revision,
            origin: self.origin,
            document_version: search_document_version(self.candidate.session.provider),
            derived_title: self.title.current.as_deref(),
            withdrawn_title: self.title.withdrawn.as_deref(),
            title_is_verified: self.title.is_verified(),
            title_authoritative: self.title.authoritative,
            local_opener: self.title.local_opener,
        }
    }
}

fn prepare_session(
    registry: &AdapterRegistry,
    candidate: &IndexCandidate,
    guard: TrajectorySourceGuard,
    reader: Option<&IndexSessionReader<'_>>,
    store: Option<&Store>,
) -> anyhow::Result<PreparedIndex> {
    let imported = candidate.session.provider == Provider::Imported;
    let read = if imported {
        read_session(registry, &candidate.session).map(|snapshot| IndexRead {
            native_session: None,
            snapshot,
            rollback_title: RollbackTitle::Unknown,
            workspace_projection: omnis_adapters::CodexWorkspaceProjection::Unchecked,
            source_complete: true,
            source_revision: SourceRevision::default(),
        })
    } else {
        let standalone = |session: &SessionRef, path: Option<&Path>, budget| {
            registry.index_session(session, path, budget)
        };
        let read = reader.unwrap_or(&standalone);
        read(
            &candidate.session,
            candidate.source_path.as_deref(),
            projection_read_budget(guard, candidate.updated_at, candidate.source_revision),
        )
    }?;
    if candidate.session.provider == Provider::Codex && candidate.source_revision.file.is_some() {
        anyhow::ensure!(
            read.source_revision
                .matches_file_contents(candidate.source_revision),
            "selected Codex rollout changed during indexing"
        );
    }
    let IndexRead {
        native_session,
        rollback_title,
        workspace_projection,
        snapshot,
        source_complete,
        source_revision,
    } = match store {
        Some(store) => verify_legacy_search_read(registry, store, guard, read)?,
        None => read,
    };
    let native_name = native_codex_name(registry, &candidate.session)?;
    let name_observed = verified_name_coverage(native_name.as_ref(), native_session.as_ref())?;
    if name_observed
        && let (Some(discovered), Some(native)) = (&candidate.metadata, &native_session)
        && discovered.title != native.title.as_deref().map(omnis_core::redact_secrets)
    {
        return Err(omnis_store::StoreError::TrajectorySourceChanged.into());
    }
    let document = trajectory_search_document(&snapshot);
    // Codex reads the rollout's actual mtime again. Discovery may predate a restore.
    let source_updated_at = if candidate.session.provider == Provider::Codex {
        snapshot.captured_at
    } else {
        candidate
            .updated_at
            .unwrap_or(snapshot.captured_at)
            .max(snapshot.captured_at)
    };
    let origin = if imported {
        SessionTrajectoryOrigin::ImportedBundle
    } else {
        SessionTrajectoryOrigin::Native
    };
    let title = IndexedTitle::from_read(
        &snapshot,
        source_complete,
        &document,
        &rollback_title,
        native_name.as_ref(),
    );
    Ok(PreparedIndex {
        workspace: native_session.as_ref().and_then(|_| {
            crate::codex_workspace_update(
                &snapshot,
                source_revision,
                source_complete,
                &workspace_projection,
            )
        }),
        guard,
        candidate: candidate.clone(),
        document,
        source_updated_at,
        source_complete,
        source_revision,
        origin,
        title,
        native_name,
    })
}

/// Shows indexing progress as one line that updates in place on an interactive stderr.
pub(crate) struct ProgressLine {
    enabled: bool,
    visible: bool,
}

impl ProgressLine {
    pub(crate) fn new(requested: bool) -> Self {
        Self {
            enabled: requested && io::stderr().is_terminal(),
            visible: false,
        }
    }

    pub(crate) fn update(&mut self, progress: &IndexProgress) {
        if !self.enabled || progress.total == 0 {
            return;
        }
        let mut stderr = io::stderr().lock();
        let _ = queue!(
            stderr,
            MoveToColumn(0),
            Clear(ClearType::CurrentLine),
            Print(format!(
                "Indexing changed sessions {}/{}…",
                progress.processed(),
                progress.total
            ))
        );
        let _ = stderr.flush();
        self.visible = true;
    }

    pub(crate) fn clear(&mut self) {
        if !self.visible {
            return;
        }
        let mut stderr = io::stderr().lock();
        let _ = queue!(stderr, MoveToColumn(0), Clear(ClearType::CurrentLine));
        let _ = stderr.flush();
        self.visible = false;
    }
}

impl Drop for ProgressLine {
    fn drop(&mut self) {
        self.clear();
    }
}

#[cfg(test)]
mod tests {
    use std::{
        cell::Cell,
        fs::File,
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
    };

    use omnis_adapters::{
        CodexAdapter, LaunchPlan, LaunchTarget, ProviderAdapter, ProviderInstallation,
    };
    use serde_json::json;

    use super::*;

    fn candidate(
        updated_at: Option<DateTime<Utc>>,
        source_path: Option<PathBuf>,
    ) -> IndexCandidate {
        IndexCandidate {
            session: SessionRef::new(Provider::Codex, "synthetic"),
            updated_at,
            metadata: None,
            source_revision: source_path
                .as_deref()
                .and_then(CodexAdapter::index_file_revision)
                .unwrap_or_default(),
            source_path,
        }
    }

    #[test]
    fn prepared_results_arrive_in_item_order_within_the_lookahead() {
        let items = (0..200_usize).collect::<Vec<_>>();
        let workers = 4;
        let consumed = AtomicUsize::new(0);
        let furthest_ahead = AtomicUsize::new(0);
        let prepare = |item: &usize| {
            furthest_ahead.fetch_max(
                item.saturating_sub(consumed.load(Ordering::SeqCst)),
                Ordering::SeqCst,
            );
            // Later items finish first, so order must come from the queue.
            std::thread::sleep(Duration::from_micros(
                u64::try_from(200 - *item).expect("small delay"),
            ));
            item * 2
        };
        let mut results = Vec::new();
        prepare_in_order(&items, workers, &prepare, |item, result| {
            results.push((*item, result));
            consumed.fetch_add(1, Ordering::SeqCst);
            true
        });

        assert_eq!(
            results,
            items
                .iter()
                .map(|item| (*item, item * 2))
                .collect::<Vec<_>>()
        );
        assert!(furthest_ahead.load(Ordering::SeqCst) <= workers * PREPARED_AHEAD_PER_WORKER);
    }

    #[test]
    fn stopping_the_consumer_stops_preparing_later_items() {
        let items = (0..10_000_usize).collect::<Vec<_>>();
        let prepared = AtomicUsize::new(0);
        let prepare = |item: &usize| {
            prepared.fetch_add(1, Ordering::SeqCst);
            *item
        };
        let mut consumed = Vec::new();
        prepare_in_order(&items, 4, &prepare, |item, _| {
            consumed.push(*item);
            *item < 9
        });

        assert_eq!(consumed, (0..10).collect::<Vec<_>>());
        // Workers stop within the lookahead of the last consumed item.
        assert!(prepared.load(Ordering::SeqCst) <= 10 + 4 * PREPARED_AHEAD_PER_WORKER);
    }

    #[test]
    fn only_matching_file_and_content_ids_allow_an_unbounded_projection_upgrade() {
        let time = Utc::now();
        let revision = SourceRevision {
            file: Some(uuid::Uuid::from_u128(1)),
            contents: Some(uuid::Uuid::from_u128(2)),
            ..SourceRevision::default()
        };
        let budget = |stored, selected, updated_at| {
            projection_read_budget(
                TrajectorySourceGuard::Observed(Some(TrajectoryIndexState {
                    source_updated_at: time,
                    source_revision: stored,
                    source_complete: true,
                    document_version: search_document_version(Provider::Codex) - 1,
                })),
                Some(updated_at),
                selected,
            )
        };
        assert_eq!(budget(revision, revision, time), u64::MAX);
        for other in [
            SourceRevision::default(),
            SourceRevision {
                contents: None,
                ..revision
            },
            SourceRevision {
                file: None,
                ..revision
            },
            SourceRevision {
                contents: Some(uuid::Uuid::nil()),
                ..revision
            },
            SourceRevision {
                file: Some(uuid::Uuid::nil()),
                ..revision
            },
        ] {
            assert_eq!(budget(other, revision, time), FULL_READ_SOURCE_BYTES);
            assert_eq!(budget(revision, other, time), FULL_READ_SOURCE_BYTES);
        }
        assert_eq!(
            budget(revision, revision, time - chrono::Duration::seconds(1)),
            FULL_READ_SOURCE_BYTES
        );
    }

    #[test]
    fn stale_versions_changed_sources_and_previews_need_indexing() {
        let now = Utc::now();
        let current = TrajectoryIndexState {
            source_updated_at: now,
            source_revision: SourceRevision::default(),
            source_complete: true,
            document_version: search_document_version(Provider::Codex),
        };
        let mut small = candidate(Some(now), None);
        small.session.provider = Provider::Claude;

        assert!(needs_index(&small, None));
        assert!(!needs_index(&small, Some(&current)));
        assert!(needs_index(
            &small,
            Some(&TrajectoryIndexState {
                document_version: 0,
                ..current
            })
        ));
        assert!(needs_index(
            &candidate(Some(now + chrono::Duration::seconds(1)), None),
            Some(&current)
        ));
        assert!(needs_index(
            &small,
            Some(&TrajectoryIndexState {
                source_complete: false,
                ..current
            })
        ));
        let stored = TrajectoryIndexState {
            source_updated_at: DateTime::from_timestamp_millis(now.timestamp_millis())
                .expect("millisecond timestamp"),
            ..current
        };
        assert!(!needs_index(&small, Some(&stored)));
    }

    #[test]
    fn sampled_documents_of_oversized_sources_stay_current() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let path = temporary.path().join("rollout.jsonl");
        File::create(&path)
            .and_then(|file| file.set_len(FULL_READ_SOURCE_BYTES + 1))
            .expect("sparse oversized source");
        let now = Utc::now();
        let sampled = TrajectoryIndexState {
            source_updated_at: now,
            source_revision: candidate(Some(now), Some(path.clone())).source_revision,
            source_complete: false,
            document_version: search_document_version(Provider::Codex),
        };

        assert!(!needs_index(
            &candidate(Some(now), Some(path)),
            Some(&sampled)
        ));
        let sampled = TrajectoryIndexState {
            source_revision: SourceRevision::default(),
            ..sampled
        };
        let compressed = candidate(Some(now), Some(temporary.path().join("rollout.jsonl.zst")));
        // Codex preserves the rollout mtime during compression. A v4 picker document can
        // have only the edges of a session that fits the decoded full-read budget.
        assert!(needs_index(
            &compressed,
            Some(&TrajectoryIndexState {
                document_version: 4,
                ..sampled
            })
        ));
        assert!(!needs_index(&compressed, Some(&sampled)));
        let mut shared_database = candidate(Some(now), None);
        shared_database.session.provider = Provider::Hermes;
        assert!(!needs_index(&shared_database, Some(&sampled)));
        assert!(needs_index(
            &shared_database,
            Some(&TrajectoryIndexState {
                document_version: 3,
                ..sampled
            })
        ));
    }

    #[test]
    fn recorded_failures_cover_unchanged_sources_and_lasting_errors_only() {
        let now = Utc::now();
        let failure = TrajectoryIndexFailure {
            source_file: None,
            source_contents: None,
            source_updated_at: Some(now),
            document_version: search_document_version(Provider::Codex),
        };

        assert!(failure_is_current(
            &candidate(Some(now), None),
            SourceRevision::default(),
            &failure
        ));
        assert!(failure_is_current(
            &candidate(None, None),
            SourceRevision::default(),
            &failure
        ));
        assert!(!failure_is_current(
            &candidate(Some(now - chrono::Duration::days(1)), None),
            SourceRevision::default(),
            &failure
        ));
        assert!(!failure_is_current(
            &candidate(Some(now + chrono::Duration::milliseconds(1)), None),
            SourceRevision::default(),
            &failure
        ));
        assert!(!failure_is_current(
            &candidate(Some(now), None),
            SourceRevision::default(),
            &TrajectoryIndexFailure {
                source_updated_at: None,
                ..failure
            }
        ));
        assert!(!failure_is_current(
            &candidate(Some(now), None),
            SourceRevision::default(),
            &TrajectoryIndexFailure {
                document_version: 0,
                ..failure
            }
        ));

        let busy = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
            None,
        );
        assert!(retryable_read_failure(
            &anyhow::Error::new(busy).context("synthetic database read")
        ));
        assert!(retryable_read_failure(&anyhow::anyhow!(
            "provider database changed during snapshot"
        )));
        assert!(retryable_read_failure(
            &anyhow::Error::new(std::io::Error::from(std::io::ErrorKind::StorageFull))
                .context("synthetic snapshot copy")
        ));
        assert!(!retryable_read_failure(&anyhow::anyhow!(
            "Cursor IDE trajectory record exceeds safe size limit"
        )));
    }

    #[test]
    fn only_sessions_with_known_sources_are_background_candidates() {
        let session = |provider, source_path: Option<&str>| NativeSession {
            session: SessionRef::new(provider, "synthetic"),
            title: None,
            project_path: None,
            git_branch: None,
            created_at: None,
            updated_at: None,
            updated_at_approximate: false,
            event_count: 0,
            source_path: source_path.map(PathBuf::from),
            source_revision: None,
        };

        assert!(
            IndexCandidate::from_session(&session(Provider::Codex, Some("/synthetic"))).is_some()
        );
        assert!(IndexCandidate::from_session(&session(Provider::Codex, None)).is_none());
        assert!(
            IndexCandidate::from_session(&session(Provider::OpenCode, Some("/synthetic")))
                .is_none()
        );
        assert!(IndexCandidate::from_session(&session(Provider::Imported, None)).is_some());
    }

    #[test]
    fn candidates_start_with_current_workspace_then_newest() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let current = temporary.path().join("current");
        let other = temporary.path().join("other");
        fs::create_dir_all(&current).expect("current workspace");
        fs::create_dir_all(&other).expect("other workspace");
        let now = Utc::now();
        let session = |id: &str, project: &Path, age_minutes| NativeSession {
            session: SessionRef::new(Provider::Codex, id),
            title: None,
            project_path: Some(project.to_path_buf()),
            git_branch: None,
            created_at: None,
            updated_at: Some(now - chrono::Duration::minutes(age_minutes)),
            updated_at_approximate: false,
            event_count: 0,
            source_path: Some(PathBuf::from("/synthetic")),
            source_revision: None,
        };

        let ordered = ordered_candidates(
            &[
                session("current-older", &current, 30),
                session("other-newest", &other, 1),
                session("current-newer", &current, 5),
            ],
            Some(&current),
        );

        assert_eq!(
            ordered
                .iter()
                .map(|candidate| candidate.session.id.as_str())
                .collect::<Vec<_>>(),
            ["current-newer", "current-older", "other-newest"]
        );
    }

    #[test]
    fn shared_database_size_does_not_hide_small_session_history() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("hermes");
        crate::hermes_import::create_fixture_store(&root).unwrap();
        let database = root.join("state.db");
        let mut connection = rusqlite::Connection::open(&database).unwrap();
        let transaction = connection.transaction().unwrap();
        transaction.execute(
            "INSERT INTO sessions (id, source, started_at, message_count) VALUES ('synthetic-budget', 'cli', 100, 1200)", [],
        ).unwrap();
        for index in 0..1_200 {
            let content = if index == 50 {
                "zephyrbudgetneedle"
            } else {
                "Synthetic budget context"
            };
            transaction.execute(
                "INSERT INTO messages (session_id, role, content, timestamp) VALUES ('synthetic-budget', 'assistant', ?1, ?2)",
                rusqlite::params![content, 100 + index],
            ).unwrap();
        }
        transaction.execute_batch("CREATE TABLE padding (data BLOB); INSERT INTO padding VALUES (zeroblob(18000000));").unwrap();
        transaction.commit().unwrap();
        drop(connection);
        let before = fs::read(&database).unwrap();
        let mut registry = AdapterRegistry::new();
        registry.register(omnis_adapters::HermesAdapter::with_root(&root));
        let listed = registry.list_sessions(Provider::Hermes, None).unwrap();
        let candidates = ordered_candidates(&listed, None);
        let store = Store::open(temporary.path().join("store.sqlite3")).unwrap();
        store
            .upsert_trajectory_document(
                &listed[0].session,
                &omnis_store::TrajectoryDocument {
                    source_revision: SourceRevision::default(),
                    redacted_text: "Synthetic old preview",
                    source_updated_at: Utc::now(),
                    source_byte_count: 21,
                    indexed_byte_count: 21,
                    truncation_strategy: "none",
                    source_complete: false,
                    origin: SessionTrajectoryOrigin::Native,
                    document_version: 2,
                    derived_title: None,
                    withdrawn_title: None,
                    title_is_verified: false,
                    title_authoritative: false,
                    local_opener: omnis_store::LocalOpenerEvidence::Unknown,
                },
            )
            .unwrap();
        let summary = index_candidates(
            &registry,
            &store,
            candidates.clone(),
            false,
            &|| false,
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(summary.indexed, 1);
        assert_eq!(summary.failed, 0);
        assert_eq!(
            store
                .search_session_trajectories("zephyrbudgetneedle", 10)
                .unwrap(),
            [SessionRef::new(Provider::Hermes, "synthetic-budget")]
        );
        assert!(store.trajectory_index_states().unwrap()[&listed[0].session].source_complete);
        assert_eq!(
            index_candidates(&registry, &store, candidates, false, &|| false, &mut |_| {})
                .unwrap()
                .stale,
            0
        );
        assert_eq!(before, fs::read(database).unwrap());
    }

    #[test]
    fn background_index_makes_unopened_conversation_text_searchable() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let workspace = temporary.path().join("workspace");
        let sessions = temporary.path().join("codex/sessions/2026/01/01");
        fs::create_dir_all(&workspace).expect("workspace");
        fs::create_dir_all(&sessions).expect("codex sessions");
        let id = "019f0000-0000-7000-8000-000000000001";
        fs::write(
            sessions.join(format!("rollout-2026-01-01T00-00-00-{id}.jsonl")),
            format!(
                "{}\n{}\n{}\n",
                json!({"type":"session_meta","timestamp":"2026-01-01T00:00:00Z","payload":{"id":id,"cwd":workspace,"git":{"branch":"main"}}}),
                json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Synthetic rate limiter request"}]}}),
                json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"The zebracorn marker lives only in this answer"}]}}),
            ),
        )
        .expect("synthetic rollout");
        let mut registry = AdapterRegistry::new();
        registry.register(CodexAdapter::with_root(temporary.path().join("codex")));
        let store = Store::open(temporary.path().join("store.sqlite3")).expect("synthetic store");
        let (listed, _) = registry
            .list_sessions_with_notes(Provider::Codex, None)
            .expect("list synthetic sessions");
        let candidates = listed
            .iter()
            .filter_map(IndexCandidate::from_session)
            .collect::<Vec<_>>();
        let mut reports = Vec::new();

        let summary = index_candidates(
            &registry,
            &store,
            candidates.clone(),
            false,
            &|| false,
            &mut |progress| {
                reports.push(progress);
            },
        )
        .expect("index synthetic sessions");

        assert_eq!(summary.indexed, 1);
        assert_eq!(
            store
                .search_session_trajectories("zebracorn", 10)
                .expect("search index"),
            vec![SessionRef::new(Provider::Codex, id)]
        );
        let final_report = reports.last().expect("final progress");
        assert_eq!((final_report.indexed, final_report.total), (1, 1));
        // A slow index run flushes titles in interim reports, so collect every batch.
        assert_eq!(
            reports
                .iter()
                .flat_map(|report| report.titles.iter())
                .filter_map(|entry| entry
                    .title
                    .clone()
                    .map(|title| (entry.session.clone(), title)))
                .collect::<Vec<_>>(),
            vec![(
                SessionRef::new(Provider::Codex, id),
                "Synthetic rate limiter request".to_owned()
            )]
        );
        let again = index_candidates(&registry, &store, candidates, false, &|| false, &mut |_| {})
            .expect("reindex synthetic sessions");
        assert_eq!(again.stale, 0);
    }

    fn write_codex_rollout(
        home: &Path,
        marker: Option<&str>,
        modified: u64,
    ) -> (AdapterRegistry, Vec<IndexCandidate>) {
        let sessions = home.join("sessions/2026/01/01");
        fs::create_dir_all(&sessions).unwrap();
        let id = "019f0000-0000-7000-8000-000000000001";
        let path = sessions.join(format!("rollout-2026-01-01T00-00-00-{id}.jsonl"));
        let mut records = vec![json!({"type":"session_meta","payload":{"id":id}}).to_string()];
        if let Some(marker) = marker {
            records.push(json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":marker}]}}).to_string());
        }
        fs::write(&path, records.join("\n") + "\n").unwrap();
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(modified))
            .unwrap();
        let mut registry = AdapterRegistry::new();
        registry.register(CodexAdapter::with_root(home));
        let listed = registry.list_sessions(Provider::Codex, None).unwrap();
        (registry, ordered_candidates(&listed, None))
    }

    #[test]
    fn restored_codex_rollout_replaces_search_text_even_with_a_delayed_writer() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("codex");
        let store = Store::open(temp.path().join("store.sqlite3")).unwrap();
        let index = |registry: &AdapterRegistry, candidates: &[IndexCandidate]| {
            index_candidates(
                registry,
                &store,
                candidates.to_vec(),
                false,
                &|| false,
                &mut |_| {},
            )
            .unwrap()
        };
        let (registry, original) = write_codex_rollout(&home, Some("oldneedle"), 1_800_100_000);
        index(&registry, &original);
        let observed = store
            .observe_selected_session(&original[0].session)
            .unwrap();
        let guard = source_guard(Provider::Codex, observed.state.as_ref());
        let delayed = prepare_session(&registry, &original[0], guard, None, Some(&store)).unwrap();

        let (registry, restored) = write_codex_rollout(&home, Some("backupneedle"), 1_800_000_000);
        let error = prepare_session(&registry, &original[0], guard, None, Some(&store))
            .err()
            .expect("discovery now refers to a replaced rollout");
        assert!(retryable_read_failure(&error));
        original[0].record_failure(
            &registry,
            &store,
            guard,
            &anyhow::anyhow!("delayed read failure"),
        );
        assert!(
            store
                .trajectory_index_failure(&original[0].session)
                .unwrap()
                .is_none()
        );
        let restored_time = restored[0].updated_at.unwrap();
        assert!(
            !store
                .session_trajectory_is_current(&restored[0].session, restored_time)
                .unwrap()
        );
        assert!(
            !store
                .session_trajectory_source_is_current(&restored[0].session, restored_time)
                .unwrap()
        );
        assert_eq!(index(&registry, &restored).indexed, 1);
        assert!(matches!(
            delayed.store(&store),
            Err(omnis_store::StoreError::TrajectorySourceChanged)
        ));
        let mut recovered = IndexSummary::default();
        let mut updates = IndexProgress::default();
        flush_batch(
            &registry,
            &store,
            &mut vec![delayed],
            &mut recovered,
            &mut updates,
        );
        assert_eq!(recovered.indexed, 1);
        assert_eq!(
            updates
                .titles
                .into_iter()
                .map(|entry| (entry.session, entry.title.unwrap()))
                .collect::<Vec<_>>(),
            [(restored[0].session.clone(), "backupneedle".to_owned())]
        );
        let hits = |term| store.search_session_trajectories(term, 10).unwrap();
        assert!(hits("oldneedle").is_empty());
        assert_eq!(hits("backupneedle"), [restored[0].session.clone()]);
        assert_eq!(index(&registry, &restored).stale, 0);

        let (registry, header_only) = write_codex_rollout(&home, None, 1_799_900_000);
        let mut updates = Vec::new();
        index_candidates(
            &registry,
            &store,
            header_only,
            false,
            &|| false,
            &mut |progress| updates.extend(progress.titles),
        )
        .unwrap();
        assert_eq!(
            updates
                .into_iter()
                .map(|entry| (entry.session, entry.title))
                .collect::<Vec<_>>(),
            [(restored[0].session.clone(), None)]
        );
    }

    #[test]
    fn failed_batch_retries_each_session_without_losing_its_neighbors() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let database = temporary.path().join("store.sqlite3");
        let store = Store::open(&database).expect("synthetic store");
        let writer = rusqlite::Connection::open(&database).expect("second store connection");
        writer
            .execute_batch(
                "CREATE TRIGGER reject_one_index BEFORE INSERT ON session_trajectories
                 WHEN NEW.session_id = 'broken'
                 BEGIN SELECT RAISE(ABORT, 'synthetic write failure'); END;",
            )
            .expect("synthetic write failure trigger");
        let now = Utc::now();
        let prepared = |id: &str| {
            let text = format!("Synthetic searchable content for {id}");
            PreparedIndex {
                workspace: None,
                guard: TrajectorySourceGuard::NewerSource,
                candidate: IndexCandidate {
                    session: SessionRef::new(Provider::Codex, id),
                    updated_at: Some(now),
                    source_path: None,
                    source_revision: SourceRevision::default(),
                    metadata: None,
                },
                document: SearchDocument {
                    source_byte_count: text.len(),
                    indexed_byte_count: text.len(),
                    text,
                    truncated: false,
                    source_complete: true,
                    truncation_strategy: omnis_core::SearchTruncationStrategy::None,
                },
                source_updated_at: now,
                source_complete: true,
                source_revision: SourceRevision::default(),
                origin: SessionTrajectoryOrigin::Native,
                native_name: None,
                title: IndexedTitle {
                    current: Some(id.to_owned()),
                    authoritative: true,
                    ..Default::default()
                },
            }
        };
        let mut batch = vec![prepared("first"), prepared("broken"), prepared("last")];
        let mut summary = IndexSummary::default();
        let mut updates = IndexProgress::default();

        flush_batch(
            &AdapterRegistry::new(),
            &store,
            &mut batch,
            &mut summary,
            &mut updates,
        );

        assert!(batch.is_empty());
        assert_eq!((summary.indexed, summary.failed), (2, 1));
        assert_eq!(
            updates
                .titles
                .into_iter()
                .map(|entry| (entry.session, entry.title.unwrap()))
                .collect::<Vec<_>>(),
            vec![
                (
                    SessionRef::new(Provider::Codex, "first"),
                    "first".to_owned()
                ),
                (SessionRef::new(Provider::Codex, "last"), "last".to_owned()),
            ]
        );
        assert_eq!(
            store
                .search_session_trajectories("searchable", 10)
                .expect("search successful neighbors"),
            vec![
                SessionRef::new(Provider::Codex, "first"),
                SessionRef::new(Provider::Codex, "last"),
            ]
        );
    }

    #[test]
    fn full_reads_of_sources_reporting_omitted_events_stay_current() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let sessions = temporary.path().join("codex/sessions/2026/01/01");
        fs::create_dir_all(&sessions).expect("codex sessions");
        let id = "019f0000-0000-7000-8000-000000000021";
        let mut records = vec![
            json!({"type":"session_meta","timestamp":"2026-01-01T00:00:00Z","payload":{"id":id,"cwd":temporary.path()}}),
            json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Synthetic toolheavy request"}]}}),
        ];
        // More tool results than the adapter retains, so a full read reports omitted events.
        records.extend((0..300).map(|index| {
            json!({"type":"response_item","payload":{"type":"function_call_output","call_id":index.to_string(),"output":"synthetic output"}})
        }));
        let mut rollout = records
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        rollout.push('\n');
        fs::write(
            sessions.join(format!("rollout-2026-01-01T00-00-00-{id}.jsonl")),
            rollout,
        )
        .expect("synthetic rollout");
        let mut registry = AdapterRegistry::new();
        registry.register(CodexAdapter::with_root(temporary.path().join("codex")));
        let store = Store::open(temporary.path().join("store.sqlite3")).expect("synthetic store");
        let (listed, _) = registry
            .list_sessions_with_notes(Provider::Codex, None)
            .expect("list synthetic sessions");
        let candidates = ordered_candidates(&listed, None);

        let first = index_candidates(
            &registry,
            &store,
            candidates.clone(),
            false,
            &|| false,
            &mut |_| {},
        )
        .expect("index synthetic session");
        let again = index_candidates(&registry, &store, candidates, false, &|| false, &mut |_| {})
            .expect("reindex synthetic session");

        assert_eq!((first.stale, first.indexed), (1, 1));
        assert_eq!(again.stale, 0);
        let matches = store
            .search_session_trajectory_matches("toolheavy", 10)
            .expect("search index");
        assert!(
            matches[0]
                .truncation_strategy
                .starts_with("source_incomplete")
        );
        assert!(!matches[0].complete);
        // Omitted events can sit anywhere, so coverage must not claim head and tail.
        assert_eq!(crate::search_coverage(&matches[0]), "preview");
    }

    #[test]
    fn stop_request_ends_indexing_at_a_session_boundary_and_next_run_continues() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let workspace = temporary.path().join("workspace");
        let sessions = temporary.path().join("codex/sessions/2026/01/01");
        fs::create_dir_all(&workspace).expect("workspace");
        fs::create_dir_all(&sessions).expect("codex sessions");
        for index in 1..=3 {
            let id = format!("019f0000-0000-7000-8000-00000000001{index}");
            fs::write(
                sessions.join(format!("rollout-2026-01-01T00-00-0{index}-{id}.jsonl")),
                format!(
                    "{}\n{}\n",
                    json!({"type":"session_meta","timestamp":"2026-01-01T00:00:00Z","payload":{"id":id,"cwd":workspace}}),
                    json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":format!("Synthetic request {index}")}]}}),
                ),
            )
            .expect("synthetic rollout");
        }
        let mut registry = AdapterRegistry::new();
        registry.register(CodexAdapter::with_root(temporary.path().join("codex")));
        let store = Store::open(temporary.path().join("store.sqlite3")).expect("synthetic store");
        let (listed, _) = registry
            .list_sessions_with_notes(Provider::Codex, None)
            .expect("list synthetic sessions");
        let candidates = ordered_candidates(&listed, None);
        let checks = Cell::new(0);

        // A Ctrl+C that lands while the first session indexes is seen before the second starts.
        let stopped = index_candidates(
            &registry,
            &store,
            candidates.clone(),
            false,
            &|| {
                checks.set(checks.get() + 1);
                checks.get() > 1
            },
            &mut |_| {},
        )
        .expect("interrupted index");

        assert!(stopped.stopped);
        assert_eq!((stopped.stale, stopped.indexed), (3, 1));
        assert_eq!(
            pending_summary(&registry, &store, candidates.clone())
                .expect("pending summary")
                .stale,
            2
        );
        let resumed =
            index_candidates(&registry, &store, candidates, false, &|| false, &mut |_| {})
                .expect("resumed index");
        assert!(!resumed.stopped);
        assert_eq!((resumed.stale, resumed.indexed), (2, 2));
    }

    /// Returns an empty session for any ID as `provider`, and records which threads read.
    struct ReaderProbe {
        inner: CodexAdapter,
        provider: Provider,
        readers: Arc<Mutex<Vec<thread::ThreadId>>>,
        batches: Arc<Mutex<Vec<(usize, usize)>>>,
    }

    impl ProviderAdapter for ReaderProbe {
        fn provider(&self) -> Provider {
            self.provider
        }

        fn probe(&self) -> ProviderInstallation {
            self.inner.probe()
        }

        fn list_sessions(&self, _project: Option<&Path>) -> Result<Vec<NativeSession>> {
            Ok(Vec::new())
        }

        fn index_batch(&self, ids: &[&str]) -> Option<Box<IndexSessionReader<'_>>> {
            assert_eq!(self.provider, Provider::Codex);
            let mut batches = self.batches.lock().unwrap();
            let batch = batches.len();
            batches.push((ids.len(), 0));
            Some(Box::new(move |session, path, budget| {
                self.batches.lock().unwrap()[batch].1 += 1;
                self.index_session(session, path, budget)
            }))
        }

        fn read_session(&self, session: &SessionRef) -> Result<CanonicalSnapshot> {
            self.readers
                .lock()
                .expect("reader list")
                .push(thread::current().id());
            let captured_at = DateTime::<Utc>::UNIX_EPOCH;
            Ok(CanonicalSnapshot {
                schema_version: omnis_ir::SCHEMA_VERSION.to_owned(),
                session: session.clone(),
                thread_id: uuid::Uuid::nil(),
                branch_id: uuid::Uuid::nil(),
                title: None,
                captured_at,
                workspace: omnis_ir::WorkspaceSnapshot {
                    schema_version: omnis_ir::SCHEMA_VERSION.to_owned(),
                    captured_at,
                    root: PathBuf::new(),
                    current_dir: PathBuf::new(),
                    git: omnis_ir::GitState::default(),
                    instruction_files: Vec::new(),
                    environment_names: Vec::new(),
                    available_tools: Vec::new(),
                },
                events: Vec::new(),
            })
        }

        fn new_session_plan(&self, target: &LaunchTarget) -> Result<LaunchPlan> {
            self.inner.new_session_plan(target)
        }

        fn launch_plan(&self, session: &SessionRef, target: &LaunchTarget) -> Result<LaunchPlan> {
            self.inner.launch_plan(session, target)
        }
    }

    /// A store, a registry with a [`ReaderProbe`], and synthetic candidates.
    struct ReaderProbeFixture {
        store: Store,
        registry: AdapterRegistry,
        readers: Arc<Mutex<Vec<thread::ThreadId>>>,
        batches: Arc<Mutex<Vec<(usize, usize)>>>,
        candidates: Vec<IndexCandidate>,
        _temporary: tempfile::TempDir,
    }

    impl ReaderProbeFixture {
        fn new(provider: Provider) -> Self {
            let temporary = tempfile::tempdir().expect("temporary directory");
            let readers = Arc::new(Mutex::new(Vec::new()));
            let batches = Arc::new(Mutex::new(Vec::new()));
            let mut registry = AdapterRegistry::new();
            registry.register(ReaderProbe {
                inner: CodexAdapter::with_root(temporary.path().join("codex")),
                provider,
                readers: Arc::clone(&readers),
                batches: Arc::clone(&batches),
            });
            Self {
                store: Store::open(temporary.path().join("store.sqlite3"))
                    .expect("synthetic store"),
                registry,
                readers,
                batches,
                candidates: (0..48)
                    .map(|index| IndexCandidate {
                        session: SessionRef::new(provider, format!("synthetic-{index}")),
                        updated_at: Some(DateTime::<Utc>::UNIX_EPOCH),
                        source_path: None,
                        source_revision: SourceRevision::default(),
                        metadata: None,
                    })
                    .collect(),
                _temporary: temporary,
            }
        }
    }

    #[test]
    fn codex_index_pass_shares_one_reader_across_its_stale_sessions() -> Result<()> {
        let fixture = ReaderProbeFixture::new(Provider::Codex);
        for count in [2, 3, 3] {
            index_candidates(
                &fixture.registry,
                &fixture.store,
                fixture.candidates[..count].to_vec(),
                false,
                &|| false,
                &mut |_| {},
            )?;
        }
        assert_eq!(*fixture.batches.lock().unwrap(), [(2, 2), (1, 1)]);
        assert_eq!(fixture.readers.lock().unwrap().len(), 3);
        Ok(())
    }

    #[test]
    fn database_providers_are_read_by_the_calling_thread() {
        let fixture = ReaderProbeFixture::new(Provider::Hermes);

        let summary = index_candidates(
            &fixture.registry,
            &fixture.store,
            fixture.candidates,
            false,
            &|| false,
            &mut |_| {},
        )
        .expect("index synthetic sessions");

        assert_eq!((summary.indexed, summary.failed), (48, 0));
        assert_eq!(
            *fixture.readers.lock().expect("reader list"),
            vec![thread::current().id(); 48]
        );
    }

    #[test]
    fn an_unchanged_child_indexes_again_only_when_its_missing_ancestor_returns() -> Result<()> {
        let root = tempfile::tempdir()?;
        let sessions = root.path().join("sessions");
        fs::create_dir(&sessions)?;
        let parent = uuid::Uuid::from_u128(1);
        let child = uuid::Uuid::from_u128(2);
        let cycle = uuid::Uuid::from_u128(3);
        let file = |id| sessions.join(format!("rollout-2026-01-01T00-00-00-{id}.jsonl"));
        let header = |id, ordinal, base| {
            json!({"type":"session_meta","ordinal":ordinal,"payload":{"id":id,
                "history_mode":"paginated","history_base":base}})
        };
        let message = json!({"type":"response_item","ordinal":1,"payload":{"type":"message",
            "role":"user","content":[{"type":"input_text","text":"inheritedneedle"}]}});
        let inherited = format!(
            "{}\n{message}\n",
            header(parent, 0, serde_json::Value::Null)
        );
        let base = |id| json!({"thread_id":id,"end_ordinal_exclusive":2,"end_byte_offset":inherited.len()});
        fs::write(file(child), format!("{}\n", header(child, 2, base(parent))))?;
        fs::write(file(cycle), format!("{}\n", header(cycle, 2, base(cycle))))?;

        let mut registry = AdapterRegistry::new();
        registry.register(CodexAdapter::with_root(root.path()));
        let store = Store::open(root.path().join("omni.sqlite3"))?;
        let sessions = registry.list_sessions(Provider::Codex, None)?;
        let pass = || {
            index_candidates(
                &registry,
                &store,
                ordered_candidates(&sessions, None),
                false,
                &|| false,
                &mut |_| {},
            )
        };
        assert_eq!(pass()?.indexed, 2);
        let child_ref = SessionRef::new(Provider::Codex, child.to_string());
        let state = store.trajectory_index_states()?[&child_ref];
        assert!(state.source_complete);
        assert_eq!(state.source_revision.missing_ancestor, Some(parent));
        assert_eq!(pass()?.stale, 0);
        fs::write(file(parent), inherited)?;
        assert_eq!(pass()?.indexed, 1);
        assert_eq!(pass()?.stale, 0);
        let matches = store.search_session_trajectory_matches("inheritedneedle", 10)?;
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].session.id, child.to_string());
        Ok(())
    }

    #[test]
    fn a_stopped_run_reads_no_further_database_session() {
        let fixture = ReaderProbeFixture::new(Provider::Hermes);
        let stop_checks = Cell::new(0);

        // The run stops when the second session has its turn.
        let summary = index_candidates(
            &fixture.registry,
            &fixture.store,
            fixture.candidates,
            false,
            &|| {
                stop_checks.set(stop_checks.get() + 1);
                stop_checks.get() > 1
            },
            &mut |_| {},
        )
        .expect("index synthetic sessions");

        assert!(summary.stopped);
        assert_eq!(summary.indexed, 1);
        assert_eq!(fixture.readers.lock().expect("reader list").len(), 1);
    }

    /// Reads through Codex, failing while `fail` is set, and counts read attempts.
    #[test]
    fn deferred_read_error_does_not_suppress_a_restored_codex_rollout() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let store = Store::open(root.path().join("store.sqlite"))?;
        let (registry, candidates) =
            write_codex_rollout(root.path(), Some("replacement prompt"), 1_800_000_000);
        let path = candidates[0].source_path.as_ref().unwrap();
        let modified = fs::metadata(path)?.modified()?;
        let replacement = root.path().join("restored.jsonl");
        let broken = fs::read_to_string(path)?
            + r#"{"type":"event_msg","payload":{"type":"thread_rolled_back","num_turns":null}}"#
            + "\n";
        for (target, contents) in [
            (path, broken.clone()),
            (&replacement, broken.replace("null", "0   ")),
        ] {
            fs::write(target, contents)?;
            File::options()
                .write(true)
                .open(target)?
                .set_modified(modified)?;
        }

        let refreshed = || -> anyhow::Result<Vec<IndexCandidate>> {
            let native = registry
                .find_native_session(Provider::Codex, &candidates[0].session.id)?
                .unwrap();
            Ok(vec![IndexCandidate::from_session(&native).unwrap()])
        };
        // The writer polls for a stop after receiving the worker's failed read.
        let changed = index_candidates(
            &registry,
            &store,
            refreshed()?,
            false,
            &|| {
                fs::rename(&replacement, path)
                    .expect("restore before the writer records the failure");
                false
            },
            &mut |_| {},
        )?;
        assert_eq!(changed.failed, 1);
        let next = index_candidates(
            &registry,
            &store,
            refreshed()?,
            false,
            &|| false,
            &mut |_| {},
        )?;
        assert_eq!((next.indexed, next.failed_skipped), (1, 0));
        Ok(())
    }

    struct FlakyReads {
        inner: CodexAdapter,
        fail: Arc<AtomicBool>,
        reads: Arc<AtomicUsize>,
    }

    impl ProviderAdapter for FlakyReads {
        fn provider(&self) -> Provider {
            self.inner.provider()
        }

        fn probe(&self) -> ProviderInstallation {
            self.inner.probe()
        }

        fn list_sessions(&self, project: Option<&Path>) -> Result<Vec<NativeSession>> {
            self.inner.list_sessions(project)
        }

        fn find_native_session(&self, id: &str) -> Result<Option<NativeSession>> {
            self.inner.find_native_session(id)
        }

        fn read_session(&self, session: &SessionRef) -> Result<CanonicalSnapshot> {
            self.inner.read_session(session)
        }

        fn index_session(
            &self,
            session: &SessionRef,
            source_path: Option<&Path>,
            full_read_bytes: u64,
        ) -> Result<IndexRead> {
            assert!(
                source_path.is_some(),
                "indexer omitted discovered source path"
            );
            self.reads.fetch_add(1, Ordering::Relaxed);
            if self.fail.load(Ordering::Relaxed) {
                anyhow::bail!("synthetic unreadable source");
            }
            self.inner
                .index_session(session, source_path, full_read_bytes)
        }

        fn new_session_plan(&self, target: &LaunchTarget) -> Result<LaunchPlan> {
            self.inner.new_session_plan(target)
        }

        fn launch_plan(&self, session: &SessionRef, target: &LaunchTarget) -> Result<LaunchPlan> {
            self.inner.launch_plan(session, target)
        }
    }

    #[test]
    fn unreadable_sessions_are_skipped_until_their_source_changes_or_a_retry_is_requested() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let sessions = temporary.path().join("codex/sessions/2026/01/01");
        fs::create_dir_all(&sessions).expect("codex sessions");
        let id = "019f0000-0000-7000-8000-000000000031";
        fs::write(
            sessions.join(format!("rollout-2026-01-01T00-00-00-{id}.jsonl")),
            format!(
                "{}\n{}\n",
                json!({"type":"session_meta","timestamp":"2026-01-01T00:00:00Z","payload":{"id":id,"cwd":temporary.path()}}),
                json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Synthetic request"}]}}),
            ),
        )
        .expect("synthetic rollout");
        let fail = Arc::new(AtomicBool::new(true));
        let reads = Arc::new(AtomicUsize::new(0));
        let mut registry = AdapterRegistry::new();
        registry.register(FlakyReads {
            inner: CodexAdapter::with_root(temporary.path().join("codex")),
            fail: Arc::clone(&fail),
            reads: Arc::clone(&reads),
        });
        let store = Store::open(temporary.path().join("store.sqlite3")).expect("synthetic store");
        let (listed, _) = registry
            .list_sessions_with_notes(Provider::Codex, None)
            .expect("list synthetic sessions");
        let candidates = ordered_candidates(&listed, None);
        assert!(candidates[0].updated_at.is_some());
        // (stale, indexed, failed, failed_skipped, reads so far)
        let pass = |candidates: &[IndexCandidate], retry_failed| {
            let summary = index_candidates(
                &registry,
                &store,
                candidates.to_vec(),
                retry_failed,
                &|| false,
                &mut |_| {},
            )
            .expect("index synthetic session");
            (
                summary.stale,
                summary.indexed,
                summary.failed,
                summary.failed_skipped,
                reads.load(Ordering::Relaxed),
            )
        };

        assert_eq!(pass(&candidates, false), (1, 0, 1, 0, 1));
        assert_eq!(pass(&candidates, false), (0, 0, 0, 1, 1));
        assert_eq!(
            pending_summary(&registry, &store, candidates.clone())
                .expect("pending summary")
                .failed_skipped,
            1
        );

        let modified = candidates[0].updated_at.unwrap() + chrono::Duration::seconds(1);
        File::options()
            .write(true)
            .open(candidates[0].source_path.as_ref().unwrap())
            .unwrap()
            .set_modified(modified.into())
            .unwrap();
        let updated = registry
            .find_native_session(Provider::Codex, id)
            .unwrap()
            .unwrap();
        let changed = ordered_candidates(&[updated], None);
        assert_eq!(pass(&changed, false), (1, 0, 1, 0, 2));
        assert_eq!(pass(&changed, false), (0, 0, 0, 1, 2));

        // A retry whose read succeeds clears the record even when storing the document fails.
        fail.store(false, Ordering::Relaxed);
        let writer = rusqlite::Connection::open(temporary.path().join("store.sqlite3"))
            .expect("second store connection");
        writer
            .execute_batch(
                "CREATE TRIGGER synthetic_write_failure BEFORE INSERT ON session_trajectories
                 BEGIN SELECT RAISE(ABORT, 'synthetic write failure'); END;",
            )
            .expect("write failure trigger");
        assert_eq!(pass(&changed, true), (1, 0, 1, 0, 3));
        assert!(
            store
                .trajectory_index_failures()
                .expect("recorded failures")
                .is_empty()
        );
        writer
            .execute_batch("DROP TRIGGER synthetic_write_failure;")
            .expect("drop write failure trigger");
        assert_eq!(pass(&changed, false), (1, 1, 0, 0, 4));
        assert_eq!(pass(&changed, false), (0, 0, 0, 0, 4));
    }
}
