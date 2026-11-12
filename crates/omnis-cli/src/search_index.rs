mod selection;

pub(crate) use selection::{FileCheck, FileSelections};

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
    AdapterRegistry, IndexRead, IndexedSearchDocument, NativeSession, PriorSearchDocument,
    RollbackTitle,
};
use omnis_core::{
    SearchDocument, SessionWorkspaceMatcher, imported_search_document_version,
    indexed_search_title, search_document_version, trajectory_search_document,
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
    catalog: Option<IndexedSession>,
    document_version: u32,
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

    fn record_codex_failure(
        &self,
        registry: &AdapterRegistry,
        store: &Store,
        guard: TrajectorySourceGuard,
        error: &anyhow::Error,
    ) -> Option<IndexedTrajectoryTitle> {
        let revision = self.source_revision;
        let compressed = self
            .source_path
            .as_deref()
            .is_some_and(omnis_adapters::CodexAdapter::is_compressed_rollout);
        if retryable_read_error(error, compressed)
            || (self.session.provider == Provider::Codex
                && self.selected_codex(registry, revision).is_none())
        {
            return None;
        }
        let failure = TrajectoryIndexFailure {
            source_updated_at: self.updated_at,
            source_file: revision.file,
            source_contents: revision.contents,
            source_time_precise: true,
            document_version: self.document_version,
        };
        store
            .record_observed_trajectory_index_failure(&self.session, &failure, guard)
            .ok()?;
        (self.session.provider == Provider::Codex)
            .then(|| store.indexed_trajectory_title(&self.session).ok().flatten())
            .flatten()
    }
    fn can_upgrade_preview(&self) -> bool {
        if self.session.provider == Provider::Hermes
            || (self.session.provider == Provider::Codex
                && self
                    .source_path
                    .as_deref()
                    .is_some_and(omnis_adapters::CodexAdapter::is_compressed_rollout))
        {
            // The physical file size cannot tell if this session fits the full-read budget.
            return false;
        }
        !self
            .source_path
            .as_deref()
            .and_then(|path| fs::metadata(path).ok())
            .is_some_and(|metadata| metadata.is_file() && metadata.len() > FULL_READ_SOURCE_BYTES)
    }

    // Sessions without a discovered source file are skipped so oversized files are never read
    // whole. Reading an OpenCode session spawns its CLI, so those index when opened instead.
    pub(crate) fn from_session(session: &NativeSession) -> Option<Self> {
        let provider = session.session.provider;
        let readable = provider == Provider::Imported
            || (provider != Provider::OpenCode && session.source_path.is_some());
        readable.then(|| Self {
            catalog: matches!(
                provider,
                Provider::Antigravity | Provider::AntigravityDesktop
            )
            .then(|| crate::indexed_session_metadata(session)),
            session: session.session.clone(),
            updated_at: session.updated_at,
            source_path: session.source_path.clone(),
            source_revision: crate::selected_search_source(session, false).unwrap_or_default(),
            document_version: search_document_version(provider),
            metadata: (provider == Provider::Codex)
                .then(|| crate::indexed_session_metadata(session)),
        })
    }

    fn file_check(&self, guard: TrajectorySourceGuard) -> FileCheck<'_> {
        FileCheck {
            session: &self.session,
            path: self.source_path.as_deref(),
            revision: self.source_revision,
            catalog: self.catalog.as_ref(),
            guard,
        }
    }

    fn clear_previous_failure(
        &self,
        store: &Store,
        failures: &HashMap<SessionRef, TrajectoryIndexFailure>,
    ) {
        if (self.source_revision.file.is_none() || self.session.provider == Provider::Codex)
            && let Some(failure) = failures.get(&self.session)
        {
            let _ = store.clear_observed_trajectory_index_failure(&self.session, failure);
        }
    }

    fn record_failure(
        &self,
        selections: &mut FileSelections<'_>,
        store: &Store,
        guard: TrajectorySourceGuard,
    ) -> omnis_store::Result<()> {
        selections.write(&[self.file_check(guard)], |verify| {
            store.record_observed_trajectory_index_failure_checked(
                &self.session,
                &TrajectoryIndexFailure {
                    source_updated_at: self.updated_at,
                    source_time_precise: true,
                    document_version: self.document_version,
                    source_file: self.source_revision.file,
                    source_contents: self.source_revision.contents,
                },
                guard,
                verify,
            )
        })
    }
}

/// Records a structural failure against the exact file examined by the full native reader.
pub(crate) fn record_strict_read_failure<'read>(
    selections: &mut FileSelections<'_>,
    store: &Store,
    guard: TrajectorySourceGuard,
    error: &'read anyhow::Error,
) -> omnis_store::Result<Option<&'read NativeSession>> {
    let Some(invalid) = error.downcast_ref::<omnis_adapters::InvalidPiTree>() else {
        return Ok(None);
    };
    let candidate = IndexCandidate::from_session(&invalid.source)
        .ok_or(omnis_store::StoreError::InvalidSessionReference)?;
    candidate.record_failure(selections, store, guard)?;
    Ok(Some(&invalid.source))
}

