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
use omnis_adapters::{AdapterRegistry, IndexRead, NativeSession};
use omnis_core::{
    SearchDocument, search_document_version, session_search_title, trajectory_search_document,
    workspace_paths_match,
};
use omnis_ir::{Provider, SessionRef, SourceRevision};
use omnis_store::{
    IndexedSession, MAX_TRAJECTORY_WRITE_BATCH_BYTES, MAX_TRAJECTORY_WRITE_BATCH_DOCUMENTS,
    SessionTrajectoryOrigin, Store, TrajectoryDocument, TrajectoryIndexFailure,
    TrajectoryIndexState, TrajectorySourceGuard,
};

use crate::read_session;

// Larger sources index only their sampled head and tail.
const FULL_READ_SOURCE_BYTES: u64 = 16 * 1024 * 1024;
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
    document_version: u32,
}

impl IndexCandidate {
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
            document_version: search_document_version(provider),
        })
    }

    fn oversized(&self) -> bool {
        self.source_path
            .as_deref()
            .and_then(|path| fs::metadata(path).ok())
            .is_some_and(|metadata| metadata.is_file() && metadata.len() > FULL_READ_SOURCE_BYTES)
    }

    fn file_matches(&self, revision: SourceRevision) -> bool {
        self.source_path
            .as_deref()
            .and_then(omnis_adapters::source_file_revision)
            .is_some_and(|current| current.matches_file_contents(revision))
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct IndexProgress {
    pub(crate) indexed: usize,
    pub(crate) total: usize,
    pub(crate) failed: usize,
    pub(crate) titles: Vec<(SessionRef, omnis_store::IndexedTitle)>,
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
    if TrajectorySourceGuard::uses_file_time(provider) {
        !cached.source_time_precise || current != cached.source_updated_at
    } else {
        current.timestamp_millis() > cached.source_updated_at.timestamp_millis()
    }
}

pub(crate) fn observe_source(
    store: &Store,
    session: &SessionRef,
) -> omnis_store::Result<TrajectorySourceGuard> {
    if TrajectorySourceGuard::uses_file_time(session.provider) {
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
    state.document_version < version
        || (candidate.source_revision.file.is_some()
            && !candidate.source_revision.matches_file_contents(state.source_revision))
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
            && candidate.session.provider != Provider::Hermes
            && !candidate.oversized())
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
                if TrajectorySourceGuard::uses_file_time(candidate.session.provider) {
                    updated_at == failed_source
                } else {
                    updated_at.timestamp_millis() <= failed_source.timestamp_millis()
                }
            })
        })
}

