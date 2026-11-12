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

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use crossterm::{
    cursor::MoveToColumn,
    queue,
    style::Print,
    terminal::{Clear, ClearType},
};
use omnis_adapters::{
    AdapterRegistry, AntigravityAdapter, AntigravityDesktopAdapter, AntigravityIdeAdapter,
    IndexRead, IndexSessionReader, IndexedSearchDocument, NativeSession, NativeSessionName,
    PriorSearchDocument, ProviderAdapter, RollbackTitle, SearchSourceObservation,
};
use omnis_core::{
    SearchDocument, SessionWorkspaceMatcher, imported_search_document_version,
    indexed_search_title, search_document_version, session_message_prefix_is_known,
    session_message_search_title, session_search_title, trajectory_search_document,
};
use omnis_ir::{CanonicalSnapshot, Provider, SessionRef, SourceRevision};
use omnis_store::{
    IndexedSession, IndexedTrajectoryTitle, LocalOpenerEvidence, MAX_TRAJECTORY_WRITE_BATCH_BYTES,
    MAX_TRAJECTORY_WRITE_BATCH_DOCUMENTS, SelectedSessionObservation, SessionTrajectoryOrigin,
    Store, TrajectoryDocument, TrajectoryIndexFailure, TrajectoryIndexState, TrajectorySourceGuard,
};

use crate::{imported_bundle_id, load_imported_bundle};

// Larger sources index only their sampled head and tail.
pub(crate) const FULL_READ_SOURCE_BYTES: u64 = omnis_adapters::CodexAdapter::FULL_READ_SOURCE_BYTES;
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
    pub(crate) metadata: Option<IndexedSession>,
    catalog: Option<IndexedSession>,
    document_version: u32,
    listed_name: Option<NativeSessionName>,
}

impl IndexCandidate {
    fn catalog_metadata_changed(
        &self,
        native_catalog: &HashMap<SessionRef, IndexedSession>,
    ) -> bool {
        self.catalog.as_ref().is_some_and(|listed| {
            native_catalog.get(&listed.session).is_none_or(|stored| {
                listed.title != stored.title
                    || listed.project_path != stored.project_path
                    || listed.event_count != stored.event_count
            })
        })
    }

    fn must_verify_grok_owners(
        &self,
        store: &Store,
        prior_conflicts: &HashSet<String>,
    ) -> Result<bool> {
        Ok(self.session.provider == Provider::Grok
            && prior_conflicts.contains(&self.session.id)
            && store.grok_updates_need_verification(&self.session, self.updates_revision())?)
    }

    fn should_retry_name(
        &self,
        registry: &AdapterRegistry,
        deferred_names: &HashMap<SessionRef, SourceRevision>,
    ) -> Result<bool> {
        Ok(
            deferred_names.get(&self.session) == Some(&self.source_revision)
                && matches!(
                    self.observed_name(registry)?,
                    Some(NativeSessionName::Absent | NativeSessionName::Gui(None))
                ),
        )
    }

    fn observed_name(&self, registry: &AdapterRegistry) -> Result<Option<NativeSessionName>> {
        match &self.listed_name {
            Some(name) => Ok(Some(name.clone())),
            None => native_codex_name(registry, &self.session),
        }
    }

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

    fn failure(&self) -> TrajectoryIndexFailure {
        TrajectoryIndexFailure {
            ancestor_revisions: Vec::new(),
            source_updated_at: self.source_time(),
            source_time_precise: true,
            native_summary: self.native_summary(),
            source_file: self.source_revision.file,
            source_contents: self.source_revision.contents,
            document_version: self.document_version,
        }
    }

    fn selected_metadata(
        &self,
        registry: &AdapterRegistry,
        revision: SourceRevision,
    ) -> Option<IndexedSession> {
        if self.session.provider == Provider::Grok {
            return self.metadata.clone().filter(|row| {
                row.source_revision
                    .is_some_and(|selected| selected.matches_file_contents(revision))
            });
        }
        self.selected_codex(registry, revision)
            .as_ref()
            .map(crate::indexed_session_metadata)
    }

    fn record_revision_failure(
        &self,
        registry: &AdapterRegistry,
        store: &Store,
        guard: &TrajectorySourceGuard,
        error: &anyhow::Error,
    ) -> Option<IndexedTrajectoryTitle> {
        if self.session.provider == Provider::Codex
            && omnis_adapters::CodexAdapter::is_current_rollout_unavailable(error)
        {
            if let Err(error) = crate::remove_unavailable_session(registry, store, &self.session) {
                eprintln!("warning: local trajectory index write: {error}");
            }
            return None;
        }
        let compressed = self
            .source_path
            .as_deref()
            .is_some_and(omnis_adapters::CodexAdapter::is_compressed_rollout);
        if retryable_read_error(error, compressed) {
            return None;
        }
        if self.session.provider == Provider::Codex {
            self.selected_codex(registry, self.source_revision)?;
        }
        let ancestor_revisions = omnis_adapters::CodexAdapter::failed_index_ancestors(error);
        if confirm_index_ancestors(registry, ancestor_revisions).is_err() {
            return None;
        }
        let failure = TrajectoryIndexFailure {
            ancestor_revisions: ancestor_revisions.to_vec(),
            ..self.failure()
        };
        store
            .record_observed_trajectory_index_failure(&self.session, &failure, *guard)
            .ok()?;
        (self.session.provider == Provider::Codex)
            .then(|| store.indexed_trajectory_title(&self.session).ok().flatten())
            .flatten()
    }

    fn record_source_failure(
        &self,
        registry: &AdapterRegistry,
        store: &Store,
        guard: &TrajectorySourceGuard,
        error: &anyhow::Error,
    ) -> omnis_store::Result<()> {
        if retryable_read_failure(error) || error.is::<omnis_adapters::TranscriptReadLimit>() {
            return Ok(());
        }
        let observed = self
            .observe_source(registry)
            .map_err(|_| omnis_store::StoreError::TrajectorySourceChanged)?;
        store.record_observed_trajectory_index_failure_checked(
            &self.session,
            &self.failure(),
            guard_observed_home(guard, observed.as_ref()),
            |_, revision| verified_source(observed.as_ref(), revision),
        )
    }