pub(crate) fn indexed_title(
    session: &SessionRef,
    snapshot: &omnis_ir::CanonicalSnapshot,
    prefix_end: Option<u64>,
) -> Option<String> {
    if matches!(
        session.provider,
        Provider::Antigravity | Provider::AntigravityDesktop
    ) {
        let (sequence, title) = omnis_core::search_title_opener_from_messages(&snapshot.events)?;
        title.filter(|_| prefix_end.is_none_or(|end| sequence < end))
    } else {
        indexed_search_title(session.provider, snapshot)
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct IndexProgress {
    unchanged: usize,
    pub(crate) indexed: usize,
    pub(crate) total: usize,
    pub(crate) failed: usize,
    pub(crate) titles: Vec<IndexedTrajectoryTitle>,
    pub(crate) workspaces: Vec<(SessionRef, omnis_store::CodexWorkspaceUpdate)>,
    pub(crate) source_titles: Vec<(SessionRef, omnis_store::IndexedTitle)>,
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

// Restoring a transcript can move its modification time backwards.
pub(crate) fn source_time_changed(
    provider: Provider,
    current: DateTime<Utc>,
    cached: &TrajectoryIndexState,
) -> bool {
    if matches!(
        provider,
        Provider::Codex | Provider::Antigravity | Provider::AntigravityDesktop
    ) {
        current.timestamp_millis() != cached.source_updated_at.timestamp_millis()
    } else if TrajectorySourceGuard::uses_file_time(provider) {
        !cached.source_time_precise || current != cached.source_updated_at
    } else {
        current.timestamp_millis() > cached.source_updated_at.timestamp_millis()
    }
}

pub(crate) fn source_guard(
    provider: Provider,
    prior: Option<&TrajectoryIndexState>,
) -> TrajectorySourceGuard {
    if provider == Provider::Codex {
        TrajectorySourceGuard::ObservedRevision(prior.copied())
    } else {
        TrajectorySourceGuard::NewerSource
    }
}

pub(crate) fn projection_read_budget(
    guard: TrajectorySourceGuard,
    updated_at: Option<DateTime<Utc>>,
    revision: SourceRevision,
) -> u64 {
    let TrajectorySourceGuard::ObservedRevision(Some(prior)) = guard else {
        return FULL_READ_SOURCE_BYTES;
    };
    if prior.source_complete
        && prior.document_version < search_document_version(Provider::Codex)
        && updated_at
            .is_some_and(|timestamp| !source_time_changed(Provider::Codex, timestamp, &prior))
        && revision.contents.is_some()
        && revision.matches_file_contents(prior.source_revision)
    {
        u64::MAX
    } else {
        FULL_READ_SOURCE_BYTES
    }
}

fn legacy_projection_source(
    guard: TrajectorySourceGuard,
    updated_at: Option<DateTime<Utc>>,
    revision: SourceRevision,
) -> Option<SourceRevision> {
    let TrajectorySourceGuard::ObservedRevision(Some(prior)) = guard else {
        return None;
    };
    if updated_at.is_some_and(|time| source_time_changed(Provider::Codex, time, &prior)) {
        return None;
    }
    if matches!(prior.document_version, 4 | 5)
        && prior.source_complete
        && prior.source_revision == SourceRevision::default()
    {
        return Some(prior.source_revision);
    }
    // Retries and picker previews can revisit a wider partial projection after its tail repair.
    (!prior.source_complete
        && (6..=search_document_version(Provider::Codex)).contains(&prior.document_version)
        && revision.file.is_some()
        && revision.contents.is_some()
        && revision.matches_file_contents(prior.source_revision))
    .then_some(prior.source_revision)
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
    {
        return Ok(read);
    }
    let Some(source) = legacy_projection_source(guard, Some(time), read.source_revision) else {
        return Ok(read);
    };
    let Some(hash) = store.codex_projection_hash(session, time, source)? else {
        return Ok(read);
    };
    if source.file.is_some() {
        use sha2::{Digest, Sha256};
        let sample = trajectory_search_document(&read.snapshot);
        if Sha256::digest(sample.text.as_bytes())[..] == hash {
            return Ok(read);
        }
    }
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

pub(crate) fn observe_source(
    store: &Store,
    session: &SessionRef,
) -> omnis_store::Result<TrajectorySourceGuard> {
    if session.provider == Provider::Codex {
        Ok(source_guard(
            Provider::Codex,
            store.trajectory_index_state(session)?.as_ref(),
        ))
    } else if TrajectorySourceGuard::uses_verified_catalog(session.provider) {
        store.observe_trajectory_document(session)
    } else {
        Ok(TrajectorySourceGuard::NewerSource)
    }
}

pub(crate) fn needs_index(
    candidate: &IndexCandidate,
    state: Option<&TrajectoryIndexState>,
) -> bool {
    let Some(state) = state else {
        return true;
    };
    let version = candidate.document_version;
    if state.document_version > version {
        return false;
    }
    let lost_source = matches!(
        candidate.session.provider,
        Provider::Antigravity | Provider::AntigravityDesktop
    ) && state.source_revision.file.is_some();
    let changed_source = (candidate.source_revision.file.is_some() || lost_source)
        && !candidate
            .source_revision
            .matches_file_contents(state.source_revision);
    state.legacy_tail_receipt.is_some()
        || state.document_version < version
        || (candidate.session.provider == Provider::Codex && (candidate.source_revision.file != state.source_revision.file || candidate.source_revision.contents != state.source_revision.contents))
        || changed_source
        || (candidate.session.provider == Provider::OhMyPi
            && !state.source_complete
            && state.title_authoritative.is_none())
        || (!state.source_time_precise && TrajectorySourceGuard::uses_file_time(candidate.session.provider))
        || candidate.updated_at.is_some_and(|updated_at| {
            source_time_changed(candidate.session.provider, updated_at, state)
        })
        // Shared database size cannot show whether a per-session preview still needs its budget.
        || (!state.source_complete
            && !state.provider_limited
            && candidate.can_upgrade_preview())
}

/// Whether a recorded failure still covers the candidate, so reading it again would repeat it.
///
/// A new document version or a change to the source timestamp or file revision permits a retry.
fn failure_is_current(candidate: &IndexCandidate, failure: &TrajectoryIndexFailure) -> bool {
    failure.document_version >= candidate.document_version
        && candidate.source_revision.file == failure.source_file
        && candidate.source_revision.contents == failure.source_contents
        && (failure.source_time_precise
            || failure.source_updated_at.is_none()
            || !TrajectorySourceGuard::uses_file_time(candidate.session.provider))
        && candidate.updated_at.is_none_or(|updated_at| {
            failure.source_updated_at.is_some_and(|failed_source| {
                if candidate.session.provider == Provider::Codex {
                    updated_at.timestamp_millis() == failed_source.timestamp_millis()
                } else if TrajectorySourceGuard::uses_file_time(candidate.session.provider) {
                    updated_at == failed_source
                } else {
                    updated_at.timestamp_millis() <= failed_source.timestamp_millis()
                }
            })
        })
}

/// Whether a read failed only because a database was busy or a source changed while it was read,
/// so a later pass can succeed even when the source time does not move.
pub(crate) fn retryable_read_failure(error: &anyhow::Error) -> bool {
    retryable_read_error(error, false)
}

fn retryable_read_error(error: &anyhow::Error, compressed_rollout: bool) -> bool {
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
            || cause.is::<omnis_adapters::IncompleteTranscript>()
            // A temporary I/O failure says nothing about the session contents.
            || cause
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| matches!(error.kind(), io::ErrorKind::StorageFull | io::ErrorKind::Interrupted)
                    || (!compressed_rollout && error.kind() == io::ErrorKind::UnexpectedEof))
    })
}

/// Builds index candidates with the current workspace first, then newest sessions first.
pub(crate) fn ordered_candidates(
    sessions: &[NativeSession],
    current_project: Option<&Path>,
) -> Vec<IndexCandidate> {
    let mut matcher = current_project.map(SessionWorkspaceMatcher::new);
    let mut ordered = sessions
        .iter()
        .filter_map(|session| {
            let candidate = IndexCandidate::from_session(session)?;
            let current = matcher
                .as_mut()
                .zip(session.project_path.as_deref())
                .is_some_and(|(matcher, path)| matcher.matches(session.session.provider, path));
            Some((Reverse(current), Reverse(candidate.updated_at), candidate))
        })
        .collect::<Vec<_>>();
    ordered.sort_by_key(|(current, updated_at, _)| (*current, *updated_at));
    ordered
        .into_iter()
        .map(|(_, _, candidate)| candidate)
        .collect()
}

#[derive(Clone, Copy)]
struct ReadBudget {
    bytes: u64,
    prior_document: Option<PriorSearchDocument>,
}

impl From<u64> for ReadBudget {
    fn from(bytes: u64) -> Self {
        Self {
            bytes,
            prior_document: None,
        }
    }
}

fn read_budget(
    candidate: &IndexCandidate,
    guard: TrajectorySourceGuard,
    prior: Option<&TrajectoryIndexState>,
    failure: Option<&TrajectoryIndexFailure>,
) -> ReadBudget {
    // A rejected duplicate may disappear before a retry selects the original file.
    let retry_full = failure.is_some() && candidate.source_revision.contents.is_some();
    // Read the full source when upgrading a document from a full or provider-limited read.
    let same_antigravity_file = candidate.session.provider == Provider::Antigravity
        && guard.matches_document_file(candidate.source_revision) == Some(true);
    let upgrade_partial = same_antigravity_file
        && prior.is_some_and(|state| {
            state.document_version < candidate.document_version
                && !state.source_complete
                && !state.provider_limited
        });
    let full = retry_full
        || upgrade_partial
        || (candidate.session.provider == Provider::Antigravity
            && matches!(
                guard,
                TrajectorySourceGuard::Verified {
                    fully_scanned: true,
                    ..
                }
            ))
        || prior.is_some_and(|state| {
            (state.source_complete || state.provider_limited)
                && (state.legacy_tail_receipt.is_some()
                    || state.document_version < candidate.document_version
                    || (matches!(
                        candidate.session.provider,
                        Provider::Antigravity | Provider::AntigravityDesktop
                    ) && !candidate
                        .source_revision
                        .matches_file_contents(state.source_revision))
                    || (candidate.source_revision.file.is_some()
                        && (state.source_revision.file.is_none()
                            || state.source_revision.contents.is_none()))
                    || (TrajectorySourceGuard::uses_file_time(candidate.session.provider)
                        && !state.source_time_precise))
        });
    ReadBudget {
        bytes: if full {
            u64::MAX
        } else {
            FULL_READ_SOURCE_BYTES
        },
        // The guard identifies the stored document, including a write after the pending scan.
        prior_document: match guard {
            TrajectorySourceGuard::Verified {
                source_byte_count: Some(byte_count),
                full_or_provider_limited,
                document_hash,
                sampled_file_bytes,
                fully_scanned,
                ..
            } if same_antigravity_file => Some(match (full_or_provider_limited, document_hash) {
                (false, Some(indexed_hash)) => PriorSearchDocument::Partial {
                    byte_count,
                    indexed_hash,
                    sampled_file_bytes,
                    recover_from_full_scan: upgrade_partial
                        && sampled_file_bytes.is_none()
                        && !fully_scanned
                        && !candidate.can_upgrade_preview(),
                },
                (true, Some(indexed_hash)) => PriorSearchDocument::Complete {
                    byte_count,
                    indexed_hash,
                },
                _ => PriorSearchDocument::RequireCompleteTail(byte_count),
            }),
            _ => None,
        },
    }
}

/// Stale candidates in order, the failures they were checked against, and their counts.
struct Pending {
    stale: Vec<(IndexCandidate, Option<TrajectoryIndexState>)>,
    failures: HashMap<SessionRef, TrajectoryIndexFailure>,
    recovered: Vec<(
        IndexCandidate,
        TrajectoryIndexState,
        Option<TrajectoryIndexFailure>,
    )>,
    hidden: Vec<IndexedTrajectoryTitle>,
    summary: IndexSummary,
}

fn imported_document_versions(store: &Store) -> Result<HashMap<String, u32>> {
    let mut versions = HashMap::new();
    for &provider in Provider::ALL {
        let version = imported_search_document_version(provider);
        if version != search_document_version(Provider::Imported) {
            let ids = store.bundle_ids_for_source_provider(provider)?;
            versions.extend(ids.into_iter().map(|id| (id, version)));
        }
    }
    Ok(versions)
}

fn database_catalogs_needing_read(
    registry: &AdapterRegistry,
    store: &Store,
    candidates: &[IndexCandidate],
) -> anyhow::Result<HashSet<SessionRef>> {
    let mut stale = HashSet::new();
    for provider in [Provider::Antigravity, Provider::AntigravityDesktop] {
        let selected: Vec<_> = candidates
            .iter()
            .filter(|candidate| {
                candidate.session.provider == provider
                    && candidate
                        .source_path
                        .as_ref()
                        .is_some_and(|path| path.extension().is_some_and(|ext| ext == "db"))
            })
            .filter_map(|candidate| Some((candidate.catalog.as_ref()?, candidate.document_version)))
            .collect();
        if selected.is_empty() {
            continue;
        }
        let current = if let Ok(Some(home)) = registry.discovery_cache_home(provider) {
            store.current_database_catalogs(provider, &home, &selected)?
        } else {
            HashSet::new()
        };
        stale.extend(
            selected
                .into_iter()
                .filter(|(catalog, _)| !current.contains(&catalog.session))
                .map(|(catalog, _)| catalog.session.clone()),
        );
    }
    Ok(stale)
}

fn pending(
    registry: &AdapterRegistry,
    store: &Store,
    candidates: Vec<IndexCandidate>,
    retry_failed: bool,
) -> Result<Pending> {
    let imported_versions = if candidates
        .iter()
        .any(|candidate| candidate.session.provider == Provider::Imported)
    {
        imported_document_versions(store)?
    } else {
        HashMap::new()
    };
    let states = store.trajectory_index_states()?;
    let failures = store.trajectory_index_failures()?;
    let providers: HashSet<_> = candidates
        .iter()
        .filter(|candidate| {
            candidate.session.provider == Provider::Codex
                || candidate.source_revision.file.is_some()
        })
        .map(|candidate| candidate.session.provider)
        .collect();
    let mut conflicts = database_catalogs_needing_read(registry, store, &candidates)?;
    for provider in providers {
        conflicts.extend(if provider == Provider::Codex {
            store.conflicting_codex_catalog_rows()?
        } else {
            store.conflicting_file_catalog_rows(provider)?
        });
    }
    let mut native_catalog = HashMap::new();
    for provider in [Provider::Antigravity, Provider::AntigravityDesktop] {
        if candidates
            .iter()
            .any(|candidate| candidate.session.provider == provider && candidate.catalog.is_some())
        {
            native_catalog.extend(
                store
                    .indexed_sessions_for_provider(provider)?
                    .into_iter()
                    .map(|row| (row.session.clone(), row)),
            );
        }
    }
    let mut summary = IndexSummary {
        candidates: candidates.len(),
        ..IndexSummary::default()
    };
    let mut stale = Vec::new();
    let mut recovered = Vec::new();
    let mut hidden = Vec::new();
    for mut candidate in candidates {
        if candidate.session.provider == Provider::Imported
            && let Some(&version) = imported_versions.get(&candidate.session.id)
        {
            candidate.document_version = version;
        }
        let prior = states.get(&candidate.session);
        let failure = failures.get(&candidate.session);
        let current_failure =
            failure.is_some_and(|failure| failure_is_current(&candidate, failure));
        let reconcile = candidate.source_revision.file.is_some()
            && prior.is_none_or(|state| state.document_version <= candidate.document_version)
            && (conflicts.contains(&candidate.session) || (failure.is_some() && !current_failure));
        let metadata_changed = candidate.catalog.as_ref().is_some_and(|listed| {
            native_catalog.get(&listed.session).is_none_or(|stored| {
                listed.title != stored.title
                    || listed.project_path != stored.project_path
                    || listed.event_count != stored.event_count
            })
        });
        let rejected_file = current_failure && candidate.source_revision.contents.is_some();
        if candidate.session.provider == Provider::Codex
            && !needs_index(&candidate, prior)
            && let Some(&prior) = prior
        {
            if conflicts.contains(&candidate.session) || (failure.is_some() && !current_failure) {
                recovered.push((candidate, prior, failure.copied()));
            }
            continue;
        }
        if !(needs_index(&candidate, prior) || reconcile || rejected_file || metadata_changed) {
            continue;
        }
        if !retry_failed && current_failure {
            summary.failed_skipped += 1;
            if candidate.session.provider == Provider::Codex {
                hidden.extend(store.indexed_trajectory_title(&candidate.session)?);
            }
        } else {
            stale.push((candidate, prior.copied()));
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
    stale: &[(IndexCandidate, TrajectorySourceGuard, ReadBudget)],
    failures: &HashMap<SessionRef, TrajectoryIndexFailure>,
) -> Result<HashMap<SessionRef, SelectedSessionObservation>> {
    let mut observations = HashMap::new();
    if !stale
        .iter()
        .any(|(candidate, _, _)| candidate.metadata.is_some())
    {
        return Ok(observations);
    }
    let catalog = store
        .indexed_sessions_for_provider(Provider::Codex)?
        .into_iter()
        .map(|row| (row.session.clone(), row))
        .collect::<HashMap<_, _>>();
    for (candidate, guard, _) in stale {
        let Some(metadata) = &candidate.metadata else {
            continue;
        };
        if failures.contains_key(&candidate.session)
            || matches!(guard, TrajectorySourceGuard::ObservedRevision(Some(prior)) if prior.source_revision.file != candidate.source_revision.file)
            || catalog
                .get(&candidate.session)
                .is_some_and(|stored| stored != metadata)
        {
            observations.insert(
                candidate.session.clone(),
                store.observe_selected_session(&candidate.session)?,
            );
        }
    }
    Ok(observations)
}

fn guard_index_reads(
    selections: &mut FileSelections<'_>,
    store: &Store,
    stale: Vec<(IndexCandidate, Option<TrajectoryIndexState>)>,
    failures: &HashMap<SessionRef, TrajectoryIndexFailure>,
) -> Result<Vec<(IndexCandidate, TrajectorySourceGuard, ReadBudget)>> {
    let mut jobs = Vec::with_capacity(stale.len());
    for (candidate, prior) in stale {
        if candidate.session.provider == Provider::Codex {
            let guard = source_guard(Provider::Codex, prior.as_ref());
            let budget =
                projection_read_budget(guard, candidate.updated_at, candidate.source_revision);
            jobs.push((candidate, guard, budget.into()));
            continue;
        }
        let guard = if let Some(file) = candidate.source_revision.file {
            let (guard, different_file) = store.observe_index_file(&candidate.session, file)?;
            if different_file
                && !selections
                    .selected(candidate.file_check(guard))
                    .unwrap_or(false)
            {
                continue;
            }
            guard
        } else if let Some(receipt) = prior.and_then(|state| state.legacy_tail_receipt) {
            // The repaired sample can reveal an earlier timestamp. Replace only the observed preview.
            TrajectorySourceGuard::ObservedDocument(Some(receipt))
        } else if prior.is_none()
            && TrajectorySourceGuard::uses_file_time(candidate.session.provider)
        {
            TrajectorySourceGuard::ObservedDocument(None)
        } else {
            observe_source(store, &candidate.session)?
        };
        let budget = read_budget(
            &candidate,
            guard,
            prior.as_ref(),
            failures.get(&candidate.session),
        );
        jobs.push((candidate, guard, budget));
    }
    Ok(jobs)
}

/// Indexes stale sessions in candidate order, reporting progress and derived titles in batches.
///
/// Later passes skip a failed read until the source or search document version changes.
/// `retry_failed` also retries unchanged sources.
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
    hidden.extend(restore_cached_sources(registry, store, recovered)?);
    report(IndexProgress {
        titles: hidden,
        total: stale.len(),
        ..IndexProgress::default()
    });
    let mut selections = FileSelections::new(registry);
    let stale = guard_index_reads(&mut selections, store, stale, &failures)?;
    summary.failed += summary.stale - stale.len();
    let observations = observe_replacements(store, &stale, &failures)?;
    let mut updates = IndexProgress::default();
    let mut processed = summary.failed;
    let mut last_report = Instant::now();

    let mut batch = IndexBatch::default();
    // `None` leaves a session to the calling thread, which reads it when its turn comes.
    let prepare =
        |(candidate, guard, budget): &(IndexCandidate, TrajectorySourceGuard, ReadBudget)| {
            // Read an oversized full transcript on the calling thread to bound peak memory.
            (reads_transcript_file(candidate.session.provider)
                && legacy_projection_source(
                    *guard,
                    candidate.updated_at,
                    candidate.source_revision,
                )
                .is_none()
                && (budget.bytes == FULL_READ_SOURCE_BYTES || candidate.can_upgrade_preview()))
            .then(|| prepare_session(registry, candidate, *guard, *budget, None))
        };
    prepare_in_order(
        &stale,
        prepare_worker_count(stale.len()),
        &prepare,
        |(candidate, guard, budget), prepared| {
            if stop() {
                batch.flush(&mut selections, store, &mut summary, &mut updates);
                summary.stopped = true;
                return false;
            }
            let result = finish_preparation(registry, store, candidate, *guard, *budget, prepared);
            match result {
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
                    candidate.clear_previous_failure(store, &failures);
                    batch.push(prepared, &mut selections, store, &mut summary, &mut updates);
                }
                Err(error) => {
                    summary.failed += 1;
                    updates.titles.extend(record_index_read_failure(
                        &mut selections,
                        store,
                        candidate,
                        *guard,
                        &error,
                    ));
                }
            }

            processed += 1;
            if batch.full() || last_report.elapsed() >= PROGRESS_INTERVAL {
                batch.flush(&mut selections, store, &mut summary, &mut updates);
            }
            if last_report.elapsed() >= PROGRESS_INTERVAL {
                report(progress(&summary, processed, &mut updates));
                last_report = Instant::now();
            }
            true
        },
    );
    batch.flush(&mut selections, store, &mut summary, &mut updates);
    report(progress(&summary, processed, &mut updates));
    Ok(summary)
}

fn finish_preparation(
    registry: &AdapterRegistry,
    store: &Store,
    candidate: &IndexCandidate,
    guard: TrajectorySourceGuard,
    budget: ReadBudget,
    prepared: Option<Result<PreparedIndex>>,
) -> Result<PreparedIndex> {
    let mut read = prepared
        .unwrap_or_else(|| prepare_session(registry, candidate, guard, budget, Some(store)))?;
    if candidate.session.provider == Provider::Codex {
        read.title.check_withdrawal(
            registry,
            store,
            &candidate.session,
            read.candidate.source_revision,
        )?;
    }
    Ok(read)
}

#[derive(Default)]
struct IndexBatch {
    documents: Vec<PreparedIndex>,
    bytes: usize,
}

impl IndexBatch {
    fn full(&self) -> bool {
        self.documents.len() >= MAX_TRAJECTORY_WRITE_BATCH_DOCUMENTS
            || self.bytes >= MAX_TRAJECTORY_WRITE_BATCH_BYTES
    }

    fn push(
        &mut self,
        prepared: PreparedIndex,
        selections: &mut FileSelections<'_>,
        store: &Store,
        summary: &mut IndexSummary,
        updates: &mut IndexProgress,
    ) {
        let provider = prepared.candidate.session.provider;
        let bytes = prepared.document.indexed_byte_count;
        if let Some(previous) = self.documents.last()
            && ((previous.candidate.session.provider == Provider::Codex)
                != (provider == Provider::Codex)
                || self.documents.len() >= MAX_TRAJECTORY_WRITE_BATCH_DOCUMENTS
                || self.bytes.saturating_add(bytes) > MAX_TRAJECTORY_WRITE_BATCH_BYTES)
        {
            self.flush(selections, store, summary, updates);
        }
        self.bytes = self.bytes.saturating_add(bytes);
        self.documents.push(prepared);
    }

    fn flush(
        &mut self,
        selections: &mut FileSelections<'_>,
        store: &Store,
        summary: &mut IndexSummary,
        updates: &mut IndexProgress,
    ) {
        flush_batch(selections, store, &mut self.documents, summary, updates);
        self.bytes = 0;
    }
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
        ..read.candidate.source_revision
    };
    anyhow::ensure!(
        TrajectorySourceGuard::ObservedRevision(observation.state) == read.guard,
        "cached source changed during indexing"
    );
    let selected = read
        .candidate
        .selected_codex(registry, revision)
        .ok_or(omnis_store::StoreError::TrajectorySourceChanged)?;
    store.upsert_selected_session_document_with_workspace(
        &crate::indexed_session_metadata(&selected),
        observation,
        read.store_document(),
        read.workspace.as_ref(),
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
    selections: &mut FileSelections<'_>,
    store: &Store,
    batch: &mut Vec<PreparedIndex>,
    summary: &mut IndexSummary,
    updates: &mut IndexProgress,
) {
    if batch.is_empty() {
        return;
    }
    if batch[0].candidate.session.provider == Provider::Codex {
        flush_codex_batch(selections.registry, store, batch, summary, updates);
        return;
    }
    let checks: Vec<_> = batch
        .iter()
        .map(|prepared| prepared.candidate.file_check(prepared.guard))
        .collect();
    let documents = batch.iter().map(|prepared| {
        (
            &prepared.candidate.session,
            prepared.store_document(),
            prepared.guard,
            prepared.verified_metadata.as_ref(),
        )
    });
    let stored = selections.write_documents(store, &checks, documents);
    if let Ok(indexed) = stored {
        summary.indexed += indexed;
        for prepared in batch.drain(..) {
            prepared.report_title(selections.registry, store, &mut updates.source_titles);
        }
    } else {
        for mut prepared in batch.drain(..) {
            let mut stored = prepared.store(selections, store);
            if matches!(
                stored,
                Err(omnis_store::StoreError::TrajectorySourceChanged)
            ) {
                // Re-read the live rollout once. The competing writer may have indexed either
                // the restored file or the file it replaced.
                if let Ok(fresh) = reread_after_conflict(selections, store, &prepared.candidate) {
                    prepared = fresh;
                    stored = prepared.store(selections, store);
                }
            }
            if let Ok(indexed) = stored {
                summary.indexed += indexed;
                prepared.report_title(selections.registry, store, &mut updates.source_titles);
            } else {
                summary.failed += 1;
            }
        }
    }
}

fn flush_codex_batch(
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
    let stored = store.upsert_observed_trajectory_documents_with_workspaces(&documents);
    if stored.is_ok() {
        summary.indexed += batch.len();
        for prepared in batch.drain(..) {
            updates.record_workspace(&prepared);
            if let Ok(Some(title)) = store.indexed_trajectory_title(&prepared.candidate.session) {
                updates.titles.push(title);
            }
        }
    } else {
        for mut prepared in batch.drain(..) {
            let mut stored = prepared.store_codex(store);
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
                        TrajectorySourceGuard::ObservedRevision(observation.state),
                        projection_read_budget(
                            TrajectorySourceGuard::ObservedRevision(observation.state),
                            selected.updated_at,
                            selected.source_revision,
                        )
                        .into(),
                        Some(store),
                    )?;
                    read.title.check_withdrawal(
                        registry,
                        store,
                        session,
                        read.candidate.source_revision,
                    )?;
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
                if let Ok(Some(title)) = store.indexed_trajectory_title(&prepared.candidate.session)
                {
                    updates.titles.push(title);
                }
            } else {
                summary.failed += 1;
            }
        }
    }
}