/// Whether a read failed only because a database was busy or a source changed while it was read,
/// so a later pass can succeed even when the source time does not move.
fn retryable_read_failure(error: &anyhow::Error) -> bool {
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

fn read_budget(candidate: &IndexCandidate, prior: Option<&TrajectoryIndexState>) -> u64 {
    // Read the full source when upgrading a document from a full or provider-limited read.
    if prior.is_some_and(|state| {
        (state.source_complete || state.provider_limited)
            && (state.document_version < candidate.document_version
                || (candidate.source_revision.file.is_some()
                    && (state.source_revision.file.is_none()
                        || state.source_revision.contents.is_none()))
                || (TrajectorySourceGuard::uses_file_time(candidate.session.provider)
                    && !state.source_time_precise))
    }) {
        u64::MAX
    } else {
        FULL_READ_SOURCE_BYTES
    }
}

/// Stale candidates in order, the failures they were checked against, and their counts.
struct Pending {
    stale: Vec<(IndexCandidate, Option<TrajectoryIndexState>, bool)>,
    failures: HashMap<SessionRef, TrajectoryIndexFailure>,
    summary: IndexSummary,
}

fn imported_document_versions(store: &Store) -> Result<HashMap<String, u32>> {
    let mut versions = HashMap::new();
    for &provider in Provider::ALL {
        let version = search_document_version(provider);
        if version != search_document_version(Provider::Imported) {
            let ids = store.bundle_ids_for_source_provider(provider)?;
            versions.extend(ids.into_iter().map(|id| (id, version)));
        }
    }
    Ok(versions)
}

fn pending(store: &Store, candidates: Vec<IndexCandidate>, retry_failed: bool) -> Result<Pending> {
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
        .filter(|candidate| candidate.source_revision.file.is_some())
        .map(|candidate| candidate.session.provider)
        .collect();
    let mut conflicts = HashSet::new();
    for provider in providers {
        conflicts.extend(store.conflicting_file_catalog_rows(provider)?);
    }
    let mut summary = IndexSummary {
        candidates: candidates.len(),
        ..IndexSummary::default()
    };
    let mut stale = Vec::new();
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
        if !needs_index(&candidate, prior) && !reconcile {
            continue;
        }
        if !retry_failed && current_failure {
            summary.failed_skipped += 1;
        } else {
            let pair_catalog = candidate.source_revision.file.is_some()
                && (reconcile
                    || failure.is_some()
                    || prior.is_some_and(|state| {
                        !candidate
                            .source_revision
                            .matches_file_contents(state.source_revision)
                    }));
            stale.push((candidate, prior.copied(), pair_catalog));
        }
    }
    summary.stale = stale.len();
    Ok(Pending {
        stale,
        failures,
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
    store: &Store,
    candidates: Vec<IndexCandidate>,
) -> Result<IndexSummary> {
    Ok(pending(store, candidates, false)?.summary)
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
        mut summary,
    } = pending(store, candidates, retry_failed)?;
    report(IndexProgress {
        total: stale.len(),
        ..IndexProgress::default()
    });
    let stale = stale
        .into_iter()
        .map(|(candidate, prior, pair_catalog)| {
            let guard = if pair_catalog {
                store.observe_verified_session(&candidate.session)?
            } else if prior.is_none()
                && TrajectorySourceGuard::uses_file_time(candidate.session.provider)
            {
                // The bulk state query already observed the missing row.
                TrajectorySourceGuard::ObservedDocument(None)
            } else {
                observe_source(store, &candidate.session)?
            };
            let budget = read_budget(&candidate, prior.as_ref());
            Ok((candidate, guard, budget))
        })
        .collect::<omnis_store::Result<Vec<_>>>()?;
    let mut titles = Vec::new();
    let mut last_report = Instant::now();
    let mut processed = 0;
    let mut batch = Vec::new();
    let mut batch_bytes = 0usize;
    // `None` leaves a session to the calling thread, which reads it when its turn comes.
    let prepare = |(candidate, guard, budget): &(IndexCandidate, TrajectorySourceGuard, u64)| {
        // Read an oversized full transcript on the calling thread to bound peak memory.
        (reads_transcript_file(candidate.session.provider)
            && (*budget == FULL_READ_SOURCE_BYTES || !candidate.oversized()))
        .then(|| prepare_session(registry, candidate, *guard, *budget))
    };
    prepare_in_order(
        &stale,
        prepare_worker_count(stale.len()),
        &prepare,
        |(candidate, guard, budget), prepared| {
            if stop() {
                flush_batch(registry, store, &mut batch, &mut summary, &mut titles);
                summary.stopped = true;
                return false;
            }
            let result =
                prepared.unwrap_or_else(|| prepare_session(registry, candidate, *guard, *budget));
            match result {
                Ok(mut prepared) => {
                    if let Some(failure) = failures.get(&candidate.session) {
                        prepared.resolve_read_failure(store, *failure);
                    }
                    if !batch.is_empty()
                        && (batch.len() >= MAX_TRAJECTORY_WRITE_BATCH_DOCUMENTS
                            || batch_bytes.saturating_add(prepared.document.indexed_byte_count)
                                > MAX_TRAJECTORY_WRITE_BATCH_BYTES)
                    {
                        flush_batch(registry, store, &mut batch, &mut summary, &mut titles);
                        batch_bytes = 0;
                    }
                    batch_bytes = batch_bytes.saturating_add(prepared.document.indexed_byte_count);
                    batch.push(prepared);
                }
                Err(error) => {
                    summary.failed += 1;
                    // Best effort: an unrecorded failure is only read again on the next pass.
                    if !retryable_read_failure(&error) {
                        let _ = store.record_observed_trajectory_index_failure_checked(
                            &candidate.session,
                            &TrajectoryIndexFailure {
                                source_updated_at: candidate.updated_at,
                                source_time_precise: true,
                                document_version: candidate.document_version,
                                source_file: candidate.source_revision.file,
                                source_contents: candidate.source_revision.contents,
                            },
                            *guard,
                            |_, revision| candidate.file_matches(revision),
                        );
                    }
                }
            }
            processed += 1;
            if batch.len() >= MAX_TRAJECTORY_WRITE_BATCH_DOCUMENTS
                || batch_bytes >= MAX_TRAJECTORY_WRITE_BATCH_BYTES
                || last_report.elapsed() >= PROGRESS_INTERVAL
            {
                flush_batch(registry, store, &mut batch, &mut summary, &mut titles);
                batch_bytes = 0;
            }
            if last_report.elapsed() >= PROGRESS_INTERVAL {
                report(progress(&summary, processed, &mut titles));
                last_report = Instant::now();
            }
            true
        },
    );
    flush_batch(registry, store, &mut batch, &mut summary, &mut titles);
    report(progress(&summary, processed, &mut titles));
    Ok(summary)
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
    titles: &mut Vec<(SessionRef, omnis_store::IndexedTitle)>,
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
            )
        })
        .collect::<Vec<_>>();
    if !batch
        .iter()
        .any(|prepared| prepared.verified_metadata.is_some())
        && let Ok(indexed) =
            store.upsert_observed_trajectory_documents_checked(&documents, |session, revision| {
                batch
                    .iter()
                    .find(|prepared| prepared.candidate.session == *session)
                    .is_some_and(|prepared| prepared.candidate.file_matches(revision))
            })
    {
        summary.indexed += indexed;
        for prepared in batch.drain(..) {
            prepared.report_title(store, titles);
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
                if let Ok(mut fresh) = reread_after_conflict(registry, store, &prepared.candidate) {
                    fresh.previous_failure = prepared.previous_failure;
                    prepared = fresh;
                    stored = prepared.store(store);
                }
            }
            if let Ok(indexed) = stored {
                summary.indexed += indexed;
                prepared.report_title(store, titles);
            } else {
                summary.failed += 1;
            }
        }
    }
}

