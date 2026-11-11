use std::{
    cmp::Reverse,
    collections::{BTreeMap, HashMap},
    fs,
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
    sync::{Condvar, Mutex, MutexGuard, PoisonError},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use crossterm::{
    cursor::MoveToColumn,
    queue,
    style::Print,
    terminal::{Clear, ClearType},
};
use omnis_adapters::{
    AdapterRegistry, AntigravityAdapter, IndexRead, NativeSession, ProviderAdapter,
    SearchSourceObservation,
};
use omnis_core::{
    SEARCH_DOCUMENT_VERSION, SearchDocument, session_search_title, trajectory_search_document,
    workspace_paths_match,
};
use omnis_ir::{Provider, SessionRef, SourceRevision};
use omnis_store::{
    MAX_TRAJECTORY_WRITE_BATCH_BYTES, MAX_TRAJECTORY_WRITE_BATCH_DOCUMENTS,
    SessionTrajectoryOrigin, Store, TrajectoryDocument, TrajectoryIndexFailure,
    TrajectoryIndexState, TrajectorySourceGuard,
};

use crate::{imported_bundle_id, load_imported_bundle};

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
    pub(crate) source_revision: SourceRevision,
}

impl IndexCandidate {
    // Sessions without a discovered source file are skipped so oversized files are never read
    // whole. Reading an OpenCode session spawns its CLI, so those index when opened instead.
    pub(crate) fn from_session(session: &NativeSession) -> Option<Self> {
        let provider = session.session.provider;
        let readable = provider == Provider::Imported
            || (provider != Provider::OpenCode
                && session.source_path.is_some()
                && (provider != Provider::Antigravity || session.source_revision.is_some()));
        readable.then(|| Self {
            session: session.session.clone(),
            // The picker may display a shared database's age for an undated session.
            updated_at: if session.updated_at_approximate
                && (matches!(provider, Provider::CursorCli | Provider::CursorIde)
                    || Self::summary_path(provider, session.source_path.as_deref()))
            {
                session.created_at
            } else {
                session.updated_at
            },
            source_path: session.source_path.clone(),
            source_revision: session.source_revision.unwrap_or_default(),
        })
    }

    pub(crate) fn summary_path(provider: Provider, path: Option<&Path>) -> bool {
        provider == Provider::Antigravity
            && path.is_some_and(AntigravityAdapter::is_summary_catalog)
    }

    fn native_summary(&self) -> bool {
        Self::summary_path(self.session.provider, self.source_path.as_deref())
    }

    fn observe_source(
        &self,
        registry: &AdapterRegistry,
    ) -> Result<Option<SearchSourceObservation>> {
        if self.source_revision.file.is_none() {
            return Ok(None);
        }
        let observed = registry
            .adapter(self.session.provider)?
            .observe_search_source(&self.session, self.source_path.as_deref())?;
        anyhow::ensure!(
            verified_source(observed.as_ref(), self.source_revision),
            "provider source changed during search read"
        );
        Ok(observed)
    }

    fn oversized(&self) -> bool {
        self.source_path
            .as_deref()
            .and_then(|path| fs::metadata(path).ok())
            .is_some_and(|metadata| metadata.is_file() && metadata.len() > FULL_READ_SOURCE_BYTES)
    }