fn record_index_read_failure(
    selections: &mut FileSelections<'_>,
    store: &Store,
    candidate: &IndexCandidate,
    guard: TrajectorySourceGuard,
    error: &anyhow::Error,
) -> Option<IndexedTrajectoryTitle> {
    if candidate.session.provider == Provider::Codex {
        return candidate.record_codex_failure(selections.registry, store, guard, error);
    }
    // If persistence fails, the next pass retries the source.
    match record_strict_read_failure(selections, store, guard, error) {
        Err(omnis_store::StoreError::TrajectorySourceChanged) => {
            // A preview can win the write while the full reader rejects the file.
            let _ = confirm_failed_file(selections, store, candidate);
        }
        Ok(None)
            if !retryable_read_failure(error)
                && !error.is::<omnis_adapters::TranscriptReadLimit>() =>
        {
            let _ = candidate.record_failure(selections, store, guard);
        }
        _ => {}
    }
    None
}

fn confirm_failed_file(
    selections: &mut FileSelections<'_>,
    store: &Store,
    candidate: &IndexCandidate,
) -> anyhow::Result<()> {
    let registry = selections.registry;
    let session = &candidate.session;
    let adapter = registry.adapter(session.provider)?;
    let guard = store.observe_verified_session(session)?;
    let Some(path) = candidate.source_path.as_deref() else {
        return Ok(());
    };
    selections.refresh(session.provider)?;
    if selections
        .selected_session_at(session, path)?
        .is_some_and(|selected| selected.source_revision == Some(candidate.source_revision))
        && let Err(error) = adapter.read_full_index_at(session, Some(path))
        && error
            .downcast_ref::<omnis_adapters::InvalidPiTree>()
            .is_some_and(|invalid| {
                invalid.source.source_revision == Some(candidate.source_revision)
            })
    {
        record_strict_read_failure(selections, store, guard, &error)?;
    }
    Ok(())
}