fn reread_after_conflict(
    registry: &AdapterRegistry,
    store: &Store,
    candidate: &IndexCandidate,
) -> anyhow::Result<PreparedIndex> {
    let session = &candidate.session;
    let guard = if TrajectorySourceGuard::uses_file_time(session.provider) {
        store.observe_verified_session(session)?
    } else {
        observe_source(store, session)?
    };
    let prior = store.trajectory_index_state(session)?;
    let current = if candidate.source_revision.file.is_some() {
        let path = candidate
            .source_path
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("selected transcript path changed during indexing"))?;
        registry
            .adapter(session.provider)?
            .selected_session_at(&session.id, path)?
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
        read_budget(&current, prior.as_ref()),
    )
}

fn progress(
    summary: &IndexSummary,
    processed: usize,
    titles: &mut Vec<(SessionRef, omnis_store::IndexedTitle)>,
) -> IndexProgress {
    IndexProgress {
        indexed: processed,
        total: summary.stale,
        failed: summary.failed,
        titles: std::mem::take(titles),
    }
}

struct PreparedIndex {
    guard: TrajectorySourceGuard,
    verified_metadata: Option<IndexedSession>,
    previous_failure: Option<TrajectoryIndexFailure>,
    candidate: IndexCandidate,
    document: SearchDocument,
    source_updated_at: DateTime<Utc>,
    source_complete: bool,
    provider_limited: bool,
    origin: SessionTrajectoryOrigin,
    title: Option<String>,
    title_authoritative: bool,
}

impl PreparedIndex {
    fn resolve_read_failure(&mut self, store: &Store, failure: TrajectoryIndexFailure) {
        if self.candidate.source_revision.file.is_some() {
            // Confirm the replacement file inside the write transaction before clearing its failure.
            self.previous_failure = Some(failure);
        } else {
            let _ =
                store.clear_observed_trajectory_index_failure(&self.candidate.session, &failure);
        }
    }

    fn report_title(
        self,
        store: &Store,
        titles: &mut Vec<(SessionRef, omnis_store::IndexedTitle)>,
    ) {
        if let Some(failure) = self.previous_failure
            && store
                .trajectory_index_state(&self.candidate.session)
                .ok()
                .flatten()
                .is_some_and(|state| state.source_revision == self.candidate.source_revision)
        {
            let _ =
                store.clear_observed_trajectory_index_failure(&self.candidate.session, &failure);
        }
        if let Ok(mut records) = store.trajectory_title_records(Some(&self.candidate.session))
            && let Some(record) = records.remove(&self.candidate.session)
            && (record.title.is_some() || record.verified_at.is_some())
        {
            titles.push((self.candidate.session, record));
        }
    }