    fn source_time(&self) -> Option<DateTime<Utc>> {
        // These readers use the Unix epoch when the session has no native timestamp.
        self.updated_at.or_else(|| {
            (self.native_summary()
                || matches!(
                    self.session.provider,
                    Provider::CursorCli | Provider::CursorIde
                ))
            .then_some(DateTime::UNIX_EPOCH)
        })
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct IndexProgress {
    pub(crate) indexed: usize,
    pub(crate) total: usize,
    pub(crate) failed: usize,
    pub(crate) titles: Vec<(SessionRef, Option<String>)>,
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

// A restore can also move a provider's reported event time backwards.
pub(crate) fn source_time_changed(
    provider: Provider,
    current: DateTime<Utc>,
    cached: &TrajectoryIndexState,
) -> bool {
    if provider == Provider::Imported {
        current.timestamp_millis() > cached.source_updated_at.timestamp_millis()
    } else {
        !cached.source_time_precise || current != cached.source_updated_at
    }
}

pub(crate) fn source_guard(
    store: &Store,
    session: &SessionRef,
    prior: Option<&TrajectoryIndexState>,
) -> omnis_store::Result<TrajectorySourceGuard> {
    if TrajectorySourceGuard::uses_file_time(session.provider) {
        Ok(match prior {
            Some(state) if !state.source_time_precise => {
                TrajectorySourceGuard::ObservedLegacy(state.source_updated_at)
            }
            _ => TrajectorySourceGuard::Observed(prior.map(|state| state.source_updated_at)),
        })
    } else if session.provider == Provider::Imported {
        Ok(TrajectorySourceGuard::NewerSource)
    } else {
        store.observe_trajectory_document(session)
    }
}

pub(crate) fn observe_source(
    store: &Store,
    session: &SessionRef,
) -> omnis_store::Result<TrajectorySourceGuard> {
    if TrajectorySourceGuard::uses_file_time(session.provider) {
        source_guard(
            store,
            session,
            store.trajectory_index_state(session)?.as_ref(),
        )
    } else {
        source_guard(store, session, None)
    }
}

pub(crate) fn needs_index(
    candidate: &IndexCandidate,
    state: Option<&TrajectoryIndexState>,
) -> bool {
    let Some(state) = state else {
        return true;
    };
    if state.document_version > SEARCH_DOCUMENT_VERSION {
        return false;
    }
    state.document_version < SEARCH_DOCUMENT_VERSION
        || (candidate.source_revision.file.is_some() && candidate.source_revision != state.source_revision)
        || candidate.native_summary() != state.native_summary
        || (!state.source_time_precise && candidate.session.provider != Provider::Imported)
        || candidate.source_time().is_some_and(|updated_at| {
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
/// Retry when the selected source, its recorded time, or the document version changes.
fn failure_is_current(candidate: &IndexCandidate, failure: &TrajectoryIndexFailure) -> bool {
    failure.document_version >= SEARCH_DOCUMENT_VERSION
        && candidate.source_revision.file == failure.source_file
        && candidate.source_revision.contents == failure.source_contents
        && candidate.native_summary() == failure.native_summary
        && (failure.source_time_precise
            || failure.source_updated_at.is_none()
            || candidate.session.provider == Provider::Imported)
        && candidate.source_time().is_none_or(|updated_at| {
            failure.source_updated_at.is_some_and(|failed_source| {
                if candidate.session.provider == Provider::Imported {
                    updated_at.timestamp_millis() == failed_source.timestamp_millis()
                } else {
                    updated_at == failed_source
                }
            })
        })
}

/// Whether a later read can succeed even if the provider's session time stays the same.
fn retryable_read_failure(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause.is::<omnis_adapters::UnverifiedSqliteWal>() || cause.is::<omnis_adapters::IncompleteTranscript>() || cause
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
    let mut workspace_matches = HashMap::new();
    let mut ordered = sessions
        .iter()
        .filter_map(|session| {
            let candidate = IndexCandidate::from_session(session)?;
            let current = if let (Some(project), Some(path)) =
                (current_project, session.project_path.as_deref())
            {
                *workspace_matches
                    .entry(path.as_os_str())
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

fn read_budget(provider: Provider, prior: Option<&TrajectoryIndexState>) -> u64 {
    // A format or timestamp migration retains the full text a previous indexer read.
    if prior.is_some_and(|state| {
        (state.source_complete || state.provider_limited)
            && (state.document_version < SEARCH_DOCUMENT_VERSION
                || (provider == Provider::Antigravity && state.source_revision.file.is_none())
                || (provider != Provider::Imported && !state.source_time_precise))
    }) {
        u64::MAX
    } else {
        FULL_READ_SOURCE_BYTES
    }
}

/// Stale candidates in order, the failures they were checked against, and their counts.
struct Pending {
    stale: Vec<(IndexCandidate, TrajectorySourceGuard, u64)>,
    failures: HashMap<SessionRef, TrajectoryIndexFailure>,
    summary: IndexSummary,
}

fn pending(
    store: &Store,
    candidates: Vec<IndexCandidate>,
    retry_failed: bool,
    observe: bool,
) -> Result<Pending> {
    let states = store.trajectory_index_states()?;
    let failures = store.trajectory_index_failures()?;
    let mut summary = IndexSummary {
        candidates: candidates.len(),
        ..IndexSummary::default()
    };
    let mut stale = Vec::new();
    for candidate in candidates {
        let prior = states.get(&candidate.session);
        let failure = failures.get(&candidate.session);
        // A verified return to the cached source retires a failure for the other source.
        let failed_other_source = candidate.session.provider == Provider::Antigravity
            && prior.is_some_and(|state| state.document_version == SEARCH_DOCUMENT_VERSION)
            && failure.is_some_and(|failure| {
                failure.document_version <= SEARCH_DOCUMENT_VERSION
                    && (failure.native_summary != candidate.native_summary()
                        || failure.source_file != candidate.source_revision.file
                        || failure.source_contents != candidate.source_revision.contents)
            });
        if !needs_index(&candidate, prior) && !failed_other_source {
            continue;
        }
        if !retry_failed && failure.is_some_and(|failure| failure_is_current(&candidate, failure)) {
            summary.failed_skipped += 1;
        } else {
            let guard = if observe {
                source_guard(store, &candidate.session, prior)?
            } else {
                TrajectorySourceGuard::NewerSource
            };
            let budget = read_budget(candidate.session.provider, prior);
            stale.push((candidate, guard, budget));
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
    Ok(pending(store, candidates, false, false)?.summary)
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
        mut summary,
    } = pending(store, candidates, retry_failed, true)?;
    report(IndexProgress {
        total: stale.len(),
        ..IndexProgress::default()
    });
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
        .then(|| prepare_session(registry, None, candidate, *guard, *budget))
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
            let result = prepared.unwrap_or_else(|| {
                prepare_session(registry, Some(store), candidate, *guard, *budget)
            });
            // Antigravity clears its failure inside the guarded history or summary write.
            if candidate.session.provider != Provider::Antigravity
                && result.is_ok()
                && failures.contains_key(&candidate.session)
            {
                let _ = store.clear_trajectory_index_failure(&candidate.session);
            }
            match result {
                Ok(prepared) => {
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
                    if !retryable_read_failure(&error)
                        && let Ok(observed) = candidate.observe_source(registry)
                    {
                        let _ = store.record_observed_trajectory_index_failure_checked(
                            &candidate.session,
                            &TrajectoryIndexFailure {
                                source_updated_at: candidate.source_time(),
                                source_time_precise: true,
                                native_summary: candidate.native_summary(),
                                source_file: candidate.source_revision.file,
                                source_contents: candidate.source_revision.contents,
                                document_version: SEARCH_DOCUMENT_VERSION,
                            },
                            *guard,
                            |_, revision| verified_source(observed.as_ref(), revision),
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
    titles: &mut Vec<(SessionRef, Option<String>)>,
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
    if let Ok(indexed) =
        store.upsert_observed_trajectory_documents_checked(&documents, |session, revision| {
            batch
                .iter()
                .find(|item| item.candidate.session == *session)
                .is_some_and(|item| item.source.verifies(revision))
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
                if let Ok(fresh) = reread_after_conflict(registry, store, &prepared.candidate) {
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
) -> Result<PreparedIndex> {
    let session = &candidate.session;
    let prior = store.trajectory_index_state(session)?;
    let guard = source_guard(store, session, prior.as_ref())?;
    let current = if candidate.source_revision.file.is_some() {
        registry
            .find_native_session(session.provider, &session.id)?
            .as_ref()
            .and_then(IndexCandidate::from_session)
            .context("provider source changed during indexing")?
    } else {
        candidate.clone()
    };
    prepare_session(
        registry,
        Some(store),
        &current,
        guard,
        read_budget(session.provider, prior.as_ref()),
    )
}

fn progress(
    summary: &IndexSummary,
    processed: usize,
    titles: &mut Vec<(SessionRef, Option<String>)>,
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
    candidate: IndexCandidate,
    document: SearchDocument,
    source: SearchSource,
    source_complete: bool,
    provider_limited: bool,
    title: Option<String>,
}

impl PreparedIndex {
    fn report_title(self, store: &Store, titles: &mut Vec<(SessionRef, Option<String>)>) {
        let session = self.candidate.session;
        if let Ok(title) = store.trajectory_title(&session) {
            titles.push((session, title));
        }
    }

    fn store(&self, store: &Store) -> omnis_store::Result<usize> {
        store.upsert_observed_trajectory_documents_checked(
            &[(&self.candidate.session, self.store_document(), self.guard)],
            |_, revision| self.source.verifies(revision),
        )
    }

    fn store_document(&self) -> TrajectoryDocument<'_> {
        TrajectoryDocument {
            redacted_text: &self.document.text,
            source_updated_at: self.source.updated_at,
            source_revision: self.source.revision(),
            source_byte_count: self.document.source_byte_count,
            indexed_byte_count: self.document.indexed_byte_count,
            truncation_strategy: self.document.truncation_strategy.as_str(),
            source_complete: self.source_complete,
            provider_limited: self.provider_limited,
            origin: self.source.origin,
            document_version: SEARCH_DOCUMENT_VERSION,
            derived_title: self.title.as_deref(),
        }
    }
}

pub(crate) fn read_imported(
    store: Option<&Store>,
    session: &SessionRef,
) -> anyhow::Result<IndexRead> {
    let store = store.context("local imported bundle store is unavailable")?;
    let bundle = load_imported_bundle(store, imported_bundle_id(session)?)?;
    Ok(IndexRead {
        snapshot: bundle.snapshot,
        source_complete: true,
        provider_limited: false,
    })
}

pub(crate) struct SearchSource {
    pub(crate) updated_at: DateTime<Utc>,
    pub(crate) origin: SessionTrajectoryOrigin,
    pub(crate) observation: Option<SearchSourceObservation>,
}

fn verified_source(
    observation: Option<&SearchSourceObservation>,
    expected: SourceRevision,
) -> bool {
    observation.map_or_else(
        || expected == SourceRevision::default(),
        |observed| observed.revision == expected && observed.is_current(),
    )
}

impl SearchSource {
    pub(crate) fn revision(&self) -> SourceRevision {
        self.observation
            .as_ref()
            .map(|source| source.revision)
            .unwrap_or_default()
    }

    pub(crate) fn verifies(&self, expected: SourceRevision) -> bool {
        verified_source(self.observation.as_ref(), expected)
    }

    pub(crate) fn imported(updated_at: DateTime<Utc>) -> Self {
        Self {
            updated_at,
            origin: SessionTrajectoryOrigin::ImportedBundle,
            observation: None,
        }
    }
}

pub(crate) fn observe_native_read<T>(
    registry: &AdapterRegistry,
    session: &SessionRef,
    path: Option<&Path>,
    expected: Option<SourceRevision>,
    read: impl FnOnce(&dyn ProviderAdapter, Option<&Path>) -> Result<(T, DateTime<Utc>)>,
) -> Result<(T, Option<SearchSource>)> {
    let observation = registry
        .adapter(session.provider)?
        .observe_search_source(session, path)?;
    if let Some(expected) = expected {
        anyhow::ensure!(
            verified_source(observation.as_ref(), expected),
            "provider source changed during search read"
        );
    }
    let result = registry.observe_search_read(session, path, |adapter, path| {
        let (value, time) = read(adapter, path)?;
        let origin = if adapter.search_source_is_summary(session, path)? {
            SessionTrajectoryOrigin::NativeSummary
        } else {
            SessionTrajectoryOrigin::Native
        };
        Ok(((value, origin), time))
    });
    let current = observation
        .as_ref()
        .is_none_or(SearchSourceObservation::is_current);
    if result.is_err() && !current {
        anyhow::bail!("provider source changed during search read");
    }
    let ((value, origin), updated_at) = result?;
    Ok((
        value,
        updated_at
            .filter(|_| current)
            .map(|updated_at| SearchSource {
                updated_at,
                origin,
                observation,
            }),
    ))
}

fn prepare_session(
    registry: &AdapterRegistry,
    imported_store: Option<&Store>,
    candidate: &IndexCandidate,
    guard: TrajectorySourceGuard,
    full_read_bytes: u64,
) -> anyhow::Result<PreparedIndex> {
    let (read, source) = if candidate.session.provider == Provider::Imported {
        let read = read_imported(imported_store, &candidate.session)?;
        let source = SearchSource::imported(read.snapshot.captured_at);
        (read, Some(source))
    } else {
        observe_native_read(
            registry,
            &candidate.session,
            candidate.source_path.as_deref(),
            Some(candidate.source_revision),
            |adapter, path| adapter.index_for_search(&candidate.session, path, full_read_bytes),
        )?
    };
    let IndexRead {
        snapshot,
        source_complete,
        provider_limited,
    } = read;
    let document = trajectory_search_document(&snapshot);
    let source = source.ok_or_else(|| anyhow::anyhow!("transcript changed during search read"))?;
    Ok(PreparedIndex {
        guard,
        candidate: candidate.clone(),
        document,
        source,
        source_complete,
        provider_limited,
        title: session_search_title(&snapshot),
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
        let now = DateTime::from_timestamp(1_800_000_000, 750_000).unwrap();
        let current = TrajectoryIndexState {
            source_revision: SourceRevision::default(),
            source_updated_at: now,
            source_time_precise: true,
            native_summary: false,
            source_complete: true,
            provider_limited: false,
            document_version: SEARCH_DOCUMENT_VERSION,
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
        assert!(needs_index(&small, Some(&stored)));
        let future = TrajectoryIndexState {
            document_version: SEARCH_DOCUMENT_VERSION + 1,
            ..stored
        };
        assert!(!needs_index(&small, Some(&future)));
        let legacy = TrajectoryIndexState {
            source_time_precise: false,
            ..stored
        };
        let exact_millisecond = candidate(Some(stored.source_updated_at), None);
        assert!(needs_index(&exact_millisecond, Some(&legacy)));
        assert!(!needs_index(&exact_millisecond, Some(&stored)));
        assert_eq!(read_budget(Provider::Codex, Some(&legacy)), u64::MAX);
    }

    #[test]
    fn losing_cursor_timestamps_retries_the_session_once() {
        let recorded = Utc::now();
        let mut undated = candidate(None, None);
        undated.session.provider = Provider::CursorIde;
        let cached = TrajectoryIndexState {
            source_revision: SourceRevision::default(),
            source_updated_at: recorded,
            source_time_precise: true,
            native_summary: false,
            source_complete: true,
            provider_limited: false,
            document_version: SEARCH_DOCUMENT_VERSION,
        };
        let failure = TrajectoryIndexFailure {
            source_file: None,
            source_contents: None,
            source_updated_at: Some(recorded),
            source_time_precise: true,
            native_summary: false,
            document_version: SEARCH_DOCUMENT_VERSION,
        };
        assert!(needs_index(&undated, Some(&cached)));
        assert!(!failure_is_current(&undated, &failure));
        let missing = DateTime::UNIX_EPOCH;
        assert!(!needs_index(
            &undated,
            Some(&TrajectoryIndexState {
                source_updated_at: missing,
                ..cached
            })
        ));
        assert!(failure_is_current(
            &undated,
            &TrajectoryIndexFailure {
                source_updated_at: Some(missing),
                ..failure
            }
        ));
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
            source_updated_at: now,
            source_time_precise: true,
            native_summary: false,
            source_complete: false,
            provider_limited: false,
            document_version: SEARCH_DOCUMENT_VERSION,
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
            native_summary: false,
            document_version: SEARCH_DOCUMENT_VERSION,
        };

        assert!(failure_is_current(&candidate(Some(now), None), &failure));
        assert!(failure_is_current(&candidate(None, None), &failure));
        assert!(!failure_is_current(
            &candidate(Some(now - chrono::Duration::nanoseconds(1)), None),
            &failure
        ));
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

        let mut network_spelling = std::ffi::OsString::from("//");
        network_spelling.push(current.strip_prefix("/").unwrap_or(&current));
        let ordered = ordered_candidates(
            &[
                session("current-older", &current, 30),
                session("other-newest", &other, 1),
                session("current-newer", &current, 5),
                session("network-newest", Path::new(&network_spelling), 0),
            ],
            Some(&current),
        );

        assert_eq!(
            ordered
                .iter()
                .map(|candidate| candidate.session.id.as_str())
                .collect::<Vec<_>>(),
            [
                "current-newer",
                "current-older",
                "network-newest",
                "other-newest"
            ]
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
                .flat_map(|report| report.titles.iter().cloned())
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

    fn write_codex_rollout(
        home: &Path,
        marker: Option<&str>,
        modified: Duration,
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
            .set_modified(std::time::UNIX_EPOCH + modified)
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
        let (registry, original) = write_codex_rollout(
            &home,
            Some("oldneedle"),
            Duration::new(1_800_000_000, 750_000),
        );
        index_candidates(
            &registry,
            &store,
            original.clone(),
            false,
            &|| false,
            &mut |_| {},
        )
        .unwrap();
        let guard = observe_source(&store, &original[0].session).unwrap();
        let delayed = prepare_session(
            &registry,
            Some(&store),
            &original[0],
            guard,
            FULL_READ_SOURCE_BYTES,
        )
        .unwrap();

        let (registry, restored) = write_codex_rollout(
            &home,
            Some("backupneedle"),
            Duration::new(1_800_000_000, 250_000),
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
        let index = || {
            index_candidates(
                &registry,
                &store,
                restored.clone(),
                false,
                &|| false,
                &mut |_| {},
            )
            .unwrap()
        };
        assert_eq!(index().indexed, 1);
        assert!(matches!(
            delayed.store(&store),
            Err(omnis_store::StoreError::TrajectorySourceChanged)
        ));
        let mut recovered = IndexSummary::default();
        let mut titles = Vec::new();
        flush_batch(
            &registry,
            &store,
            &mut vec![delayed],
            &mut recovered,
            &mut titles,
        );
        assert_eq!(recovered.indexed, 0);
        assert_eq!(
            titles,
            [(restored[0].session.clone(), Some("backupneedle".to_owned()))]
        );
        assert!(
            store
                .search_session_trajectories("oldneedle", 10)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            store
                .search_session_trajectories("backupneedle", 10)
                .unwrap(),
            [restored[0].session.clone()]
        );
        assert_eq!(index().stale, 0);

        let (registry, header_only) =
            write_codex_rollout(&home, None, Duration::from_secs(1_799_900_000));
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
        assert_eq!(updates, [(restored[0].session.clone(), None)]);
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
                guard: TrajectorySourceGuard::NewerSource,
                candidate: IndexCandidate {
                    source_revision: SourceRevision::default(),
                    session: SessionRef::new(Provider::Codex, id),
                    updated_at: Some(now),
                    source_path: None,
                },
                document: SearchDocument {
                    source_byte_count: text.len(),
                    indexed_byte_count: text.len(),
                    text,
                    truncated: false,
                    source_complete: true,
                    truncation_strategy: omnis_core::SearchTruncationStrategy::None,
                },
                source: SearchSource {
                    updated_at: now,
                    origin: SessionTrajectoryOrigin::Native,
                    observation: None,
                },
                source_complete: true,
                provider_limited: false,
                title: Some(id.to_owned()),
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
                    Some("first".to_owned())
                ),
                (
                    SessionRef::new(Provider::Codex, "last"),
                    Some("last".to_owned())
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

        let modified = candidates[0].updated_at.unwrap() + chrono::Duration::seconds(1);
        File::options()
            .write(true)
            .open(candidates[0].source_path.as_ref().unwrap())
            .unwrap()
            .set_modified(modified.into())
            .unwrap();
        let changed = [IndexCandidate {
            updated_at: Some(modified),
            ..candidates[0].clone()
        }];
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