    // Sessions without a discovered source file are skipped so oversized files are never read
    // whole. Reading an OpenCode session spawns its CLI, so those index when opened instead.
    pub(crate) fn from_session(session: &NativeSession) -> Option<Self> {
        let provider = session.session.provider;
        let readable = provider == Provider::Imported
            || (provider != Provider::OpenCode
                && session.source_path.is_some()
                && (!TrajectorySourceGuard::uses_catalog_home(provider)
                    || session.source_revision.is_some()));
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
            catalog: matches!(
                provider,
                Provider::Antigravity | Provider::AntigravityDesktop
            )
            .then(|| crate::indexed_session_metadata(session)),
            document_version: search_document_version(provider),
            listed_name: None,
            metadata: matches!(
                provider,
                Provider::Codex | Provider::Grok | Provider::Pi | Provider::OhMyPi
            )
            .then(|| crate::indexed_session_metadata(session)),
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

    fn can_upgrade_preview(&self) -> bool {
        if self.session.provider == Provider::Grok {
            return self
                .source_path
                .as_deref()
                .and_then(omnis_adapters::GrokAdapter::updates_size)
                .is_some_and(|size| size <= FULL_READ_SOURCE_BYTES);
        }
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

    fn updates_revision(&self) -> Option<SourceRevision> {
        if self.session.provider == Provider::Grok {
            self.source_path
                .as_deref()
                .and_then(omnis_adapters::GrokAdapter::updates_revision)
        } else {
            None
        }
    }

    fn file_check(&self, guard: &TrajectorySourceGuard) -> FileCheck<'_> {
        FileCheck {
            session: &self.session,
            path: self.source_path.as_deref(),
            revision: self.source_revision,
            catalog: self.catalog.as_ref(),
            guard: *guard,
        }
    }

    fn record_failure(
        &self,
        selections: &mut FileSelections<'_>,
        store: &Store,
        guard: &TrajectorySourceGuard,
    ) -> omnis_store::Result<()> {
        select_pi_candidates(store, std::slice::from_ref(self))?;
        let source = if matches!(
            self.session.provider,
            Provider::Antigravity | Provider::AntigravityDesktop
        ) {
            self.observe_source(selections.registry)
                .map_err(|_| omnis_store::StoreError::TrajectorySourceChanged)?
        } else {
            None
        };
        let guard = guard_observed_home(guard, source.as_ref());
        selections.write(&[self.file_check(&guard)], |verify| {
            store.record_observed_trajectory_index_failure_checked(
                &self.session,
                &self.failure(),
                guard,
                |session, revision| {
                    verify(session, revision)
                        && source
                            .as_ref()
                            .is_none_or(|source| verified_source(Some(source), revision))
                },
            )
        })
    }
}

/// Records a structural failure against the exact file examined by the full native reader.
pub(crate) fn record_strict_read_failure<'read>(
    selections: &mut FileSelections<'_>,
    store: &Store,
    guard: &TrajectorySourceGuard,
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
        Provider::Antigravity | Provider::AntigravityDesktop | Provider::AntigravityIde
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

// A restore can also move a provider's reported event time backwards.

pub(crate) fn source_time_changed(
    provider: Provider,
    current: DateTime<Utc>,
    cached: &TrajectoryIndexState,
) -> bool {
    if TrajectorySourceGuard::uses_catalog_home(provider) && cached.source_revision.file.is_some() {
        // The verified physical revision covers the history. The catalog has its own clock.
        false
    } else if provider == Provider::Grok {
        current.timestamp_millis() != cached.source_updated_at.timestamp_millis()
    } else if provider == Provider::Imported {
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
    if matches!(
        session.provider,
        Provider::Antigravity | Provider::AntigravityDesktop
    ) {
        let catalog = store.observe_trajectory_document(session)?;
        Ok(catalog.attach_verified_file(store.observe_verified_session(session)?))
    } else if TrajectorySourceGuard::uses_guarded_catalog(session.provider) {
        store.observe_verified_session(session)
    } else if session.provider == Provider::Grok
        || TrajectorySourceGuard::uses_file_time(session.provider)
    {
        Ok(TrajectorySourceGuard::Observed(prior.copied()))
    } else if session.provider == Provider::Imported {
        Ok(TrajectorySourceGuard::NewerSource)
    } else {
        store.observe_trajectory_document(session)
    }
}

fn background_source_guard(
    store: &Store,
    session: &SessionRef,
    prior: Option<&TrajectoryIndexState>,
) -> omnis_store::Result<TrajectorySourceGuard> {
    if TrajectorySourceGuard::uses_catalog_home(session.provider) {
        store.observe_discovered_trajectory_document(session)
    } else {
        source_guard(store, session, prior)
    }
}

fn guard_observed_home(
    guard: &TrajectorySourceGuard,
    observation: Option<&SearchSourceObservation>,
) -> TrajectorySourceGuard {
    observation.map_or(*guard, |source| {
        guard_observed_steps(&guard.for_published_catalog_home(&source.home), source)
    })
}

pub(super) fn guard_observed_steps(
    guard: &TrajectorySourceGuard,
    source: &SearchSourceObservation,
) -> TrajectorySourceGuard {
    let guard =
        guard.with_database_contents(source.database_contents(), source.database_checkpoint());
    if source.is_ide_desktop_database() && !source.desktop_summaries_unavailable {
        guard.with_database_alias_step_count(source.session.event_count)
    } else {
        guard.with_catalog_step_count(
            source.session.session.provider,
            source.session.event_count,
            source.desktop_summaries_unavailable,
        )
    }
}

pub(crate) fn observe_source(
    store: &Store,
    session: &SessionRef,
) -> omnis_store::Result<TrajectorySourceGuard> {
    if session.provider == Provider::Grok
        || (TrajectorySourceGuard::uses_file_time(session.provider)
            && !TrajectorySourceGuard::uses_guarded_catalog(session.provider))
    {
        source_guard(
            store,
            session,
            store.trajectory_index_state(session)?.as_ref(),
        )
    } else {
        source_guard(store, session, None)
    }
}

// Some older indexers stored a zero remainder even when the rollout clock had finer precision.
// Each caller also checks the file and contents or verifies the historical projection hash.
fn codex_projection_time_matches(current: DateTime<Utc>, prior: &TrajectoryIndexState) -> bool {
    let rounded = (5..=7).contains(&prior.document_version)
        && prior
            .source_updated_at
            .timestamp_subsec_nanos()
            .is_multiple_of(1_000_000);
    if !prior.source_time_precise || rounded {
        current.timestamp_millis() == prior.source_updated_at.timestamp_millis()
    } else {
        current == prior.source_updated_at
    }
}

/// Keeps a full search document when the same rollout needs a projection upgrade.
pub(crate) fn projection_read_budget(
    provider: Provider,
    guard: &TrajectorySourceGuard,
    updated_at: Option<DateTime<Utc>>,
    revision: SourceRevision,
    default_budget: u64,
) -> u64 {
    let TrajectorySourceGuard::Observed(Some(prior)) = guard else {
        return default_budget;
    };
    if provider == Provider::Grok
        && (prior.source_complete || prior.provider_limited)
        && revision.contents.is_some()
        && revision.matches_file_contents(prior.source_revision)
        && prior.document_version == search_document_version(provider)
        && updated_at.is_some_and(|time| source_time_changed(provider, time, prior))
    {
        // The store can update the timestamp while retaining the verified search document.
        return 0;
    }
    if provider == Provider::Grok && prior.source_complete {
        let refresh = match (revision.file, prior.source_revision.file) {
            (Some(selected), Some(previous)) => {
                selected == previous
                    && (revision.contents != prior.source_revision.contents
                        || prior.document_version < search_document_version(provider))
            }
            // Older catalogs and initial picker rows can lack the source identity.
            // Rebuild from the selected provider file before replacing a complete document.
            _ => true,
        };
        if refresh {
            return omnis_adapters::GrokAdapter::MAX_INDEX_REFRESH_BYTES;
        }
    }
    if provider == Provider::Codex
        && prior.source_complete
        && (prior.document_version < search_document_version(Provider::Codex)
            || !prior.source_time_precise)
        && updated_at.is_some_and(|timestamp| codex_projection_time_matches(timestamp, prior))
        && revision.contents.is_some()
        && revision.matches_file_contents(prior.source_revision)
    {
        u64::MAX
    } else {
        default_budget
    }
}

fn legacy_projection_source(
    guard: &TrajectorySourceGuard,
    updated_at: Option<DateTime<Utc>>,
    revision: SourceRevision,
) -> Option<SourceRevision> {
    let TrajectorySourceGuard::Observed(Some(prior)) = guard else {
        return None;
    };
    if updated_at.is_some_and(|time| !codex_projection_time_matches(time, prior)) {
        return None;
    }
    if (matches!(prior.document_version, 4 | 5)
        || (prior.document_version == 6 && prior.source_time_precise))
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
    guard: &TrajectorySourceGuard,
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
    let source = legacy_projection_source(guard, Some(time), read.source_revision);
    let hash = match source {
        Some(source) if source == SourceRevision::default() => {
            store.codex_projection_comparison_hash(session, time)?
        }
        Some(source) => store.codex_projection_hash(session, time, source)?,
        None if matches!(guard, TrajectorySourceGuard::Observed(None)) => store
            .unassigned_codex_projection_hash(
                session,
                time,
                read.source_revision,
                search_document_version(Provider::Codex),
            )?,
        None => None,
    };
    let Some(hash) = hash else {
        return Ok(read);
    };
    if source.is_some_and(|revision| revision.file.is_some()) {
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
    state.legacy_tail_receipt.is_some()
        || state.document_version < version
        || (candidate.session.provider == Provider::OhMyPi
            && !state.source_complete
            && state.title_authoritative.is_none())
        || candidate.source_revision.file != state.source_revision.file
        || candidate.source_revision.contents != state.source_revision.contents
        || candidate.native_summary() != state.native_summary
        || (!state.source_time_precise && !matches!(candidate.session.provider, Provider::Imported | Provider::Grok))
        || candidate.source_time().is_some_and(|updated_at| {
            source_time_changed(candidate.session.provider, updated_at, state)
        })
        // Shared database size cannot show whether a per-session preview still needs its budget.
        || (!state.source_complete
            && !state.provider_limited
            && candidate.can_upgrade_preview())
}

fn verify_grok_owners(
    store: &Store,
    session: &SessionRef,
    path: Option<&Path>,
    revision: SourceRevision,
    log: Option<SourceRevision>,
    updated_at: Option<DateTime<Utc>>,
) -> Result<()> {
    if let (Some(path), Some(log)) = (path, log)
        && store.grok_updates_need_verification(session, Some(log))?
    {
        omnis_adapters::GrokAdapter::verify_update_owners(
            session, path, log, revision, updated_at,
        )?;
        store.record_verified_grok_updates(session, log)?;
    }
    Ok(())
}

pub(crate) fn check_grok_preview(
    store: &Store,
    read: &IndexRead,
    failure: Option<&TrajectoryIndexFailure>,
) -> Result<()> {
    if read.snapshot.session.provider != Provider::Grok {
        return Ok(());
    }
    if let Some(native) = &read.native_session {
        verify_grok_owners(
            store,
            &native.session,
            native.source_path.as_deref(),
            read.source_revision,
            read.ownership_revision,
            native.updated_at,
        )?;
    }
    if !read.source_complete
        && failure.is_some_and(|failed| failed.matches_file_revision(read.source_revision))
    {
        anyhow::bail!("the selected Grok source requires a complete read after its rejection");
    }
    Ok(())
}

/// Hides text from a rejected Grok source while its on-disk and database selections still match.
pub(crate) fn record_grok_rejection(
    registry: &AdapterRegistry,
    store: &Store,
    session: &SessionRef,
    observed: &SelectedSessionObservation,
    error: &anyhow::Error,
) -> Result<()> {
    let Some(rejected) = error.downcast_ref::<omnis_adapters::GrokReadRejection>() else {
        return Ok(());
    };
    if retryable_read_failure(&rejected.cause) {
        return Ok(());
    }
    let mut observation = observed.clone();
    for _ in 0..3 {
        let Some(failure) = selected_grok_failure(registry, session, rejected) else {
            return Ok(());
        };
        observation.ownership_revision = rejected.updates;
        if observation.failure.as_ref() == Some(&failure)
            && (failure.source_contents.is_some() || observation.metadata.is_none())
            && (rejected.updates.is_none()
                || store.grok_updates_need_verification(session, rejected.updates)?)
        {
            return Ok(());
        }
        match store.record_selected_trajectory_index_failure(session, &failure, &observation) {
            Err(omnis_store::StoreError::TrajectorySourceChanged) => {
                observation = store.observe_selected_session(session)?;
            }
            result => return Ok(result?),
        }
    }
    Err(omnis_store::StoreError::TrajectorySourceChanged.into())
}

fn selected_grok_failure(
    registry: &AdapterRegistry,
    session: &SessionRef,
    rejected: &omnis_adapters::GrokReadRejection,
) -> Option<TrajectoryIndexFailure> {
    let (revision, updated_at) = match registry.find_native_session(Provider::Grok, &session.id) {
        Ok(Some(row)) => {
            let selected = row.source_revision?;
            if let Some(log) = rejected.updates {
                if row
                    .source_path
                    .as_deref()
                    .and_then(omnis_adapters::GrokAdapter::updates_revision)
                    != Some(log)
                {
                    return None;
                }
            } else if selected != rejected.revision || row.updated_at != rejected.updated_at {
                return None;
            }
            (selected, row.updated_at)
        }
        Err(error)
            if rejected.revision.contents.is_none()
                && error
                    .downcast_ref::<omnis_adapters::GrokReadRejection>()
                    .is_some_and(|current| current.revision == rejected.revision) =>
        {
            (rejected.revision, rejected.updated_at)
        }
        _ => return None,
    };
    Some(TrajectoryIndexFailure {
        native_summary: false,
        source_updated_at: updated_at,
        source_file: revision.file,
        source_contents: revision.contents,
        document_version: search_document_version(Provider::Grok),
        source_time_precise: true,
        ancestor_revisions: Vec::new(),
    })
}

pub(super) fn reject_cached_grok_preflight(
    registry: &AdapterRegistry,
    session: &SessionRef,
    error: &anyhow::Error,
) -> Result<()> {
    if error.is::<omnis_adapters::GrokReadRejection>()
        && let Some(store) = Store::open_existing_default()?
    {
        let observed = store.observe_selected_session(session)?;
        record_grok_rejection(registry, &store, session, &observed, error)?;
    }
    Ok(())
}

fn hide_unavailable_grok_sources(registry: &AdapterRegistry, store: &Store) -> Result<()> {
    let Ok(adapter) = registry.adapter(Provider::Grok) else {
        return Ok(());
    };
    for id in adapter.unavailable_session_ids() {
        let session = SessionRef::new(Provider::Grok, &id);
        let observed = store.observe_selected_session(&session)?;
        if (observed.state.is_some() || observed.metadata.is_some())
            && let Err(error) = adapter.find_native_session(&id)
        {
            record_grok_rejection(registry, store, &session, &observed, &error)?;
        }
    }
    Ok(())
}

/// Whether a recorded failure still covers the candidate, so reading it again would repeat it.
///
/// Retry when the selected source, its recorded time, or the document version changes.
fn failure_is_current(candidate: &IndexCandidate, failure: &TrajectoryIndexFailure) -> bool {
    if failure.document_version < candidate.document_version
        || candidate.source_revision.file != failure.source_file
        || candidate.source_revision.contents != failure.source_contents
        || candidate.native_summary() != failure.native_summary
    {
        return false;
    }
    if TrajectorySourceGuard::uses_catalog_home(candidate.session.provider)
        && failure.source_file.is_some()
    {
        return true;
    }
    (failure.source_time_precise
        || failure.source_updated_at.is_none()
        || matches!(
            candidate.session.provider,
            Provider::Imported | Provider::Grok
        ))
        && candidate.source_time().is_none_or(|updated_at| {
            failure.source_updated_at.is_some_and(|failed_source| {
                if matches!(
                    candidate.session.provider,
                    Provider::Imported | Provider::Grok
                ) {
                    updated_at.timestamp_millis() == failed_source.timestamp_millis()
                } else {
                    updated_at == failed_source
                }
            })
        })
}

/// Whether a later read can succeed even if the provider's session time stays the same.
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
            || cause.is::<omnis_adapters::UnverifiedSqliteWal>()
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
    guard: &TrajectorySourceGuard,
    prior: Option<&TrajectoryIndexState>,
    failure: Option<&TrajectoryIndexFailure>,
) -> ReadBudget {
    // A rejected duplicate may disappear before a retry selects the original file.
    let retry_full = TrajectorySourceGuard::uses_verified_catalog(candidate.session.provider)
        && failure.is_some()
        && candidate.source_revision.contents.is_some();
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
                    || (TrajectorySourceGuard::uses_catalog_home(candidate.session.provider)
                        && state.source_revision.file.is_none())
                    || (candidate.session.provider != Provider::Imported
                        && !state.source_time_precise))
        });
    ReadBudget {
        bytes: if full {
            u64::MAX
        } else {
            FULL_READ_SOURCE_BYTES
        },
        // The guard identifies the stored document, including a write after the pending scan.
        prior_document: match *guard {
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
    titles: Vec<IndexedTrajectoryTitle>,
    summary: IndexSummary,
}

pub(crate) fn imported_document_versions(
    store: &Store,
    ids: &[&str],
) -> Result<HashMap<String, u32>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    Ok(store
        .bundle_source_providers_for_ids(ids)?
        .into_iter()
        .map(|(id, provider)| (id, imported_search_document_version(provider)))
        .collect())
}

fn select_pi_candidates<'a>(
    store: &Store,
    candidates: impl IntoIterator<Item = &'a IndexCandidate>,
) -> omnis_store::Result<()> {
    let copies = candidates
        .into_iter()
        .filter(|c| matches!(c.session.provider, Provider::Pi | Provider::OhMyPi))
        .filter_map(|c| {
            let metadata = c.metadata.as_ref()?;
            let workspace = metadata.project_path.clone()?;
            let revision = c
                .source_path
                .as_deref()
                .filter(|p| {
                    omnis_adapters::source_file_revision(p)
                        .is_some_and(|current| current.matches_file_contents(c.source_revision))
                })
                .and_then(|p| omnis_adapters::TranscriptRevision::read(p).ok())
                .map(|r| r.key);
            Some((
                workspace,
                IndexedSession {
                    source_path: c.source_path.clone(),
                    ..metadata.clone()
                },
                revision,
            ))
        })
        .collect::<Vec<_>>();
    store.select_pi_workspaces(&copies)
}

fn listed_codex_names<'a>(
    registry: &AdapterRegistry,
    candidates: impl Iterator<Item = &'a IndexCandidate>,
    recheck: bool,
) -> Result<HashMap<String, NativeSessionName>> {
    let selected: Vec<_> = candidates
        .filter(|candidate| candidate.session.provider == Provider::Codex)
        .filter_map(|candidate| {
            Some((
                candidate.session.id.as_str(),
                candidate.source_path.as_deref()?,
                candidate.source_revision,
            ))
        })
        .collect();
    if selected.is_empty() {
        return Ok(HashMap::new());
    }
    let adapter = registry.adapter(Provider::Codex)?;
    Ok(if recheck {
        adapter.listed_session_names_still_current(&selected)
    } else {
        adapter.listed_session_names(&selected)
    })
}