fn reread_after_conflict(
    selections: &mut FileSelections<'_>,
    store: &Store,
    candidate: &IndexCandidate,
) -> anyhow::Result<PreparedIndex> {
    let registry = selections.registry;
    let session = &candidate.session;
    let guard = if TrajectorySourceGuard::verifies_search_file(session.provider)
        || candidate.source_revision.file.is_some()
    {
        store.observe_verified_session(session)?
    } else {
        observe_source(store, session)?
    };
    selections.refresh(session.provider)?;
    let prior = store.trajectory_index_state(session)?;
    let current = if candidate.source_revision.file.is_some() {
        let path = candidate
            .source_path
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("selected transcript path changed during indexing"))?;
        selections
            .selected_session_at(session, path)?
            .as_ref()
            .and_then(IndexCandidate::from_session)
            .ok_or_else(|| anyhow::anyhow!("selected transcript changed during indexing"))?
    } else {
        candidate.clone()
    };
    prepare_session(
        registry,
        &current,
        guard,
        read_budget(
            &current,
            guard,
            prior.as_ref(),
            store.trajectory_index_failure(session)?.as_ref(),
        ),
        Some(store),
    )
    .inspect_err(|error| {
        let _ = record_strict_read_failure(selections, store, guard, error);
    })
}

impl IndexProgress {
    pub(crate) fn processed(&self) -> usize {
        self.indexed + self.failed + self.unchanged
    }

    fn record_workspace(&mut self, prepared: &PreparedIndex) {
        if let Some(workspace) = &prepared.workspace {
            self.workspaces
                .push((prepared.candidate.session.clone(), workspace.clone()));
        }
    }
}

fn progress(
    summary: &IndexSummary,
    processed: usize,
    updates: &mut IndexProgress,
) -> IndexProgress {
    IndexProgress {
        unchanged: processed.saturating_sub(summary.indexed + summary.failed),
        indexed: summary.indexed,
        total: summary.stale,
        failed: summary.failed,
        ..std::mem::take(updates)
    }
}

#[derive(Default)]
pub(crate) struct IndexedTitle {
    pub(crate) current: Option<String>,
    pub(crate) verified: bool,
    pub(crate) withdrawn: Option<String>,
    pub(crate) local_opener: LocalOpenerEvidence,
    uncertain_rollback: RollbackTitle,
    check_local_rollback: bool,
    opener_may_be_omitted: bool,
}