    fn store(&self, store: &Store) -> omnis_store::Result<usize> {
        if let Some(metadata) = &self.verified_metadata {
            return store.upsert_verified_session_document_checked(
                metadata,
                self.store_document(),
                self.guard,
                |_, revision| self.candidate.file_matches(revision),
            );
        }
        store.upsert_observed_trajectory_documents_checked(
            &[(&self.candidate.session, self.store_document(), self.guard)],
            |_, revision| self.candidate.file_matches(revision),
        )
    }

    fn store_document(&self) -> TrajectoryDocument<'_> {
        TrajectoryDocument {
            redacted_text: &self.document.text,
            source_updated_at: self.source_updated_at,
            source_revision: self.candidate.source_revision,
            source_byte_count: self.document.source_byte_count,
            indexed_byte_count: self.document.indexed_byte_count,
            truncation_strategy: self.document.truncation_strategy.as_str(),
            source_complete: self.source_complete,
            provider_limited: self.provider_limited,
            origin: self.origin,
            document_version: self.candidate.document_version,
            derived_title: self.title.as_deref(),
            title_authoritative: self.title_authoritative,
        }
    }
}

fn prepare_session(
    registry: &AdapterRegistry,
    candidate: &IndexCandidate,
    guard: TrajectorySourceGuard,
    full_read_bytes: u64,
) -> anyhow::Result<PreparedIndex> {
    let imported = candidate.session.provider == Provider::Imported;
    let (read, source_updated_at) = if imported {
        let snapshot = read_session(registry, &candidate.session)?;
        let updated_at = snapshot.captured_at;
        (
            IndexRead {
                native_session: None,
                source_revision: SourceRevision::default(),
                snapshot,
                source_complete: true,
                provider_limited: false,
            },
            Some(updated_at),
        )
    } else if candidate.source_revision.file.is_some() {
        let read = registry.index_session(
            &candidate.session,
            candidate.source_path.as_deref(),
            full_read_bytes,
        )?;
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
        (read, modified)
    } else {
        registry.observe_search_read(
            &candidate.session,
            candidate.source_path.as_deref(),
            |adapter, path| {
                let read = adapter.index_session(&candidate.session, path, full_read_bytes)?;
                let captured = read.snapshot.captured_at;
                let updated_at = candidate.updated_at.unwrap_or(captured).max(captured);
                Ok((read, updated_at))
            },
        )?
    };
    let IndexRead {
        snapshot,
        source_complete,
        provider_limited,
        native_session,
        source_revision,
    } = read;
    let document = trajectory_search_document(&snapshot);
    let source_updated_at = source_updated_at
        .ok_or_else(|| anyhow::anyhow!("transcript changed during search read"))?;
    let origin = if imported {
        SessionTrajectoryOrigin::ImportedBundle
    } else {
        SessionTrajectoryOrigin::Native
    };
    let mut selected = candidate.clone();
    selected.source_revision = source_revision;
    if source_revision.file.is_some() {
        selected.source_path = native_session
            .as_ref()
            .and_then(|native| native.source_path.clone());
    }
    Ok(PreparedIndex {
        guard,
        previous_failure: None,
        verified_metadata: native_session
            .as_ref()
            .filter(|_| {
                (source_complete || provider_limited)
                    && matches!(guard, TrajectorySourceGuard::Verified(_))
            })
            .map(crate::indexed_session_metadata),
        candidate: selected,
        document,
        source_updated_at,
        source_complete,
        provider_limited,
        origin,
        title: session_search_title(&snapshot),
        title_authoritative: omnis_core::session_search_title_is_authoritative(
            &snapshot,
            source_complete,
        ),
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
                progress.indexed, progress.total
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
    use omnis_ir::CanonicalSnapshot;
    use serde_json::json;

    use super::*;

    fn candidate(
        updated_at: Option<DateTime<Utc>>,
        source_path: Option<PathBuf>,
    ) -> IndexCandidate {
        IndexCandidate {
            source_revision: SourceRevision::default(),
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
    fn stale_versions_changed_sources_and_previews_need_indexing() {
        let now = Utc::now();
        let current = TrajectoryIndexState {
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
            source_revision: SourceRevision::default(),
            title_authoritative: Some(false),
            source_updated_at: now,
            source_time_precise: true,
            provider_limited: false,
            source_complete: false,
            document_version: search_document_version(Provider::Codex),
        };

        assert!(!needs_index(
            &candidate(Some(now), Some(path)),
            Some(&sampled)
        ));
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
                    indexed_byte_count: 21,
                    truncation_strategy: "none",
                    source_complete: false,
                    provider_limited: false,
                    origin: SessionTrajectoryOrigin::Native,
                    document_version: 2,
                    derived_title: None,
                    title_authoritative: false,
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
                    .map(|(session, record)| (session.clone(), record.title.clone())))
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
            FULL_READ_SOURCE_BYTES,
        )?;
        replace("bravo")?;
        let guard = store.observe_verified_session(&session)?;
        let (read, source_time) = crate::read_search_snapshot(&registry, &session, true)?;

        crate::store_search_document(
            &store,
            &session,
            &read.snapshot,
            true,
            (source_time.unwrap(), read.source_revision),
            guard,
            read.native_session.as_ref(),
        )?;
        assert_eq!(store.trajectory_title(&session)?.as_deref(), Some("bravo"));

        replace("delta")?;
        let mut summary = IndexSummary::default();
        flush_batch(
            &registry,
            &store,
            &mut vec![prepared],
            &mut summary,
            &mut Vec::new(),
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
                previous_failure: None,
                candidate: IndexCandidate {
                    source_revision: SourceRevision::default(),
                    session: SessionRef::new(Provider::Codex, id),
                    source_path: None,
                    updated_at: Some(now),
                    document_version: search_document_version(Provider::Codex),
                },
                guard: TrajectorySourceGuard::NewerSource,
                verified_metadata: None,
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
                provider_limited: false,
                origin: SessionTrajectoryOrigin::Native,
                title: Some(id.to_owned()),
                title_authoritative: true,
            }
        };
        let mut batch = vec![prepared("first"), prepared("broken"), prepared("last")];
        let mut summary = IndexSummary::default();
        let mut titles = Vec::new();

        flush_batch(
            &AdapterRegistry::new(),
            &store,
            &mut batch,
            &mut summary,
            &mut titles,
        );

        assert!(batch.is_empty());
        assert_eq!((summary.indexed, summary.failed), (2, 1));
        assert_eq!(
            titles,
            vec![
                (
                    SessionRef::new(Provider::Codex, "first"),
                    omnis_store::IndexedTitle {
                        source_revision: SourceRevision::default(),
                        title: Some("first".to_owned()),
                        verified_at: None,
                        cached_at: None,
                    }
                ),
                (
                    SessionRef::new(Provider::Codex, "last"),
                    omnis_store::IndexedTitle {
                        source_revision: SourceRevision::default(),
                        title: Some("last".to_owned()),
                        verified_at: None,
                        cached_at: None,
                    }
                ),
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
            pending_summary(&store, candidates.clone())
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
                    indexed_byte_count: 3,
                    truncation_strategy: "none",
                    source_complete: true,
                    provider_limited: false,
                    origin: SessionTrajectoryOrigin::Native,
                    document_version: 4,
                    derived_title: None,
                    title_authoritative: false,
                },
            )
            .unwrap();
        let archive = IndexCandidate {
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

        fn read_session(&self, session: &SessionRef) -> Result<CanonicalSnapshot> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            if self.fail.load(Ordering::Relaxed) {
                anyhow::bail!("synthetic unreadable source");
            }
            self.inner.read_session(session)
        }

        fn read_session_at(
            &self,
            session: &SessionRef,
            source_path: Option<&Path>,
        ) -> Result<CanonicalSnapshot> {
            assert!(
                source_path.is_some(),
                "indexer omitted discovered source path"
            );
            self.read_session(session)
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
            pending_summary(&store, candidates.clone())
                .expect("pending summary")
                .failed_skipped,
            1
        );

        let changed = candidates
            .iter()
            .cloned()
            .map(|candidate| IndexCandidate {
                updated_at: candidate
                    .updated_at
                    .map(|updated_at| updated_at + chrono::Duration::seconds(1)),
                ..candidate
            })
            .collect::<Vec<_>>();
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