fn bind_listed_codex_names(
    registry: &AdapterRegistry,
    candidates: &mut [IndexCandidate],
) -> Result<()> {
    let mut names = listed_codex_names(registry, candidates.iter(), false)?;
    let mut seen = HashSet::new();
    for candidate in candidates
        .iter()
        .filter(|row| row.session.provider == Provider::Codex)
    {
        if !seen.insert(&candidate.session.id) {
            names.remove(&candidate.session.id);
        }
    }
    for candidate in candidates
        .iter_mut()
        .filter(|row| row.session.provider == Provider::Codex)
    {
        candidate.listed_name = names.remove(&candidate.session.id);
    }
    Ok(())
}

fn verify_cached_codex_name(
    registry: &AdapterRegistry,
    store: &Store,
    candidate: &IndexCandidate,
) -> Result<Option<IndexedTrajectoryTitle>> {
    if let (Some(row), Some(path)) = (&candidate.metadata, candidate.source_path.as_deref()) {
        let needs_observation = if let Some(name) = row.title.as_deref() {
            !store.has_codex_name_source(&candidate.session, candidate.source_revision, name)?
        } else {
            store
                .codex_name_prefix_length(&candidate.session, candidate.source_revision)?
                .is_some()
        };
        if needs_observation
            && !store.is_gui_codex_name(&candidate.session, candidate.source_revision)?
        {
            let name = candidate.observed_name(registry)?;
            verify_codex_name_source(
                store,
                &candidate.session,
                path,
                candidate.source_revision,
                name.as_ref(),
            )?;
            return Ok(store.indexed_trajectory_title(&candidate.session)?);
        }
    }
    Ok(None)
}

fn conflicting_catalog_rows(
    store: &Store,
    candidates: &[IndexCandidate],
) -> Result<HashSet<SessionRef>> {
    let providers: HashSet<_> = candidates
        .iter()
        .filter(|candidate| {
            candidate.session.provider == Provider::Codex
                || ((candidate.session.provider == Provider::Grok
                    || TrajectorySourceGuard::uses_guarded_catalog(candidate.session.provider))
                    && candidate.source_revision.file.is_some())
        })
        .map(|candidate| candidate.session.provider)
        .collect();
    let mut conflicts = HashSet::new();
    for provider in providers {
        conflicts.extend(if matches!(provider, Provider::Codex | Provider::Grok) {
            store.conflicting_native_catalog_rows(provider)?
        } else {
            store.conflicting_file_catalog_rows(provider)?
        });
    }
    Ok(conflicts)
}

fn needs_source_recheck(
    candidate: &IndexCandidate,
    prior: Option<&TrajectoryIndexState>,
    failure: Option<&TrajectoryIndexFailure>,
    current_failure: bool,
    catalog_conflict: bool,
) -> bool {
    if TrajectorySourceGuard::uses_catalog_home(candidate.session.provider) {
        // Returning to the verified native source retires a failure for another source.
        return prior.is_some_and(|state| state.document_version == candidate.document_version)
            && failure.is_some_and(|failure| {
                failure.document_version <= candidate.document_version
                    && (failure.native_summary != candidate.native_summary()
                        || failure.source_file != candidate.source_revision.file
                        || failure.source_contents != candidate.source_revision.contents)
            });
    }
    (candidate.session.provider == Provider::Grok
        || TrajectorySourceGuard::uses_guarded_catalog(candidate.session.provider))
        && candidate.source_revision.file.is_some()
        && ((prior.is_none_or(|state| state.document_version <= candidate.document_version)
            && (catalog_conflict || (failure.is_some() && !current_failure)))
            || (current_failure && candidate.source_revision.contents.is_some()))
}

fn is_antigravity_database(candidate: &IndexCandidate) -> bool {
    matches!(
        candidate.session.provider,
        Provider::Antigravity | Provider::AntigravityDesktop
    ) && candidate
        .source_path
        .as_ref()
        .and_then(|path| path.extension())
        .is_some_and(|ext| ext == "db")
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
                candidate.session.provider == provider && is_antigravity_database(candidate)
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

fn indexed_database_catalogs(
    store: &Store,
    candidates: &[IndexCandidate],
) -> Result<HashMap<SessionRef, IndexedSession>> {
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
    Ok(native_catalog)
}

fn current_native_archive(
    registry: &AdapterRegistry,
    store: &Store,
    candidate: &IndexCandidate,
    prior: Option<&TrajectoryIndexState>,
) -> Result<bool> {
    if !TrajectorySourceGuard::uses_catalog_home(candidate.session.provider)
        || prior.is_none_or(|state| state.source_revision.file == candidate.source_revision.file)
    {
        return Ok(false);
    }
    let Some(home) = registry.discovery_cache_home(candidate.session.provider)? else {
        return Ok(false);
    };
    if is_antigravity_database(candidate) {
        let Some(catalog) = &candidate.catalog else {
            return Ok(false);
        };
        Ok(store.current_scoped_database_catalog(catalog, &home, candidate.document_version)?)
    } else {
        Ok(store.current_limited_native_archive(
            &candidate.session,
            &home,
            candidate.source_revision,
            candidate.document_version,
        )?)
    }
}

struct PendingIndexState {
    imported_versions: HashMap<String, u32>,
    prior_conflicts: HashSet<String>,
    states: HashMap<SessionRef, TrajectoryIndexState>,
    failures: HashMap<SessionRef, TrajectoryIndexFailure>,
    database_conflicts: HashSet<SessionRef>,
    conflicts: HashSet<SessionRef>,
    deferred_names: HashMap<SessionRef, SourceRevision>,
    native_catalog: HashMap<SessionRef, IndexedSession>,
}

fn pending_index_state(
    registry: &AdapterRegistry,
    store: &Store,
    candidates: &[IndexCandidate],
) -> Result<PendingIndexState> {
    let imported_ids: Vec<_> = candidates
        .iter()
        .filter(|candidate| candidate.session.provider == Provider::Imported)
        .map(|candidate| candidate.session.id.as_str())
        .collect();
    let imported_versions = imported_document_versions(store, &imported_ids)?;
    let includes_provider = |provider| {
        candidates
            .iter()
            .any(|candidate| candidate.session.provider == provider)
    };
    let prior_conflicts = if includes_provider(Provider::Grok) {
        store.grok_sessions_with_rejected_updates()?
    } else {
        HashSet::new()
    };
    let states = store.trajectory_index_states()?;
    let failures = store.trajectory_index_failures()?;
    let database_conflicts = database_catalogs_needing_read(registry, store, candidates)?;
    let conflicts = conflicting_catalog_rows(store, candidates)?;
    let deferred_names = if includes_provider(Provider::Codex) {
        store.legacy_codex_name_retries()?
    } else {
        HashMap::new()
    };
    let native_catalog = indexed_database_catalogs(store, candidates)?;
    Ok(PendingIndexState {
        imported_versions,
        prior_conflicts,
        states,
        failures,
        database_conflicts,
        conflicts,
        deferred_names,
        native_catalog,
    })
}

fn pending(
    registry: &AdapterRegistry,
    store: &Store,
    mut candidates: Vec<IndexCandidate>,
    retry_failed: bool,
    verify_current_names: bool,
) -> Result<Pending> {
    select_pi_candidates(store, &candidates)?;
    hide_unavailable_grok_sources(registry, store)?;
    let indexed = pending_index_state(registry, store, &candidates)?;
    let mut summary = IndexSummary {
        candidates: candidates.len(),
        ..IndexSummary::default()
    };
    let mut stale = Vec::new();
    let mut recovered = Vec::new();
    let restored = restored_legacy_ancestors(registry, &candidates, &indexed.states);
    let (changed_documents, changed_failures) =
        changed_index_ancestors(registry, store, &candidates, &indexed.failures)?;
    let mut titles = Vec::new();
    if verify_current_names {
        bind_listed_codex_names(registry, &mut candidates)?;
    }
    for mut candidate in candidates {
        if candidate.session.provider == Provider::Imported
            && let Some(&version) = indexed.imported_versions.get(&candidate.session.id)
        {
            candidate.document_version = version;
        }
        let ancestry_changed = candidate.session.provider == Provider::Codex
            && changed_documents.contains(&candidate.session.id);
        let prior = indexed.states.get(&candidate.session);
        let failure = indexed.failures.get(&candidate.session);
        let failure_changed = changed_failures.contains(&candidate.session);
        let ancestor_restored = candidate.session.provider == Provider::Codex
            && prior
                .filter(|state| state.ancestor_stamp.is_none())
                .and_then(|s| s.source_revision.missing_ancestor)
                .is_some_and(|id| restored.contains(&id));
        let retry_name = candidate.should_retry_name(registry, &indexed.deferred_names)?;
        let current_failure = !failure_changed
            && failure.is_some_and(|failure| failure_is_current(&candidate, failure));
        let catalog_conflict = indexed.conflicts.contains(&candidate.session);
        let refresh = needs_index(&candidate, prior)
            || retry_name
            || failure_changed
            || ancestor_restored
            || ancestry_changed;
        let rejected_file = current_failure && candidate.source_revision.contents.is_some();
        let verify_owners = candidate.must_verify_grok_owners(store, &indexed.prior_conflicts)?;
        let can_restore = match candidate.session.provider {
            Provider::Codex => true,
            Provider::Grok => !verify_owners && !rejected_file,
            _ => false,
        };
        if can_restore
            && !refresh
            && let Some(&prior) = prior
        {
            if verify_current_names && candidate.session.provider == Provider::Codex {
                titles.extend(verify_cached_codex_name(registry, store, &candidate)?);
            }
            if catalog_conflict || (failure.is_some() && !current_failure) {
                recovered.push((candidate, prior, failure.cloned()));
            }
            continue;
        }
        let metadata_changed = candidate.catalog_metadata_changed(&indexed.native_catalog);
        if !(refresh
            || verify_owners
            || metadata_changed
            || indexed.database_conflicts.contains(&candidate.session)
            || needs_source_recheck(
                &candidate,
                prior,
                failure,
                current_failure,
                catalog_conflict,
            ))
        {
            continue;
        }
        if failure.is_none()
            && !metadata_changed
            && current_native_archive(registry, store, &candidate, prior)?
        {
            continue;
        }
        if !retry_failed && current_failure {
            summary.failed_skipped += 1;
            if candidate.session.provider == Provider::Codex {
                titles.extend(store.indexed_trajectory_title(&candidate.session)?);
            }
        } else {
            stale.push((candidate, prior.copied()));
        }
    }
    summary.stale = stale.len();
    Ok(Pending {
        stale,
        failures: indexed.failures,
        recovered,
        titles,
        summary,
    })
}

// Older documents record the first missing ancestor without a revision for its inventory.
fn restored_legacy_ancestors(
    registry: &AdapterRegistry,
    candidates: &[IndexCandidate],
    states: &HashMap<SessionRef, TrajectoryIndexState>,
) -> HashSet<uuid::Uuid> {
    let missing: HashSet<_> = candidates
        .iter()
        .filter(|candidate| candidate.session.provider == Provider::Codex)
        .filter_map(|candidate| {
            states
                .get(&candidate.session)
                .filter(|prior| prior.ancestor_stamp.is_none())?
                .source_revision
                .missing_ancestor
        })
        .collect();
    registry
        .get(Provider::Codex)
        .filter(|_| !missing.is_empty())
        .map_or_else(HashSet::new, |adapter| {
            adapter.available_index_sources(&missing)
        })
}

pub(crate) fn changed_codex_ancestors(
    registry: &AdapterRegistry,
    store: &Store,
    candidates: &[IndexCandidate],
) -> Result<HashSet<String>> {
    Ok(changed_index_ancestors(registry, store, candidates, &HashMap::new())?.0)
}

fn changed_index_ancestors(
    registry: &AdapterRegistry,
    store: &Store,
    candidates: &[IndexCandidate],
    failures: &HashMap<SessionRef, TrajectoryIndexFailure>,
) -> Result<(HashSet<String>, HashSet<SessionRef>)> {
    let sessions: HashSet<_> = candidates
        .iter()
        .filter(|candidate| candidate.session.provider == Provider::Codex)
        .map(|candidate| candidate.session.id.as_str())
        .collect();
    if sessions.is_empty() {
        return Ok((HashSet::new(), HashSet::new()));
    }
    let mut stored = store.codex_index_ancestors()?;
    stored.retain(|id, _| sessions.contains(id.as_str()));
    let relevant_failures = failures.iter().filter(|(session, failure)| {
        session.provider == Provider::Codex
            && sessions.contains(session.id.as_str())
            && !failure.ancestor_revisions.is_empty()
    });
    let revisions = current_ancestor_revisions(
        registry,
        stored.values().map(Vec::as_slice).chain(
            relevant_failures
                .clone()
                .map(|(_, failure)| failure.ancestor_revisions.as_slice()),
        ),
    );
    let changed_failures = relevant_failures
        .filter(|(_, failure)| !ancestors_match(&failure.ancestor_revisions, &revisions))
        .map(|(session, _)| session.clone())
        .collect();
    let changed_documents = stored
        .into_iter()
        .filter(|(_, sources)| !ancestors_match(sources, &revisions))
        .map(|(session, _)| session)
        .collect();
    Ok((changed_documents, changed_failures))
}

fn current_ancestor_revisions<'a>(
    registry: &AdapterRegistry,
    documents: impl Iterator<Item = &'a [(uuid::Uuid, uuid::Uuid)]>,
) -> HashMap<uuid::Uuid, uuid::Uuid> {
    let ids = documents
        .flatten()
        .map(|(id, _)| *id)
        .collect::<HashSet<_>>();
    if ids.is_empty() {
        return HashMap::new();
    }
    registry
        .get(Provider::Codex)
        .map(|adapter| adapter.index_ancestor_revisions(&ids))
        .unwrap_or_default()
}