impl IndexedTitle {
    pub(crate) fn from_read(
        provider: Provider,
        snapshot: &CanonicalSnapshot,
        full_read: bool,
        document: &SearchDocument,
        rollback_title: &RollbackTitle,
    ) -> Self {
        let has_native_name = snapshot
            .title
            .as_deref()
            .is_some_and(|title| !title.trim().is_empty());
        let provider_named = provider != Provider::Codex && has_native_name;
        if !provider_named {
            match rollback_title {
                RollbackTitle::Known(current) => {
                    return Self {
                        current: current.clone(),
                        verified: true,
                        local_opener: LocalOpenerEvidence::Current,
                        ..Self::default()
                    };
                }
                RollbackTitle::UnknownAfterWithdrawal(_) => {
                    return Self {
                        uncertain_rollback: rollback_title.clone(),
                        ..Self::default()
                    };
                }
                RollbackTitle::Unknown => {}
            }
        }
        let check_local_rollback = !document.source_complete
            && !provider_named
            && snapshot.session.provider == Provider::Codex
            && snapshot.events.iter().any(|event| {
                event.source.raw_record_type.as_deref() == Some("omnisession.codex_history_base")
                    && event.source.provider == Provider::Codex
                    && event.source.native_session_id == snapshot.session.id
                    && event.payload["native_history_unverified"].as_bool() == Some(true)
            });
        Self {
            current: indexed_search_title(provider, snapshot),
            verified: provider_named || (full_read && document.source_complete),
            local_opener: LocalOpenerEvidence::Unknown,
            uncertain_rollback: RollbackTitle::Unknown,
            check_local_rollback,
            opener_may_be_omitted: check_local_rollback
                && snapshot.events.iter().any(|event| {
                    matches!(
                        event.source.raw_record_type.as_deref(),
                        Some("omnisession.preview_limit" | "omnisession.codex_event_limit")
                    )
                }),
            withdrawn: None,
        }
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
        let selected_session = || -> Result<NativeSession> {
            registry
                .find_native_session(Provider::Codex, &session.id)?
                .filter(|selected| {
                    selected
                        .source_revision
                        .is_some_and(|selected| selected.matches_file_contents(revision))
                })
                .ok_or_else(|| omnis_store::StoreError::TrajectorySourceChanged.into())
        };
        let selected_path = || -> Result<PathBuf> {
            selected_session()?
                .source_path
                .ok_or_else(|| omnis_store::StoreError::TrajectorySourceChanged.into())
        };
        if (uncertain || self.local_opener == LocalOpenerEvidence::Current)
            && let Some(prior) = store.legacy_codex_opener_candidate(session, file)?
            && let NativeSession {
                title: None,
                source_path: Some(path),
                ..
            } = selected_session()?
            && omnis_adapters::CodexAdapter::verify_legacy_local_opener(
                &path,
                session,
                revision,
                &prior.title,
                prior.requires_tool_catalog,
            )?
        {
            let checked = selected_session()?;
            anyhow::ensure!(
                checked.title.is_none() && checked.source_path.as_ref() == Some(&path),
                omnis_store::StoreError::TrajectorySourceChanged
            );
            self.withdrawn = Some(prior.title);
            self.local_opener = if uncertain {
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
                        &selected_path()?,
                        session,
                        revision,
                        &prior,
                    )?)
            {
                selected_path()?;
                self.withdrawn = Some(prior);
                self.local_opener = LocalOpenerEvidence::Withdrawn;
            }
            return Ok(());
        }
        let matches_projection = self.current.as_ref() == Some(&prior);
        if matches_projection && !self.opener_may_be_omitted {
            return Ok(());
        }
        if omnis_adapters::CodexAdapter::rollout_withdrew_search_title(
            &selected_path()?,
            session,
            revision,
            &prior,
        )? {
            selected_path()?;
            if matches_projection {
                self.current = None;
            }
            self.withdrawn = Some(prior);
        }
        Ok(())
    }
}

struct PreparedIndex {
    workspace: Option<omnis_store::CodexWorkspaceUpdate>,
    guard: TrajectorySourceGuard,
    verified_metadata: Option<IndexedSession>,
    candidate: IndexCandidate,
    document: SearchDocument,
    source_updated_at: DateTime<Utc>,
    source_complete: bool,
    provider_limited: bool,
    origin: SessionTrajectoryOrigin,
    title: IndexedTitle,
    title_authoritative: bool,
}

pub(crate) fn stored_title_update(
    registry: &AdapterRegistry,
    store: &Store,
    session: &SessionRef,
) -> Option<(SessionRef, omnis_store::IndexedTitle)> {
    let (session, record) = store
        .trajectory_title_records(Some(session))
        .ok()?
        .remove_entry(session)?;
    if matches!(
        session.provider,
        Provider::Antigravity | Provider::AntigravityDesktop
    ) {
        let home = registry.discovery_cache_home(session.provider).ok()??;
        let revision = store
            .selected_document_revisions(
                session.provider,
                &home,
                &[(&session.id, SourceRevision::default())],
            )
            .ok()?
            .remove(&session.id)?;
        if !revision.matches_file_contents(record.source_revision) {
            return None;
        }
    }
    let known = record.title.is_some()
        || record.verified_at.is_some()
        || (record.source_revision.contents.is_some()
            && matches!(
                session.provider,
                Provider::Antigravity | Provider::AntigravityDesktop
            ));
    known.then_some((session, record))
}

impl PreparedIndex {
    fn store_codex(&self, store: &Store) -> omnis_store::Result<()> {
        store.upsert_observed_trajectory_documents_with_workspaces(&[(
            &self.candidate.session,
            self.store_document(),
            self.guard,
            self.workspace.as_ref(),
        )])
    }

    fn report_title(
        self,
        registry: &AdapterRegistry,
        store: &Store,
        titles: &mut Vec<(SessionRef, omnis_store::IndexedTitle)>,
    ) {
        titles.extend(stored_title_update(
            registry,
            store,
            &self.candidate.session,
        ));
    }

    fn store(
        &self,
        selections: &mut FileSelections<'_>,
        store: &Store,
    ) -> omnis_store::Result<usize> {
        selections.write_documents(
            store,
            &[self.candidate.file_check(self.guard)],
            std::iter::once((
                &self.candidate.session,
                self.store_document(),
                self.guard,
                self.verified_metadata.as_ref(),
            )),
        )
    }

    fn store_document(&self) -> TrajectoryDocument<'_> {
        TrajectoryDocument {
            redacted_text: &self.document.text,
            source_updated_at: self.source_updated_at,
            source_revision: self.candidate.source_revision,
            source_byte_count: self.document.source_byte_count,
            projection: self.document.projection.as_ref(),
            indexed_byte_count: self.document.indexed_byte_count,
            truncation_strategy: self.document.truncation_strategy.as_str(),
            source_complete: self.source_complete,
            provider_limited: self.provider_limited,
            origin: self.origin,
            document_version: self.candidate.document_version,
            derived_title: self.title.current.as_deref(),
            withdrawn_title: self.title.withdrawn.as_deref(),
            title_is_verified: self.title.verified,
            local_opener: self.title.local_opener,
            title_authoritative: self.title_authoritative,
        }
    }
}

fn read_candidate(
    registry: &AdapterRegistry,
    candidate: &IndexCandidate,
    guard: TrajectorySourceGuard,
    budget: ReadBudget,
    store: Option<&Store>,
) -> Result<(IndexedSearchDocument, Option<DateTime<Utc>>)> {
    Ok(if candidate.session.provider == Provider::Imported {
        let snapshot = read_session(registry, &candidate.session)?;
        let updated_at = snapshot.captured_at;
        (
            IndexRead {
                verified_prefix_end: None,
                database_aliases: std::sync::Arc::default(),
                search_record_groups: Vec::new(),
                native_session: None,
                rollback_title: RollbackTitle::Unknown,
                workspace_projection: omnis_adapters::CodexWorkspaceProjection::Unchecked,
                source_revision: SourceRevision::default(),
                snapshot,
                source_complete: true,
                provider_limited: false,
            }
            .into(),
            Some(updated_at),
        )
    } else if candidate.session.provider == Provider::Codex {
        let indexed = registry.adapter(Provider::Codex)?.index_search_document(
            &candidate.session,
            candidate.source_path.as_deref(),
            budget.bytes,
            budget.prior_document,
        )?;
        anyhow::ensure!(
            candidate.source_revision.file.is_none()
                || indexed
                    .read
                    .source_revision
                    .matches_file_contents(candidate.source_revision),
            "selected Codex rollout changed during indexing"
        );
        let time = indexed.read.snapshot.captured_at;
        let indexed = match store {
            Some(store) if !indexed.read.source_complete => {
                verify_legacy_search_read(registry, store, guard, indexed.read)?.into()
            }
            _ => indexed,
        };
        (indexed, Some(time))
    } else if candidate.source_revision.file.is_some() {
        let indexed = registry
            .adapter(candidate.session.provider)?
            .index_search_document(
                &candidate.session,
                candidate.source_path.as_deref(),
                budget.bytes,
                budget.prior_document,
            )?;
        let read = &indexed.read;
        anyhow::ensure!(
            candidate
                .source_revision
                .matches_file_contents(read.source_revision),
            "selected transcript changed during indexing"
        );
        let modified = read
            .native_session
            .as_ref()
            .and_then(|native| native.updated_at);
        (indexed, modified)
    } else {
        registry.observe_search_read(
            &candidate.session,
            candidate.source_path.as_deref(),
            |adapter, path| {
                let indexed = adapter.index_search_document(
                    &candidate.session,
                    path,
                    budget.bytes,
                    budget.prior_document,
                )?;
                let captured = indexed.read.snapshot.captured_at;
                let updated_at = candidate.updated_at.unwrap_or(captured).max(captured);
                Ok((indexed, updated_at))
            },
        )?
    })
}