fn ancestors_match(
    sources: &[(uuid::Uuid, uuid::Uuid)],
    observed: &HashMap<uuid::Uuid, uuid::Uuid>,
) -> bool {
    sources
        .iter()
        .all(|(id, stamp)| !stamp.is_nil() && observed.get(id) == Some(stamp))
}

pub(crate) fn confirm_index_ancestors(
    registry: &AdapterRegistry,
    sources: &[(uuid::Uuid, uuid::Uuid)],
) -> Result<()> {
    let revisions = current_ancestor_revisions(registry, std::iter::once(sources));
    anyhow::ensure!(
        ancestors_match(sources, &revisions),
        "Codex ancestor rollout changed during indexing"
    );
    Ok(())
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
    let pending = pending(registry, store, candidates, false, false)?;
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
        if let Some(selected) = candidate.selected_metadata(registry, revision)
            && store
                .restore_indexed_source_with_log(
                    &selected,
                    prior,
                    observed.as_ref(),
                    failure.as_ref(),
                    candidate.updates_revision(),
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
    if !stale.iter().any(|(candidate, _, _)| {
        matches!(candidate.session.provider, Provider::Codex | Provider::Grok)
    }) {
        return Ok(observations);
    }
    let inventory = store.observe_codex_catalog_inventory()?;
    let catalog = inventory
        .observed
        .iter()
        .map(|row| (&row.session, row))
        .collect::<HashMap<_, _>>();
    for (candidate, guard, _) in stale {
        if !matches!(candidate.session.provider, Provider::Codex | Provider::Grok) {
            continue;
        }
        let Some(metadata) = &candidate.metadata else {
            continue;
        };
        if candidate.session.provider == Provider::Grok
            || inventory.legacy_codex_source(&candidate.session).is_some()
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

fn guard_index_reads(
    selections: &mut FileSelections<'_>,
    store: &Store,
    stale: Vec<(IndexCandidate, Option<TrajectoryIndexState>)>,
    failures: &HashMap<SessionRef, TrajectoryIndexFailure>,
) -> Result<Vec<(IndexCandidate, TrajectorySourceGuard, ReadBudget)>> {
    let mut jobs = Vec::with_capacity(stale.len());
    select_pi_candidates(store, stale.iter().map(|(candidate, _)| candidate))?;
    for (candidate, prior) in stale {
        if matches!(candidate.session.provider, Provider::Codex | Provider::Grok) {
            let provider = candidate.session.provider;
            let guard = source_guard(store, &candidate.session, prior.as_ref())?;
            let budget = if provider == Provider::Grok
                && failures
                    .get(&candidate.session)
                    .is_some_and(|failure| failure.matches_file_revision(candidate.source_revision))
            {
                u64::MAX
            } else {
                projection_read_budget(
                    provider,
                    &guard,
                    candidate.updated_at,
                    candidate.source_revision,
                    FULL_READ_SOURCE_BYTES,
                )
            };
            jobs.push((candidate, guard, budget.into()));
            continue;
        }
        let catalog_guard = if TrajectorySourceGuard::uses_catalog_home(candidate.session.provider)
        {
            let guard = background_source_guard(store, &candidate.session, prior.as_ref())?;
            if prior.is_some_and(|state| state.legacy_tail_receipt.is_some())
                && store.trajectory_index_state(&candidate.session)? != prior
            {
                continue;
            }
            Some(guard)
        } else {
            None
        };
        let guard = if TrajectorySourceGuard::uses_verified_catalog(candidate.session.provider)
            && let Some(file) = candidate.source_revision.file
        {
            let (guard, different_file) = store.observe_index_file(&candidate.session, file)?;
            if different_file
                && !selections
                    .selected(&candidate.file_check(&guard))
                    .unwrap_or(false)
            {
                continue;
            }
            guard
        } else if let Some(guard) = catalog_guard {
            if TrajectorySourceGuard::uses_verified_catalog(candidate.session.provider) {
                store.observe_verified_session(&candidate.session)?
            } else {
                guard
            }
        } else if let Some(receipt) = prior.and_then(|state| state.legacy_tail_receipt) {
            // The repaired sample can reveal an earlier timestamp. Replace only the observed preview.
            TrajectorySourceGuard::ObservedReceipt(Some(receipt))
        } else if prior.is_none()
            && TrajectorySourceGuard::uses_guarded_catalog(candidate.session.provider)
        {
            TrajectorySourceGuard::ObservedReceipt(None)
        } else {
            source_guard(store, &candidate.session, prior.as_ref())?
        };
        let budget = read_budget(
            &candidate,
            &guard,
            prior.as_ref(),
            failures.get(&candidate.session),
        );
        let guard = catalog_guard.map_or(guard, |catalog| catalog.attach_verified_file(guard));
        jobs.push((candidate, guard, budget));
    }
    Ok(jobs)
}

fn batch_session_reader<'a>(
    registry: &'a AdapterRegistry,
    stale: &[(IndexCandidate, TrajectorySourceGuard, ReadBudget)],
) -> Option<Box<IndexSessionReader<'a>>> {
    let ids: Vec<_> = stale
        .iter()
        .filter(|(candidate, _, _)| candidate.session.provider == Provider::Codex)
        .map(|(candidate, _, _)| candidate.session.id.as_str())
        .collect();
    if ids.is_empty() {
        return None;
    }
    let listed = stale
        .iter()
        .filter(|(candidate, _, _)| {
            candidate.session.provider == Provider::Codex && candidate.listed_name.is_some()
        })
        .map(|(candidate, _, _)| candidate.session.id.as_str())
        .collect();
    registry
        .get(Provider::Codex)?
        .index_search_batch(&ids, &listed)
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
    let mut counts = HashMap::<SessionRef, usize>::new();
    let mut batches = Vec::<Vec<IndexCandidate>>::new();
    for candidate in candidates {
        let index = if matches!(candidate.session.provider, Provider::Pi | Provider::OhMyPi) {
            let n = counts.entry(candidate.session.clone()).or_default();
            *n += 1;
            *n - 1
        } else {
            0
        };
        if index == batches.len() {
            batches.push(Vec::new());
        }
        batches[index].push(candidate);
    }
    let mut result = IndexSummary::default();
    for batch in batches {
        let indexed =
            index_distinct_candidates(registry, store, batch, retry_failed, stop, report)?;
        result.candidates += indexed.candidates;
        result.indexed += indexed.indexed;
        result.stale += indexed.stale;
        result.failed += indexed.failed;
        result.failed_skipped += indexed.failed_skipped;
        result.stopped |= indexed.stopped;
        if result.stopped {
            break;
        }
    }
    Ok(result)
}

fn index_distinct_candidates(
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
        mut titles,
        mut summary,
    } = pending(registry, store, candidates, retry_failed, true)?;
    titles.extend(restore_cached_sources(registry, store, recovered)?);
    report(IndexProgress {
        titles,
        total: stale.len(),
        ..IndexProgress::default()
    });
    let mut selections = FileSelections::new(registry);
    let stale = guard_index_reads(&mut selections, store, stale, &failures)?;
    summary.failed += summary.stale - stale.len();
    let unassigned_projections = unassigned_codex_projections(store, &stale)?;
    let observations = observe_replacements(store, &stale, &failures)?;
    let mut updates = IndexProgress::default();
    let mut processed = summary.failed;
    let mut last_report = Instant::now();

    let mut batch = IndexBatch::default();
    let reader = batch_session_reader(registry, &stale);
    let prepare_read = |candidate: &IndexCandidate,
                        guard: &TrajectorySourceGuard,
                        budget: ReadBudget,
                        store: Option<&Store>| {
        prepare_session_with_reader(registry, candidate, guard, budget, store, reader.as_deref())
    };
    // Read an oversized full transcript on the calling thread to bound peak memory.
    let prepare =
        |(candidate, guard, budget): &(IndexCandidate, TrajectorySourceGuard, ReadBudget)| {
            can_read_in_worker(candidate, guard, budget.bytes, &unassigned_projections)
                .then(|| prepare_read(candidate, guard, *budget, None))
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
            let result = prepared
                .unwrap_or_else(|| prepare_read(candidate, guard, *budget, Some(store)))
                .and_then(|read| finish_preparation(registry, store, candidate, read));
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
                    if !TrajectorySourceGuard::uses_catalog_home(candidate.session.provider)
                        && (candidate.source_revision.file.is_none()
                            || !TrajectorySourceGuard::uses_guarded_catalog(
                                candidate.session.provider,
                            ))
                        && let Some(failure) = failures.get(&candidate.session)
                    {
                        let _ = store
                            .clear_observed_trajectory_index_failure(&candidate.session, failure);
                    }
                    batch.push(prepared, &mut selections, store, &mut summary, &mut updates);
                }
                Err(error) => {
                    summary.failed += 1;
                    updates.titles.extend(record_index_read_failure(
                        &mut selections,
                        store,
                        candidate,
                        guard,
                        observations.get(&candidate.session),
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

fn can_read_in_worker(
    candidate: &IndexCandidate,
    guard: &TrajectorySourceGuard,
    bytes: u64,
    unassigned_projections: &HashSet<SessionRef>,
) -> bool {
    reads_transcript_file(candidate.session.provider)
        && !unassigned_projections.contains(&candidate.session)
        && (candidate.session.provider != Provider::Codex
            || legacy_projection_source(guard, candidate.updated_at, candidate.source_revision)
                .is_none())
        && (bytes == FULL_READ_SOURCE_BYTES || candidate.can_upgrade_preview())
}

fn unassigned_codex_projections(
    store: &Store,
    stale: &[(IndexCandidate, TrajectorySourceGuard, ReadBudget)],
) -> Result<HashSet<SessionRef>> {
    let mut unassigned_projections = HashSet::new();
    for (candidate, guard, _) in stale {
        if candidate.session.provider == Provider::Codex
            && matches!(guard, TrajectorySourceGuard::Observed(None))
            && let Some(time) = candidate.updated_at
            && store
                .unassigned_codex_projection_hash(
                    &candidate.session,
                    time,
                    candidate.source_revision,
                    candidate.document_version,
                )?
                .is_some()
        {
            unassigned_projections.insert(candidate.session.clone());
        }
    }
    Ok(unassigned_projections)
}

fn finish_preparation(
    registry: &AdapterRegistry,
    store: &Store,
    candidate: &IndexCandidate,
    mut read: PreparedIndex,
) -> Result<PreparedIndex> {
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
            && (batch_kind(previous.candidate.session.provider) != batch_kind(provider)
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
        Ok((written, title)) => {
            summary.indexed += written;
            updates.titles.extend(title);
            updates.record_workspace(prepared);
        }
        Err(error) => {
            summary.failed += 1;
            if let Some(failure) = &observation.failure
                && !matches!(
                    error.downcast_ref::<omnis_store::StoreError>(),
                    Some(
                        omnis_store::StoreError::TrajectorySourceChanged
                            | omnis_store::StoreError::TrajectoryPreviewRejected
                    )
                )
            {
                if observation.state.is_none() {
                    let _ = store.clear_observed_trajectory_index_failure(session, failure);
                } else {
                    let _ = store.retry_observed_trajectory_index_failure(session, failure);
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
) -> Result<(usize, Option<IndexedTrajectoryTitle>)> {
    let revision = SourceRevision {
        missing_ancestor: None,
        ..read.candidate.source_revision
    };
    anyhow::ensure!(
        TrajectorySourceGuard::Observed(observation.state) == read.guard,
        "cached source changed during indexing"
    );
    confirm_index_ancestors(registry, &read.ancestor_revisions)?;
    if read.candidate.session.provider == Provider::Grok {
        return publish_selected_grok_read(registry, store, read, observation, revision);
    }
    let selected = read
        .candidate
        .selected_codex(registry, revision)
        .ok_or(omnis_store::StoreError::TrajectorySourceChanged)?;
    let name_observed = verified_name_coverage(read.native_name.as_ref(), Some(&selected))?;
    anyhow::ensure!(
        native_codex_name(registry, &read.candidate.session)? == read.native_name,
        omnis_store::StoreError::TrajectorySourceChanged
    );
    read.verify_name_source(store)?;
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
        if let Some(name) = observation.confirmed_ordinary_name() {
            metadata.title = Some(name.to_owned());
            retained = omnis_core::compact_search_title(name);
            document.derived_title = retained.as_deref();
            document.title_authoritative = retained.is_some();
            document.title_is_verified = retained.is_some();
            document.local_opener = LocalOpenerEvidence::Unknown;
            document.withdrawn_title = None;
        }
    }
    let written = store.upsert_selected_session_document_with_selected_name(
        &metadata,
        observation,
        document,
        read.workspace.as_ref(),
        read.native_name.as_ref().filter(|_| name_observed),
        history.as_ref().map(|prior| prior.assignment.as_str()),
    )?;
    Ok((
        written,
        store.indexed_trajectory_title(&read.candidate.session)?,
    ))
}

fn publish_selected_grok_read(
    registry: &AdapterRegistry,
    store: &Store,
    read: &PreparedIndex,
    observation: &SelectedSessionObservation,
    revision: SourceRevision,
) -> Result<(usize, Option<IndexedTrajectoryTitle>)> {
    let selected = read
        .candidate
        .selected_metadata(registry, revision)
        .ok_or(omnis_store::StoreError::TrajectorySourceChanged)?;
    verify_grok_owners(
        store,
        &read.candidate.session,
        read.candidate.source_path.as_deref(),
        read.candidate.source_revision,
        read.ownership_revision,
        read.candidate.updated_at,
    )
    .inspect_err(|error| {
        let _ = record_grok_rejection(registry, store, &read.candidate.session, observation, error);
    })?;
    let mut observation = observation.clone();
    observation.ownership_revision = read.ownership_revision;
    store.upsert_selected_session_document_with_workspace(
        &selected,
        &observation,
        read.store_document(),
        read.workspace.as_ref(),
    )?;
    Ok((1, store.indexed_trajectory_title(&read.candidate.session)?))
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum IndexBatchKind {
    Codex,
    SelectedFile,
    ObservedSource,
}

fn batch_kind(provider: Provider) -> IndexBatchKind {
    match provider {
        Provider::Codex => IndexBatchKind::Codex,
        _ if TrajectorySourceGuard::verifies_search_file(provider) => IndexBatchKind::SelectedFile,
        _ => IndexBatchKind::ObservedSource,
    }
}

fn flush_batch(
    selections: &mut FileSelections<'_>,
    store: &Store,
    batch: &mut Vec<PreparedIndex>,
    summary: &mut IndexSummary,
    updates: &mut IndexProgress,
) {
    let Some(first) = batch.first() else { return };
    let kind = batch_kind(first.candidate.session.provider);
    let changed_sources = if kind == IndexBatchKind::Codex {
        let mut changed = verify_codex_batch_names(selections.registry, store, batch);
        let revisions = current_ancestor_revisions(
            selections.registry,
            batch.iter().map(|read| read.ancestor_revisions.as_slice()),
        );
        changed.extend(
            batch
                .iter()
                .filter(|read| !ancestors_match(&read.ancestor_revisions, &revisions))
                .map(|read| read.candidate.session.clone()),
        );
        changed
    } else {
        HashSet::new()
    };
    if changed_sources.is_empty()
        && let Ok(indexed) = store_batch(selections, store, kind, batch)
    {
        summary.indexed += indexed;
        for prepared in batch.drain(..) {
            updates.record_read(selections.registry, store, prepared);
        }
        return;
    }
    for prepared in batch.drain(..) {
        let stored = if changed_sources.contains(&prepared.candidate.session) {
            Err(omnis_store::StoreError::TrajectorySourceChanged)
        } else {
            prepared.store(selections, store)
        };
        let result = match stored {
            Ok(indexed) => Ok((prepared, indexed)),
            Err(omnis_store::StoreError::TrajectorySourceChanged) => {
                reread_after_conflict(selections, store, &prepared)
            }
            Err(error) => Err(error.into()),
        };
        if let Ok((prepared, indexed)) = result {
            summary.indexed += indexed;
            updates.record_read(selections.registry, store, prepared);
        } else {
            summary.failed += 1;
        }
    }
}

fn store_batch(
    selections: &mut FileSelections<'_>,
    store: &Store,
    kind: IndexBatchKind,
    batch: &[PreparedIndex],
) -> omnis_store::Result<usize> {
    if kind == IndexBatchKind::SelectedFile {
        select_pi_candidates(store, batch.iter().map(|prepared| &prepared.candidate))?;
        let checks: Vec<_> = batch
            .iter()
            .map(|read| read.candidate.file_check(&read.guard))
            .collect();
        let documents = batch.iter().map(|read| {
            (
                &read.candidate.session,
                read.store_document(),
                read.guard,
                read.verified_metadata.as_ref(),
            )
        });
        return selections.write_documents_checked(
            store,
            &checks,
            documents,
            |session, revision| {
                !TrajectorySourceGuard::uses_catalog_home(session.provider)
                    || batch
                        .iter()
                        .find(|read| read.candidate.session == *session)
                        .is_some_and(|read| read.verifies(revision))
            },
        );
    }
    let documents = batch
        .iter()
        .map(|read| {
            (
                &read.candidate.session,
                read.store_document(),
                read.guard,
                read.workspace.as_ref(),
            )
        })
        .collect::<Vec<_>>();
    let names = batch
        .iter()
        .filter_map(|read| read.candidate.metadata.as_ref())
        .collect::<Vec<_>>();
    store.upsert_discovered_trajectory_documents_checked(&documents, &names, |session, revision| {
        batch
            .iter()
            .find(|read| read.candidate.session == *session)
            .is_some_and(|read| read.verifies(revision))
    })
}

fn verify_codex_batch_names(
    registry: &AdapterRegistry,
    store: &Store,
    batch: &[PreparedIndex],
) -> HashSet<SessionRef> {
    let current = listed_codex_names(
        registry,
        batch
            .iter()
            .filter(|read| read.candidate.listed_name.is_some())
            .map(|read| &read.candidate),
        true,
    );
    batch
        .iter()
        .filter(|read| {
            let unchanged = read.candidate.listed_name.as_ref().is_none_or(|name| {
                current
                    .as_ref()
                    .is_ok_and(|names| names.get(&read.candidate.session.id) == Some(name))
            });
            !unchanged || read.verify_name_source(store).is_err()
        })
        .map(|read| read.candidate.session.clone())
        .collect()
}

fn record_index_read_failure(
    selections: &mut FileSelections<'_>,
    store: &Store,
    candidate: &IndexCandidate,
    guard: &TrajectorySourceGuard,
    observation: Option<&SelectedSessionObservation>,
    error: &anyhow::Error,
) -> Option<IndexedTrajectoryTitle> {
    if candidate.session.provider == Provider::Grok
        && error.is::<omnis_adapters::GrokReadRejection>()
        && let Some(observation) = observation
    {
        let _ = record_grok_rejection(
            selections.registry,
            store,
            &candidate.session,
            observation,
            error,
        );
        return None;
    }
    if matches!(candidate.session.provider, Provider::Codex | Provider::Grok) {
        return candidate.record_revision_failure(selections.registry, store, guard, error);
    }
    if batch_kind(candidate.session.provider) != IndexBatchKind::SelectedFile {
        let _ = candidate.record_source_failure(selections.registry, store, guard, error);
        return None;
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
        record_strict_read_failure(selections, store, &guard, &error)?;
    }
    Ok(())
}

fn reread_after_conflict(
    selections: &mut FileSelections<'_>,
    store: &Store,
    previous: &PreparedIndex,
) -> Result<(PreparedIndex, usize)> {
    let registry = selections.registry;
    let candidate = &previous.candidate;
    let session = &candidate.session;
    let observation = (session.provider == Provider::Codex)
        .then(|| store.observe_selected_session(session))
        .transpose()?;
    let (prior, guard, budget_guard) = if let Some(selected) = &observation {
        (
            selected.state,
            TrajectorySourceGuard::Observed(selected.state),
            TrajectorySourceGuard::Observed(selected.state),
        )
    } else {
        let prior = store.trajectory_index_state(session)?;
        if batch_kind(session.provider) == IndexBatchKind::SelectedFile {
            let catalog_guard = TrajectorySourceGuard::uses_catalog_home(session.provider)
                .then(|| store.observe_discovered_trajectory_document(session))
                .transpose()?;
            let file_guard = store.observe_verified_session(session)?;
            let guard = catalog_guard.map_or(file_guard, |catalog| {
                catalog.attach_verified_file(file_guard)
            });
            (prior, guard, file_guard)
        } else {
            let guard = background_source_guard(store, session, prior.as_ref())?;
            (prior, guard, guard)
        }
    };
    let current = if batch_kind(session.provider) == IndexBatchKind::SelectedFile
        && let Some(path) = candidate.source_path.as_deref()
    {
        selections.refresh(session.provider)?;
        selections
            .selected_session_at(session, path)?
            .as_ref()
            .and_then(IndexCandidate::from_session)
            .context("selected transcript changed during indexing")?
    } else if observation.is_some() || candidate.source_revision.file.is_some() {
        registry
            .find_native_session(session.provider, &session.id)?
            .as_ref()
            .and_then(IndexCandidate::from_session)
            .context("provider source changed during indexing")?
    } else {
        candidate.clone()
    };
    let budget = if observation.is_some() {
        projection_read_budget(
            session.provider,
            &guard,
            current.updated_at,
            current.source_revision,
            FULL_READ_SOURCE_BYTES,
        )
        .into()
    } else {
        read_budget(
            &current,
            &budget_guard,
            prior.as_ref(),
            store.trajectory_index_failure(session)?.as_ref(),
        )
    };
    let mut read =
        prepare_session(registry, &current, &guard, budget, Some(store)).inspect_err(|error| {
            let _ = record_strict_read_failure(selections, store, &guard, error);
        })?;
    anyhow::ensure!(
        previous.source.native_home() == read.source.native_home(),
        omnis_store::StoreError::TrajectorySourceChanged
    );
    let indexed = if let Some(observation) = observation {
        read.title
            .check_withdrawal(registry, store, session, read.candidate.source_revision)?;
        publish_selected_read(registry, store, &read, &observation)?.0
    } else {
        read.store(selections, store)?
    };
    Ok((read, indexed))
}

impl IndexProgress {
    pub(crate) fn processed(&self) -> usize {
        self.indexed + self.failed + self.unchanged
    }

    fn record_read(&mut self, registry: &AdapterRegistry, store: &Store, read: PreparedIndex) {
        let provider = read.candidate.session.provider;
        if provider == Provider::Codex || TrajectorySourceGuard::uses_catalog_home(provider) {
            self.record_workspace(&read);
            read.report_native_title(store, &mut self.titles);
        }
        if !matches!(provider, Provider::Codex | Provider::AntigravityIde) {
            read.report_title(registry, store, &mut self.source_titles);
        }
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

pub(crate) fn observe_codex_name_source(
    store: &Store,
    native: &NativeSession,
    revision: SourceRevision,
    name: Option<&NativeSessionName>,
) -> Result<()> {
    if native
        .source_revision
        .is_some_and(|source| source.matches_file_contents(revision))
        && let Some(path) = native.source_path.as_deref()
    {
        verify_codex_name_source(store, &native.session, path, revision, name)?;
    }
    Ok(())
}

fn verify_codex_name_source(
    store: &Store,
    session: &SessionRef,
    path: &Path,
    revision: SourceRevision,
    name: Option<&NativeSessionName>,
) -> Result<()> {
    use omnis_adapters::CodexAdapter;
    match name {
        Some(NativeSessionName::Named(name)) => {
            if !store.has_codex_name_source(session, revision, name)? {
                let receipt = CodexAdapter::rollout_name_anchor(path, revision, &session.id)
                    .or_else(|| {
                        (path.metadata().ok()?.len() > FULL_READ_SOURCE_BYTES
                            && CodexAdapter::index_file_revision(path)?
                                .matches_file_contents(revision))
                        .then_some((0, [0; 32]))
                    });
                let Some((bytes, digest)) = receipt else {
                    return Ok(());
                };
                store.record_codex_name_source(session, revision, name, bytes, digest)?;
            }
        }
        Some(NativeSessionName::Unknown) => {
            if let Some(bytes) = store.codex_name_prefix_length(session, revision)? {
                verify_codex_name_prefix(store, session, path, revision, bytes)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn verify_codex_name_prefix(
    store: &Store,
    session: &SessionRef,
    path: &Path,
    revision: SourceRevision,
    bytes: u64,
) -> Result<()> {
    use omnis_adapters::CodexAdapter;
    if let Some(prior) = CodexAdapter::rollout_prefix_hash(path, revision, Some(bytes)) {
        let current = CodexAdapter::rollout_name_anchor(path, revision, &session.id);
        store.verify_codex_name_append(session, revision, prior, current)?;
    }
    Ok(())
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
    let Some(
        name @ (NativeSessionName::Absent
        | NativeSessionName::Gui(None)
        | NativeSessionName::Unknown),
    ) = native_codex_name(registry, session)?
    else {
        return Ok(None);
    };
    let path = selected_path()?;
    if !omnis_adapters::CodexAdapter::verify_legacy_local_opener(
        &path,
        session,
        revision,
        &prior.title,
        prior.requires_tool_catalog,
    )? {
        return Ok(None);
    }
    anyhow::ensure!(
        selected_path()? == path && native_codex_name(registry, session)?.as_ref() == Some(&name),
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
        provider: Provider,
        snapshot: &CanonicalSnapshot,
        full_read: bool,
        document: &SearchDocument,
        rollback_title: &RollbackTitle,
        native_name: Option<&NativeSessionName>,
    ) -> Self {
        if TrajectorySourceGuard::uses_catalog_home(provider) {
            // The native listing supplies the catalog title separately from the conversation.
            return Self {
                current: session_message_search_title(snapshot),
                authoritative: session_message_prefix_is_known(snapshot, full_read),
                ..Self::default()
            };
        }
        if provider != Provider::Codex {
            return Self {
                current: session_search_title(snapshot),
                authoritative: omnis_core::session_search_title_is_authoritative(
                    snapshot, full_read,
                ),
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
            let opener_authoritative =
                native_name.is_some_and(|name| *name != NativeSessionName::Unknown);
            return Self {
                current,
                authoritative: opener_authoritative,
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
            && native_name.is_some_and(|name| matches!(name, NativeSessionName::Unknown));
        Self {
            current: if snapshot.session.provider == Provider::Codex && !authoritative {
                None
            } else {
                session_message_search_title(snapshot)
            },
            authoritative: authoritative && !unresolved_prefix,
            local_opener: if unresolved_prefix {
                LocalOpenerEvidence::VisiblePrefix
            } else if authoritative
                && matches!(
                    native_name,
                    Some(NativeSessionName::Named(_) | NativeSessionName::Gui(_))
                )
            {
                LocalOpenerEvidence::Current
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
    ancestor_revisions: Vec<(uuid::Uuid, uuid::Uuid)>,
    ownership_revision: Option<SourceRevision>,
    workspace: Option<omnis_store::CodexWorkspaceUpdate>,
    guard: TrajectorySourceGuard,
    verified_metadata: Option<IndexedSession>,
    candidate: IndexCandidate,
    document: SearchDocument,
    database_step_proof: Option<Vec<uuid::Uuid>>,
    source: SearchSource,
    source_complete: bool,
    provider_limited: bool,
    title: IndexedTitle,
    native_name: Option<NativeSessionName>,
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
    if TrajectorySourceGuard::uses_catalog_home(session.provider) {
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
            && TrajectorySourceGuard::uses_catalog_home(session.provider));
    known.then_some((session, record))
}

impl PreparedIndex {
    fn verify_name_source(&self, store: &Store) -> Result<()> {
        if let Some(path) = self.candidate.source_path.as_deref() {
            verify_codex_name_source(
                store,
                &self.candidate.session,
                path,
                self.candidate.source_revision,
                self.native_name.as_ref(),
            )?;
        }
        Ok(())
    }

    fn report_native_title(&self, store: &Store, titles: &mut Vec<IndexedTrajectoryTitle>) {
        let session = &self.candidate.session;
        if let Some(source) = &self.source.observation {
            if let Ok(title) = store.trajectory_title_for_native_source(
                session,
                &source.home,
                source.revision.file,
            ) {
                titles.push(IndexedTrajectoryTitle {
                    session: session.clone(),
                    title,
                    source_revision: source.revision,
                    document_version: self.candidate.document_version,
                    verified_at: None,
                    cached_at: None,
                });
            }
        } else if let Ok(Some(title)) = store.indexed_trajectory_title(session) {
            titles.push(title);
        }
    }

    fn verifies(&self, revision: SourceRevision) -> bool {
        if self.candidate.session.provider == Provider::Codex {
            self.candidate
                .source_path
                .as_deref()
                .and_then(omnis_adapters::CodexAdapter::index_file_revision)
                .is_some_and(|current| current.matches_file_contents(revision))
        } else if self.candidate.session.provider == Provider::Grok {
            self.candidate
                .metadata
                .as_ref()
                .and_then(|row| row.source_revision)
                .is_some_and(|selected| selected.matches_file_contents(revision))
                && self.candidate.updates_revision() == self.ownership_revision
        } else {
            self.source.verifies(revision)
        }
    }

    fn store_observed(&self, store: &Store) -> omnis_store::Result<usize> {
        let names = self.candidate.metadata.iter().collect::<Vec<_>>();
        store.upsert_discovered_trajectory_documents_checked(
            &[(
                &self.candidate.session,
                self.store_document(),
                self.guard,
                self.workspace.as_ref(),
            )],
            &names,
            |_, revision| self.verifies(revision),
        )
    }

    fn report_title(
        self,
        registry: &AdapterRegistry,
        store: &Store,
        titles: &mut Vec<(SessionRef, omnis_store::IndexedTitle)>,
    ) {
        if let Some((mut session, record)) =
            stored_title_update(registry, store, &self.candidate.session)
        {
            if matches!(session.provider, Provider::Pi | Provider::OhMyPi)
                && let Some(path) = Store::pi_file_key(
                    Some(self.candidate.source_revision),
                    self.candidate.source_path.as_deref(),
                )
            {
                session.id = crate::pi_copy_selector(&session.id, &path);
            }
            titles.push((session, record));
        }
    }

    fn store(
        &self,
        selections: &mut FileSelections<'_>,
        store: &Store,
    ) -> omnis_store::Result<usize> {
        if batch_kind(self.candidate.session.provider) != IndexBatchKind::SelectedFile {
            return self.store_observed(store);
        }
        select_pi_candidates(store, std::slice::from_ref(&self.candidate))?;
        selections.write_documents_checked(
            store,
            &[self.candidate.file_check(&self.guard)],
            std::iter::once((
                &self.candidate.session,
                self.store_document(),
                self.guard,
                self.verified_metadata.as_ref(),
            )),
            |_, revision| {
                !TrajectorySourceGuard::uses_catalog_home(self.candidate.session.provider)
                    || self.verifies(revision)
            },
        )
    }

    fn store_document(&self) -> TrajectoryDocument<'_> {
        let mut document = TrajectoryDocument {
            ancestor_revisions: &self.ancestor_revisions,
            database_step_proof: self.database_step_proof.as_deref(),
            projection: self.document.projection.as_ref(),
            database_wal_commits: None,
            database_summary_revision: None,
            desktop_projection: self.document.desktop_projection.as_ref(),
            legacy_utf8_text: self.document.legacy_utf8_text.as_ref(),
            redacted_text: &self.document.text,
            source_updated_at: self.source.updated_at,
            source_revision: self.candidate.source_revision,
            source_byte_count: self.document.source_byte_count,
            indexed_byte_count: self.document.indexed_byte_count,
            truncation_strategy: self.document.truncation_strategy.as_str(),
            source_complete: self.source_complete,
            provider_limited: self.provider_limited,
            origin: self.source.origin,
            document_version: self.candidate.document_version,
            derived_title: self.title.current.as_deref(),
            withdrawn_title: self.title.withdrawn.as_deref(),
            title_is_verified: self.title.is_verified(),
            title_authoritative: self.title.authoritative,
            local_opener: self.title.local_opener,
        };
        self.source.fill_wal_receipt(&mut document);
        document
    }
}

pub(crate) fn read_imported(
    store: Option<&Store>,
    session: &SessionRef,
) -> anyhow::Result<IndexRead> {
    let store = store.context("local imported bundle store is unavailable")?;
    let bundle = load_imported_bundle(store, imported_bundle_id(session)?)?;
    Ok(IndexRead {
        ancestor_revisions: Vec::new(),
        ownership_revision: None,
        verified_prefix_end: None,
        database_aliases: std::sync::Arc::default(),
        search_record_groups: Vec::new(),
        native_session: None,
        snapshot: bundle.snapshot,
        rollback_title: RollbackTitle::Unknown,
        workspace_projection: omnis_adapters::CodexWorkspaceProjection::Unchecked,
        source_complete: true,
        provider_limited: false,
        source_revision: SourceRevision::default(),
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
    pub(super) fn fill_wal_receipt<'a>(&'a self, document: &mut TrajectoryDocument<'a>) {
        if document.database_step_proof.is_some()
            && let Some(source) = &self.observation
        {
            document.database_wal_commits = source.database_wal_commits();
            document.database_summary_revision = source.database_summary_revision();
        }
    }

    pub(super) fn database_step_proof(
        &self,
        snapshot: &CanonicalSnapshot,
    ) -> Option<Vec<uuid::Uuid>> {
        let observed = self.observation.as_ref()?;
        if observed.session.session != snapshot.session
            || !observed.session.source_path.as_deref().is_some_and(|path| {
                path.extension().is_some_and(|ext| ext == "db")
                    && !AntigravityAdapter::is_summary_catalog(path)
            })
            || observed.revision.file.is_none()
            || observed.revision.contents.is_none()
        {
            return None;
        }
        AntigravityAdapter::database_step_proof(snapshot)
    }

    fn native_home(&self) -> Option<&std::ffi::OsStr> {
        self.observation
            .as_ref()
            .map(|source| source.home.as_os_str())
    }

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

fn database_recovery_reader(
    store: &Store,
    session: &SessionRef,
    source: &SearchSourceObservation,
) -> Result<Option<Box<dyn ProviderAdapter>>> {
    use omnis_store::DesktopHistoryCount::{Disputed, Known};
    let (Some(file), Some(checkpoint)) = (source.revision.file, source.database_checkpoint())
    else {
        return Ok(None);
    };
    let prior_rows = store.restored_database_rows(
        session,
        &source.home,
        file,
        Some(checkpoint),
        source.database_wal_commits(),
        source.database_summary_revision(),
    )?;
    let count = if source.desktop_summaries_unavailable {
        store.reconcile_desktop_step_count(
            session,
            &source.home,
            file,
            Some(checkpoint),
            |count, contents| source.matches_database_declaration(count, contents),
        )?
    } else {
        None
    };
    let Some(count) = count.or_else(|| prior_rows.as_ref().map(|rows| Known(rows.len()))) else {
        return Ok(None);
    };
    if source.is_ide_desktop_database() {
        let reader = match count {
            Known(n) => AntigravityIdeAdapter::with_cached_step_count(&source.home, file, n),
            Disputed { minimum } => {
                AntigravityIdeAdapter::with_uncertain_step_count(&source.home, file, minimum)
            }
        };
        Ok(Some(Box::new(if let Some(rows) = prior_rows {
            reader.with_previous_wal_rows(rows)
        } else {
            reader
        })))
    } else {
        let reader = match count {
            Known(n) => AntigravityDesktopAdapter::with_cached_step_count(&source.home, file, n),
            Disputed { minimum } => {
                AntigravityDesktopAdapter::with_uncertain_step_count(&source.home, file, minimum)
            }
        };
        Ok(Some(Box::new(if let Some(rows) = prior_rows {
            reader.with_previous_wal_rows(rows)
        } else {
            reader
        })))
    }
}

pub(crate) fn observe_native_read<T>(
    registry: &AdapterRegistry,
    store: Option<&Store>,
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
        let desktop = match (store, observation.as_ref()) {
            (Some(store), Some(source)) => database_recovery_reader(store, session, source)?,
            _ => None,
        };
        let reader = desktop.as_deref().unwrap_or(adapter);
        let (value, time) = read(reader, path)?;
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
            .filter(|_| {
                current
                    && (!matches!(
                        session.provider,
                        Provider::AntigravityDesktop | Provider::AntigravityIde
                    ) || observation.is_some())
            })
            .map(|updated_at| SearchSource {
                updated_at,
                origin,
                observation,
            }),
    ))
}

/// Marks a known gap in a verified native database so the store can retain richer text.
pub(super) fn index_source_coverage(
    session: &SessionRef,
    snapshot: &CanonicalSnapshot,
    source: &SearchSource,
    source_complete: bool,
    guard: &TrajectorySourceGuard,
) -> Option<(bool, TrajectorySourceGuard)> {
    if session.provider == Provider::Imported
        || (!AntigravityAdapter::has_missing_database_steps(snapshot)
            && !AntigravityIdeAdapter::has_unreadable_log_records(snapshot))
    {
        return Some((source_complete, *guard));
    }
    source
        .observation
        .as_ref()
        .map(|_| (false, guard.with_incomplete_native_history(true)))
}

fn read_index_candidate(
    registry: &AdapterRegistry,
    store: Option<&Store>,
    candidate: &IndexCandidate,
    budget: ReadBudget,
    reader: Option<&IndexSessionReader<'_>>,
) -> anyhow::Result<(IndexedSearchDocument, SearchSource)> {
    let session = &candidate.session;
    let path = candidate.source_path.as_deref();
    Ok(match session.provider {
        Provider::Imported => {
            let read = read_imported(store, session)?;
            let source = SearchSource::imported(read.snapshot.captured_at);
            (read.into(), source)
        }
        Provider::Codex | Provider::Grok => {
            let adapter = registry.adapter(session.provider)?;
            let indexed = if let Some(reader) = reader
                .filter(|_| session.provider == Provider::Codex && budget.prior_document.is_none())
            {
                reader(session, path, budget.bytes)?.into()
            } else if session.provider == Provider::Grok || candidate.listed_name.is_some() {
                adapter.index_search_document(session, path, budget.bytes, budget.prior_document)?
            } else {
                adapter.index_session(session, path, budget.bytes)?.into()
            };
            anyhow::ensure!(
                candidate.source_revision.file.is_none()
                    || indexed
                        .read
                        .source_revision
                        .matches_file_contents(candidate.source_revision),
                "selected {} source changed during indexing",
                session.provider
            );
            let source = SearchSource {
                updated_at: indexed.read.snapshot.captured_at,
                origin: SessionTrajectoryOrigin::Native,
                observation: None,
            };
            (indexed, source)
        }
        provider
            if TrajectorySourceGuard::uses_guarded_catalog(provider)
                && !TrajectorySourceGuard::uses_catalog_home(provider)
                && candidate.source_revision.file.is_some() =>
        {
            let indexed = registry.adapter(provider)?.index_search_document(
                session,
                path,
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
            let updated_at = read
                .native_session
                .as_ref()
                .and_then(|native| native.updated_at)
                .context("transcript changed during search read")?;
            (
                indexed,
                SearchSource {
                    updated_at,
                    origin: SessionTrajectoryOrigin::Native,
                    observation: None,
                },
            )
        }
        _ => {
            let (read, source) = observe_native_read(
                registry,
                store,
                session,
                path,
                Some(candidate.source_revision),
                |adapter, path| {
                    adapter.index_for_search(session, path, budget.bytes, budget.prior_document)
                },
            )?;
            (
                read,
                source.context("transcript changed during search read")?,
            )
        }
    })
}

fn verify_candidate_name(
    candidate: &IndexCandidate,
    native_name: Option<&NativeSessionName>,
    native_session: Option<&NativeSession>,
) -> Result<()> {
    if let (Some(name), Some(discovered)) = (&candidate.listed_name, &candidate.metadata) {
        let title = match name {
            NativeSessionName::Named(name) | NativeSessionName::Gui(Some(name)) => {
                Some(omnis_core::redact_secrets(name))
            }
            NativeSessionName::Gui(None)
            | NativeSessionName::Absent
            | NativeSessionName::Unknown => None,
        };
        anyhow::ensure!(
            title == discovered.title,
            omnis_store::StoreError::TrajectorySourceChanged
        );
    } else if verified_name_coverage(native_name, native_session)?
        && let (Some(discovered), Some(native)) = (&candidate.metadata, native_session)
        && discovered.title != native.title.as_deref().map(omnis_core::redact_secrets)
    {
        return Err(omnis_store::StoreError::TrajectorySourceChanged.into());
    }
    Ok(())
}

fn prepare_session(
    registry: &AdapterRegistry,
    candidate: &IndexCandidate,
    guard: &TrajectorySourceGuard,
    budget: ReadBudget,
    store: Option<&Store>,
) -> anyhow::Result<PreparedIndex> {
    prepare_session_with_reader(registry, candidate, guard, budget, store, None)
}

fn prepare_session_with_reader(
    registry: &AdapterRegistry,
    candidate: &IndexCandidate,
    guard: &TrajectorySourceGuard,
    budget: ReadBudget,
    store: Option<&Store>,
    reader: Option<&IndexSessionReader<'_>>,
) -> anyhow::Result<PreparedIndex> {
    let session = &candidate.session;
    let (indexed, source) = read_index_candidate(registry, store, candidate, budget, reader)?;
    let IndexedSearchDocument { read, document } = if session.provider == Provider::Codex
        && let Some(store) = store
        && !indexed.read.source_complete
    {
        verify_legacy_search_read(registry, store, guard, indexed.read)?.into()
    } else {
        indexed
    };
    let IndexRead {
        ancestor_revisions,
        ownership_revision,
        native_session,
        rollback_title,
        workspace_projection,
        snapshot,
        source_complete,
        provider_limited,
        source_revision,
        verified_prefix_end,
        ..
    } = read;

    let source_revision = source
        .observation
        .as_ref()
        .map_or(source_revision, |observed| observed.revision);
    let native_name = candidate.observed_name(registry)?;
    verify_candidate_name(candidate, native_name.as_ref(), native_session.as_ref())?;
    let (source_complete, guard) =
        index_source_coverage(session, &snapshot, &source, source_complete, guard)
            .ok_or(omnis_adapters::IncompleteTranscript)?;
    let mut title = IndexedTitle::from_read(
        session.provider,
        &snapshot,
        source_complete,
        &document,
        &rollback_title,
        native_name.as_ref(),
    );
    if TrajectorySourceGuard::uses_catalog_home(session.provider) {
        title.current = indexed_title(session, &snapshot, verified_prefix_end);
    }
    let mut selected = candidate.clone();
    selected.source_revision = source_revision;
    if session.provider == Provider::Grok {
        selected.metadata = native_session.as_ref().map(crate::indexed_session_metadata);
    }
    if source_revision.file.is_some()
        && session.provider != Provider::Codex
        && native_session.is_some()
    {
        selected.source_path = native_session
            .as_ref()
            .and_then(|native| native.source_path.clone());
    }
    Ok(PreparedIndex {
        ancestor_revisions,
        ownership_revision,
        verified_metadata: source
            .observation
            .as_ref()
            .filter(|observed| {
                observed.session.session == *session
                    && source_revision.file.is_some()
                    && observed.revision == source_revision
                    && TrajectorySourceGuard::verifies_search_file(session.provider)
            })
            .map(|observed| crate::indexed_session_metadata(&observed.session))
            .or_else(|| {
                native_session
                    .as_ref()
                    .filter(|_| {
                        source_revision.file.is_some()
                            && matches!(guard, TrajectorySourceGuard::Verified { .. })
                    })
                    .map(crate::indexed_session_metadata)
            }),
        workspace: native_session.as_ref().and_then(|_| {
            crate::codex_workspace_update(
                &snapshot,
                source_revision,
                source_complete,
                &workspace_projection,
            )
        }),
        guard: guard_observed_home(&guard, source.observation.as_ref()),
        candidate: selected,
        document,
        database_step_proof: source.database_step_proof(&snapshot),
        source,
        source_complete,
        provider_limited: provider_limited
            || (session.provider != Provider::Imported
                && AntigravityAdapter::has_omitted_stored_steps(&snapshot)),
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
    use omnis_ir::CanonicalSnapshot;
    use serde_json::json;

    use super::*;

    fn candidate(
        updated_at: Option<DateTime<Utc>>,
        source_path: Option<PathBuf>,
    ) -> IndexCandidate {
        IndexCandidate {
            listed_name: None,
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

    fn projection_guard(
        time: DateTime<Utc>,
        source_revision: SourceRevision,
        source_time_precise: bool,
        document_version: u32,
    ) -> TrajectorySourceGuard {
        TrajectorySourceGuard::Observed(Some(TrajectoryIndexState {
            source_byte_count: 0,
            legacy_tail_receipt: None,
            ancestor_stamp: None,
            title_authoritative: Some(true),
            source_updated_at: time,
            source_revision,
            source_time_precise,
            native_summary: false,
            source_complete: true,
            provider_limited: false,
            document_version,
        }))
    }

    #[test]
    fn only_matching_file_and_content_ids_allow_an_unbounded_projection_upgrade() {
        let time = DateTime::from_timestamp_millis(1_800_000_000_123).unwrap();
        let revision = SourceRevision {
            file: Some(uuid::Uuid::from_u128(1)),
            contents: Some(uuid::Uuid::from_u128(2)),
            ..SourceRevision::default()
        };
        let guard = |source, precise, version| projection_guard(time, source, precise, version);
        let budget = |stored, selected, updated_at, document_version| {
            projection_read_budget(
                Provider::Codex,
                &guard(stored, true, document_version),
                Some(updated_at),
                selected,
                FULL_READ_SOURCE_BYTES,
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

        let same_millisecond = time + chrono::Duration::microseconds(500);
        let imprecise = guard(revision, false, 4);
        assert_eq!(
            projection_read_budget(
                Provider::Codex,
                &imprecise,
                Some(same_millisecond),
                revision,
                FULL_READ_SOURCE_BYTES
            ),
            u64::MAX
        );
        assert_eq!(
            budget(revision, revision, same_millisecond, 4),
            FULL_READ_SOURCE_BYTES
        );
        for version in 5..=7 {
            assert_eq!(
                budget(revision, revision, same_millisecond, version),
                u64::MAX
            );
            let other = SourceRevision {
                contents: Some(uuid::Uuid::from_u128(3)),
                ..revision
            };
            assert_eq!(
                budget(revision, other, same_millisecond, version),
                FULL_READ_SOURCE_BYTES
            );
        }
        let next_millisecond = time + chrono::Duration::milliseconds(1);
        assert_eq!(
            projection_read_budget(
                Provider::Codex,
                &imprecise,
                Some(next_millisecond),
                revision,
                FULL_READ_SOURCE_BYTES
            ),
            FULL_READ_SOURCE_BYTES
        );

        let legacy_v6 = |precise| guard(SourceRevision::default(), precise, 6);
        assert_eq!(
            legacy_projection_source(&legacy_v6(true), Some(time), revision),
            Some(SourceRevision::default())
        );
        assert!(legacy_projection_source(&legacy_v6(false), Some(time), revision).is_none());
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
            database_step_proof: None,
            database_wal_commits: None,
            database_summary_revision: None,
            desktop_projection: None,
            legacy_utf8_text: None,
            ancestor_revisions: &[],
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
        let pending = pending(&registry, &store, vec![candidate], false, false)?;
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
        let now = DateTime::from_timestamp(1_800_000_000, 750_000).unwrap();
        let mut small = candidate(Some(now), None);
        small.session.provider = Provider::Claude;
        small.document_version = search_document_version(Provider::Claude);
        let current = TrajectoryIndexState {
            source_byte_count: 0,
            legacy_tail_receipt: None,
            ancestor_stamp: None,
            source_revision: SourceRevision::default(),
            title_authoritative: Some(true),
            source_updated_at: now,
            source_time_precise: true,
            provider_limited: false,
            native_summary: false,
            source_complete: true,
            document_version: small.document_version,
        };

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
            &IndexCandidate {
                updated_at: Some(now + chrono::Duration::seconds(1)),
                ..small.clone()
            },
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
            document_version: small.document_version + 1,
            ..stored
        };
        assert!(!needs_index(&small, Some(&future)));
        let legacy = TrajectoryIndexState {
            source_time_precise: false,
            ..stored
        };
        let exact_millisecond = IndexCandidate {
            updated_at: Some(stored.source_updated_at),
            ..small.clone()
        };
        assert!(needs_index(&exact_millisecond, Some(&legacy)));
        assert!(!needs_index(&exact_millisecond, Some(&stored)));
        let mut claude = small;
        claude.session.provider = Provider::Claude;
        claude.document_version = search_document_version(Provider::Claude);
        let legacy = TrajectoryIndexState {
            document_version: claude.document_version,
            ..legacy
        };
        assert_eq!(
            read_budget(
                &claude,
                &TrajectorySourceGuard::NewerSource,
                Some(&legacy),
                None
            )
            .bytes,
            u64::MAX
        );
    }

    #[test]
    fn losing_cursor_timestamps_retries_the_session_once() {
        let recorded = Utc::now();
        let mut undated = candidate(None, None);
        undated.session.provider = Provider::CursorIde;
        undated.document_version = search_document_version(Provider::CursorIde);
        let cached = TrajectoryIndexState {
            source_byte_count: 0,
            legacy_tail_receipt: None,
            ancestor_stamp: None,
            title_authoritative: Some(true),
            source_revision: SourceRevision::default(),
            source_updated_at: recorded,
            source_time_precise: true,
            native_summary: false,
            source_complete: true,
            provider_limited: false,
            document_version: undated.document_version,
        };
        let failure = TrajectoryIndexFailure {
            ancestor_revisions: Vec::new(),
            source_file: None,
            source_contents: None,
            source_updated_at: Some(recorded),
            source_time_precise: true,
            native_summary: false,
            document_version: undated.document_version,
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
            source_byte_count: 0,
            legacy_tail_receipt: None,
            ancestor_stamp: None,
            source_updated_at: now,
            source_time_precise: true,
            native_summary: false,
            source_revision: candidate(Some(now), Some(path.clone())).source_revision,
            source_complete: false,
            provider_limited: false,
            title_authoritative: Some(false),
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
        shared_database.document_version = search_document_version(Provider::Hermes);
        let sampled = TrajectoryIndexState {
            document_version: shared_database.document_version,
            ..sampled
        };
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
            ancestor_revisions: Vec::new(),
            source_file: None,
            source_contents: None,
            source_updated_at: Some(now),
            source_time_precise: true,
            native_summary: false,
            document_version: search_document_version(Provider::Codex),
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
                ..failure.clone()
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
    fn late_grok_ownership_error_cannot_hide_a_repaired_log() {
        let root = tempfile::tempdir().unwrap();
        let id = "11111111-2222-4333-8444-555555555555";
        let directory = root.path().join("sessions/project").join(id);
        fs::create_dir_all(&directory).unwrap();
        let summary = directory.join("summary.json");
        let updates = directory.join("updates.jsonl");
        fs::write(
            &summary,
            json!({"info":{"id":id},"num_messages":1}).to_string(),
        )
        .unwrap();
        let update = |owner: &str| {
            json!({"params":{"sessionId":owner,"update":{
                "sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"healthy"}
            }}})
            .to_string()
        };
        let store = Store::open(root.path().join("store.sqlite3")).unwrap();
        let session = SessionRef::new(Provider::Grok, id);
        let mut registry = AdapterRegistry::new();
        registry.register(omnis_adapters::GrokAdapter::with_root(
            root.path().join("sessions"),
        ));
        let before = store.observe_selected_session(&session).unwrap();
        fs::write(&updates, update("00000000-1111-4222-8333-444444444444")).unwrap();
        let error = registry
            .index_session(&session, Some(&summary), u64::MAX)
            .err()
            .unwrap();

        fs::write(&updates, update(id)).unwrap();
        let row = registry
            .find_native_session(Provider::Grok, id)
            .unwrap()
            .unwrap();
        let candidate = IndexCandidate::from_session(&row).unwrap();
        let prepared = prepare_session(
            &registry,
            &candidate,
            &TrajectorySourceGuard::Observed(None),
            FULL_READ_SOURCE_BYTES.into(),
            Some(&store),
        )
        .unwrap();
        publish_selected_read(&registry, &store, &prepared, &before).unwrap();
        record_grok_rejection(&registry, &store, &session, &before, &error).unwrap();

        assert!(store.trajectory_index_failure(&session).unwrap().is_none());
        assert_eq!(
            store.search_session_trajectories("healthy", 1).unwrap(),
            [session]
        );
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
                    database_step_proof: None,
                    database_wal_commits: None,
                    database_summary_revision: None,
                    desktop_projection: None,
                    legacy_utf8_text: None,
                    ancestor_revisions: &[],
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
        assert_eq!((final_report.processed(), final_report.total), (1, 1));
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
                    .record_failure(&mut FileSelections::new(&registry), &store, &guard)
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
            let first = adapter
                .selected_session_at("same", &root.join("first.jsonl"))?
                .unwrap();
            let failed_copy = IndexCandidate::from_session(&first).unwrap();
            select_pi_candidates(&store, &[failed_copy])?;
            let failure = &store.trajectory_index_failures()?[&original.session];
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
        prepare_session(&registry, &original, &guard, full, Some(&store))?
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

        let failed_copy = selected()?;
        let withdrawn = temp.path().join("withdrawn");
        fs::rename(&duplicate_dir, &withdrawn)?;
        select_pi_candidates(&store, std::slice::from_ref(&original))?;
        let guard = store.observe_verified_session(&original.session)?;
        let delayed = prepare_session(&registry, &original, &guard, full, Some(&store))?;
        fs::rename(&withdrawn, &duplicate_dir)?;
        delayed.store(&mut FileSelections::new(&registry), &store)?;
        select_pi_candidates(&store, &[failed_copy])?;
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
                    prepare_session(&registry, &candidate, &guard, 0.into(), Some(&store))
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
                &store.observe_verified_session(&candidate.session)?,
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
        prepare_session(&registry, &candidate, &guard, 0.into(), Some(&store))?
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
            &observe_source(&store, &session)?,
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
            &guard,
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
        let write_rollout =
            |marker: &str, modified| write_codex_rollout(&home, Some(marker), modified);
        let (registry, original) =
            write_rollout("oldneedle", Duration::new(1_800_000_000, 750_000));
        index_test_candidates(&registry, &store, &original);
        let guard = observe_source(&store, &original[0].session).unwrap();
        let prepare_original = |registry: &AdapterRegistry| {
            prepare_session(
                registry,
                &original[0],
                &guard,
                FULL_READ_SOURCE_BYTES.into(),
                Some(&store),
            )
        };
        let delayed = prepare_original(&registry).unwrap();
        let (registry, restored) =
            write_rollout("backupneedle", Duration::new(1_800_000_000, 250_000));
        let error = prepare_original(&registry)
            .err()
            .expect("discovery now refers to a replaced rollout");
        assert!(retryable_read_failure(&error));
        original[0].record_revision_failure(
            &registry,
            &store,
            &guard,
            &anyhow::anyhow!("delayed read failure"),
        );
        assert!(
            store
                .trajectory_index_failure(&original[0].session)
                .unwrap()
                .is_none()
        );
        let selected = &restored[0];
        let session = &selected.session;
        let restored_time = selected.updated_at.unwrap();
        assert!(
            !store
                .session_trajectory_is_current(session, restored_time)
                .unwrap()
        );
        assert!(
            !store
                .session_trajectory_source_is_current(session, restored_time)
                .unwrap()
        );
        assert_eq!(
            index_test_candidates(&registry, &store, &restored).indexed,
            1
        );
        assert!(matches!(
            delayed.store_observed(&store),
            Err(omnis_store::StoreError::TrajectorySourceChanged)
        ));
        let mut recovered = IndexSummary::default();
        let mut updates = IndexProgress::default();
        flush_batch(
            &mut FileSelections::new(&registry),
            &store,
            &mut vec![delayed],
            &mut recovered,
            &mut updates,
        );
        assert_eq!(recovered.indexed, 0);
        assert_rollout_title(&updates.titles, session, Some("backupneedle"));
        let hits = |term| store.search_session_trajectories(term, 10).unwrap();
        assert!(hits("oldneedle").is_empty());
        assert_eq!(hits("backupneedle"), std::slice::from_ref(session));
        assert_eq!(index_test_candidates(&registry, &store, &restored).stale, 0);
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
        assert_rollout_title(&updates, session, None);
    }

    fn assert_rollout_title(
        updates: &[IndexedTrajectoryTitle],
        session: &SessionRef,
        title: Option<&str>,
    ) {
        let [entry] = updates else {
            panic!("expected one rollout title, got {updates:?}")
        };
        assert_eq!((&entry.session, entry.title.as_deref()), (session, title));
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
                database_step_proof: None,
                ancestor_revisions: Vec::new(),
                ownership_revision: None,
                workspace: None,
                verified_metadata: None,
                provider_limited: false,
                guard: TrajectorySourceGuard::NewerSource,
                candidate: IndexCandidate {
                    catalog: None,
                    session: SessionRef::new(Provider::Codex, id),
                    updated_at: Some(now),
                    source_path: None,
                    source_revision: SourceRevision::default(),
                    listed_name: None,
                    metadata: None,
                    document_version: search_document_version(Provider::Codex),
                },
                document: SearchDocument {
                    desktop_projection: None,
                    legacy_utf8_text: None,
                    projection: None,
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
            &mut FileSelections::new(&AdapterRegistry::new()),
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
            batches: Arc::default(),
        });
        let session = SessionRef::new(Provider::Pi, "archive");
        let now = Utc::now();
        store
            .upsert_trajectory_document(
                &session,
                &TrajectoryDocument {
                    database_step_proof: None,
                    database_wal_commits: None,
                    database_summary_revision: None,
                    desktop_projection: None,
                    legacy_utf8_text: None,
                    ancestor_revisions: &[],
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
            listed_name: None,
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

    /// A store, a registry with a [`ReaderProbe`], and candidates for the selected provider.
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
                        listed_name: None,
                        catalog: None,
                        metadata: None,
                        source_revision: SourceRevision::default(),
                        session: SessionRef::new(provider, format!("synthetic-{index}")),
                        updated_at: Some(DateTime::<Utc>::UNIX_EPOCH),
                        source_path: None,
                        document_version: search_document_version(provider),
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
    fn unchanged_children_reindex_when_their_ancestor_copies_change() -> Result<()> {
        let root = tempfile::tempdir()?;
        let sessions = root.path().join("sessions");
        fs::create_dir_all(sessions.join("unrelated/a/b/c/d/e"))?;
        let parent = uuid::Uuid::from_u128(1);
        let child = uuid::Uuid::from_u128(2);
        let cycle = uuid::Uuid::from_u128(3);
        let file = |id| sessions.join(format!("rollout-2026-01-01T00-00-00-{id}.jsonl"));
        let header = |id, ordinal, base| {
            json!({"type":"session_meta","ordinal":ordinal,"payload":{"id":id,
                "history_mode":"paginated","history_base":base}})
        };
        let message = json!({"type":"response_item","ordinal":2,"payload":{"type":"message",
            "role":"user","content":[{"type":"input_text","text":"inheritedneedle"}]}});
        let inherited = format!(
            "{}\n{}\n{message}\n",
            header(parent, 0, serde_json::Value::Null),
            json!({"ordinal":1,"type":"turn_context","payload":{"cwd":"/synthetic"}})
        );
        let base = |id| json!({"thread_id":id,"end_ordinal_exclusive":3,"end_byte_offset":inherited.len()});
        fs::write(file(child), format!("{}\n", header(child, 3, base(parent))))?;
        fs::write(file(cycle), format!("{}\n", header(cycle, 3, base(cycle))))?;

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
        fs::write(file(parent), &inherited)?;
        assert_eq!(pass()?.indexed, 1);
        assert_eq!(pass()?.stale, 0);
        let matches = store.search_session_trajectory_matches("inheritedneedle", 10)?;
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].session.id, child.to_string());

        let archive = root
            .path()
            .join("archived_sessions")
            .join(file(parent).file_name().unwrap());
        fs::create_dir_all(archive.parent().unwrap())?;
        fs::write(&archive, &inherited)?;
        fs::write(file(parent), inherited.replacen("ordinal", "ordhnal", 1))?;
        let inherited_hit = || -> Result<bool> {
            Ok(store
                .search_session_trajectories("inheritedneedle", 10)?
                .contains(&child_ref))
        };
        assert_eq!(pass()?.indexed, 1);
        assert!(inherited_hit()?);
        assert_eq!(pass()?.stale, 0);
        fs::write(
            &archive,
            inherited.replace("inheritedneedle", "differentneedle"),
        )?;
        assert!(inherited_hit()?);
        assert_eq!(pass()?.indexed, 1);
        assert!(!inherited_hit()?);
        assert_eq!(pass()?.stale, 0);
        fs::remove_file(&archive)?;
        assert_eq!(pass()?.indexed, 1);
        assert!(!inherited_hit()?);
        fs::write(&archive, &inherited)?;
        assert_eq!(pass()?.indexed, 1);
        assert!(inherited_hit()?);
        assert_eq!(pass()?.stale, 0);
        fs::remove_file(&archive)?;
        fs::write(
            file(parent),
            inherited.replace(r#""ordinal":1"#, r#""ordinal":8"#),
        )?;
        assert_eq!(pass()?.failed, 1);
        let unchanged = pass()?;
        assert_eq!((unchanged.stale, unchanged.failed_skipped), (0, 1));
        fs::write(file(parent), &inherited)?;
        let repaired = pass()?;
        assert_eq!((repaired.indexed, repaired.failed_skipped), (1, 0));
        assert!(inherited_hit()?);
        assert_eq!(pass()?.stale, 0);
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

    #[test]
    fn deferred_read_error_does_not_suppress_a_restored_codex_rollout() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let store = Store::open(root.path().join("store.sqlite"))?;
        let (registry, candidates) = write_codex_rollout(
            root.path(),
            Some("replacement prompt"),
            Duration::from_secs(1_800_000_000),
        );
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