fn prepare_session(
    registry: &AdapterRegistry,
    candidate: &IndexCandidate,
    guard: TrajectorySourceGuard,
    budget: ReadBudget,
    store: Option<&Store>,
) -> anyhow::Result<PreparedIndex> {
    let (IndexedSearchDocument { document, read }, source_updated_at) =
        read_candidate(registry, candidate, guard, budget, store)?;
    let IndexRead {
        verified_prefix_end,
        database_aliases: _,
        search_record_groups: _,
        snapshot,
        source_complete,
        provider_limited,
        native_session,
        source_revision,
        rollback_title,
        workspace_projection,
    } = read;
    let source_updated_at = source_updated_at
        .ok_or_else(|| anyhow::anyhow!("transcript changed during search read"))?;
    let origin = if candidate.session.provider == Provider::Imported {
        SessionTrajectoryOrigin::ImportedBundle
    } else {
        SessionTrajectoryOrigin::Native
    };
    let mut selected = candidate.clone();
    selected.source_revision = source_revision;
    if source_revision.file.is_some() && candidate.session.provider != Provider::Codex {
        selected.source_path = native_session
            .as_ref()
            .and_then(|native| native.source_path.clone());
    }
    let mut title = IndexedTitle::from_read(
        candidate.session.provider,
        &snapshot,
        source_complete,
        &document,
        &rollback_title,
    );
    let title_authoritative =
        omnis_core::session_search_title_is_authoritative(&snapshot, source_complete);
    if candidate.session.provider != Provider::Codex {
        title.current = indexed_title(&candidate.session, &snapshot, verified_prefix_end);
        title.verified = title_authoritative;
    }
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
        verified_metadata: native_session
            .as_ref()
            .filter(|_| {
                source_revision.file.is_some()
                    && matches!(guard, TrajectorySourceGuard::Verified { .. })
            })
            .map(crate::indexed_session_metadata),
        candidate: selected,
        document,
        source_updated_at,
        source_complete,
        provider_limited,
        origin,
        title,
        title_authoritative,
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
            catalog: None,
            metadata: None,
            source_revision: source_path
                .as_deref()
                .and_then(CodexAdapter::index_file_revision)
                .unwrap_or_default(),
            session: SessionRef::new(Provider::Codex, "synthetic"),
            updated_at,
            source_path,
            document_version: search_document_version(Provider::Codex),
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
        let budget = |stored, selected, updated_at, document_version| {
            projection_read_budget(
                TrajectorySourceGuard::ObservedRevision(Some(TrajectoryIndexState {
                    source_byte_count: 0,
                    legacy_tail_receipt: None,
                    source_updated_at: time,
                    title_authoritative: Some(true),
                    source_time_precise: true,
                    provider_limited: false,
                    source_revision: stored,
                    source_complete: true,
                    document_version,
                })),
                Some(updated_at),
                selected,
            )
        };
        let current_version = search_document_version(Provider::Codex);
        for previous_version in [4, current_version - 1] {
            assert_eq!(budget(revision, revision, time, previous_version), u64::MAX);
        }
        assert_eq!(
            budget(revision, revision, time, current_version),
            FULL_READ_SOURCE_BYTES
        );
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
            assert_eq!(budget(other, revision, time, 4), FULL_READ_SOURCE_BYTES);
            assert_eq!(budget(revision, other, time, 4), FULL_READ_SOURCE_BYTES);
        }
        assert_eq!(
            budget(revision, revision, time - chrono::Duration::seconds(1), 4),
            FULL_READ_SOURCE_BYTES
        );
    }

    #[test]
    fn a_delayed_tail_repair_cannot_replace_a_concurrent_repair() -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let path = temporary.path().join("store.sqlite3");
        let store = Store::open(&path)?;
        let now = Utc::now();
        let candidate = candidate(Some(now), None);
        let session = candidate.session.clone();
        let old = TrajectoryDocument {
            projection: None,
            source_revision: SourceRevision::default(),
            redacted_text: "old",
            source_updated_at: now,
            source_byte_count: 3,
            indexed_byte_count: 3,
            truncation_strategy: "none",
            source_complete: false,
            provider_limited: false,
            origin: SessionTrajectoryOrigin::Native,
            document_version: candidate.document_version,
            derived_title: None,
            withdrawn_title: None,
            title_is_verified: false,
            local_opener: LocalOpenerEvidence::Unknown,
            title_authoritative: false,
        };
        store.upsert_trajectory_document(&session, &old)?;
        // A legacy writer assigns the content hash without recording a tail reader version.
        rusqlite::Connection::open(path)?.execute(
            "UPDATE session_trajectories SET content_hash = content_hash",
            [],
        )?;
        let registry = AdapterRegistry::new();
        let pending = pending(&registry, &store, vec![candidate], false)?;
        let guards = || {
            guard_index_reads(
                &mut FileSelections::new(&registry),
                &store,
                pending.stale.clone(),
                &pending.failures,
            )
        };
        let guard = guards()?[0].1;
        let winner = TrajectoryDocument {
            redacted_text: "new",
            source_updated_at: now - chrono::Duration::seconds(2),
            ..old
        };
        assert_eq!(
            store.upsert_observed_trajectory_documents_counted(&[(&session, winner, guard)])?,
            1
        );
        let delayed = TrajectoryDocument {
            redacted_text: "bad",
            ..winner
        };
        assert!(matches!(
            store.upsert_observed_trajectory_documents_counted(&[(
                &session,
                delayed,
                guards()?[0].1
            )]),
            Err(omnis_store::StoreError::TrajectorySourceChanged)
        ));
        assert_eq!(store.search_session_trajectories("new", 1)?, [session]);
        Ok(())
    }

    #[test]
    fn stale_versions_changed_sources_and_previews_need_indexing() {
        let now = Utc::now();
        let current = TrajectoryIndexState {
            source_byte_count: 0,
            legacy_tail_receipt: None,
            source_revision: SourceRevision::default(),
            title_authoritative: Some(true),
            source_updated_at: now,
            source_time_precise: true,
            provider_limited: false,
            source_complete: true,
            document_version: search_document_version(Provider::Codex),
        };
        let small = candidate(Some(now), None);

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
            source_byte_count: 0,
            legacy_tail_receipt: None,
            source_updated_at: now,
            source_revision: candidate(Some(now), Some(path.clone())).source_revision,
            source_complete: false,
            provider_limited: false,
            title_authoritative: Some(false),
            source_time_precise: true,
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
            source_time_precise: true,
            document_version: search_document_version(Provider::Codex),
        };

        assert!(failure_is_current(&candidate(Some(now), None), &failure));
        assert!(failure_is_current(&candidate(None, None), &failure));
        assert!(!failure_is_current(
            &candidate(Some(now - chrono::Duration::days(1)), None),
            &failure
        ));
        assert!(!failure_is_current(
            &candidate(Some(now + chrono::Duration::milliseconds(1)), None),
            &failure
        ));
        assert!(!failure_is_current(
            &candidate(Some(now), None),
            &TrajectoryIndexFailure {
                source_updated_at: None,
                ..failure
            }
        ));
        assert!(!failure_is_current(
            &candidate(Some(now), None),
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
            source_revision: None,
            session: SessionRef::new(provider, "synthetic"),
            title: None,
            project_path: None,
            git_branch: None,
            created_at: None,
            updated_at: None,
            updated_at_approximate: false,
            event_count: 0,
            source_path: source_path.map(PathBuf::from),
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
            source_revision: None,
            session: SessionRef::new(Provider::Codex, id),
            title: None,
            project_path: Some(project.to_path_buf()),
            git_branch: None,
            created_at: None,
            updated_at: Some(now - chrono::Duration::minutes(age_minutes)),
            updated_at_approximate: false,
            event_count: 0,
            source_path: Some(PathBuf::from("/synthetic")),
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
                    projection: None,
                    indexed_byte_count: 21,
                    truncation_strategy: "none",
                    source_complete: false,
                    provider_limited: false,
                    origin: SessionTrajectoryOrigin::Native,
                    document_version: 2,
                    derived_title: None,
                    title_authoritative: false,
                    title_is_verified: false,
                    withdrawn_title: None,
                    local_opener: LocalOpenerEvidence::Unknown,
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
                .flat_map(|report| report
                    .titles
                    .iter()
                    .map(|record| (record.session.clone(), record.title.clone())))
                .collect::<Vec<_>>(),
            vec![(
                SessionRef::new(Provider::Codex, id),
                Some("Synthetic rate limiter request".to_owned())
            )]
        );
        let again = index_candidates(&registry, &store, candidates, false, &|| false, &mut |_| {})
            .expect("reindex synthetic sessions");
        assert_eq!(again.stale, 0);
    }

    #[test]
    fn queued_index_preserves_a_failure_on_the_new_first_file() -> Result<()> {
        for checkpoint in ["before index", "after pending", "after read"] {
            let temp = tempfile::tempdir()?;
            let root = temp.path().join("sessions");
            fs::create_dir(&root)?;
            let header = json!({"type":"session","version":3,"id":"same","cwd":temp.path()});
            let mut ask = json!({"type":"message","id":"request","parentId":null,
                "message":{"role":"user","content":"formerprimaryneedle"}});
            fs::write(root.join("later.jsonl"), format!("{header}\n{ask}\n"))?;
            let store = Store::open(temp.path().join("search.sqlite3"))?;
            let mut registry = AdapterRegistry::new();
            registry.register(omnis_adapters::PiAdapter::with_root(&root));
            let adapter = registry.adapter(Provider::Pi)?;
            let original = adapter.list_search_sessions(None)?.remove(0);
            let queued = IndexCandidate::from_session(&original).unwrap();
            ask["parentId"] = json!("absent");
            let failed = std::cell::Cell::new(None);
            let confirm_failure = || {
                if failed.get().is_some() {
                    return;
                }
                fs::write(root.join("first.jsonl"), format!("{header}\n{ask}\n")).unwrap();
                let first = adapter.list_search_sessions(None).unwrap().remove(0);
                assert_ne!(first.source_revision, original.source_revision);
                assert!(adapter.read_session(&first.session).is_err());
                let guard = store.observe_verified_session(&first.session).unwrap();
                IndexCandidate::from_session(&first)
                    .unwrap()
                    .record_failure(&mut FileSelections::new(&registry), &store, guard)
                    .unwrap();
                failed.set(first.source_revision);
            };
            if checkpoint == "before index" {
                confirm_failure();
            }
            index_candidates(
                &registry,
                &store,
                vec![queued],
                false,
                &|| {
                    if checkpoint == "after read" {
                        confirm_failure();
                    }
                    false
                },
                &mut |_| {
                    if checkpoint == "after pending" {
                        confirm_failure();
                    }
                },
            )?;
            let failure = store.trajectory_index_failures()?[&original.session];
            assert_eq!(
                failure.source_file,
                failed.get().unwrap().file,
                "{checkpoint}"
            );
            assert!(
                store
                    .search_session_trajectories("formerprimaryneedle", 1)?
                    .is_empty(),
                "{checkpoint}"
            );
        }
        Ok(())
    }

    #[test]
    fn withdrawing_a_failed_duplicate_restores_the_original_full_search_document() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().join("sessions");
        fs::create_dir(&root)?;
        let header = json!({"type":"session","version":3,"id":"same","cwd":temp.path()});
        let request = json!({"type":"message","id":"task","parentId":null,
            "message":{"role":"user","content":"retainedoriginalneedle"}});
        let original_dir = root.join("later");
        fs::create_dir(&original_dir)?;
        let file = original_dir.join("session.jsonl");
        let padding = format!(
            "{{\"type\":\"synthetic-padding\",\"text\":\"{}\"}}\n",
            "x".repeat(65536)
        );
        let mut writer = std::io::BufWriter::new(File::create(&file)?);
        writeln!(writer, "{header}")?;
        for _ in 0..128 {
            writer.write_all(padding.as_bytes())?;
        }
        writeln!(writer, "{request}")?;
        for _ in 0..128 {
            writer.write_all(padding.as_bytes())?;
        }
        writer.flush()?;
        assert!(file.metadata()?.len() > FULL_READ_SOURCE_BYTES);
        let store = Store::open(temp.path().join("state.db"))?;
        let mut registry = AdapterRegistry::new();
        registry.register(omnis_adapters::PiAdapter::with_root(&root));
        let selected = || -> Result<IndexCandidate> {
            let native = registry.find_native_session(Provider::Pi, "same")?.unwrap();
            Ok(IndexCandidate::from_session(&native).unwrap())
        };
        let original = selected()?;
        let guard = store.observe_verified_session(&original.session)?;
        let full = ReadBudget {
            bytes: u64::MAX,
            prior_document: None,
        };
        prepare_session(&registry, &original, guard, full, Some(&store))?
            .store(&mut FileSelections::new(&registry), &store)?;
        let duplicate_dir = root.join("first");
        fs::create_dir(&duplicate_dir)?;
        let duplicate = duplicate_dir.join("session.jsonl");
        let mut broken = request;
        broken["parentId"] = json!("missing");
        fs::write(&duplicate, format!("{header}\n{broken}\n"))?;
        let index = |retry| {
            index_candidates(
                &registry,
                &store,
                vec![selected()?],
                retry,
                &|| false,
                &mut |_| {},
            )
        };
        assert_eq!(index(false)?.failed, 1);
        assert_eq!(index(false)?.failed_skipped, 1);
        assert!(
            store
                .search_session_trajectories("retainedoriginalneedle", 1)?
                .is_empty()
        );

        let withdrawn = temp.path().join("withdrawn");
        fs::rename(&duplicate_dir, &withdrawn)?;
        let guard = store.observe_verified_session(&original.session)?;
        let delayed = prepare_session(&registry, &original, guard, full, Some(&store))?;
        fs::rename(&withdrawn, &duplicate_dir)?;
        assert!(matches!(
            delayed.store(&mut FileSelections::new(&registry), &store),
            Err(omnis_store::StoreError::TrajectorySourceChanged)
        ));
        assert!(store.trajectory_index_failure(&original.session)?.is_some());

        fs::remove_file(duplicate)?;
        assert_eq!(index(false)?.failed, 0);
        assert!(store.trajectory_index_failure(&original.session)?.is_none());
        assert!(
            store
                .trajectory_index_state(&original.session)?
                .unwrap()
                .source_complete
        );
        assert_eq!(
            store.search_session_trajectories("retainedoriginalneedle", 1)?,
            [original.session]
        );
        assert_eq!(index(true)?.stale, 0);
        Ok(())
    }

    #[test]
    fn a_concurrent_preview_cannot_discard_the_first_structural_read_failure() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let root = temporary.path().join("sessions");
        fs::create_dir(&root)?;
        let header = json!({"type":"session","version":3,"id":"missing","cwd":temporary.path()});
        let request = json!({"type":"message","id":"task","parentId":null,
            "message":{"role":"user","content":"Review the synthetic session."}});
        let orphan = json!({"type":"message","id":"leaf","parentId":"absent",
            "message":{"role":"assistant","content":[{"type":"text","text":"lostparentneedle"}]}});
        fs::write(
            root.join("session.jsonl"),
            format!("{header}\n{request}\n{orphan}\n"),
        )?;
        let mut registry = AdapterRegistry::new();
        registry.register(omnis_adapters::PiAdapter::with_root(&root));
        let store = Store::open(temporary.path().join("store.sqlite3"))?;
        let selected = registry
            .adapter(Provider::Pi)?
            .list_search_sessions(None)?
            .remove(0);
        let candidate = IndexCandidate::from_session(&selected).unwrap();
        let sampled = std::cell::Cell::new(false);
        let summary = index_candidates(
            &registry,
            &store,
            vec![candidate.clone()],
            false,
            &|| {
                if !sampled.replace(true) {
                    let guard = store.observe_verified_session(&candidate.session).unwrap();
                    prepare_session(&registry, &candidate, guard, 0.into(), Some(&store))
                        .unwrap()
                        .store(&mut FileSelections::new(&registry), &store)
                        .unwrap();
                    assert!(
                        !store
                            .search_session_trajectories("lostparentneedle", 1)
                            .unwrap()
                            .is_empty()
                    );
                }
                false
            },
            &mut |_| {},
        )?;
        assert_eq!(summary.failed, 1);
        let failure = store.trajectory_index_failure(&candidate.session)?.unwrap();
        assert!(failure.matches_file_revision(candidate.source_revision));
        assert!(
            store
                .search_session_trajectories("lostparentneedle", 1)?
                .is_empty()
        );
        let skipped = pending_summary(&registry, &store, vec![candidate.clone()])?;
        assert_eq!((skipped.stale, skipped.failed_skipped), (0, 1));
        assert_eq!(
            read_budget(
                &candidate,
                store.observe_verified_session(&candidate.session)?,
                None,
                Some(&failure)
            )
            .bytes,
            u64::MAX
        );
        Ok(())
    }

    #[test]
    fn sampled_pi_read_publishes_the_catalog_revision_with_its_search_text() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let root = temporary.path().join("sessions");
        fs::create_dir(&root)?;
        let header = json!({"type":"session","version":3,"id":"sampled","cwd":temporary.path()});
        let message = json!({"type":"message","id":"ask","parentId":null,
            "message":{"role":"user","content":"sampledneedle"}});
        fs::write(root.join("session.jsonl"), format!("{header}\n{message}\n"))?;
        let mut registry = AdapterRegistry::new();
        registry.register(omnis_adapters::PiAdapter::with_root(root));
        let store = Store::open(temporary.path().join("store.sqlite3"))?;
        let listed = registry
            .find_native_session(Provider::Pi, "sampled")?
            .unwrap();
        let catalog = crate::indexed_session_metadata(&listed);
        let session = &listed.session;
        store.upsert_indexed_session(&omnis_store::IndexedSession {
            project_path: Some(temporary.path().join("previous")),
            source_revision: None,
            ..catalog.clone()
        })?;
        let candidate = IndexCandidate::from_session(&listed).unwrap();
        let guard = store.observe_verified_session(session)?;
        prepare_session(&registry, &candidate, guard, 0.into(), Some(&store))?
            .store(&mut FileSelections::new(&registry), &store)?;

        let document = store.trajectory_index_state(session)?.unwrap();
        assert!(!document.source_complete && !document.provider_limited);
        assert_eq!(Some(document.source_revision), catalog.source_revision);
        assert_eq!(
            store.indexed_sessions_for_provider(Provider::Pi)?,
            [catalog]
        );
        assert_eq!(
            store.search_session_trajectories("sampledneedle", 1)?,
            std::slice::from_ref(session)
        );
        Ok(())
    }

    #[test]
    fn first_index_read_replaces_a_discovered_catalog_entry_with_its_current_header() -> Result<()>
    {
        let temporary = tempfile::tempdir()?;
        let root = temporary.path().join("sessions");
        fs::create_dir(&root)?;
        let path = root.join("first.jsonl");
        let mut registry = AdapterRegistry::new();
        registry.register(omnis_adapters::PiAdapter::with_root(root));
        let store = Store::open(temporary.path().join("store.sqlite3"))?;
        let session = SessionRef::new(Provider::Pi, "first");
        let select = |workspace: &str| -> Result<NativeSession> {
            let header = json!({"type":"session","version":3,"id":session.id,"cwd":temporary.path().join(workspace)});
            let message = json!({"type":"message","id":"ask","parentId":null,
                "message":{"role":"user","content":format!("{workspace}body")}});
            fs::write(&path, format!("{header}\n{message}\n"))?;
            Ok(registry
                .find_native_session(Provider::Pi, &session.id)?
                .unwrap())
        };
        let prior = select("previous")?;
        store.upsert_indexed_session(&crate::indexed_session_metadata(&prior))?;
        let current = select("current")?;
        let candidates = ordered_candidates(std::slice::from_ref(&current), None);
        let result = index_candidates(
            &registry,
            &store,
            candidates.clone(),
            false,
            &|| false,
            &mut |_| {},
        )?;
        assert_eq!((result.stale, result.indexed, result.failed), (1, 1, 0));
        assert_eq!(
            store.indexed_sessions_for_provider(Provider::Pi)?,
            [crate::indexed_session_metadata(&current)]
        );
        assert_eq!(
            store.search_session_trajectories("currentbody", 1)?,
            [session]
        );
        assert_eq!(pending_summary(&registry, &store, candidates)?.stale, 0);
        Ok(())
    }

    #[test]
    fn stale_pi_index_worker_rereads_the_latest_same_time_replacement() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let root = temporary.path().join("sessions");
        fs::create_dir(&root)?;
        let path = root.join("session.jsonl");
        let modified = std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let replace = |revision: &str| -> Result<()> {
            let workspace = temporary.path().join(revision);
            fs::create_dir(&workspace)?;
            let header = json!({"type":"session","version":3,"id":"same-time","cwd":workspace,"title":revision});
            let message = json!({"type":"message","id":"ask","parentId":null,
                "message":{"role":"user","content":format!("{revision}body")}});
            let next = path.with_extension("next");
            fs::write(&next, format!("{header}\n{message}\n"))?;
            File::options()
                .write(true)
                .open(&next)?
                .set_modified(modified)?;
            fs::rename(next, &path)?;
            Ok(())
        };
        let mut registry = AdapterRegistry::new();
        registry.register(omnis_adapters::PiAdapter::with_root(root));
        let store = Store::open(temporary.path().join("store.sqlite3"))?;
        let session = SessionRef::new(Provider::Pi, "same-time");

        replace("alpha")?;
        store.upsert_session_trajectory(&session, "Initial preview", modified.into(), false)?;
        let listed = registry
            .find_native_session(Provider::Pi, &session.id)?
            .unwrap();
        let candidate = IndexCandidate::from_session(&listed).unwrap();
        let prepared = prepare_session(
            &registry,
            &candidate,
            observe_source(&store, &session)?,
            FULL_READ_SOURCE_BYTES.into(),
            Some(&store),
        )?;
        replace("bravo")?;
        let guard = store.observe_verified_session(&session)?;
        let (read, source_time) = crate::read_search_snapshot(&registry, &session, true)?;

        crate::store_verified_search_document(
            &store,
            &session,
            &read,
            true,
            source_time.unwrap(),
            guard,
            read.native_session
                .as_ref()
                .map(|native| (&registry, native)),
        )?;
        assert_eq!(store.trajectory_title(&session)?.as_deref(), Some("bravo"));

        replace("delta")?;
        let mut summary = IndexSummary::default();
        flush_batch(
            &mut FileSelections::new(&registry),
            &store,
            &mut vec![prepared],
            &mut summary,
            &mut IndexProgress::default(),
        );
        assert_eq!((summary.indexed, summary.failed), (1, 0));
        let catalog = store.indexed_sessions_for_provider(Provider::Pi)?;
        assert_eq!(
            catalog[0].project_path,
            Some(temporary.path().join("delta"))
        );
        assert_eq!(catalog[0].title.as_deref(), Some("delta"));
        assert_eq!(store.trajectory_title(&session)?.as_deref(), Some("delta"));
        assert_eq!(
            store.search_session_trajectories("deltabody", 1)?,
            [session]
        );
        for stale in ["alphabody", "bravobody"] {
            assert!(
                store.search_session_trajectories(stale, 1)?.is_empty(),
                "{stale}"
            );
        }
        Ok(())
    }

    fn index_test_candidates(
        registry: &AdapterRegistry,
        store: &Store,
        candidates: &[IndexCandidate],
    ) -> IndexSummary {
        index_candidates(
            registry,
            store,
            candidates.to_vec(),
            false,
            &|| false,
            &mut |_| {},
        )
        .unwrap()
    }

    #[test]
    fn restored_codex_rollout_replaces_search_text_even_with_a_delayed_writer() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("codex");
        let sessions = home.join("sessions/2026/01/01");
        fs::create_dir_all(&sessions).unwrap();
        let id = "019f0000-0000-7000-8000-000000000001";
        let path = sessions.join(format!("rollout-2026-01-01T00-00-00-{id}.jsonl"));
        let store = Store::open(temp.path().join("store.sqlite3")).unwrap();
        let write_rollout = |marker: &str, modified: u64| {
            fs::write(&path, format!("{}\n{}\n",
                json!({"type":"session_meta","payload":{"id":id}}),
                json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":marker}]}}),
            )).unwrap();
            File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(modified))
                .unwrap();
            let mut registry = AdapterRegistry::new();
            registry.register(CodexAdapter::with_root(&home));
            let listed = registry.list_sessions(Provider::Codex, None).unwrap();
            (registry, ordered_candidates(&listed, None))
        };

        let (registry, original) = write_rollout("oldneedle", 1_800_100_000);
        index_test_candidates(&registry, &store, &original);
        let guard = observe_source(&store, &original[0].session).unwrap();
        let prepare_original = |registry: &AdapterRegistry| {
            prepare_session(
                registry,
                &original[0],
                guard,
                FULL_READ_SOURCE_BYTES.into(),
                Some(&store),
            )
        };
        let delayed = prepare_original(&registry).unwrap();
        let (registry, restored) = write_rollout("backupneedle", 1_800_000_000);
        let error = prepare_original(&registry)
            .err()
            .expect("discovery now refers to a replaced rollout");
        assert!(retryable_read_failure(&error));
        original[0].record_codex_failure(
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
        assert_eq!(
            index_test_candidates(&registry, &store, &restored).indexed,
            1
        );
        assert!(matches!(
            delayed.store_codex(&store),
            Err(omnis_store::StoreError::TrajectorySourceChanged)
        ));
        let mut recovered = IndexSummary::default();
        let mut updates = IndexProgress::default();
        flush_codex_batch(
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
        assert_eq!(index_test_candidates(&registry, &store, &restored).stale, 0);
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
                verified_metadata: None,
                provider_limited: false,
                title_authoritative: true,
                guard: TrajectorySourceGuard::NewerSource,
                candidate: IndexCandidate {
                    catalog: None,
                    session: SessionRef::new(Provider::Codex, id),
                    updated_at: Some(now),
                    source_path: None,
                    source_revision: SourceRevision::default(),
                    metadata: None,
                    document_version: search_document_version(Provider::Codex),
                },
                document: SearchDocument {
                    projection: None,
                    source_byte_count: text.len(),
                    indexed_byte_count: text.len(),
                    text,
                    truncated: false,
                    source_complete: true,
                    truncation_strategy: omnis_core::SearchTruncationStrategy::None,
                },
                source_updated_at: now,
                source_complete: true,
                origin: SessionTrajectoryOrigin::Native,
                title: IndexedTitle {
                    current: Some(id.to_owned()),
                    ..Default::default()
                },
            }
        };
        let mut batch = vec![prepared("first"), prepared("broken"), prepared("last")];
        let mut summary = IndexSummary::default();
        let mut updates = IndexProgress::default();

        flush_codex_batch(
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

        fn read_session(&self, session: &SessionRef) -> Result<CanonicalSnapshot> {
            self.readers
                .lock()
                .expect("reader list")
                .push(thread::current().id());
            let captured_at = Utc::now();
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

    #[test]
    fn rebuilding_an_unchanged_pi_archive_keeps_full_source_coverage() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("archive.jsonl");
        fs::File::create(&source)
            .unwrap()
            .set_len(FULL_READ_SOURCE_BYTES + 1)
            .unwrap();
        let store = Store::open(temporary.path().join("store.sqlite3")).unwrap();
        let mut registry = AdapterRegistry::new();
        registry.register(ReaderProbe {
            inner: CodexAdapter::with_root(temporary.path().join("codex")),
            provider: Provider::Pi,
            readers: Arc::default(),
        });
        let session = SessionRef::new(Provider::Pi, "archive");
        let now = Utc::now();
        store
            .upsert_trajectory_document(
                &session,
                &TrajectoryDocument {
                    source_revision: SourceRevision::default(),
                    redacted_text: "old",
                    source_updated_at: now,
                    source_byte_count: 3,
                    projection: None,
                    indexed_byte_count: 3,
                    truncation_strategy: "none",
                    source_complete: true,
                    provider_limited: false,
                    origin: SessionTrajectoryOrigin::Native,
                    document_version: 4,
                    derived_title: None,
                    title_authoritative: false,
                    title_is_verified: false,
                    withdrawn_title: None,
                    local_opener: LocalOpenerEvidence::Unknown,
                },
            )
            .unwrap();
        let archive = IndexCandidate {
            catalog: None,
            metadata: None,
            source_revision: SourceRevision::default(),
            session: session.clone(),
            source_path: Some(source),
            updated_at: Some(now + chrono::Duration::milliseconds(1)),
            document_version: search_document_version(Provider::Pi),
        };
        let summary = index_candidates(
            &registry,
            &store,
            vec![archive],
            false,
            &|| false,
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(summary.indexed, 1);
        let rebuilt = store.trajectory_index_states().unwrap()[&session];
        assert_eq!(
            rebuilt.document_version,
            search_document_version(Provider::Pi)
        );
        assert!(rebuilt.source_complete);
    }

    /// A store, a registry with a [`ReaderProbe`] for Hermes, and 48 Hermes candidates.
    struct ReaderProbeFixture {
        store: Store,
        registry: AdapterRegistry,
        readers: Arc<Mutex<Vec<thread::ThreadId>>>,
        candidates: Vec<IndexCandidate>,
        _temporary: tempfile::TempDir,
    }

    impl ReaderProbeFixture {
        fn new() -> Self {
            let temporary = tempfile::tempdir().expect("temporary directory");
            let readers = Arc::new(Mutex::new(Vec::new()));
            let mut registry = AdapterRegistry::new();
            registry.register(ReaderProbe {
                inner: CodexAdapter::with_root(temporary.path().join("codex")),
                provider: Provider::Hermes,
                readers: Arc::clone(&readers),
            });
            Self {
                store: Store::open(temporary.path().join("store.sqlite3"))
                    .expect("synthetic store"),
                registry,
                readers,
                candidates: (0..48)
                    .map(|index| IndexCandidate {
                        catalog: None,
                        metadata: None,
                        source_revision: SourceRevision::default(),
                        session: SessionRef::new(Provider::Hermes, format!("synthetic-{index}")),
                        updated_at: Some(Utc::now()),
                        source_path: None,
                        document_version: search_document_version(Provider::Hermes),
                    })
                    .collect(),
                _temporary: temporary,
            }
        }
    }

    #[test]
    fn database_providers_are_read_by_the_calling_thread() {
        let fixture = ReaderProbeFixture::new();

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
    fn a_stopped_run_reads_no_further_database_session() {
        let fixture = ReaderProbeFixture::new();
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
