use std::{
    cell::OnceCell,
    cmp::Reverse,
    collections::{BTreeMap, BinaryHeap, HashMap, HashSet},
    env,
    ffi::OsString,
    fs::{self, File},
    io::{self, BufReader, IsTerminal, Read, Seek, Write},
    path::{Path, PathBuf},
    process::{Child, Command, ExitCode, ExitStatus, Stdio},
    thread,
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use chrono::Utc;
use clap::{Args, Parser, Subcommand};
use omnis_adapters::{
    AdapterRegistry, CodexWorkspaceProjection, EnvChange, LaunchPlan, LaunchTarget, NativeListing,
    NativeSession, NativeSessionName, ProviderAdapter, ProviderInstallation,
    installed_opencode_model_with_binary,
};
use omnis_core::{
    build_fidelity_report, build_native_fork_report,
    build_native_materialization_report_for_snapshot, build_official_import_report_for_snapshot,
    build_semantic_handoff_report_for_snapshot, fidelity_report_for_snapshot, redact_json_secrets,
    redact_secrets, render_markdown_export, render_semantic_handoff, safe_terminal_line,
    trajectory_search_document, workspace_paths_match, workspace_remote_fingerprint,
    workspace_root, workspace_state_matches,
};
use omnis_ir::{
    BundleManifest, CanonicalSnapshot, EventKind, FidelityEntry, FidelityReport, FidelityStatus,
    PortableBundle, Provider, ReplayPolicy, SCHEMA_VERSION, Sensitivity, SessionRef,
    SourceRevision, TransferMode,
};
use omnis_store::{
    DiscoveryScope, IndexedSession, IndexedTrajectoryTitle, SessionDiscoveryAttempt,
    SessionDiscoveryCheckpoint, SessionTrajectoryMatch, SessionTrajectoryOrigin, Store, StoreError,
    TrajectorySelection, TrajectorySourceGuard,
    search_query::{SearchQuery, fold_case},
    state_root,
};
use serde_json::{Value, json};
use tempfile::NamedTempFile;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
use uuid::Uuid;
use wait_timeout::ChildExt;

mod antigravity_import;
mod claude_delete;
mod claude_import;
mod clipboard;
mod codex_import;
#[cfg(test)]
mod conversion_matrix_tests;
mod cursor_ide_import;
mod cursor_import;
mod fuzzy;
mod grok_import;
mod handoff;
mod hermes_import;
mod import_choice;
mod interrupt;
mod launch_mode;
mod launch_profile;
#[cfg(any(target_os = "macos", test))]
mod macos_ps;
mod native_path;
mod opencode_import;
mod pi_import;
mod private_store_lock;
mod provider_compatibility;
mod provider_launch;
mod recovery;
mod search_index;
mod self_update;
mod session_picker;
#[cfg(all(test, unix))]
mod test_support;
mod transfer;
mod version_gate;
#[cfg(windows)]
mod windows_console;

use launch_profile::AgentTarget;
use provider_launch::{
    cursor_ide_binary, cursor_ide_cross_import_ready, provider_process, resolved_provider_binary,
    runnable_target_providers,
};
#[cfg(test)]
use transfer::{
    ResolvedResumeRequest, can_resume_without_snapshot, may_attempt_native_import_on,
    requires_materialized_fork, resume_project, rollback_failed, selected_native_workspace,
};
use transfer::{error_after_rollback, fork, may_attempt_native_import, provider_name, resume};

const PROVIDERS: [Provider; 12] = provider_compatibility::PROVIDER_PRIORITY;
const MAX_BUNDLE_SIZE: u64 = 64 * 1024 * 1024;
const MAX_MARKDOWN_SIZE: u64 = 64 * 1024 * 1024;
// Private-store deletion needs verified active-writer detection, which exists on Linux and macOS.
#[cfg(any(target_os = "linux", target_os = "macos"))]
const DELETE_PROVIDERS: [Provider; 9] = [
    Provider::Codex,
    Provider::OpenCode,
    Provider::Grok,
    Provider::Hermes,
    Provider::Claude,
    Provider::Antigravity,
    Provider::Pi,
    Provider::CursorCli,
    Provider::CursorIde,
];
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
const DELETE_PROVIDERS: [Provider; 4] = [
    Provider::Codex,
    Provider::OpenCode,
    Provider::Grok,
    Provider::Hermes,
];

trait IndexedSessionReader {
    fn read_session_indexed(&self, session: &SessionRef) -> Result<CanonicalSnapshot> {
        self.read_selected_session_indexed(session, None)
            .map(|read| read.snapshot)
    }

    fn read_selected_session_indexed(
        &self,
        session: &SessionRef,
        expected: Option<SourceRevision>,
    ) -> Result<omnis_adapters::IndexRead>;
}

fn full_index_read(
    registry: &AdapterRegistry,
    session: &SessionRef,
    expected: Option<SourceRevision>,
) -> Result<omnis_adapters::IndexRead> {
    let read = registry.index_session(session, None, u64::MAX)?;
    if let Some(expected) = expected {
        anyhow::ensure!(
            expected.matches_file_contents(read.source_revision),
            "selected Codex rollout changed before the continuation could read it"
        );
    }
    Ok(read)
}

fn complete_snapshot_read(
    provider: Provider,
    snapshot: CanonicalSnapshot,
) -> omnis_adapters::IndexRead {
    let source_complete = provider == Provider::Imported
        || !omnis_adapters::AntigravityAdapter::has_missing_database_steps(&snapshot);
    omnis_adapters::IndexRead {
        native_session: None,
        snapshot,
        rollback_title: omnis_adapters::RollbackTitle::Unknown,
        workspace_projection: CodexWorkspaceProjection::Unchecked,
        source_complete,
        provider_limited: false,
        source_revision: SourceRevision::default(),
    }
}

impl IndexedSessionReader for AdapterRegistry {
    fn read_selected_session_indexed(
        &self,
        session: &SessionRef,
        expected: Option<SourceRevision>,
    ) -> Result<omnis_adapters::IndexRead> {
        if session.provider == Provider::Imported {
            let store = Store::open_default().context("opening OmniSession state")?;
            let bundle = load_imported_bundle(&store, imported_bundle_id(session)?)?;
            if let Err(error) = refresh_opened_bundle(&store, session, &bundle) {
                eprintln!("warning: local imported source index: {error}");
            }
            return Ok(complete_snapshot_read(session.provider, bundle.snapshot));
        }
        let store = match Store::open_default() {
            Ok(store) => store,
            Err(error) => {
                eprintln!("warning: local trajectory index unavailable: {error}");
                return if session.provider == Provider::Codex {
                    full_index_read(self, session, expected)
                } else {
                    self.read_session(session)
                        .map(|snapshot| complete_snapshot_read(session.provider, snapshot))
                };
            }
        };
        if session.provider == Provider::Codex {
            return read_indexed_codex(self, &store, session, expected);
        }
        let mut retry = true;
        loop {
            let observed = store.trajectory_index_state(session).and_then(|prior| {
                let guard = search_index::source_guard(&store, session, prior.as_ref())?;
                Ok((prior, guard))
            });
            let (snapshot, source) =
                read_native_search_snapshot(self, Some(&store), session, true)?;
            let (prior, guard) = match observed {
                Ok(state) => state,
                Err(error) => {
                    eprintln!("warning: local trajectory index unavailable: {error}");
                    return Ok(complete_snapshot_read(session.provider, snapshot));
                }
            };
            if let Some(source) = source {
                let current = full_document_current(session, &source, prior.as_ref());
                // A database can change while its event time stays the same.
                let compare_document = matches!(guard, TrajectorySourceGuard::ObservedDocument(_))
                    && prior.is_some_and(|state| {
                        state.document_version
                            <= omnis_core::search_document_version(session.provider)
                    });
                if (compare_document || !current)
                    && let Err(error) = store_observed_search_document(
                        &store, session, &snapshot, true, &source, guard, None,
                    )
                {
                    if retry && matches!(error, StoreError::TrajectorySourceChanged) {
                        retry = false;
                        continue;
                    }
                    eprintln!("warning: local trajectory index write: {error}");
                }
            }
            return Ok(complete_snapshot_read(session.provider, snapshot));
        }
    }
}

fn read_indexed_codex(
    registry: &AdapterRegistry,
    store: &Store,
    session: &SessionRef,
    expected: Option<SourceRevision>,
) -> Result<omnis_adapters::IndexRead> {
    let mut retry = true;
    loop {
        let observed = store.observe_selected_session(session);
        let mut read = full_index_read(registry, session, expected).inspect_err(|error| {
            if omnis_adapters::CodexAdapter::is_current_rollout_unavailable(error)
                && let Ok(observation) = &observed
                && let Some(metadata) = &observation.metadata
                && let Err(error) = registry.adapter(Provider::Codex).and_then(|adapter| {
                    remove_confirmed_unavailable(adapter, store, vec![metadata.clone()])
                })
            {
                eprintln!("warning: local trajectory index write: {error}");
            }
        })?;
        let observed = match observed {
            Ok(observed) => observed,
            Err(error) => {
                eprintln!("warning: local trajectory index unavailable: {error}");
                return Ok(read);
            }
        };
        let name = search_index::native_codex_name(registry, session)?;
        let selected = read.native_session.as_ref().map(|native| {
            selected_read_metadata(
                registry,
                native,
                read.source_revision,
                &mut read.snapshot,
                name.as_ref(),
                Some(&observed),
            )
        });
        let target = selected.as_ref().map_or(
            SearchDocumentTarget::Session(session),
            |(selected, history)| {
                SearchDocumentTarget::Selected(selected, &observed, history.as_deref())
            },
        );
        if let Err(error) = store_search_document(
            store,
            target,
            SearchDocumentInput::Read(&read, name.as_ref()),
            Some(registry),
            search_index::source_guard(store, session, observed.state.as_ref())?,
        ) {
            if retry
                && matches!(
                    error.downcast_ref::<StoreError>(),
                    Some(StoreError::TrajectorySourceChanged)
                )
            {
                retry = false;
                continue;
            }
            eprintln!("warning: local trajectory index write: {error}");
        }
        return Ok(read);
    }
}

fn full_document_current(
    session: &SessionRef,
    source: &search_index::SearchSource,
    prior: Option<&omnis_store::TrajectoryIndexState>,
) -> bool {
    prior.is_some_and(|state| {
        state.document_version > omnis_core::search_document_version(session.provider)
            || (state.source_complete
                && state.document_version == omnis_core::search_document_version(session.provider)
                && state.source_revision == source.revision()
                && state.native_summary
                    == (source.origin == SessionTrajectoryOrigin::NativeSummary)
                && !search_index::source_time_changed(session.provider, source.updated_at, state))
    })
}

/// Stores a versioned search document and returns whether the store rewrote it.
///
/// `full_read` marks a snapshot read from the whole source rather than a preview. Omitted events
/// reported by the adapter stay visible through the document's truncation strategy, so a full
/// read of such a source is current and does not need indexing again.
fn store_observed_search_document(
    store: &Store,
    session: &SessionRef,
    snapshot: &CanonicalSnapshot,
    full_read: bool,
    source: &search_index::SearchSource,
    guard: TrajectorySourceGuard,
    locator: Option<&IndexedSession>,
) -> omnis_store::Result<bool> {
    if session.provider != Provider::Imported
        && omnis_adapters::AntigravityAdapter::has_missing_database_steps(snapshot)
    {
        return Ok(false);
    }
    let document = trajectory_search_document(snapshot);
    let title = search_index::IndexedTitle::from_read(
        session.provider,
        snapshot,
        full_read,
        &document,
        &omnis_adapters::RollbackTitle::Unknown,
        None,
    );
    let document = trajectory_document(
        session.provider,
        &document,
        &title,
        source.updated_at,
        source.revision(),
        full_read,
        source.origin,
    );
    if let Some(locator) = locator {
        return store.upsert_imported_session_document(locator, &document);
    }
    if let Some(observation) = &source.observation {
        let metadata = indexed_session_metadata(&observation.session);
        return store
            .upsert_verified_session_document_with_catalog(
                &metadata,
                Some(&observation.home),
                observation.desktop_summaries_unavailable,
                document,
                guard,
                |_, revision| source.verifies(revision),
            )
            .map(|rewritten| rewritten != 0);
    }
    store
        .upsert_observed_trajectory_documents_checked(
            &[(session, document, guard)],
            |_, revision| source.verifies(revision),
        )
        .map(|rewritten| rewritten != 0)
}

fn trajectory_document<'a>(
    provider: Provider,
    document: &'a omnis_core::SearchDocument,
    title: &'a search_index::IndexedTitle,
    source_updated_at: chrono::DateTime<Utc>,
    source_revision: SourceRevision,
    source_complete: bool,
    origin: SessionTrajectoryOrigin,
) -> omnis_store::TrajectoryDocument<'a> {
    omnis_store::TrajectoryDocument {
        redacted_text: &document.text,
        source_updated_at,
        source_byte_count: document.source_byte_count,
        indexed_byte_count: document.indexed_byte_count,
        truncation_strategy: document.truncation_strategy.as_str(),
        source_complete,
        source_revision,
        origin,
        document_version: omnis_core::search_document_version(provider),
        provider_limited: false,
        derived_title: title.current.as_deref(),
        withdrawn_title: title.withdrawn.as_deref(),
        title_is_verified: title.is_verified(),
        title_authoritative: title.authoritative,
        local_opener: title.local_opener,
    }
}

fn selected_read_metadata(
    registry: &AdapterRegistry,
    native: &NativeSession,
    revision: SourceRevision,
    snapshot: &mut CanonicalSnapshot,
    name: Option<&NativeSessionName>,
    observed: Option<&omnis_store::SelectedSessionObservation>,
) -> (IndexedSession, Option<String>) {
    let mut selected = indexed_session_metadata(native);
    selected.project_path = (!snapshot.workspace.current_dir.as_os_str().is_empty())
        .then(|| snapshot.workspace.current_dir.clone());
    selected
        .git_branch
        .clone_from(&snapshot.workspace.git.branch);
    let previous = observed
        .and_then(|obs| obs.metadata.as_ref())
        .filter(|old| {
            revision.file.is_some()
                && revision.contents.is_some()
                && selected
                    .source_revision
                    .is_some_and(|source| source.matches_file_contents(revision))
                && selected.source_revision == old.source_revision
                && selected.project_path == old.project_path
                && !old.updated_at_approximate
                && selected.updated_at.is_some()
                && selected.updated_at == old.updated_at
        });
    let history = observed.and_then(|obs| {
        search_index::corroborated_codex_name(registry, native, revision, &selected, obs, name)
    });
    if name.is_some_and(|name| !name.confirms(native.title.as_deref())) {
        let unindexed_name = match (name, observed) {
            (Some(NativeSessionName::Unknown), Some(obs)) => obs.unindexed_codex_name(&selected),
            _ => None,
        };
        selected.title = previous
            .and_then(|old| old.title.as_deref())
            .or(unindexed_name)
            .or_else(|| history.as_ref().map(|prior| prior.stored))
            .map(str::to_owned);
        snapshot.title.clone_from(&selected.title);
    }
    if let Some(old) = previous {
        selected.event_count = selected.event_count.max(old.event_count);
    }
    (selected, history.map(|prior| prior.assignment))
}

fn codex_workspace_update(
    snapshot: &CanonicalSnapshot,
    revision: SourceRevision,
    source_complete: bool,
    workspace_projection: &CodexWorkspaceProjection,
) -> Option<omnis_store::CodexWorkspaceUpdate> {
    if snapshot.session.provider != Provider::Codex
        || revision.file.is_none()
        || revision.contents.is_none()
    {
        return None;
    }
    let directory = &snapshot.workspace.current_dir;
    let directory = match workspace_projection {
        CodexWorkspaceProjection::Observed(directory) => directory.as_deref(),
        CodexWorkspaceProjection::Unchecked if !source_complete => return None,
        CodexWorkspaceProjection::Unchecked => {
            if snapshot.events.iter().any(|event| {
                matches!(
                    event.source.raw_record_type.as_deref(),
                    Some(
                        "omnisession.codex_malformed_records"
                            | "omnisession.record_size_limit"
                            | "omnisession.codex_workspace_unknown"
                    )
                ) && event.payload["workspace_unknown"] == true
            }) {
                return Some(omnis_store::CodexWorkspaceUpdate::Withdrawn(revision));
            }
            (!directory.as_os_str().is_empty()).then_some(directory.as_path())
        }
    };
    let directory = directory.filter(|path| path.is_absolute());
    let git_branch = directory
        .filter(|path| path.as_os_str() == snapshot.workspace.current_dir.as_os_str())
        .and(snapshot.workspace.git.branch.clone());
    Some(omnis_store::CodexWorkspaceUpdate::Verified(
        omnis_store::CodexWorkspace {
            source_revision: revision,
            git_branch,
            project_path: directory.map(Path::to_path_buf),
        },
    ))
}

fn verified_codex_directory(read: &omnis_adapters::IndexRead) -> Option<PathBuf> {
    match codex_workspace_update(
        &read.snapshot,
        read.source_revision,
        read.source_complete,
        &read.workspace_projection,
    )? {
        omnis_store::CodexWorkspaceUpdate::Verified(workspace) => workspace
            .project_path
            .filter(|dir| !omnis_core::may_reach_network(dir)),
        omnis_store::CodexWorkspaceUpdate::Withdrawn(_) => None,
    }
}

fn codex_repository_matches(
    snapshot: &CanonicalSnapshot,
    directory: Option<&Path>,
    project: &Path,
) -> Result<bool> {
    let Some(directory) =
        directory.filter(|dir| dir.is_absolute() && !omnis_core::may_reach_network(dir))
    else {
        return Ok(false);
    };
    if directory.as_os_str() == snapshot.workspace.current_dir.as_os_str() {
        Ok(workspace_state_matches(&snapshot.workspace, project)?)
    } else {
        Ok(workspace_paths_match(directory, project))
    }
}

enum SearchDocumentTarget<'a> {
    Session(&'a SessionRef),
    VisibleNative(&'a NativeSession),
    Selected(
        &'a IndexedSession,
        &'a omnis_store::SelectedSessionObservation,
        Option<&'a str>,
    ),
}

impl<'a> From<&'a SessionRef> for SearchDocumentTarget<'a> {
    fn from(session: &'a SessionRef) -> Self {
        Self::Session(session)
    }
}

#[derive(Clone, Copy)]
enum SearchDocumentInput<'a> {
    Read(&'a omnis_adapters::IndexRead, Option<&'a NativeSessionName>),
    Bundle(&'a CanonicalSnapshot),
}

/// Stores the observed document and source revision, then returns its indexed title.
fn store_search_document<'a>(
    store: &Store,
    target: impl Into<SearchDocumentTarget<'a>>,
    input: SearchDocumentInput<'_>,
    native_registry: Option<&AdapterRegistry>,
    guard: TrajectorySourceGuard,
) -> Result<Option<IndexedTrajectoryTitle>> {
    let (snapshot, read, native_name, name_observed) = match input {
        SearchDocumentInput::Read(read, name) => {
            let observed =
                search_index::verified_name_coverage(name, read.native_session.as_ref())?;
            (&read.snapshot, Some(read), name, observed)
        }
        SearchDocumentInput::Bundle(snapshot) => (snapshot, None, None, false),
    };
    let source_complete = read.is_none_or(|read| read.source_complete);
    let source_revision = read.map(|read| read.source_revision).unwrap_or_default();
    let workspace_projection = read.map_or(&CodexWorkspaceProjection::Unchecked, |read| {
        &read.workspace_projection
    });
    let rollback_title = read.map_or(&omnis_adapters::RollbackTitle::Unknown, |read| {
        &read.rollback_title
    });
    let document = trajectory_search_document(snapshot);
    let provider = if read.is_some() {
        snapshot.session.provider
    } else {
        Provider::Imported
    };
    let mut title = search_index::IndexedTitle::from_read(
        provider,
        snapshot,
        source_complete,
        &document,
        rollback_title,
        native_name,
    );
    let origin = if let Some(registry) = native_registry {
        title.check_withdrawal(registry, store, &snapshot.session, source_revision)?;
        SessionTrajectoryOrigin::Native
    } else {
        SessionTrajectoryOrigin::ImportedBundle
    };
    let mut document = trajectory_document(
        provider,
        &document,
        &title,
        snapshot.captured_at,
        source_revision,
        source_complete,
        origin,
    );
    document.provider_limited = read.is_some_and(|read| read.provider_limited);
    let session = match target.into() {
        SearchDocumentTarget::Session(session) => {
            store.upsert_observed_trajectory_documents(&[(session, document, guard)])?;
            session
        }
        SearchDocumentTarget::VisibleNative(native) => {
            let workspace = codex_workspace_update(
                snapshot,
                source_revision,
                source_complete,
                workspace_projection,
            );
            store.upsert_observed_trajectory_documents_with_workspaces(&[(
                &native.session,
                document,
                guard,
                workspace.as_ref(),
            )])?;
            &native.session
        }
        SearchDocumentTarget::Selected(selected, observed, prior_assignment) => {
            let workspace = codex_workspace_update(
                snapshot,
                source_revision,
                source_complete,
                workspace_projection,
            );
            store.upsert_selected_session_document_with_name_history(
                selected,
                observed,
                document,
                workspace.as_ref(),
                name_observed,
                prior_assignment,
            )?;
            &selected.session
        }
    };
    Ok(store.indexed_trajectory_title(session)?)
}

fn build_search_index(
    registry: &AdapterRegistry,
    args: &IndexArgs,
    json_output: bool,
) -> Result<()> {
    let store = Store::open_default().context("opening OmniSession state")?;
    let started = std::time::Instant::now();
    let (sessions, notes, _) = discover_sessions(
        registry,
        args.provider,
        None,
        Some(&store),
        DiscoveryMode::Index,
    );
    let current = current_project().ok();
    let candidates = search_index::ordered_candidates(&sessions, current.as_deref());
    let summary = index_with_interrupt(
        registry,
        &store,
        candidates,
        args.retry_failed,
        !json_output,
    )?;
    let seconds = started.elapsed().as_secs_f64();
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "sessions": summary.candidates,
                "stale": summary.stale,
                "indexed": summary.indexed,
                "failed": summary.failed,
                "failed_skipped": summary.failed_skipped,
                "interrupted": summary.stopped,
                "seconds": (seconds * 100.0).round() / 100.0,
                "notes": notes.iter().map(String::as_str).map(safe_terminal_line).collect::<Vec<_>>(),
            }))?
        );
    } else {
        if summary.stopped {
            println!(
                "Interrupted after indexing {} of {} stale sessions ({} discovered, {} unreadable) in {seconds:.1}s. Run `omni index` again to continue where it left off.",
                summary.indexed, summary.stale, summary.candidates, summary.failed
            );
        } else {
            println!(
                "Indexed {} of {} stale sessions ({} discovered, {} unreadable) in {seconds:.1}s.",
                summary.indexed, summary.stale, summary.candidates, summary.failed
            );
        }
        if summary.failed_skipped > 0 {
            println!(
                "Skipped unreadable sessions unchanged since they failed: {}. Run `omni index --retry-failed` to read them again.",
                summary.failed_skipped
            );
        }
        for note in notes {
            eprintln!("note: {}", safe_terminal_line(&note));
        }
    }
    if summary.stopped {
        return Err(interrupt::Interrupted.into());
    }
    Ok(())
}

/// Indexes candidates with in-place progress. The first Ctrl+C stops between sessions, which each
/// commit on their own, so the index stays consistent.
fn index_with_interrupt(
    registry: &AdapterRegistry,
    store: &Store,
    candidates: Vec<search_index::IndexCandidate>,
    retry_failed: bool,
    show_progress: bool,
) -> Result<search_index::IndexSummary> {
    let interrupt = interrupt::InterruptGuard::install();
    let mut progress = search_index::ProgressLine::new(show_progress);
    search_index::index_candidates(
        registry,
        store,
        candidates,
        retry_failed,
        &|| interrupt.requested(),
        &mut |update| progress.update(&update),
    )
}

enum DiscoveryOwnership {
    Wide(SessionDiscoveryAttempt, Option<SessionDiscoveryCheckpoint>),
    Checked(SessionDiscoveryCheckpoint),
    Untracked,
}

struct CatalogCheck<'a> {
    provider: Provider,
    home: Option<PathBuf>,
    store: Option<&'a Store>,
    persist: bool,
    ownership: DiscoveryOwnership,
}

impl<'a> CatalogCheck<'a> {
    fn begin(
        registry: &AdapterRegistry,
        provider: Provider,
        project: Option<&Path>,
        store: Option<&'a Store>,
        persist: bool,
    ) -> Result<Self> {
        let home = registry.discovery_cache_home(provider)?;
        let ownership = match (store, home.as_deref()) {
            (Some(store), Some(home)) => {
                let version = omnis_core::search_document_version(provider);
                if persist && project.is_none() {
                    let attempt =
                        store.mark_session_discovery_attempted(provider, version, home)?;
                    let checkpoint = if provider == Provider::Antigravity {
                        Some(store.session_discovery_checkpoint(provider, version, home, None)?)
                    } else {
                        None
                    };
                    DiscoveryOwnership::Wide(attempt, checkpoint)
                } else {
                    DiscoveryOwnership::Checked(
                        store.session_discovery_checkpoint(provider, version, home, project)?,
                    )
                }
            }
            _ => DiscoveryOwnership::Untracked,
        };
        Ok(Self {
            provider,
            home,
            store,
            persist,
            ownership,
        })
    }

    fn native_project<'p>(&self, project: Option<&'p Path>) -> Option<&'p Path> {
        if self.provider == Provider::AntigravityDesktop && self.store.is_some() {
            None
        } else {
            project
        }
    }

    fn restore_desktop_summaries(&self, store: &Store, listing: &mut NativeListing) {
        if listing.desktop_summaries_unavailable
            && let Some(home) = &self.home
            && let Err(error) = fill_cached_desktop_summaries(store, home, &mut listing.sessions)
        {
            listing
                .notes
                .push(format!("{} cached summaries: {error}", self.provider));
        }
    }

    fn accept(
        &self,
        store: &Store,
        registry: &AdapterRegistry,
        project: Option<&Path>,
        listing: &mut NativeListing,
        finished_at: chrono::DateTime<Utc>,
    ) -> Result<bool> {
        let NativeListing {
            sessions: rows,
            notes,
            desktop_summaries_unavailable: unavailable,
            unverified_desktop_ids,
        } = listing;
        let indexed = if self.persist {
            rows.iter().map(indexed_session_metadata).collect()
        } else {
            Vec::new()
        };
        let file_at = |position: usize| {
            rows[position]
                .source_revision
                .and_then(|revision| revision.file)
        };
        if let DiscoveryOwnership::Checked(checkpoint) = &self.ownership {
            if !self.persist {
                return Ok(store.discovery_checkpoint_is_current(checkpoint)?);
            }
            let scope = if *unavailable {
                DiscoveryScope::DesktopDatabasesWithoutSummaries
            } else {
                DiscoveryScope::Workspace
            };
            return Ok(store.merge_catalog_at_discovery_checkpoint(
                checkpoint,
                &indexed,
                scope.with_source_files(&file_at),
            )?);
        }
        if !self.persist {
            return Ok(true);
        }
        let scope = if project.is_some() {
            DiscoveryScope::Workspace
        } else {
            discovery_scope(
                notes.is_empty() && registry.adapter(self.provider)?.discovery_is_complete(),
                *unavailable,
                unverified_desktop_ids,
            )
        };
        if let DiscoveryOwnership::Wide(attempt, checkpoint) = &self.ownership {
            let scope = scope.with_source_files(&file_at);
            return Ok(if let Some(checkpoint) = checkpoint {
                store.publish_at_discovery_checkpoint(
                    attempt,
                    checkpoint,
                    &indexed,
                    scope,
                    finished_at,
                )?
            } else {
                store.publish_session_discovery(attempt, &indexed, scope, finished_at)?
            });
        }
        if let Err(error) =
            store.cache_discovered_sessions(self.provider, &indexed, scope, self.home.as_deref())
        {
            notes.push(format!("{} session cache: {error}", self.provider));
        }
        Ok(true)
    }

    fn finish(
        mut self,
        registry: &AdapterRegistry,
        project: Option<&Path>,
        mut listing: NativeListing,
        mut finished_at: chrono::DateTime<Utc>,
    ) -> Result<(Vec<NativeSession>, Vec<String>)> {
        let Some(store) = self.store else {
            return Ok((listing.sessions, listing.notes));
        };
        // A fresh native listing retains source paths and revisions. The winner can finish during the first retry.
        for retry in 0..=2 {
            let current_home = registry.discovery_cache_home(self.provider)?;
            if current_home.as_deref().map(Path::as_os_str)
                != self.home.as_deref().map(Path::as_os_str)
            {
                break;
            }
            if self.provider == Provider::AntigravityDesktop
                && let Some(project) = project
            {
                self.restore_desktop_summaries(store, &mut listing);
                listing.sessions.retain(|session| {
                    session
                        .project_path
                        .as_deref()
                        .is_some_and(|path| workspace_paths_match(path, project))
                });
            }
            if self.accept(store, registry, project, &mut listing, finished_at)? {
                if project.is_none() {
                    self.restore_desktop_summaries(store, &mut listing);
                }
                return Ok((listing.sessions, listing.notes));
            }
            let Some(home) = self.home.as_deref() else {
                break;
            };
            let checkpoint = store.session_discovery_checkpoint(
                self.provider,
                omnis_core::search_document_version(self.provider),
                home,
                project,
            )?;
            let same_home = match &self.ownership {
                DiscoveryOwnership::Checked(prior) => checkpoint.can_follow(prior),
                DiscoveryOwnership::Wide(attempt, Some(prior)) if prior.owns(attempt) => {
                    checkpoint.can_follow(prior)
                }
                _ => checkpoint.is_for_selected_home(),
            };
            if retry == 2 || !same_home {
                break;
            }
            self.ownership = match &self.ownership {
                DiscoveryOwnership::Wide(attempt, _) if checkpoint.owns(attempt) => {
                    DiscoveryOwnership::Wide(attempt.clone(), Some(checkpoint))
                }
                _ => DiscoveryOwnership::Checked(checkpoint),
            };
            listing = registry.list_sessions_in_home(
                self.provider,
                self.native_project(project),
                Some(home),
            )?;
            finished_at = Utc::now();
        }
        self.record_failure(registry, Utc::now())?;
        anyhow::bail!("catalog changed during discovery. Retry to read its current sessions.")
    }

    fn record_failure(
        &self,
        registry: &AdapterRegistry,
        finished_at: chrono::DateTime<Utc>,
    ) -> Result<()> {
        if let (Some(store), DiscoveryOwnership::Wide(attempt, _)) = (self.store, &self.ownership)
            && registry
                .discovery_cache_home(self.provider)?
                .as_deref()
                .map(Path::as_os_str)
                == Some(attempt.home().as_os_str())
        {
            store.finish_session_discovery(attempt, finished_at)?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum DiscoveryMode {
    List,
    Index,
    Search { index: bool },
}

impl DiscoveryMode {
    fn list_sessions(
        self,
        registry: &AdapterRegistry,
        provider: Provider,
        project: Option<&Path>,
        home: Option<&Path>,
    ) -> Result<NativeListing> {
        if provider == Provider::Codex
            && matches!(self, Self::Search { index: true })
            && let Some(project) = project
        {
            let (sessions, notes) = registry
                .adapter(provider)?
                .project_search_candidates(project)?;
            Ok(NativeListing {
                sessions,
                notes,
                ..NativeListing::default()
            })
        } else {
            registry.list_sessions_in_home(provider, project, home)
        }
    }

    fn persists_metadata(self, provider: Provider) -> bool {
        match self {
            Self::Index => true,
            Self::Search { index } => {
                index
                    && (provider == Provider::Codex
                        || TrajectorySourceGuard::uses_catalog_home(provider))
            }
            Self::List => false,
        }
    }
}

/// Lists sessions from every selected provider in parallel, plus imported bundle sources.
fn discover_sessions(
    registry: &AdapterRegistry,
    provider: Option<Provider>,
    project: Option<&Path>,
    store: Option<&Store>,
    mode: DiscoveryMode,
) -> (Vec<NativeSession>, Vec<String>, HashMap<Provider, PathBuf>) {
    let include_imported = provider.is_none_or(|provider| provider == Provider::Imported);
    let providers = provider
        .as_ref()
        .map_or(PROVIDERS.as_slice(), std::slice::from_ref);
    let mut sessions = Vec::new();
    let mut homes = HashMap::new();
    let mut warnings = Vec::new();
    let project_search = matches!(mode, DiscoveryMode::Search { index: true });
    let observed_codex = store
        .filter(|_| mode.persists_metadata(Provider::Codex) && providers.contains(&Provider::Codex))
        .and_then(|store| {
            store
                .observe_codex_catalog_inventory()
                .inspect_err(|error| warnings.push(format!("codex session cache: {error}")))
                .ok()
                .map(|inventory| (store, inventory))
        });
    let discovered = thread::scope(|scope| {
        let handles = providers
            .iter()
            .copied()
            .filter(|provider| *provider != Provider::Imported)
            .filter_map(|provider| {
                let persist = provider != Provider::Codex && mode.persists_metadata(provider);
                let check = match CatalogCheck::begin(registry, provider, project, store, persist) {
                    Ok(check) => check,
                    Err(error) => {
                        warnings.push(format!("{provider} session discovery: {error}"));
                        return None;
                    }
                };
                let selected = check.home.clone();
                let native_project = check.native_project(project);
                let handle = scope.spawn(omnis_adapters::inherit_provider_probes(move || {
                    let rows =
                        mode.list_sessions(registry, provider, native_project, selected.as_deref());
                    (rows, Utc::now())
                }));
                Some((check, handle))
            })
            .collect::<Vec<_>>();
        handles
            .into_iter()
            .map(|(check, handle)| {
                (
                    check,
                    handle.join().unwrap_or_else(|_| {
                        (Err(anyhow!("provider discovery panicked")), Utc::now())
                    }),
                )
            })
            .collect::<Vec<_>>()
    });
    for (check, (result, finished_at)) in discovered {
        let provider = check.provider;
        let home = check.home.clone();
        match result {
            Ok(listing) => match check.finish(registry, project, listing, finished_at) {
                Ok((mut found, notes)) => {
                    if provider == Provider::Codex
                        && let Some((store, inventory)) = &observed_codex
                        && let Err(error) = cache_codex_sessions(
                            registry,
                            store,
                            &mut found,
                            inventory,
                            project_search,
                            project,
                        )
                    {
                        warnings.push(format!("{provider} session cache: {error}"));
                    }
                    if let Some(home) = home {
                        homes.insert(provider, home);
                    }
                    sessions.extend(found);
                    warnings.extend(notes);
                }
                Err(error) => warnings.push(format!("{provider}: {error}")),
            },
            Err(error) => {
                warnings.push(format!("{provider}: {error}"));
                if let Err(error) = check.record_failure(registry, finished_at) {
                    warnings.push(format!("{provider} session cache: {error}"));
                }
            }
        }
    }
    if include_imported {
        match indexed_imported_sessions(store, project) {
            Ok((imported, imported_warnings)) => {
                sessions.extend(imported);
                warnings.extend(imported_warnings);
            }
            Err(error) => warnings.push(format!("imported: {error}")),
        }
    }
    (sessions, warnings, homes)
}

fn prior_codex_assignments(
    adapter: &dyn ProviderAdapter,
    sessions: &[NativeSession],
    inventory: &omnis_store::CodexCatalogInventory,
    names_complete: bool,
) -> HashMap<String, String> {
    if names_complete || !inventory.has_legacy_codex_names() {
        return HashMap::new();
    }
    let candidates = sessions
        .iter()
        .filter(|session| {
            session.title.is_none()
                && !session.updated_at_approximate
                && inventory
                    .legacy_codex_source(&session.session)
                    .is_some_and(|old| {
                        session.source_revision.is_some_and(|current| {
                            old.file.is_some()
                                && old.file == current.file
                                && old.contents.is_some()
                                && current.contents.is_some()
                        })
                    })
        })
        .collect::<Vec<_>>();
    if candidates.is_empty() {
        return HashMap::new();
    }
    adapter.prior_session_names(&candidates)
}

fn cache_codex_sessions(
    registry: &AdapterRegistry,
    store: &Store,
    sessions: &mut [NativeSession],
    inventory: &omnis_store::CodexCatalogInventory,
    project_search: bool,
    project: Option<&Path>,
) -> Result<()> {
    let adapter = registry.adapter(Provider::Codex)?;
    let indexed = sessions
        .iter()
        .map(indexed_session_metadata)
        .collect::<Vec<_>>();
    let names_complete = adapter.session_titles_are_complete();
    let prior_names = prior_codex_assignments(adapter, sessions, inventory, names_complete);
    if !project_search {
        let complete = project.is_none() && adapter.discovery_is_complete();
        store.refresh_codex_catalog_with_name_history(
            &indexed,
            complete,
            names_complete,
            inventory,
            &prior_names,
        )?;
        if !complete && project.is_none() {
            remove_unavailable_search_sessions(
                adapter,
                store,
                indexed.iter().map(|row| (row.session.id.as_str(), None)),
                project,
            )?;
        }
        return Ok(());
    }
    let merged = store.merge_codex_search_catalog_with_name_history(
        &indexed,
        names_complete,
        &inventory.observed,
        &prior_names,
    );
    if matches!(
        merged,
        Err(omnis_store::StoreError::TrajectorySourceChanged)
    ) {
        for session in sessions {
            if !adapter.confirms_session_title(&session.session.id, session.title.as_deref())
                && let Ok(Some(current)) = adapter.find_native_session(&session.session.id)
                && let Some(revision) = current.source_revision
                && revision.file.is_some()
                && revision.contents.is_some()
                && session.source_revision == Some(revision)
                && current.updated_at == session.updated_at
            {
                session.title = current.title;
            }
        }
    }
    Ok(merged?)
}

fn remove_unavailable_search_sessions<'a>(
    adapter: &dyn ProviderAdapter,
    store: &Store,
    found: impl Iterator<Item = (&'a str, Option<Uuid>)>,
    project: Option<&Path>,
) -> Result<()> {
    let mut absent = store.indexed_sessions_for_provider(Provider::Codex)?;
    if absent.is_empty() {
        return Ok(());
    }
    let found: HashMap<_, _> = found.collect();
    let mut matches = HashMap::new();
    absent.retain(|row| {
        found.get(row.session.id.as_str()).is_none_or(|file| {
            file.zip(row.source_revision.and_then(|source| source.file))
                .is_some_and(|(selected, prior)| selected != prior)
        }) && cached_project_matches(row.project_path.as_deref(), project, &mut matches)
    });
    remove_confirmed_unavailable(adapter, store, absent)
}

fn remove_confirmed_unavailable(
    adapter: &dyn ProviderAdapter,
    store: &Store,
    mut absent: Vec<IndexedSession>,
) -> Result<()> {
    if !absent.is_empty() {
        let sources = absent
            .iter()
            .map(|row| (row.session.id.clone(), row.source_revision))
            .collect::<Vec<_>>();
        let unavailable = adapter.unavailable_codex_rollouts(&sources)?;
        absent.retain(|row| unavailable.contains(&row.session.id));
        store.remove_unavailable_codex_sessions(&absent)?;
    }
    Ok(())
}

const SEARCH_TEXT_WIDTH: usize = 80;
const SEARCH_TITLE_MIN_WIDTH: usize = 16;
const SEARCH_FOLDER_MAX_WIDTH: usize = 16;
const SEARCH_SNIPPET_CHARACTERS: usize = 320;

fn discovery_scope(
    complete: bool,
    summaries_unavailable: bool,
    unverified_ids: &[String],
) -> DiscoveryScope {
    if !unverified_ids.is_empty() {
        DiscoveryScope::DesktopUnverifiedArchives {
            session_ids: unverified_ids.to_vec(),
            summaries_unavailable,
        }
    } else if summaries_unavailable {
        DiscoveryScope::DesktopDatabasesWithoutSummaries
    } else if complete {
        DiscoveryScope::CompleteProvider
    } else {
        DiscoveryScope::PartialProvider
    }
}

fn fill_cached_desktop_summaries(
    store: &Store,
    home: &Path,
    sessions: &mut [NativeSession],
) -> omnis_store::Result<()> {
    let listed = sessions
        .iter()
        .map(indexed_session_metadata)
        .collect::<Vec<_>>();
    let cached = store.desktop_sessions_with_cached_summaries(home, &listed)?;
    for (session, row) in sessions.iter_mut().zip(cached) {
        session.title = row.title;
        session.project_path = row.project_path;
        session.git_branch = row.git_branch;
        session.created_at = row.created_at;
        session.updated_at = row.updated_at;
        session.updated_at_approximate = row.updated_at_approximate;
        session.event_count = row.event_count;
    }
    Ok(())
}

fn indexed_session_metadata(session: &NativeSession) -> IndexedSession {
    IndexedSession {
        source_revision: session
            .source_revision
            .filter(|_| {
                session.session.provider == Provider::Codex
                    || TrajectorySourceGuard::uses_catalog_home(session.session.provider)
            })
            .map(|revision| SourceRevision {
                missing_ancestor: None,
                ..revision
            }),
        session: session.session.clone(),
        title: session.title.as_deref().map(redact_secrets),
        project_path: session.project_path.clone(),
        git_branch: session.git_branch.clone(),
        created_at: session
            .created_at
            .and_then(|t| chrono::DateTime::from_timestamp_millis(t.timestamp_millis())),
        updated_at: session
            .updated_at
            .and_then(|t| chrono::DateTime::from_timestamp_millis(t.timestamp_millis())),
        updated_at_approximate: session.updated_at_approximate,
        event_count: session.event_count,
    }
}

fn cached_native_sessions(
    snapshot: &omnis_store::TrajectorySearchSnapshot<'_>,
    args: &SearchArgs,
    project: &Path,
) -> Result<(Vec<NativeSession>, Vec<IndexedTrajectoryTitle>)> {
    let (indexed, titles) = snapshot.cached_sessions_with_titles(args.provider)?;
    let sources = titles
        .iter()
        .map(|entry| (&entry.session, entry.source_revision))
        .collect::<HashMap<_, _>>();
    let mut matcher = omnis_core::SessionWorkspaceMatcher::new(project);
    let sessions = indexed
        .into_iter()
        .filter(|session| session.session.provider != Provider::Imported)
        .filter(|session| {
            args.all_projects
                || session
                    .project_path
                    .as_deref()
                    .is_some_and(|path| matcher.matches(session.session.provider, path))
        })
        .map(|session| NativeSession {
            source_revision: session.source_revision.or_else(|| {
                match sources.get(&session.session) {
                    Some(revision) if revision.file.is_some() => Some(*revision),
                    Some(_) => None,
                    None => Some(SourceRevision::default()),
                }
            }),
            session: session.session,
            title: session.title,
            project_path: session.project_path,
            git_branch: session.git_branch,
            created_at: session.created_at,
            updated_at: session.updated_at,
            updated_at_approximate: session.updated_at_approximate,
            event_count: session.event_count,
            source_path: None,
        })
        .collect::<Vec<_>>();
    Ok((sessions, titles))
}

fn cached_project_matches(
    path: Option<&Path>,
    project: Option<&Path>,
    matches: &mut HashMap<OsString, bool>,
) -> bool {
    project.is_none_or(|project| {
        path.is_some_and(|path| {
            if let Some(&found) = matches.get(path.as_os_str()) {
                return found;
            }
            let found = workspace_paths_match(path, project);
            matches.insert(path.as_os_str().to_owned(), found);
            found
        })
    })
}

fn live_search_sessions(
    registry: &AdapterRegistry,
    store: &Store,
    args: &SearchArgs,
    project: &Path,
) -> (Vec<NativeSession>, Vec<String>, HashMap<Provider, PathBuf>) {
    let project = (!args.all_projects).then_some(project);
    let (sessions, mut warnings, homes) = discover_sessions(
        registry,
        args.provider,
        project,
        Some(store),
        DiscoveryMode::Search {
            index: !args.no_index,
        },
    );
    if args
        .provider
        .is_none_or(|provider| provider == Provider::Codex)
    {
        let found = sessions
            .iter()
            .filter(|row| row.session.provider == Provider::Codex)
            .map(|row| {
                // Ordinary indexing publishes the replacement. A metadata-only search must
                // retire a superseded file without reading its replacement's conversation.
                let file = row
                    .source_revision
                    .filter(|_| args.no_index)
                    .and_then(|source| source.file);
                (row.session.id.as_str(), file)
            });
        let removed = registry
            .adapter(Provider::Codex)
            .and_then(|adapter| remove_unavailable_search_sessions(adapter, store, found, project));
        if let Err(error) = removed {
            warnings.push(format!("codex session cache: {error}"));
        }
    }
    (sessions, warnings, homes)
}

/// One ranked result. Conversation-only matches carry their full-text match.
struct SearchHit<'a> {
    session: &'a NativeSession,
    title: Option<String>,
    conversation: Option<SessionTrajectoryMatch>,
}

fn validated_search_query(args: &SearchArgs) -> Result<String> {
    let query = args.query.join(" ");
    if query.trim().is_empty() {
        bail!("search query cannot be empty");
    }
    if SearchQuery::parse(&query).is_empty() {
        bail!("search query needs at least one letter or digit");
    }
    if args.limit == 0 {
        bail!("`--limit` must be at least 1");
    }
    Ok(query)
}

fn search_sessions(registry: &AdapterRegistry, args: &SearchArgs, json_output: bool) -> Result<()> {
    let query = validated_search_query(args)?;
    let project = omnis_core::canonicalize_path(&args.project)
        .with_context(|| format!("resolving project `{}`", args.project.display()))?;
    let store = Store::open_default().context("opening OmniSession state")?;
    let (mut sessions, mut warnings, homes) = if args.cached {
        let (sessions, warnings) = if args.provider.is_none_or(|p| p == Provider::Imported) {
            indexed_imported_sessions(
                Some(&store),
                (!args.all_projects).then_some(project.as_path()),
            )?
        } else {
            (Vec::new(), Vec::new())
        };
        (sessions, warnings, HashMap::new())
    } else {
        live_search_sessions(registry, &store, args, &project)
    };
    let (mut index, stale_known) = if args.cached {
        warnings.push(
            "Cached results only. Provider stores were not checked. Run `omni index` to refresh."
                .to_owned(),
        );
        cached_search_index(&store, &sessions, args, &mut warnings)
    } else {
        order_search_sessions(&mut sessions);
        let candidates = search_index::ordered_candidates(&sessions, Some(&project));
        let index = if args.no_index {
            search_index::pending_summary(registry, &store, candidates)?
        } else {
            index_with_interrupt(registry, &store, candidates, false, !json_output)?
        };
        (index, true)
    };
    if !args.cached {
        refresh_search_workspaces(&mut sessions, (!args.all_projects).then_some(&project))?;
    }
    let (hits, has_more) = store.read_trajectory_search(|snapshot| {
        let titles = if args.cached {
            let (native, titles) = cached_native_sessions(snapshot, args, &project)?;
            sessions.extend(native);
            order_search_sessions(&mut sessions);
            index.candidates = sessions.len();
            titles
        } else {
            snapshot.trajectory_titles_with_sources()?
        };
        let selections = scoped_search_selections(snapshot, &homes, &sessions, args.cached)?;
        rank_search_hits(snapshot, &query, &sessions, &selections, titles, args.limit)
    })?;
    if json_output {
        let value = search_json(
            &query,
            &index,
            args,
            &hits,
            has_more,
            stale_known,
            &warnings,
        );
        return write_search_output(&format!("{}\n", serde_json::to_string_pretty(&value)?));
    }
    write_search_output(&search_text(
        &query,
        &hits,
        has_more,
        args.show_text,
        search_output_width(),
    ))?;
    for warning in warnings {
        eprintln!("warning: {}", safe_terminal_line(&warning));
    }
    if index.stopped {
        eprintln!(
            "note: indexing interrupted; results cover sessions indexed so far. Search again to finish indexing."
        );
    } else if args.no_index && !args.cached && index.stale > 0 {
        eprintln!(
            "note: {} sessions are new or changed since indexing; their conversation text was not searched.",
            index.stale
        );
    }
    Ok(())
}

fn order_search_sessions(sessions: &mut Vec<NativeSession>) {
    sessions.sort_by_key(|session| Reverse(session.updated_at));
    let mut seen = HashSet::new();
    sessions.retain(|session| seen.insert(session.session.clone()));
}

fn scoped_search_selections(
    snapshot: &omnis_store::TrajectorySearchSnapshot<'_>,
    homes: &HashMap<Provider, PathBuf>,
    sessions: &[NativeSession],
    cached: bool,
) -> omnis_store::Result<Vec<TrajectorySelection>> {
    let conflicts = conflicting_search_documents(
        snapshot,
        (!cached).then_some(homes),
        sessions.iter().map(|session| {
            (
                &session.session,
                session.source_revision.and_then(|revision| revision.file),
            )
        }),
    )?;
    let mut selections = search_selections(sessions, cached);
    selections.retain(|entry| !conflicts.contains(&entry.session));
    Ok(selections)
}

fn conflicting_search_documents<'a>(
    snapshot: &omnis_store::TrajectorySearchSnapshot<'_>,
    live_homes: Option<&HashMap<Provider, PathBuf>>,
    candidates: impl Iterator<Item = (&'a SessionRef, Option<Uuid>)>,
) -> omnis_store::Result<HashSet<SessionRef>> {
    let mut conflicts = HashSet::new();
    let mut selected = HashMap::<Provider, Vec<_>>::new();
    for (session, file) in candidates {
        if TrajectorySourceGuard::uses_catalog_home(session.provider)
            && live_homes.is_none_or(|homes| homes.contains_key(&session.provider))
        {
            selected
                .entry(session.provider)
                .or_default()
                .push((session.id.as_str(), file));
        }
    }
    for (provider, sources) in selected {
        if let Some(homes) = live_homes {
            conflicts.extend(snapshot.conflicting_native_documents(
                provider,
                &homes[&provider],
                &sources,
            )?);
        } else {
            let ids = sources.iter().map(|(id, _)| *id).collect::<Vec<_>>();
            conflicts.extend(snapshot.conflicting_cached_documents(provider, &ids)?);
        }
    }
    Ok(conflicts)
}

// Indexing can establish a different surviving workspace and retire the creation branch.
fn refresh_search_workspaces(
    sessions: &mut Vec<NativeSession>,
    project: Option<&Path>,
) -> Result<()> {
    if !sessions
        .iter()
        .any(|session| session.session.provider == Provider::Codex)
    {
        return Ok(());
    }
    let workspaces = omnis_store::CodexWorkspace::discovery_index(None)?;
    let mut matches = HashMap::<OsString, bool>::new();
    sessions.retain_mut(|session| {
        if session.session.provider != Provider::Codex {
            return true;
        }
        session.apply_codex_workspace_index(&workspaces, false);
        project.is_none_or(|project| {
            session.project_path.as_deref().is_some_and(|path| {
                if let Some(&found) = matches.get(path.as_os_str()) {
                    return found;
                }
                let found = workspace_paths_match(path, project);
                matches.insert(path.as_os_str().to_owned(), found);
                found
            })
        })
    });
    Ok(())
}

fn search_selections(sessions: &[NativeSession], cached: bool) -> Vec<TrajectorySelection> {
    sessions
        .iter()
        .map(|session| TrajectorySelection {
            session: session.session.clone(),
            source: if session.session.provider != Provider::Codex {
                None
            } else if cached {
                session.source_revision
            } else {
                Some(session.source_revision.unwrap_or_default())
            },
        })
        .collect()
}

/// Ranks metadata matches by fuzzy score, then conversation-only matches in full-text rank order.
fn rank_search_hits<'a>(
    snapshot: &omnis_store::TrajectorySearchSnapshot<'_>,
    query: &str,
    sessions: &'a [NativeSession],
    selections: &[TrajectorySelection],
    indexed_titles: Vec<IndexedTrajectoryTitle>,
    limit: usize,
) -> Result<(Vec<SearchHit<'a>>, bool)> {
    let sources: HashMap<_, _> = selections
        .iter()
        .map(|entry| (&entry.session, entry.source))
        .collect();
    let titles: HashMap<_, _> = indexed_titles
        .into_iter()
        .filter_map(|entry| {
            let selected = sources.get(&entry.session)?;
            let title = entry.title?;
            selected
                .is_none_or(|revision| revision.matches_file_contents(entry.source_revision))
                .then_some((entry.session, title))
        })
        .collect();
    let terms = fuzzy::query_terms(query);
    let display_title = |session: &NativeSession| {
        session
            .title
            .as_deref()
            .filter(|title| !title.trim().is_empty())
            .or_else(|| titles.get(&session.session).map(String::as_str))
            .map(|title| compact_line(&redact_secrets(title)))
    };
    let mut best = BinaryHeap::with_capacity(limit.min(sessions.len()));
    let mut metadata_count = 0;
    for (position, session) in sessions.iter().enumerate() {
        let Some(relevance) =
            fuzzy::SearchFields::new(session, titles.get(&session.session).map(String::as_str))
                .score(&terms)
        else {
            continue;
        };
        metadata_count += 1;
        // The input position preserves the previous stable ordering of equal-ranked matches.
        let rank = (Reverse(relevance), Reverse(session.updated_at), position);
        if best.len() < limit {
            best.push(rank);
        } else if let Some(mut worst) = best.peek_mut()
            && rank < *worst
        {
            *worst = rank;
        }
    }
    let metadata = best
        .into_sorted_vec()
        .into_iter()
        .map(|(_, _, position)| &sessions[position])
        .collect::<Vec<_>>();
    let mut hits = metadata
        .iter()
        .map(|&session| SearchHit {
            session,
            title: display_title(session),
            conversation: None,
        })
        .collect::<Vec<_>>();
    if metadata_count > limit {
        return Ok((hits, true));
    }
    let matched = metadata
        .iter()
        .map(|session| &session.session)
        .collect::<HashSet<_>>();
    // Each metadata match can also appear among conversation matches, so ask for that many more.
    let page = snapshot
        .search_selected_trajectory_page(query, limit.saturating_add(metadata.len()), selections)
        .context("searching indexed conversations")?;
    let by_reference = sessions
        .iter()
        .map(|session| (&session.session, session))
        .collect::<HashMap<_, _>>();
    hits.extend(
        page.matches
            .into_iter()
            .filter(|conversation| !matched.contains(&conversation.session))
            .filter_map(|conversation| {
                let session = *by_reference.get(&conversation.session)?;
                Some(SearchHit {
                    session,
                    title: display_title(session),
                    conversation: Some(conversation),
                })
            }),
    );
    let has_more = page.has_more || hits.len() > limit;
    hits.truncate(limit);
    Ok((hits, has_more))
}

fn search_json(
    query: &str,
    index: &search_index::IndexSummary,
    args: &SearchArgs,
    hits: &[SearchHit<'_>],
    has_more: bool,
    stale_known: bool,
    warnings: &[String],
) -> Value {
    let results = hits
        .iter()
        .map(|hit| {
            json!({
                "session": hit.session.session.to_string(),
                "provider": hit.session.session.provider,
                // Titles can quote prompts, so titles and transcript text stay out unless requested.
                "title": hit.title.as_ref().filter(|_| args.show_text),
                "project_path": hit.session.project_path,
                "updated_at": hit.session.updated_at,
                "match": if hit.conversation.is_some() { "conversation" } else { "metadata" },
                "snippet": hit.conversation.as_ref().filter(|_| args.show_text).map(|conversation| search_snippet(&conversation.snippet)),
                "coverage": hit.conversation.as_ref().map(search_coverage),
            })
        })
        .collect::<Vec<_>>();
    let mut value = json!({
        "query": query,
        "index": {
            "candidates": index.candidates,
            "stale": stale_known.then_some(index.stale),
            "indexed": index.indexed,
            "failed": index.failed,
            "failed_skipped": index.failed_skipped,
            "skipped": args.no_index || (args.cached && index.stale == 0),
            "interrupted": index.stopped,
        },
        "results": results,
        "has_more": has_more,
        "warnings": warnings.iter().map(String::as_str).map(safe_terminal_line).collect::<Vec<_>>(),
    });
    if args.cached {
        value["cached"] = json!(true);
    }
    value
}

fn search_text(
    query: &str,
    hits: &[SearchHit<'_>],
    has_more: bool,
    show_text: bool,
    width: usize,
) -> String {
    use std::fmt::Write as _;

    if hits.is_empty() {
        return format!("No sessions match “{}”.\n", compact_line(query));
    }
    let references = hits
        .iter()
        .map(|hit| safe_terminal_line(&hit.session.session.to_string()))
        .collect::<Vec<_>>();
    let ages = hits
        .iter()
        .map(|hit| compact_age(hit.session.updated_at))
        .collect::<Vec<_>>();
    let folders = hits
        .iter()
        .map(|hit| fit_width(&project_folder(hit.session), SEARCH_FOLDER_MAX_WIDTH))
        .collect::<Vec<_>>();
    let column = |values: &[String]| {
        values
            .iter()
            .map(|value| UnicodeWidthStr::width(value.as_str()))
            .max()
            .unwrap_or(0)
    };
    let (reference_width, age_width, folder_width) =
        (column(&references), column(&ages), column(&folders));
    let title_width = width
        .saturating_sub(reference_width + age_width + folder_width + 6)
        .max(SEARCH_TITLE_MIN_WIDTH);
    let mut output = String::new();
    for (index, hit) in hits.iter().enumerate() {
        let lead = format!(
            "{}  {}{}  {}",
            pad_width(&references[index], reference_width),
            " ".repeat(age_width - UnicodeWidthStr::width(ages[index].as_str())),
            ages[index],
            pad_width(&folders[index], folder_width),
        );
        if !show_text {
            let kind = if hit.conversation.is_some() {
                "conversation"
            } else {
                "metadata"
            };
            let _ = writeln!(output, "{lead}  {kind}");
            continue;
        }
        let _ = writeln!(
            output,
            "{lead}  {}",
            fit_width(hit.title.as_deref().unwrap_or("(untitled)"), title_width),
        );
        if let Some(conversation) = &hit.conversation {
            let prefix = format!("    [{}] ", search_coverage(conversation));
            let available = width
                .saturating_sub(prefix.len())
                .max(SEARCH_TITLE_MIN_WIDTH);
            let _ = writeln!(
                output,
                "{prefix}{}",
                snippet_window(&search_snippet(&conversation.snippet), query, available)
            );
        }
    }
    if has_more {
        output.push_str("… more matches; raise --limit\n");
    }
    output
}

fn search_coverage(conversation: &SessionTrajectoryMatch) -> &'static str {
    if conversation.complete {
        "complete"
    } else if head_tail_coverage(conversation) {
        "head-tail"
    } else {
        "preview"
    }
}

/// Omitted provider events can sit anywhere in a source, so a full read that reports them never
/// claims head-tail coverage.
fn head_tail_coverage(conversation: &SessionTrajectoryMatch) -> bool {
    conversation.source_complete
        && !conversation
            .truncation_strategy
            .starts_with("source_incomplete")
}

/// Bounds an index snippet to one redacted terminal-safe line.
fn search_snippet(snippet: &str) -> String {
    let line = compact_line(&redact_secrets(snippet));
    if line.chars().count() <= SEARCH_SNIPPET_CHARACTERS {
        return line;
    }
    line.chars()
        .take(SEARCH_SNIPPET_CHARACTERS - 1)
        .chain(std::iter::once('…'))
        .collect()
}

/// Starts a long snippet shortly before its first query term so truncation keeps the match.
fn snippet_window(snippet: &str, query: &str, width: usize) -> String {
    let characters = snippet.chars().collect::<Vec<_>>();
    let lowered = characters
        .iter()
        .copied()
        .map(fold_case)
        .collect::<Vec<_>>();
    let first_match = SearchQuery::parse(query)
        .terms()
        .iter()
        .map(|term| term.text().chars().map(fold_case).collect::<Vec<_>>())
        .filter(|term| !term.is_empty())
        .filter_map(|term| {
            lowered
                .windows(term.len())
                .position(|window| window == term.as_slice())
                .map(|start| (start, start + term.len()))
        })
        .min();
    let cells = |characters: &[char]| {
        characters
            .iter()
            .map(|character| UnicodeWidthChar::width(*character).unwrap_or(0))
            .sum::<usize>()
    };
    let Some((start, _)) = first_match.filter(|(_, end)| cells(&characters[..*end]) >= width)
    else {
        return fit_width(snippet, width);
    };
    let mut window_start = start;
    while window_start > 0 && cells(&characters[window_start - 1..start]) <= width / 3 {
        window_start -= 1;
    }
    let tail = characters[window_start..].iter().collect::<String>();
    fit_width(&format!("…{}", tail.trim_start()), width)
}

fn compact_line(value: &str) -> String {
    safe_terminal_line(value)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn compact_age(updated_at: Option<chrono::DateTime<Utc>>) -> String {
    let Some(updated_at) = updated_at else {
        return "-".to_owned();
    };
    let seconds = (Utc::now() - updated_at).num_seconds().max(0);
    match seconds {
        0..60 => "now".to_owned(),
        60..3_600 => format!("{}m", seconds / 60),
        3_600..86_400 => format!("{}h", seconds / 3_600),
        86_400..604_800 => format!("{}d", seconds / 86_400),
        604_800..2_629_746 => format!("{}w", seconds / 604_800),
        2_629_746..31_556_952 => format!("{}mo", seconds / 2_629_746),
        _ => format!("{}y", seconds / 31_556_952),
    }
}

fn project_folder(session: &NativeSession) -> String {
    session
        .project_path
        .as_deref()
        .and_then(Path::file_name)
        .map_or_else(
            || "-".to_owned(),
            |name| compact_line(&name.to_string_lossy()),
        )
}

fn fit_width(value: &str, width: usize) -> String {
    if UnicodeWidthStr::width(value) <= width {
        return value.to_owned();
    }
    let mut used = 0;
    let mut fitted = String::new();
    for character in value.chars() {
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
        if used + character_width + 1 > width {
            break;
        }
        used += character_width;
        fitted.push(character);
    }
    fitted.push('…');
    fitted
}

fn pad_width(value: &str, width: usize) -> String {
    format!(
        "{value}{}",
        " ".repeat(width.saturating_sub(UnicodeWidthStr::width(value)))
    )
}

fn search_output_width() -> usize {
    if io::stdout().is_terminal() {
        crossterm::terminal::size().map_or(SEARCH_TEXT_WIDTH, |(columns, _)| {
            usize::from(columns).max(40)
        })
    } else {
        SEARCH_TEXT_WIDTH
    }
}

fn write_search_output(text: &str) -> Result<()> {
    let mut stdout = io::stdout().lock();
    match stdout
        .write_all(text.as_bytes())
        .and_then(|()| stdout.flush())
    {
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        result => result.context("writing search results"),
    }
}

fn imported_bundle_id(session: &SessionRef) -> Result<Uuid> {
    let bundle_id = Uuid::parse_str(&session.id)
        .with_context(|| format!("imported source `{session}` has an invalid bundle UUID"))?;
    if session.id != bundle_id.to_string() {
        bail!("imported source must use canonical `imported:{bundle_id}` syntax");
    }
    Ok(bundle_id)
}

fn read_native_search_snapshot(
    registry: &AdapterRegistry,
    store: Option<&Store>,
    session: &SessionRef,
    full_read: bool,
) -> Result<(CanonicalSnapshot, Option<search_index::SearchSource>)> {
    search_index::observe_native_read(registry, store, session, None, None, |adapter, path| {
        adapter.read_for_search(session, path, full_read)
    })
}

fn continuation_target_provider(session: &SessionRef) -> Result<Provider> {
    if session.provider != Provider::Imported {
        return Ok(session.provider);
    }
    let bundle_id = imported_bundle_id(session)?;
    let store = Store::open_default().context("opening OmniSession state")?;
    load_imported_bundle(&store, bundle_id).map(|bundle| bundle.snapshot.session.provider)
}

fn load_imported_bundle(store: &Store, bundle_id: Uuid) -> Result<PortableBundle> {
    let bundle = store
        .load_bundle(bundle_id)
        .context("loading imported bundle")?
        .with_context(|| format!("imported bundle `{bundle_id}` was not found"))?;
    validate_bundle(&bundle).context("validating stored imported bundle")?;
    if bundle.manifest.bundle_id != bundle_id {
        bail!("stored imported bundle `{bundle_id}` has mismatched identity");
    }
    Ok(bundle)
}

#[derive(Debug, Parser)]
#[command(
    name = "omni",
    version,
    about = "Continue coding sessions across agents",
    after_help = "Run `omni` to choose a session and target. Use `omni resume ...` to continue or `omni fork ...` to branch."
)]
struct Cli {
    #[arg(long, global = true, help = "Emit machine-readable JSON")]
    json: bool,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Choose a session and continue it in any available agent.
    Resume(ResumeArgs),
    /// Fork a session into any available agent.
    Fork(ForkArgs),
    /// Find sessions by title, folder, branch, ID, or conversation text.
    Search(SearchArgs),
    /// Export visible conversation history as Markdown.
    Markdown(MarkdownArgs),
    /// Save session context and copy handoff instructions to the clipboard.
    Handoff(SessionArgs),
    /// Check provider installations, stores, and `OmniSession` state.
    Doctor,
    /// Diagnostic: list native sessions.
    List(ListArgs),
    /// Diagnostic: render safe context from one native session.
    Show(SessionArgs),
    /// Diagnostic: report transfer fidelity for one source and target.
    Inspect(InspectArgs),
    /// Advanced: move selected routing binding to another provider.
    Switch(SwitchArgs),
    /// Advanced: manage persistent routing bindings.
    Task(TaskArgs),
    /// Advanced: select routing binding for current workspace.
    Checkout(CheckoutArgs),
    /// Advanced: write a redacted portable bundle.
    Export(ExportArgs),
    /// Advanced: validate and store a portable bundle locally.
    Import(ImportArgs),
    /// Diagnostic: verify one session can be read.
    Verify(SessionArgs),
    /// Diagnostic: build the local conversation search index.
    Index(IndexArgs),
    /// List built-in adapter capabilities.
    Adapters(AdaptersArgs),
    /// List launch profiles from the local profile file and check that they can start.
    Profiles,
}

#[derive(Debug, Args)]
struct AdaptersArgs {
    /// Run each installed agent's version command and report whether a cross-agent native
    /// import would run or fall back to semantic handoff.
    #[arg(long)]
    check_imports: bool,
}

#[derive(Debug, Args)]
struct ListArgs {
    #[arg(long)]
    provider: Option<Provider>,
    #[arg(long, value_name = "PATH", default_value = ".")]
    project: PathBuf,
    #[arg(long, help = "List sessions across all known workspaces")]
    all_projects: bool,
    #[arg(long, default_value_t = 50)]
    limit: usize,
}

#[derive(Debug, Args)]
struct IndexArgs {
    #[arg(long, help = "Index only one source provider")]
    provider: Option<Provider>,
    #[arg(
        long,
        help = "Read sessions that failed before again, even when they have not changed"
    )]
    retry_failed: bool,
}

#[derive(Debug, Args)]
#[allow(clippy::struct_excessive_bools)]
struct SearchArgs {
    #[arg(
        value_name = "QUERY",
        required = true,
        num_args = 1..,
        help = "Words to find in titles, folders, branches, IDs, and conversation text"
    )]
    query: Vec<String>,
    #[arg(long, help = "Search only one source provider")]
    provider: Option<Provider>,
    #[arg(
        long,
        value_name = "PATH",
        default_value = ".",
        help = "Project to search"
    )]
    project: PathBuf,
    #[arg(long, help = "Search sessions across all known workspaces")]
    all_projects: bool,
    #[arg(
        long,
        value_name = "N",
        default_value_t = 20,
        help = "Maximum sessions to print"
    )]
    limit: usize,
    #[arg(
        long,
        help = "Show session titles and redacted conversation text around each match"
    )]
    show_text: bool,
    #[arg(
        long,
        help = "Search the existing index without indexing changed sessions first"
    )]
    no_index: bool,
    #[arg(
        long,
        help = "Search cached sessions without discovering or reading provider stores"
    )]
    cached: bool,
}

#[derive(Debug, Args)]
struct SessionArgs {
    #[arg(
        value_name = "SESSION",
        help = "Provider-qualified reference or exact session ID"
    )]
    session: String,
}

#[derive(Debug, Args)]
struct MarkdownArgs {
    #[arg(
        value_name = "SESSION",
        help = "Provider-qualified reference or exact session ID"
    )]
    source: String,
    #[arg(short, long, value_name = "FILE")]
    output: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct InspectArgs {
    #[arg(
        value_name = "SESSION",
        help = "Provider-qualified reference or exact session ID"
    )]
    session: String,
    #[arg(long, value_name = "AGENT", help = "Target agent or launch profile")]
    target: Option<AgentTarget>,
    #[arg(
        long,
        value_enum,
        value_name = "MODE",
        help = "Check the permission mode used to launch the target (default: OMNI_MODE or the agent default)"
    )]
    mode: Option<launch_mode::ModeKind>,
}

#[derive(Debug, Args, Default)]
#[allow(clippy::struct_excessive_bools)]
struct ResumeArgs {
    // Set when an interactive picker, not `--in`, chose the target.
    #[arg(skip)]
    picked_target: bool,
    #[arg(
        value_name = "SOURCE",
        help = "Provider-qualified reference or exact session ID"
    )]
    source: Option<String>,
    #[arg(
        long = "in",
        value_name = "AGENT",
        help = "Target agent or launch profile; omit to choose interactively"
    )]
    target: Option<AgentTarget>,
    #[arg(
        long = "from",
        value_name = "PROVIDER",
        conflicts_with = "source",
        help = "Start interactive selection filtered to one source provider"
    )]
    source_provider: Option<Provider>,
    #[arg(
        long = "all",
        visible_alias = "all-projects",
        conflicts_with = "source",
        help = "Start interactive selection across all workspaces"
    )]
    all_projects: bool,
    #[arg(long)]
    dry_run: bool,
    #[arg(
        long,
        help = "Create and verify supported native target session without launching it"
    )]
    materialize_only: bool,
    #[arg(
        long,
        conflicts_with = "no_fork",
        help = "Fork same-provider session into a new session before continuing"
    )]
    fork: bool,
    #[arg(
        long,
        conflicts_with = "fork",
        help = "Resume same-provider session in place instead of forking"
    )]
    no_fork: bool,
    #[arg(
        long,
        help = "Allow explicit transfer across different workspace roots"
    )]
    allow_workspace_mismatch: bool,
    #[arg(
        long,
        value_enum,
        value_name = "MODE",
        conflicts_with = "materialize_only",
        help = "Permission mode the agent starts in; without it no permission flags are passed and the agent keeps its own default"
    )]
    mode: Option<launch_mode::ModeKind>,
}

#[derive(Debug, Args)]
struct ForkArgs {
    #[arg(
        value_name = "SESSION",
        help = "Provider-qualified reference or exact session ID"
    )]
    source: String,
    #[arg(
        long = "in",
        value_name = "AGENT",
        help = "Target agent or launch profile; omit to choose interactively"
    )]
    target: Option<AgentTarget>,
    #[arg(long)]
    dry_run: bool,
    #[arg(
        long,
        help = "Create and verify supported native target session without launching it"
    )]
    materialize_only: bool,
    #[arg(
        long,
        help = "Allow explicit transfer across different workspace roots"
    )]
    allow_workspace_mismatch: bool,
    #[arg(
        long,
        value_enum,
        value_name = "MODE",
        conflicts_with = "materialize_only",
        help = "Permission mode the agent starts in; without it no permission flags are passed and the agent keeps its own default"
    )]
    mode: Option<launch_mode::ModeKind>,
}

#[derive(Debug, Args)]
struct SwitchArgs {
    #[arg(value_name = "AGENT", help = "Target agent or launch profile")]
    target: AgentTarget,
    #[arg(long)]
    dry_run: bool,
    #[arg(long, default_value = "main")]
    branch: String,
    #[arg(
        long,
        value_enum,
        value_name = "MODE",
        help = "Permission mode the agent starts in; without it no permission flags are passed and the agent keeps its own default"
    )]
    mode: Option<launch_mode::ModeKind>,
}

#[derive(Debug, Args)]
struct TaskArgs {
    #[command(subcommand)]
    command: TaskCommand,
}

#[derive(Debug, Subcommand)]
enum TaskCommand {
    /// Create and select a task for current workspace.
    Start {
        name: String,
        #[arg(long)]
        from: Option<SessionRef>,
        #[arg(long, default_value = "main")]
        branch: String,
    },
    /// Show selected task and branch head.
    Status {
        #[arg(long, default_value = "main")]
        branch: String,
    },
    /// Bind selected task branch to a verified native session.
    Bind {
        session: SessionRef,
        #[arg(long, default_value = "main")]
        branch: String,
    },
}

#[derive(Debug, Args)]
struct CheckoutArgs {
    name: String,
}

#[derive(Debug, Args)]
struct ExportArgs {
    #[arg(
        value_name = "SESSION",
        help = "Provider-qualified reference or exact session ID"
    )]
    session: String,
    #[arg(short, long, value_name = "FILE")]
    output: PathBuf,
}

#[derive(Debug, Args)]
struct ImportArgs {
    input: PathBuf,
}

fn main() -> ExitCode {
    let result = interrupt::with_provider_probes(|| run(Cli::parse()));
    match result {
        Ok(()) => ExitCode::SUCCESS,
        // The command already reported the interrupted work.
        Err(error) if error.is::<interrupt::Interrupted>() => ExitCode::from(130),
        Err(error) => {
            let _ = writeln!(io::stderr().lock(), "error: {error:#}");
            match error.downcast_ref::<ProviderExit>() {
                // Keep the provider's own status, including Windows codes wider than a byte.
                Some(ProviderExit(status)) => {
                    std::process::exit(provider_launch::shell_exit_code(*status))
                }
                None => ExitCode::FAILURE,
            }
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    let command = command_or_resume(cli.command);
    if discovers_imported_sources(&command)
        && let Err(error) = backfill_missing_imported_sources()
    {
        eprintln!("warning: local imported source migration: {error}");
    }
    let registry = AdapterRegistry::with_local_adapters();
    match command {
        Commands::Doctor => doctor(&registry, cli.json),
        Commands::List(args) => list(&registry, &args, cli.json),
        Commands::Show(args) => {
            let session = resolve_session_ref(&registry, &args.session)?;
            show(&registry, &session, cli.json)
        }
        Commands::Markdown(args) => markdown(&registry, &args, cli.json),
        Commands::Handoff(args) => {
            let source = resolve_session_ref(&registry, &args.session)?;
            handoff::run(&registry, &source, None, cli.json)
        }
        Commands::Inspect(args) => inspect(&registry, &args, cli.json),
        Commands::Resume(args) => resume(&registry, &args, cli.json, None),
        Commands::Fork(args) => fork(&registry, &args, cli.json),
        Commands::Search(args) => search_sessions(&registry, &args, cli.json),
        Commands::Switch(args) => switch(&registry, &args, cli.json),
        Commands::Task(args) => task(&registry, args, cli.json),
        Commands::Checkout(args) => checkout(&args, cli.json),
        Commands::Export(args) => export(&registry, &args, cli.json),
        Commands::Import(args) => import(&args, cli.json),
        Commands::Verify(args) => {
            let session = resolve_session_ref(&registry, &args.session)?;
            verify(&registry, &session, cli.json)
        }
        Commands::Adapters(args) => adapters(&registry, &args, cli.json),
        Commands::Profiles => launch_profile::list(cli.json),
        Commands::Index(args) => build_search_index(&registry, &args, cli.json),
    }
}

fn discovers_imported_sources(command: &Commands) -> bool {
    let provider = match command {
        Commands::List(args) => args.provider,
        Commands::Index(args) => args.provider,
        Commands::Search(args) if !args.no_index => args.provider,
        // The picker lets the user change its initial provider filter.
        Commands::Resume(args) if args.source.is_none() => None,
        _ => return false,
    };
    provider.is_none_or(|provider| provider == Provider::Imported)
}

fn command_or_resume(command: Option<Commands>) -> Commands {
    command.unwrap_or_else(|| Commands::Resume(ResumeArgs::default()))
}

#[derive(Debug)]
#[allow(clippy::struct_excessive_bools)]
struct ProviderStatus {
    installation: ProviderInstallation,
    launcher: Option<PathBuf>,
    source_detected: bool,
    read_index: bool,
    clean_start: bool,
    same_provider_resume: bool,
    cross_provider_import_declared: bool,
    cross_provider_import: bool,
    native_writer_ready: bool,
}

impl ProviderStatus {
    fn installed(&self) -> bool {
        self.source_detected || self.launcher.is_some()
    }

    fn cross_provider_route(&self, provider: Provider) -> &'static str {
        if self.cross_provider_import {
            if matches!(provider, Provider::OpenCode | Provider::Hermes) {
                "official_import"
            } else {
                "native_materialization"
            }
        } else if self.clean_start {
            "semantic_handoff"
        } else {
            "unavailable"
        }
    }

    fn native_writer_readiness(&self) -> &'static str {
        if self.native_writer_ready {
            "ready"
        } else if !self.cross_provider_import_declared {
            "not_declared"
        } else if self.launcher.is_none() {
            "launcher_not_detected"
        } else if self.cross_provider_import {
            "runtime_validation_required"
        } else {
            "unavailable"
        }
    }
}

fn session_discovery_status(installed: bool, read_index_declared: bool) -> &'static str {
    if !installed {
        "not_installed"
    } else if !read_index_declared {
        "not_declared"
    } else {
        "no_source"
    }
}

fn cross_provider_import_ready(
    provider: Provider,
    declared: bool,
    launcher: Option<&Path>,
) -> bool {
    declared
        && launcher.is_some_and(|binary| {
            provider != Provider::CursorIde || cursor_ide_cross_import_ready(binary)
        })
}

fn provider_status(registry: &AdapterRegistry, provider: Provider) -> Result<ProviderStatus> {
    use provider_compatibility::{Capability, supports_capability};

    let installation = registry.adapter(provider)?.probe();
    let launcher = if provider == Provider::CursorIde {
        cursor_ide_binary().ok()
    } else {
        resolved_provider_binary(provider).ok()
    };
    let source_detected = match provider {
        Provider::OpenCode => false,
        Provider::CursorIde => installation.installed,
        Provider::Hermes => installation
            .data_root
            .as_ref()
            .is_some_and(|root| root.join("state.db").is_file()),
        _ => installation
            .data_root
            .as_ref()
            .is_some_and(|root| root.is_dir()),
    };
    let read_index = supports_capability(provider, Capability::ReadIndex)
        && (source_detected || (provider == Provider::OpenCode && launcher.is_some()));
    let clean_start = supports_capability(provider, Capability::CleanStart) && launcher.is_some();
    let same_provider_resume =
        supports_capability(provider, Capability::SameProviderResume) && launcher.is_some();
    let cross_provider_import_declared =
        supports_capability(provider, Capability::CrossProviderImport);
    let cross_provider_import = cross_provider_import_ready(
        provider,
        cross_provider_import_declared,
        launcher.as_deref(),
    );
    Ok(ProviderStatus {
        installation,
        launcher,
        source_detected,
        read_index,
        clean_start,
        same_provider_resume,
        cross_provider_import_declared,
        cross_provider_import,
        native_writer_ready: false,
    })
}

fn doctor(registry: &AdapterRegistry, json_output: bool) -> Result<()> {
    let project = current_project()?;
    let mut results = Vec::new();
    for provider in PROVIDERS {
        let status = provider_status(registry, provider)?;
        let import_check = native_import_check(provider, &status);
        let read_index_declared = provider_compatibility::supports_capability(
            provider,
            provider_compatibility::Capability::ReadIndex,
        );
        let sessions = if status.read_index {
            session_discovery_report(registry.adapter(provider)?, &project)
        } else {
            json!({
                "status": session_discovery_status(status.installed(), read_index_declared),
                "count": 0,
                "all_projects_count": 0,
                "notes": [],
            })
        };
        results.push(json!({
            "provider": provider,
            "installed": status.installed(),
            "source_detected": status.source_detected,
            "launcher_detected": status.launcher.is_some(),
            "executable": status.launcher,
            "data_root": status.installation.data_root,
            "sessions": sessions,
            "native_writer_readiness": status.native_writer_readiness(),
            "native_import_check": import_check,
        }));
    }

    let store = Store::open_default().context("opening OmniSession state")?;
    let selected = store
        .selected_task(&project)
        .context("reading selected task")?;
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "schema_version": SCHEMA_VERSION,
                "version": env!("CARGO_PKG_VERSION"),
                "providers": results,
                "selected_task": selected.as_ref().map(|task| &task.name),
            }))?
        );
        return Ok(());
    }

    println!("OmniSession {}", env!("CARGO_PKG_VERSION"));
    println!(
        "Native import checks cover version gates only. Schema, active-writer, rollback, and read-back validation still run at transfer time."
    );
    for result in results {
        for line in doctor_provider_lines(&result) {
            println!("{line}");
        }
    }
    match selected {
        Some(task) => println!("selected task: {}", task.name),
        None => println!("selected task: none"),
    }
    Ok(())
}

fn session_discovery_report(adapter: &dyn ProviderAdapter, project: &Path) -> Value {
    match adapter.list_sessions(Some(project)) {
        Ok(project_sessions) => {
            let (all_projects_count, mut notes) = match adapter.list_sessions(None) {
                Ok(all) => (Some(all.len()), adapter.discovery_notes()),
                Err(error) => {
                    let mut notes = adapter.discovery_notes();
                    notes.push(format!("all-workspace discovery failed: {error}"));
                    (None, notes)
                }
            };
            if project_sessions.is_empty()
                && let Some(all) = all_projects_count.filter(|count| *count > 0)
            {
                notes.push(format!(
                    "No sessions match this workspace; {all} found across all workspaces. Press Tab in the picker or pass `--all-projects`."
                ));
            }
            json!({
                "status": "ok",
                "count": project_sessions.len(),
                "all_projects_count": all_projects_count,
                "notes": notes,
            })
        }
        Err(error) => json!({
            "status": "degraded",
            "count": 0,
            "error": error.to_string(),
            "notes": adapter.discovery_notes(),
        }),
    }
}

fn doctor_provider_lines(result: &Value) -> Vec<String> {
    let provider = result["provider"].as_str().unwrap_or("unknown");
    let status = result["sessions"]["status"].as_str().unwrap_or("unknown");
    let count = result["sessions"]["count"].as_u64().unwrap_or(0);
    let mut lines = Vec::new();
    match result["sessions"]["all_projects_count"].as_u64() {
        Some(all) => lines.push(format!(
            "{provider:<12} {status:<14} {count:>5} this workspace / {all:>5} all workspaces"
        )),
        None => lines.push(format!(
            "{provider:<12} {status:<14} {count:>5} this workspace"
        )),
    }
    if let Some(error) = result["sessions"]["error"].as_str() {
        lines.push(format!("             error: {}", safe_terminal_line(error)));
    }
    if let Some(notes) = result["sessions"]["notes"].as_array() {
        for note in notes {
            if let Some(note) = note.as_str() {
                lines.push(format!("             note: {}", safe_terminal_line(note)));
            }
        }
    }
    lines.extend(doctor_import_lines(result));
    lines
}

fn doctor_import_lines(result: &Value) -> Vec<String> {
    let check = &result["native_import_check"];
    let mut lines = Vec::new();
    match check["ready"].as_bool() {
        Some(false) => lines.push(format!(
            "             warning: native import blocked: {}",
            safe_terminal_line(check["blocker"].as_str().unwrap_or("unknown reason"))
        )),
        Some(true) => {
            let version = check["version"].as_str();
            let minimum = check["minimum_version"]
                .as_str()
                .map_or_else(String::new, |minimum| format!("; minimum {minimum}"));
            lines.push(format!(
                "             native import: {}; runtime validation required",
                version.map_or_else(
                    || "no version gate (provider-owned import)".to_owned(),
                    |version| format!(
                        "version gate passed ({}{minimum})",
                        safe_terminal_line(version)
                    )
                )
            ));
            if check["version_matches_tested"].as_bool() == Some(false) {
                lines.push(format!(
                    "             note: installed version differs from compatibility baseline {} ({}); this is not evidence of breaking compatibility",
                    safe_terminal_line(check["tested_version"].as_str().unwrap_or("unknown")),
                    safe_terminal_line(check["tested_source"].as_str().unwrap_or("unknown evidence"))
                ));
            }
        }
        None => {
            if let Some(readiness) = result["native_writer_readiness"].as_str() {
                lines.push(format!(
                    "             native import: {}",
                    safe_terminal_line(readiness)
                ));
            }
        }
    }
    lines
}

fn list(registry: &AdapterRegistry, args: &ListArgs, json_output: bool) -> Result<()> {
    let project = if args.all_projects {
        None
    } else {
        Some(
            omnis_core::canonicalize_path(&args.project)
                .with_context(|| format!("resolving project `{}`", args.project.display()))?,
        )
    };
    let (mut sessions, warnings, _) = discover_sessions(
        registry,
        args.provider,
        project.as_deref(),
        None,
        DiscoveryMode::List,
    );
    sessions.sort_by_key(|session| Reverse(session.updated_at));
    sessions.truncate(args.limit);

    if json_output {
        let values = sessions.iter().map(session_json).collect::<Vec<_>>();
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({"sessions": values, "warnings": warnings}))?
        );
        return Ok(());
    }
    for session in sessions {
        let updated = session
            .updated_at
            .map_or_else(|| "unknown".to_owned(), |time| time.to_rfc3339());
        println!(
            "{:<48}  {}",
            safe_terminal_line(&session.session.to_string()),
            updated
        );
    }
    for warning in warnings {
        eprintln!("warning: {}", safe_terminal_line(&warning));
    }
    Ok(())
}

fn indexed_imported_sessions(
    store: Option<&Store>,
    project: Option<&Path>,
) -> Result<(Vec<NativeSession>, Vec<String>)> {
    let default_store;
    let store = if let Some(store) = store {
        store
    } else {
        default_store = Store::open_default()?;
        &default_store
    };
    let mut sessions = Vec::new();
    let mut warnings = Vec::new();
    let mut matcher = project
        .map(|project| ImportedWorkspaceMatcher::new(store, project))
        .transpose()?;
    for session in store.indexed_sessions_for_provider(Provider::Imported)? {
        if let Some(matcher) = &mut matcher {
            match matcher.matches(&session.session, session.project_path.as_deref()) {
                Ok(ImportedWorkspaceMatch::Other) => continue,
                Ok(_) => {}
                Err(error) => {
                    warnings.push(format!("{}: {error:#}", session.session));
                    continue;
                }
            }
        }
        sessions.push(NativeSession {
            session: session.session,
            title: session.title,
            project_path: session.project_path,
            git_branch: session.git_branch,
            created_at: session.created_at,
            updated_at: session.updated_at,
            updated_at_approximate: session.updated_at_approximate,
            event_count: session.event_count,
            source_path: None,
            source_revision: None,
        });
    }
    Ok((sessions, warnings))
}

enum ImportedWorkspaceMatch {
    Recorded,
    Relocated,
    Other,
}

struct ImportedWorkspaceMatcher<'a> {
    store: &'a Store,
    project: &'a Path,
    paths: omnis_core::SessionWorkspaceMatcher<'a>,
    relocatable_paths: HashMap<Provider, HashMap<OsString, bool>>,
    providers: HashMap<String, Provider>,
    current_remote: OnceCell<Option<String>>,
}

impl<'a> ImportedWorkspaceMatcher<'a> {
    fn new(store: &'a Store, project: &'a Path) -> Result<Self> {
        Ok(Self {
            store,
            project,
            paths: omnis_core::SessionWorkspaceMatcher::new(project),
            relocatable_paths: HashMap::new(),
            providers: store.bundle_source_providers()?,
            current_remote: OnceCell::new(),
        })
    }

    fn can_relocate(&mut self, provider: Provider, path: &Path) -> Result<bool> {
        let paths = self.relocatable_paths.entry(provider).or_default();
        if let Some(&result) = paths.get(path.as_os_str()) {
            return Ok(result);
        }
        let result = can_relocate_workspace(provider, path)?;
        paths.insert(path.as_os_str().to_owned(), result);
        Ok(result)
    }

    fn matches(
        &mut self,
        locator: &SessionRef,
        path: Option<&Path>,
    ) -> Result<ImportedWorkspaceMatch> {
        if let (Some(&provider), Some(path)) = (self.providers.get(&locator.id), path) {
            if self.paths.matches(provider, path) {
                return Ok(ImportedWorkspaceMatch::Recorded);
            }
            if !self.can_relocate(provider, path)? {
                return Ok(ImportedWorkspaceMatch::Other);
            }
        } else if path.is_some_and(|recorded| self.paths.matches(Provider::Imported, recorded)) {
            // A locator can still identify its workspace when its bundle is missing.
            return Ok(ImportedWorkspaceMatch::Recorded);
        }
        let bundle = load_imported_bundle(self.store, imported_bundle_id(locator)?)?;
        if source_workspace_matches_with_cache(
            locator,
            &bundle.snapshot,
            self.project,
            &self.current_remote,
        )? {
            Ok(ImportedWorkspaceMatch::Relocated)
        } else {
            Ok(ImportedWorkspaceMatch::Other)
        }
    }
}

fn session_json(session: &NativeSession) -> Value {
    json!({
        "session": session.session,
        "project_path": session.project_path,
        "git_branch": session.git_branch,
        "created_at": session.created_at,
        "updated_at": session.updated_at,
        "event_count": session.event_count,
        "source_path": session.source_path,
    })
}

fn resolve_session_ref(registry: &AdapterRegistry, selector: &str) -> Result<SessionRef> {
    if selector.contains(':') {
        let mut session = selector.parse::<SessionRef>()?;
        if session.provider == Provider::Imported {
            session.id = Uuid::parse_str(&session.id)
                .with_context(|| {
                    format!(
                        "imported source `{}` has an invalid bundle UUID",
                        safe_terminal_line(selector)
                    )
                })?
                .to_string();
        }
        return Ok(session);
    }
    if selector.trim().is_empty() {
        bail!("session ID cannot be empty");
    }

    let discovered = thread::scope(|scope| {
        let handles = PROVIDERS.map(|provider| {
            (
                provider,
                scope.spawn(omnis_adapters::inherit_provider_probes(move || {
                    registry.find_native_session(provider, selector)
                })),
            )
        });
        handles.map(|(provider, handle)| {
            (
                provider,
                handle
                    .join()
                    .unwrap_or_else(|_| Err(anyhow!("provider discovery panicked"))),
            )
        })
    });
    let mut matches = Vec::new();
    let mut failures = Vec::new();
    let mut codex_unavailable = false;
    for (provider, result) in discovered {
        match result {
            Ok(session) => matches.extend(session.map(|session| session.session)),
            Err(error) => {
                codex_unavailable |= provider == Provider::Codex
                    && omnis_adapters::CodexAdapter::is_current_rollout_unavailable(&error);
                failures.push(format!("{provider}: {error}"));
            }
        }
    }
    if matches.is_empty() && codex_unavailable {
        let session = SessionRef::new(Provider::Codex, selector);
        let cleanup = Store::open_default()
            .map_err(anyhow::Error::from)
            .and_then(|store| {
                if let Some(prior) = store.indexed_session(&session)? {
                    remove_confirmed_unavailable(
                        registry.adapter(Provider::Codex)?,
                        &store,
                        vec![prior],
                    )?;
                }
                Ok(())
            });
        if let Err(error) = cleanup {
            eprintln!("warning: local trajectory index write: {error}");
        }
    }
    let selected = select_discovered_session(selector, matches, &failures)?;
    if !failures.is_empty() {
        let providers = failures
            .iter()
            .filter_map(|failure| failure.split_once(':').map(|(provider, _)| provider))
            .collect::<Vec<_>>()
            .join(", ");
        progress_line(&format!(
            "warning: resolved `{}` as `{}`, but could not check duplicate IDs in {providers}. Use `provider:id` when ambiguity matters.",
            safe_terminal_line(selector),
            safe_terminal_line(&selected.to_string())
        ))?;
    }
    Ok(selected)
}

fn select_discovered_session(
    selector: &str,
    matches: Vec<SessionRef>,
    failures: &[String],
) -> Result<SessionRef> {
    if matches.is_empty() && !failures.is_empty() {
        bail!(
            "cannot resolve bare session ID while provider discovery failed ({}); use provider:id",
            failures
                .iter()
                .map(|failure| safe_terminal_line(failure))
                .collect::<Vec<_>>()
                .join("; ")
        );
    }
    select_exact_session(selector, matches)
}

fn select_exact_session(selector: &str, mut matches: Vec<SessionRef>) -> Result<SessionRef> {
    matches.sort_by_key(ToString::to_string);
    match matches.as_slice() {
        [session] => Ok(session.clone()),
        [] => bail!(
            "no provider session found with exact ID `{}`",
            safe_terminal_line(selector)
        ),
        _ => bail!(
            "session ID `{}` is ambiguous ({}); use provider:id",
            safe_terminal_line(selector),
            matches
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn show(registry: &AdapterRegistry, session: &SessionRef, json_output: bool) -> Result<()> {
    let snapshot = registry
        .read_session_indexed(session)
        .with_context(|| format!("reading `{session}`"))?;
    if json_output {
        let safe = sanitize_snapshot(snapshot);
        println!("{}", serde_json::to_string_pretty(&safe)?);
    } else {
        println!("Source: {session}");
        if let Some(title) = &snapshot.title {
            println!("Title: {}", safe_terminal_line(&redact_secrets(title)));
        }
        println!("Events: {}", snapshot.events.len());
        println!();
        println!("{}", render_semantic_handoff(&snapshot));
    }
    Ok(())
}

fn markdown(registry: &AdapterRegistry, args: &MarkdownArgs, json_output: bool) -> Result<()> {
    if json_output && args.output.is_none() {
        bail!("`--json` requires `--output` for Markdown export");
    }
    let source = resolve_session_ref(registry, &args.source)?;
    let snapshot = registry
        .read_session_indexed(&source)
        .with_context(|| format!("reading `{source}`"))?;
    let snapshot = sanitize_snapshot(snapshot);
    let document = render_markdown_export(&snapshot);

    let Some(output) = &args.output else {
        io::stdout()
            .lock()
            .write_all(document.as_bytes())
            .context("writing Markdown to stdout")?;
        return Ok(());
    };

    write_markdown(output, &document)?;
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "source": source,
                "output": output,
                "events": snapshot.events.len(),
            }))?
        );
    } else {
        println!("Exported `{source}` to `{}`.", output.display());
    }
    Ok(())
}

fn inspect(registry: &AdapterRegistry, args: &InspectArgs, json_output: bool) -> Result<()> {
    let report = inspected_fidelity(registry, args)?;
    if json_output {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_fidelity(&report)?;
    }
    Ok(())
}

fn inspected_fidelity(registry: &AdapterRegistry, args: &InspectArgs) -> Result<FidelityReport> {
    let source = resolve_session_ref(registry, &args.session)?;
    let read = registry
        .read_selected_session_indexed(&source, None)
        .with_context(|| format!("reading `{source}`"))?;
    let snapshot = &read.snapshot;
    let target = match args.target {
        Some(target) => target,
        None => transfer::target_for_recorded_provider(
            &source,
            snapshot.session.provider.into(),
            "--target",
        )?,
    };
    transfer::reject_unsupported_target(target.provider)?;
    transfer::check_launch_profile(target)?;
    codex_import::check_continuation_target(target, &source)?;
    let current = current_project()?;
    let (project, matches) = if source.provider == Provider::Codex {
        let directory = verified_codex_directory(&read);
        let matches = codex_repository_matches(snapshot, directory.as_deref(), &current)?;
        (directory.filter(|_| matches).unwrap_or(current), matches)
    } else if source.provider == target.provider
        && matches!(target.provider, Provider::CursorCli | Provider::CursorIde)
    {
        transfer::cursor_session_workspace(
            target.provider,
            Some(&snapshot.workspace.root),
            &current,
            false,
            None,
        )?
    } else {
        let matches = transfer::transfer_workspace_matches(snapshot, &current)?;
        (current, matches)
    };
    let mode_args = transfer::launch_mode_args(
        target,
        args.mode,
        &transfer::cursor_working_directory(target.provider, &project)?,
        true,
    )?;
    if source.provider == target.provider
        && !transfer::requires_import_for_native_resume(registry, &source, target)?
    {
        if target.provider == Provider::CursorIde {
            let (_, report) = transfer::cursor_ide_continuation(snapshot, &project, matches)?;
            return Ok(report);
        }
        let plan = registry.launch_plan(
            &source,
            &LaunchTarget {
                cwd: Some(project),
                fork: false,
                prompt: None,
            },
        )?;
        check_launcher_starts(target, &plan)?;
        if target.provider == Provider::Codex {
            let mut plan = target.launch_with_mode(plan, &mode_args)?;
            transfer::verify_codex_launch(registry, &source, &source, &mut plan)?;
        }
        return Ok(fidelity_report_for_snapshot(
            snapshot,
            target.provider,
            matches,
        ));
    }
    inspect_transfer_report(registry, snapshot, target, matches, &project)
}

fn inspect_transfer_report(
    registry: &AdapterRegistry,
    snapshot: &CanonicalSnapshot,
    agent: AgentTarget,
    repository_matches: bool,
    project: &Path,
) -> Result<FidelityReport> {
    let target = agent.provider;
    let check_new_launcher = || -> Result<()> {
        let plan = registry.new_session_plan(
            target,
            &LaunchTarget {
                cwd: Some(project.to_path_buf()),
                fork: true,
                prompt: Some(transfer::HANDOFF_PREVIEW_PROMPT.to_owned()),
            },
        )?;
        check_launcher_starts(agent, &plan)
    };
    let stats = if may_attempt_native_import(target) {
        inspect_import_stats(snapshot, agent, project)
    } else {
        None
    };
    let warning = match stats {
        Some(Ok((truncated, tool_events, native_tool_records, official))) => {
            if agent.profile.is_some() {
                // The native import uses the installed provider. Its new session starts through the profile.
                check_new_launcher()?;
            }
            return Ok(if official {
                build_official_import_report_for_snapshot(
                    snapshot,
                    repository_matches,
                    truncated,
                    tool_events,
                    native_tool_records,
                )
            } else {
                build_native_materialization_report_for_snapshot(
                    snapshot,
                    target,
                    repository_matches,
                    truncated,
                    tool_events,
                    native_tool_records,
                )
            });
        }
        Some(Err(error)) => Some(format!(
            "native import unavailable: {}",
            safe_terminal_line(&format!("{error:#}"))
        )),
        None => None,
    };
    check_new_launcher().with_context(|| {
        let context = format!("checking the {agent} launcher for a semantic handoff");
        match &warning {
            Some(reason) => format!("{context} ({reason})"),
            None => context,
        }
    })?;
    let mut report =
        build_semantic_handoff_report_for_snapshot(snapshot, target, repository_matches);
    if let Some(warning) = warning {
        report.warnings.push(warning);
    }
    Ok(report)
}

fn inspect_import_stats(
    snapshot: &CanonicalSnapshot,
    agent: AgentTarget,
    project: &Path,
) -> Option<Result<(bool, usize, usize, bool)>> {
    let target = agent.provider;
    let stats = match target {
        Provider::Claude => resolved_provider_binary(target)
            .and_then(|binary| claude_import::ensure_supported(&binary))
            .and_then(|_| claude_import::build(snapshot, project))
            .map(|import| {
                (
                    import.truncated,
                    import.tool_events,
                    import.native_tool_records,
                    false,
                )
            }),
        Provider::Codex => resolved_provider_binary(target)
            .and_then(|binary| codex_import::ensure_supported(&binary))
            .and_then(|_| codex_import::build(snapshot))
            .and_then(|import| import.for_launch_profile(agent.profile))
            .map(|import| (import.truncated, import.tool_events, 0, false)),
        Provider::OpenCode => resolved_provider_binary(target)
            .and_then(|binary| installed_opencode_model_with_binary(&binary, project))
            .and_then(|model| opencode_import::build(snapshot, project, &model))
            .map(|import| {
                (
                    import.truncated,
                    import.tool_events,
                    import.native_tool_records,
                    true,
                )
            }),
        Provider::Grok => resolved_provider_binary(target)
            .and_then(|binary| grok_import::ensure_supported(&binary))
            .and_then(|_| grok_import::build(snapshot, project))
            .map(|import| {
                (
                    import.truncated,
                    import.tool_events,
                    import.native_tool_records,
                    false,
                )
            }),
        Provider::Hermes => resolved_provider_binary(target)
            .and_then(|binary| hermes_import::ensure_supported(&binary))
            .and_then(|_| hermes_import::build(snapshot, project))
            .map(|import| {
                (
                    import.truncated,
                    import.tool_events,
                    import.native_tool_records,
                    false,
                )
            }),
        Provider::Antigravity => resolved_provider_binary(target)
            .and_then(|binary| antigravity_import::ensure_supported(&binary))
            .and_then(|_| antigravity_import::build(snapshot, project))
            .map(|import| (import.truncated, import.tool_events, 0, false)),
        Provider::Pi | Provider::OhMyPi => resolved_provider_binary(target)
            .and_then(|binary| pi_import::ensure_supported_for(target, &binary))
            .and_then(|_| pi_import::build_for(target, snapshot, project))
            .map(|import| {
                (
                    import.truncated,
                    import.tool_events,
                    import.native_tool_records,
                    false,
                )
            }),
        Provider::CursorCli => resolved_provider_binary(target)
            .and_then(|binary| cursor_import::ensure_supported(&binary))
            .and_then(|_| cursor_import::build(snapshot, project))
            .map(|import| (import.truncated, import.tool_events, 0, false)),
        Provider::CursorIde => cursor_ide_binary()
            .and_then(|binary| cursor_ide_import::ensure_supported(&binary))
            .and_then(|_| cursor_ide_import::build(snapshot, project))
            .map(|import| (import.truncated, import.tool_events, 0, false)),
        Provider::AntigravityIde
        | Provider::AntigravityDesktop
        | Provider::GenericAcp
        | Provider::Imported => return None,
    };
    Some(stats)
}

fn switch(registry: &AdapterRegistry, args: &SwitchArgs, json_output: bool) -> Result<()> {
    let project = current_project()?;
    let store = Store::open_default().context("opening OmniSession state")?;
    let task = store
        .selected_task(&project)
        .context("reading selected task")?
        .ok_or_else(|| {
            anyhow!("no selected task; run `omni task start NAME --from PROVIDER:ID`")
        })?;
    let binding = store
        .current_binding(task.id, &args.branch)
        .context("reading branch head")?
        .ok_or_else(|| {
            anyhow!(
                "task `{}` branch `{}` has no session binding",
                task.name,
                args.branch
            )
        })?;
    let resume_args = ResumeArgs {
        source: Some(binding.session.to_string()),
        target: Some(args.target),
        source_provider: None,
        all_projects: false,
        dry_run: args.dry_run,
        materialize_only: false,
        fork: false,
        no_fork: true,
        allow_workspace_mismatch: false,
        picked_target: false,
        mode: args.mode,
    };
    resume(registry, &resume_args, json_output, Some(&binding))
}

fn task(registry: &AdapterRegistry, args: TaskArgs, json_output: bool) -> Result<()> {
    let project = current_project()?;
    let store = Store::open_default().context("opening OmniSession state")?;
    match args.command {
        TaskCommand::Start { name, from, branch } => {
            validate_branch(&branch)?;
            if let Some(session) = &from {
                validate_session_workspace(registry, session, &project)?;
            }
            let task = store
                .start_task(&name, &project, &branch, from.as_ref())
                .context("starting task")?;
            if json_output {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "task": task.name,
                        "workspace": task.workspace_root,
                        "branch": branch,
                        "session": from,
                    }))?
                );
            } else {
                println!("Selected task `{}` for `{}`.", task.name, project.display());
                if let Some(session) = from {
                    println!("Bound `{branch}` to `{session}`.");
                }
            }
        }
        TaskCommand::Status { branch } => {
            let selected = store
                .selected_task(&project)
                .context("reading selected task")?;
            let binding = selected
                .as_ref()
                .map(|task| store.current_binding(task.id, &branch))
                .transpose()
                .context("reading branch head")?
                .flatten();
            if json_output {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "task": selected.as_ref().map(|task| &task.name),
                        "workspace": project,
                        "branch": branch,
                        "session": binding.as_ref().map(|binding| &binding.session),
                    }))?
                );
            } else if let Some(task) = selected {
                println!("Task: {}", task.name);
                println!("Branch: {branch}");
                match binding {
                    Some(binding) => println!("Head: {}", binding.session),
                    None => println!("Head: none"),
                }
            } else {
                println!("No task selected.");
            }
        }
        TaskCommand::Bind { session, branch } => {
            bind_task_session(registry, &store, &project, &session, &branch, json_output)?;
        }
    }
    Ok(())
}

fn bind_task_session(
    registry: &AdapterRegistry,
    store: &Store,
    project: &Path,
    session: &SessionRef,
    branch: &str,
    json_output: bool,
) -> Result<()> {
    validate_branch(branch)?;
    validate_session_workspace(registry, session, project)?;
    let selected = store
        .selected_task(project)
        .context("reading selected task")?
        .ok_or_else(|| anyhow!("no selected task; run `omni task start NAME`"))?;
    store
        .bind_session(selected.id, branch, session)
        .context("binding session")?;
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "task": selected.name,
                "workspace": project,
                "branch": branch,
                "session": session,
            }))?
        );
    } else {
        println!(
            "Bound task `{}` branch `{branch}` to `{session}`.",
            selected.name
        );
    }
    Ok(())
}

fn validate_branch(branch: &str) -> Result<()> {
    if branch.trim().is_empty() {
        bail!("branch name cannot be empty");
    }
    Ok(())
}

fn validate_session_workspace(
    registry: &AdapterRegistry,
    session: &SessionRef,
    project: &Path,
) -> Result<()> {
    let read = registry
        .read_selected_session_indexed(session, None)
        .with_context(|| format!("validating `{session}`"))?;
    let directory = if session.provider == Provider::Codex {
        let directory = verified_codex_directory(&read).with_context(|| {
            format!("session `{session}` has no verified workspace for this task")
        })?;
        if workspace_paths_match(&directory, project) {
            return Ok(());
        }
        directory
    } else if source_workspace_matches_for_session(session, &read.snapshot, project)? {
        return Ok(());
    } else {
        read.snapshot.workspace.root
    };
    bail!(
        "session `{session}` belongs to `{}`, not `{}`",
        directory.display(),
        project.display()
    )
}

fn source_workspace_matches_for_session(
    session: &SessionRef,
    snapshot: &CanonicalSnapshot,
    project: &Path,
) -> Result<bool> {
    source_workspace_matches_with_cache(session, snapshot, project, &OnceCell::new())
}

fn source_workspace_matches_with_cache(
    session: &SessionRef,
    snapshot: &CanonicalSnapshot,
    project: &Path,
    current_remote: &OnceCell<Option<String>>,
) -> Result<bool> {
    if omnis_core::session_workspace_matches(
        snapshot.session.provider,
        &snapshot.workspace.root,
        project,
    ) {
        return Ok(true);
    }
    if session.provider != Provider::Imported
        || !can_relocate_workspace(snapshot.session.provider, &snapshot.workspace.root)?
        || snapshot
            .workspace
            .git
            .remote_fingerprint
            .as_ref()
            .is_none_or(String::is_empty)
    {
        return Ok(false);
    }
    let current = if let Some(remote) = current_remote.get() {
        remote
    } else {
        let remote = workspace_remote_fingerprint(project)?;
        current_remote.get_or_init(|| remote)
    };
    Ok(snapshot.workspace.git.remote_fingerprint.as_ref() == current.as_ref())
}

fn has_cursor_workspace_extension(provider: Provider, path: &Path) -> bool {
    provider == Provider::CursorIde && path.extension().is_some_and(|ext| ext == "code-workspace")
}

fn cursor_workspace_file_path(provider: Provider, path: &Path) -> bool {
    has_cursor_workspace_extension(provider, path)
        && !omnis_core::local_cursor_workspace_path(path)
            .is_ok_and(|resolved| resolved.is_some_and(|physical| physical.is_dir()))
}

fn can_relocate_workspace(provider: Provider, path: &Path) -> Result<bool> {
    if path.as_os_str().is_empty()
        || omnis_core::may_reach_network(path)
        // A missing saved workspace cannot identify one repository. An existing folder with
        // the same extension also stays in place, so neither case needs a configuration read.
        || has_cursor_workspace_extension(provider, path)
    {
        return Ok(false);
    }
    Ok(!path.try_exists().context("checking source workspace")?)
}

fn checkout(args: &CheckoutArgs, json_output: bool) -> Result<()> {
    let project = current_project()?;
    let store = Store::open_default().context("opening OmniSession state")?;
    let task = store
        .select_task(&project, &args.name)
        .with_context(|| format!("selecting task `{}`", args.name))?;
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({"task": task.name, "workspace": project}))?
        );
    } else {
        println!("Selected task `{}`.", task.name);
    }
    Ok(())
}

fn export(registry: &AdapterRegistry, args: &ExportArgs, json_output: bool) -> Result<()> {
    let source = resolve_session_ref(registry, &args.session)?;
    let mut snapshot = registry
        .read_session_indexed(&source)
        .with_context(|| format!("reading `{source}`"))?;
    // A saved workspace can contain several repositories. Its file path does not identify one.
    if snapshot.workspace.git.remote_fingerprint.is_none()
        && !cursor_workspace_file_path(snapshot.session.provider, &snapshot.workspace.root)
    {
        match current_project() {
            Ok(current) if workspace_paths_match(&snapshot.workspace.root, &current) => {
                match workspace_remote_fingerprint(&current) {
                    Ok(remote) => snapshot.workspace.git.remote_fingerprint = remote,
                    Err(error) => eprintln!(
                        "warning: exported repository identity unavailable: {}",
                        safe_terminal_line(&error.to_string())
                    ),
                }
            }
            _ => {}
        }
    }
    let safe_snapshot = sanitize_snapshot(snapshot);
    let bundle_source = safe_snapshot.session.clone();
    let bundle = PortableBundle {
        manifest: BundleManifest {
            schema_version: SCHEMA_VERSION.to_owned(),
            bundle_id: Uuid::new_v4(),
            created_at: Utc::now(),
            source: bundle_source.clone(),
            event_count: safe_snapshot.events.len(),
            redactions: vec![
                "secret events omitted".to_owned(),
                "credential patterns redacted".to_owned(),
            ],
        },
        snapshot: safe_snapshot,
        fidelity: Some(FidelityReport {
            source: bundle_source.provider,
            target: bundle_source.provider,
            mode: TransferMode::PortableExport,
            repository_matches: false,
            entries: vec![
                FidelityEntry {
                    feature: "Visible canonical events".to_owned(),
                    status: FidelityStatus::Summarized,
                    detail: Some(
                        "Provider-specific and unrecognized records are not exported".to_owned(),
                    ),
                },
                FidelityEntry {
                    feature: "Explicitly secret events".to_owned(),
                    status: FidelityStatus::Omitted,
                    detail: Some("Events classified secret are excluded".to_owned()),
                },
                FidelityEntry {
                    feature: "Credential-like values".to_owned(),
                    status: FidelityStatus::Redacted,
                    detail: Some(
                        "Common credential patterns and sensitive object keys are redacted"
                            .to_owned(),
                    ),
                },
                FidelityEntry {
                    feature: "Private provider state".to_owned(),
                    status: FidelityStatus::Unsupported,
                    detail: None,
                },
            ],
            warnings: vec![
                "Export is a redacted canonical snapshot, not a native-session backup.".to_owned(),
            ],
        }),
    };
    // Refuse a bundle `omni import` would reject, such as one naming a network workspace.
    validate_bundle(&bundle).context("validating exported bundle")?;
    write_bundle(&args.output, &bundle)?;
    let store = Store::open_default().context("opening OmniSession state")?;
    store.save_bundle(&bundle).context("storing bundle")?;
    index_bundle_source(&store, &bundle).context("indexing bundle source")?;
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "bundle_id": bundle.manifest.bundle_id,
                "output": args.output,
                "events": bundle.manifest.event_count,
            }))?
        );
    } else {
        println!(
            "Exported {} redacted events to `{}`.",
            bundle.manifest.event_count,
            args.output.display()
        );
    }
    Ok(())
}

fn import(args: &ImportArgs, json_output: bool) -> Result<()> {
    let link_metadata = fs::symlink_metadata(&args.input)
        .with_context(|| format!("reading `{}`", args.input.display()))?;
    if link_metadata.file_type().is_symlink() {
        bail!("refusing symlink bundle `{}`", args.input.display());
    }
    let file =
        File::open(&args.input).with_context(|| format!("opening `{}`", args.input.display()))?;
    let metadata = file.metadata().context("reading opened bundle metadata")?;
    if !metadata.is_file() {
        bail!("bundle input must be a regular file");
    }
    if metadata.len() > MAX_BUNDLE_SIZE {
        bail!("bundle exceeds {MAX_BUNDLE_SIZE} byte limit");
    }
    let reader = BufReader::new(file.take(MAX_BUNDLE_SIZE + 1));
    let bundle: PortableBundle = serde_json::from_reader(reader).context("parsing bundle")?;
    validate_bundle(&bundle)?;
    let store = Store::open_default().context("opening OmniSession state")?;
    match store.save_new_bundle(&bundle) {
        Ok(()) => {}
        Err(StoreError::BundleAlreadyExists) => {
            let existing = store
                .load_bundle(bundle.manifest.bundle_id)
                .context("loading existing bundle")?;
            if existing.as_ref() != Some(&bundle) {
                bail!(
                    "bundle UUID {} already exists with different content",
                    bundle.manifest.bundle_id
                );
            }
        }
        Err(error) => return Err(error).context("saving bundle"),
    }
    index_bundle_source(&store, &bundle).context("indexing imported bundle source")?;
    let imported = imported_session_ref(bundle.manifest.bundle_id);
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "bundle_id": bundle.manifest.bundle_id,
                "session": imported,
                "source": bundle.manifest.source,
                "events": bundle.manifest.event_count,
            }))?
        );
    } else {
        println!(
            "Imported `{imported}` from `{}`.",
            safe_terminal_line(&bundle.manifest.source.to_string())
        );
    }
    Ok(())
}

fn imported_session_ref(bundle_id: Uuid) -> SessionRef {
    SessionRef::new(Provider::Imported, bundle_id.to_string())
}

fn refresh_opened_bundle(
    store: &Store,
    session: &SessionRef,
    bundle: &PortableBundle,
) -> Result<()> {
    let prior = store.trajectory_index_state(session)?;
    if !full_document_current(
        session,
        &search_index::SearchSource::imported(bundle.snapshot.captured_at),
        prior.as_ref(),
    ) {
        index_bundle_source(store, bundle)?;
    } else if !store.has_indexed_session(session)? {
        index_bundle_metadata(store, bundle)?;
    }
    Ok(())
}

fn bundle_locator(bundle: &PortableBundle) -> IndexedSession {
    IndexedSession {
        source_revision: None,
        session: imported_session_ref(bundle.manifest.bundle_id),
        title: bundle.snapshot.title.as_deref().map(redact_secrets),
        project_path: Some(redact_path(&bundle.snapshot.workspace.root)),
        git_branch: bundle
            .snapshot
            .workspace
            .git
            .branch
            .as_deref()
            .map(redact_secrets),
        created_at: Some(bundle.manifest.created_at),
        updated_at: Some(bundle.snapshot.captured_at),
        updated_at_approximate: false,
        event_count: bundle.manifest.event_count,
    }
}

fn index_bundle_metadata(store: &Store, bundle: &PortableBundle) -> Result<()> {
    store
        .upsert_indexed_session(&bundle_locator(bundle))
        .context("indexing imported bundle metadata")
}

fn index_bundle_source(store: &Store, bundle: &PortableBundle) -> Result<bool> {
    let locator = bundle_locator(bundle);
    store_observed_search_document(
        store,
        &locator.session,
        &bundle.snapshot,
        true,
        &search_index::SearchSource::imported(bundle.snapshot.captured_at),
        TrajectorySourceGuard::NewerSource,
        Some(&locator),
    )
    .context("indexing imported bundle trajectory")
}

fn backfill_missing_imported_sources() -> Result<()> {
    let store = Store::open_default().context("opening OmniSession state")?;
    let missing = store
        .bundle_ids_needing_imported_source(None)
        .context("finding missing imported sources")?;
    for bundle_id in missing {
        let Ok(bundle) = load_imported_bundle(&store, bundle_id) else {
            eprintln!("warning: ignored malformed stored bundle `{bundle_id}`");
            continue;
        };
        index_bundle_metadata(&store, &bundle)
            .with_context(|| format!("migrating imported bundle `{bundle_id}`"))?;
    }
    Ok(())
}

fn cached_search_index(
    store: &Store,
    sessions: &[NativeSession],
    args: &SearchArgs,
    warnings: &mut Vec<String>,
) -> (search_index::IndexSummary, bool) {
    let mut summary = search_index::IndexSummary {
        candidates: sessions.len(),
        ..search_index::IndexSummary::default()
    };
    let imported_only = args.provider == Some(Provider::Imported);
    if !imported_only && args.no_index {
        return (summary, false);
    }
    match refresh_cached_imported_sources(store, sessions, args.no_index, &mut summary, warnings) {
        Ok(()) => (summary, imported_only),
        Err(error) => {
            warnings.push(format!("local imported source migration: {error}"));
            (summary, false)
        }
    }
}

fn refresh_cached_imported_sources(
    store: &Store,
    sessions: &[NativeSession],
    no_index: bool,
    summary: &mut search_index::IndexSummary,
    warnings: &mut Vec<String>,
) -> Result<()> {
    let eligible: Vec<_> = sessions
        .iter()
        .filter(|session| session.session.provider == Provider::Imported)
        .map(|session| session.session.id.as_str())
        .collect();
    if eligible.is_empty() {
        return Ok(());
    }
    let outdated = store
        .outdated_imported_documents(
            &eligible,
            omnis_core::search_document_version(Provider::Imported),
        )
        .context("finding outdated imported documents")?;
    summary.stale = outdated.len();
    if no_index {
        return Ok(());
    }
    for id in outdated {
        let session = SessionRef::new(Provider::Imported, id);
        let Ok(bundle_id) = imported_bundle_id(&session) else {
            summary.failed += 1;
            warnings.push("invalid imported session locator in the cache".to_owned());
            continue;
        };
        match load_imported_bundle(store, bundle_id)
            .context("missing or malformed stored bundle")
            .and_then(|bundle| index_bundle_source(store, &bundle))
        {
            Ok(rewritten) => summary.indexed += usize::from(rewritten),
            Err(error) => {
                summary.failed += 1;
                warnings.push(format!("refreshing `{session}`: {error:#}"));
            }
        }
    }
    Ok(())
}

fn validate_bundle(bundle: &PortableBundle) -> Result<()> {
    if bundle.manifest.schema_version != SCHEMA_VERSION
        || bundle.snapshot.schema_version != SCHEMA_VERSION
        || bundle.snapshot.workspace.schema_version != SCHEMA_VERSION
        || bundle
            .snapshot
            .events
            .iter()
            .any(|event| event.schema_version != SCHEMA_VERSION)
    {
        bail!("bundle contains unsupported schema version; expected `{SCHEMA_VERSION}`");
    }
    // Workspace matching resolves these paths, which could contact a host a bundle names.
    let workspace = &bundle.snapshot.workspace;
    if [
        Some(&workspace.root),
        Some(&workspace.current_dir),
        workspace.git.worktree.as_ref(),
    ]
    .into_iter()
    .flatten()
    .any(omnis_core::may_reach_network)
    {
        bail!("bundle workspace names a network or automounted path");
    }
    if bundle.manifest.source != bundle.snapshot.session {
        bail!("bundle source does not match snapshot session");
    }
    if bundle.manifest.source.id.trim().is_empty() {
        bail!("bundle source session ID is empty");
    }
    if bundle.manifest.event_count != bundle.snapshot.events.len() {
        bail!("bundle event count does not match snapshot");
    }
    if bundle
        .fidelity
        .as_ref()
        .is_some_and(|report| report.source != bundle.manifest.source.provider)
    {
        bail!("bundle fidelity source does not match manifest source");
    }
    let mut ids = HashSet::new();
    let mut previous_sequence = None;
    for (index, event) in bundle.snapshot.events.iter().enumerate() {
        if previous_sequence.is_some_and(|previous| event.sequence <= previous) {
            bail!("bundle event sequence is not strictly increasing at index {index}");
        }
        previous_sequence = Some(event.sequence);
        if !ids.insert(event.event_id) {
            bail!("bundle contains duplicate event ID {}", event.event_id);
        }
        if event.thread_id != bundle.snapshot.thread_id
            || event.branch_id != bundle.snapshot.branch_id
            || event.source.native_session_id.trim().is_empty()
        {
            bail!("bundle event {index} does not match snapshot identity");
        }
    }
    Ok(())
}

fn verify(registry: &AdapterRegistry, session: &SessionRef, json_output: bool) -> Result<()> {
    let snapshot = registry
        .read_session_indexed(session)
        .with_context(|| format!("reading `{session}`"))?;
    let mut kinds = BTreeMap::new();
    for event in &snapshot.events {
        *kinds.entry(format!("{:?}", event.kind)).or_insert(0usize) += 1;
    }
    let omitted_events = snapshot
        .events
        .iter()
        .filter(|event| event.kind == EventKind::ProviderEvent)
        .filter_map(|event| event.payload.get("omitted_events").and_then(Value::as_u64))
        .sum::<u64>();
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "session": session,
                "readable": true,
                "events": snapshot.events.len(),
                "omitted_events": omitted_events,
                "kinds": kinds,
            }))?
        );
    } else {
        println!("{session}: readable, {} events", snapshot.events.len());
        if omitted_events > 0 {
            println!("  {omitted_events} provider events omitted");
        }
        for (kind, count) in kinds {
            println!("  {kind:<24} {count}");
        }
    }
    Ok(())
}

/// Version gate of a cross-agent native import into `provider`: the installed version, or why
/// the import would fall back to semantic handoff. `None` when no version gates it.
fn native_import_gate(provider: Provider) -> Option<Result<String>> {
    let gate: fn(&Path) -> Result<String> = match provider {
        Provider::Claude => claude_import::ensure_supported,
        Provider::Codex => codex_import::ensure_supported,
        Provider::Grok => grok_import::ensure_supported,
        Provider::Hermes => hermes_import::ensure_supported,
        Provider::Antigravity => antigravity_import::ensure_supported,
        Provider::Pi => pi_import::ensure_supported,
        Provider::OhMyPi => pi_import::ensure_omp_supported,
        Provider::CursorCli => cursor_import::ensure_supported,
        Provider::CursorIde => {
            return Some(
                cursor_ide_binary().and_then(|binary| cursor_ide_import::ensure_supported(&binary)),
            );
        }
        Provider::OpenCode
        | Provider::AntigravityIde
        | Provider::AntigravityDesktop
        | Provider::GenericAcp
        | Provider::Imported => return None,
    };
    Some(resolved_provider_binary(provider).and_then(|binary| gate(&binary)))
}

fn native_import_check(provider: Provider, status: &ProviderStatus) -> Value {
    if !status.cross_provider_import_declared || status.launcher.is_none() {
        return Value::Null;
    }
    let mut check = match native_import_gate(provider) {
        None => json!({ "ready": true, "version": Value::Null, "blocker": Value::Null }),
        Some(Ok(version)) => json!({ "ready": true, "version": version, "blocker": Value::Null }),
        Some(Err(error)) => json!({
            "ready": false,
            "version": Value::Null,
            "blocker": safe_terminal_line(&format!("{error:#}")),
        }),
    };
    let versions = provider_compatibility::version_expectations(provider);
    check["minimum_version"] = json!(versions.minimum);
    check["tested_version"] = json!(versions.tested);
    check["tested_source"] = json!(versions.source);
    check["version_matches_tested"] = json!(
        check["version"]
            .as_str()
            .zip(versions.tested)
            .map(|(version, tested)| version == tested)
    );
    check["validation_scope"] = json!("version_gate_only");
    check
}

fn adapters(registry: &AdapterRegistry, args: &AdaptersArgs, json_output: bool) -> Result<()> {
    let values = PROVIDERS
        .iter()
        .map(|provider| {
            let status = provider_status(registry, *provider)?;
            let route = status.cross_provider_route(*provider);
            let writer_readiness = status.native_writer_readiness();
            let import_check = if args.check_imports {
                native_import_check(*provider, &status)
            } else {
                Value::Null
            };
            Ok(json!({
                "provider": provider,
                "installed": status.installed(),
                "source_detected": status.source_detected,
                "launcher_detected": status.launcher.is_some(),
                "executable": status.launcher,
                "data_root": status.installation.data_root,
                "read_index": status.read_index,
                "clean_start": status.clean_start,
                "same_provider_resume": status.same_provider_resume,
                "cross_provider_import_declared": status.cross_provider_import_declared,
                // Backward-compatible name for declared platform support.
                "cross_provider_import_supported": status.cross_provider_import_declared,
                "cross_provider_import": status.cross_provider_import,
                "native_writer_ready": status.native_writer_ready,
                "native_writer_readiness": writer_readiness,
                // Backward-compatible aliases retained for existing consumers.
                "native_resume": status.same_provider_resume,
                "native_write": status.native_writer_ready,
                "official_import": status.cross_provider_import
                    && matches!(*provider, Provider::OpenCode | Provider::Hermes),
                "cross_provider": route,
                "native_import_check": import_check,
            }))
        })
        .collect::<Result<Vec<_>>>()?;
    if json_output {
        println!("{}", serde_json::to_string_pretty(&values)?);
    } else {
        for value in values {
            let check = &value["native_import_check"];
            let import = match (check["ready"].as_bool(), check["version"].as_str()) {
                (Some(true), Some(version)) => format!(" import=ready ({version})"),
                (Some(true), None) => " import=ready (no version gate)".to_owned(),
                (Some(false), _) => format!(
                    " import=blocked ({})",
                    check["blocker"].as_str().unwrap_or("unknown")
                ),
                (None, _) => String::new(),
            };
            println!(
                "{:<16} installed={:<5} source={:<5} launcher={:<5} read_index={:<5} clean_start={:<5} same_resume={:<5} cross_declared={:<5} cross_import={:<5} route={:<22} writer={}{import}",
                value["provider"].as_str().unwrap_or("unknown"),
                value["installed"].as_bool().unwrap_or(false),
                value["source_detected"].as_bool().unwrap_or(false),
                value["launcher_detected"].as_bool().unwrap_or(false),
                value["read_index"].as_bool().unwrap_or(false),
                value["clean_start"].as_bool().unwrap_or(false),
                value["same_provider_resume"].as_bool().unwrap_or(false),
                value["cross_provider_import_supported"]
                    .as_bool()
                    .unwrap_or(false),
                value["cross_provider_import"].as_bool().unwrap_or(false),
                value["cross_provider"].as_str().unwrap_or("unavailable"),
                value["native_writer_readiness"]
                    .as_str()
                    .unwrap_or("unavailable"),
            );
        }
    }
    Ok(())
}

fn current_project() -> Result<PathBuf> {
    workspace_root(std::env::current_dir()?).context("resolving current workspace")
}

fn print_fidelity(report: &omnis_ir::FidelityReport) -> Result<()> {
    let mut stdout = io::stdout().lock();
    writeln!(stdout, "Transfer: {} -> {}", report.source, report.target)?;
    writeln!(stdout, "Mode: {}", report.mode)?;
    writeln!(stdout, "Repository match: {}", report.repository_matches)?;
    for entry in &report.entries {
        if let Some(detail) = &entry.detail {
            writeln!(
                stdout,
                "  {:<24} {:?} ({})",
                entry.feature,
                entry.status,
                safe_terminal_line(detail)
            )?;
        } else {
            writeln!(stdout, "  {:<24} {:?}", entry.feature, entry.status)?;
        }
    }
    stdout.flush()?;
    for warning in &report.warnings {
        progress_line(&format!("warning: {warning}"))?;
    }
    Ok(())
}

fn progress_line(message: &str) -> Result<()> {
    let mut stderr = io::stderr().lock();
    writeln!(stderr, "{message}").context("writing import progress")?;
    stderr.flush().context("flushing import progress")
}

fn flush_stdout() -> Result<()> {
    io::stdout().flush().context("flushing command output")
}

/// The launch as JSON. A value that a launch profile read from a credential-like variable is
/// hidden wherever it appears, because a profile can put a secret in an argument.
fn launch_json(plan: &LaunchPlan) -> Value {
    json!({
        "program": launch_profile::hide_secret_values(&plan.program),
        "args": plan
            .args
            .iter()
            .map(|arg| launch_profile::hide_secret_values(arg))
            .collect::<Vec<_>>(),
        "cwd": plan.cwd,
        "env": plan.env.iter().map(env_change_json).collect::<Vec<_>>(),
    })
}

fn env_change_json(change: &EnvChange) -> Value {
    match change {
        EnvChange::Set { name, value } => {
            json!({"name": name, "value": redacted_env_value(name, value)})
        }
        EnvChange::Remove(name) => json!({"name": name, "unset": true}),
    }
}

/// A value for display. Credentials in a variable value never reach output or logs.
///
/// The value is redacted as the right side of `NAME=value`, so the name decides whether it is a
/// credential, as it does in a shell command. Anything unexpected hides the whole value.
fn redacted_env_value(name: &str, value: &str) -> String {
    let value = launch_profile::hide_secret_values(value);
    redact_secrets(&format!("{name}={value}"))
        .strip_prefix(&format!("{name}="))
        .map_or_else(|| "[REDACTED]".to_owned(), str::to_owned)
}

/// The command as a shell would run it. Environment changes come first, as arguments of `env`.
fn display_command(plan: &LaunchPlan) -> String {
    let environment = plan.env.iter().flat_map(|change| match change {
        EnvChange::Set { name, value } => {
            vec![format!("{name}={}", redacted_env_value(name, value))]
        }
        EnvChange::Remove(name) => vec!["-u".to_owned(), name.clone()],
    });
    let command = std::iter::once(plan.program.as_str())
        .chain(plan.args.iter().map(String::as_str))
        .map(launch_profile::hide_secret_values);
    let prefix = (!plan.env.is_empty()).then(|| "env".to_owned());
    prefix
        .into_iter()
        .chain(environment)
        .chain(command)
        .map(|part| format!("{part:?}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn run_launch(plan: &LaunchPlan) -> Result<()> {
    let child = spawn_launch(plan)?;
    wait_for_launch(child, plan)
}

/// A launched provider. On Windows it also holds the console interrupt policy until it exits.
struct LaunchedProvider {
    child: Child,
    // Ctrl+C reaches the provider, which shares the console, while `omni` keeps waiting for it.
    #[cfg(windows)]
    _interrupts: Option<windows_console::InterruptScope>,
}

fn prepare_launch_command(plan: &LaunchPlan) -> Result<Command> {
    let provider = match plan.program.as_str() {
        "claude" => Some(Provider::Claude),
        "codex" => Some(Provider::Codex),
        "opencode" => Some(Provider::OpenCode),
        "grok" => Some(Provider::Grok),
        "hermes" => Some(Provider::Hermes),
        "agy" => Some(Provider::Antigravity),
        "pi" => Some(Provider::Pi),
        "omp" => Some(Provider::OhMyPi),
        "cursor-agent" => Some(Provider::CursorCli),
        _ => None,
    };
    let program = provider.map_or_else(
        || Ok(PathBuf::from(&plan.program)),
        resolved_provider_binary,
    )?;
    let mut command =
        provider_process(&program).with_context(|| format!("launching `{}`", plan.program))?;
    command.args(&plan.args);
    for change in &plan.env {
        match change {
            EnvChange::Set { name, value } => command.env(name, value),
            EnvChange::Remove(name) => command.env_remove(name),
        };
    }
    if let Some(cwd) = &plan.cwd {
        command.current_dir(cwd);
    }
    Ok(command)
}

/// Checks the CLI executable and environment without opening a session.
fn check_launcher_starts(target: AgentTarget, plan: &LaunchPlan) -> Result<()> {
    let mut failures = Vec::new();
    for flag in ["--help", "--version"] {
        let probe = target.launch(LaunchPlan {
            args: vec![flag.to_owned()],
            ..plan.clone()
        })?;
        let mut command = prepare_launch_command(&probe)?;
        let _home = version_gate::isolate_first_codex_probe(target.provider, &mut command)?;
        match omnis_adapters::bounded_provider_output(
            command.stdin(Stdio::null()),
            version_gate::PROBE_TIMEOUT,
            None,
            None,
        ) {
            Ok(output) if output.status.success() => return Ok(()),
            Ok(output) => failures.push(format!("{flag} returned {}", output.status)),
            Err(error) => failures.push(format!("{flag} failed: {error:#}")),
        }
    }
    bail!(
        "could not verify the {} launcher: {}",
        target,
        failures.join(", ")
    )
}

fn spawn_launch(plan: &LaunchPlan) -> Result<LaunchedProvider> {
    let mut command = prepare_launch_command(plan)?;
    #[cfg(windows)]
    let interrupts = windows_console::InterruptScope::ignore();
    let child = command
        .spawn()
        .with_context(|| format!("launching `{}`", plan.program))?;
    Ok(LaunchedProvider {
        child,
        #[cfg(windows)]
        _interrupts: interrupts,
    })
}

fn wait_for_launch(mut launched: LaunchedProvider, plan: &LaunchPlan) -> Result<()> {
    let status = launched
        .child
        .wait()
        .with_context(|| format!("waiting for `{}`", plan.program))?;
    if !status.success() {
        return Err(ProviderExit(status).into());
    }
    Ok(())
}

/// A launched provider exited unsuccessfully. `omni` exits with the status a shell would report.
#[derive(Debug)]
struct ProviderExit(ExitStatus);

impl std::fmt::Display for ProviderExit {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "target exited with status {}", self.0)
    }
}

impl std::error::Error for ProviderExit {}

fn native_delete_plan(session: &SessionRef, workspace: Option<&Path>) -> Result<LaunchPlan> {
    let args = match session.provider {
        Provider::Codex => {
            Uuid::parse_str(&session.id).context("Codex session ID must be a UUID")?;
            vec![
                "delete".to_owned(),
                session.id.clone(),
                "--force".to_owned(),
            ]
        }
        Provider::Grok => {
            validate_grok_session_id(&session.id)?;
            vec![
                "sessions".to_owned(),
                "delete".to_owned(),
                session.id.clone(),
            ]
        }
        Provider::Hermes => vec![
            "sessions".to_owned(),
            "delete".to_owned(),
            session.id.clone(),
            "--yes".to_owned(),
        ],
        provider => bail!(
            "{} does not expose documented session deletion; session was not changed",
            provider_name(provider)
        ),
    };
    Ok(LaunchPlan {
        program: session
            .provider
            .command()
            .context("session provider has no command")?
            .to_owned(),
        args,
        cwd: workspace
            .filter(|path| path.is_dir())
            .map(Path::to_path_buf),
        env: Vec::new(),
    })
}

fn delete_native_session(
    registry: &AdapterRegistry,
    session: &SessionRef,
    workspace: Option<&Path>,
) -> Result<()> {
    let native = unique_native_session(
        registry
            .list_sessions(session.provider, None)
            .with_context(|| format!("locating selected {} session", session.provider))?,
        session,
    )?;
    let _private_write_guard = match session.provider {
        Provider::Claude => Some(claude_delete::delete_session(
            session,
            native
                .source_path
                .as_deref()
                .context("Claude session discovery omitted transcript path")?,
        )?),
        Provider::Antigravity => {
            let binary = resolved_provider_binary(Provider::Antigravity)?;
            Some(antigravity_import::delete_session(session, &binary)?)
        }
        Provider::Pi => {
            pi_import::delete_session(
                session,
                native
                    .source_path
                    .as_deref()
                    .context("Pi session discovery omitted source path")?,
            )?;
            None
        }
        Provider::CursorCli => {
            cursor_import::delete_session(
                session,
                native
                    .source_path
                    .as_deref()
                    .context("Cursor CLI discovery omitted metadata path")?,
            )?;
            None
        }
        Provider::CursorIde => {
            let binary = cursor_ide_binary()?;
            Some(cursor_ide_import::delete_session(session, &binary)?)
        }
        Provider::OpenCode => {
            let binary = resolved_provider_binary(Provider::OpenCode)?;
            let cwd = workspace
                .into_iter()
                .chain(native.project_path.as_deref())
                .find(|path| path.is_dir());
            transfer::delete_opencode_session(&binary, session, cwd)?;
            None
        }
        Provider::Codex | Provider::Grok | Provider::Hermes => {
            delete_with_provider_command(session, workspace)?;
            None
        }
        provider => bail!(
            "{} does not support guarded native deletion; session was not changed",
            provider_name(provider)
        ),
    };
    // The provider has applied the deletion. Finish read-back under the same interrupt policy.
    let _verification = interrupt::ProtectedCommand::enter();
    let sessions = registry
        .list_sessions(session.provider, None)
        .with_context(|| format!("verifying {} session deletion", session.provider))?;
    if sessions
        .iter()
        .any(|candidate| candidate.session == *session)
        || (session.provider != Provider::OpenCode && registry.read_session(session).is_ok())
        || (session.provider == Provider::Grok
            && grok_session_directory_exists(
                registry
                    .adapter(Provider::Grok)?
                    .probe()
                    .data_root
                    .as_deref(),
                &session.id,
            )?)
    {
        bail!(
            "{} deletion did not remove exact source session",
            provider_name(session.provider)
        );
    }
    let _ = Store::open_default().and_then(|store| store.forget_session(session));
    Ok(())
}

fn unique_native_session(
    sessions: Vec<NativeSession>,
    selected: &SessionRef,
) -> Result<NativeSession> {
    let mut matches = sessions
        .into_iter()
        .filter(|candidate| candidate.session == *selected);
    let session = matches
        .next()
        .context("selected source session is no longer discoverable")?;
    if matches.next().is_some() {
        bail!(
            "selected source session ID `{}` is ambiguous; no session was deleted",
            selected.id
        );
    }
    Ok(session)
}

/// Grok names each session directory after its lowercase hyphenated UUID. Any other spelling could
/// reach outside that directory, or miss one that still exists and pass deletion verification.
fn validate_grok_session_id(id: &str) -> Result<()> {
    let canonical = Uuid::parse_str(id)
        .ok()
        .map(|uuid| uuid.hyphenated().to_string());
    if canonical.as_deref() != Some(id) {
        bail!("Grok session ID must be a lowercase hyphenated UUID");
    }
    Ok(())
}

fn grok_session_directory_exists(root: Option<&Path>, id: &str) -> Result<bool> {
    validate_grok_session_id(id)?;
    let Some(root) = root else {
        return Ok(false);
    };
    let root_metadata = fs::symlink_metadata(root).context("reading Grok session root")?;
    if !root_metadata.is_dir() || root_metadata.file_type().is_symlink() {
        bail!("Grok session root is not a safe directory");
    }
    for entry in fs::read_dir(root).context("reading Grok session workspaces")? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if !file_type.is_dir() || file_type.is_symlink() {
            continue;
        }
        let candidate = entry.path().join(id);
        let Ok(metadata) = fs::symlink_metadata(&candidate) else {
            continue;
        };
        if metadata.file_type().is_symlink() {
            bail!("Grok session path is an unsafe symlink");
        }
        if metadata.is_dir() {
            return Ok(true);
        }
    }
    Ok(false)
}

fn delete_with_provider_command(session: &SessionRef, workspace: Option<&Path>) -> Result<()> {
    let plan = native_delete_plan(session, workspace)?;
    let binary = resolved_provider_binary(session.provider)?;
    let mut stderr = tempfile::tempfile().context("creating session deletion error buffer")?;
    let mut command = provider_process(&binary)
        .with_context(|| format!("preparing {} session deletion", session.provider))?;
    command
        .args(&plan.args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr.try_clone()?));
    if let Some(cwd) = &plan.cwd {
        command.current_dir(cwd);
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("starting {} session deletion", session.provider))?;
    let Some(status) = child
        .wait_timeout(Duration::from_secs(30))
        .context("waiting for provider session deletion")?
    else {
        child
            .kill()
            .context("stopping timed-out session deletion")?;
        child.wait().context("reaping timed-out session deletion")?;
        bail!(
            "{} session deletion timed out",
            provider_name(session.provider)
        );
    };
    if session.provider == Provider::Grok {
        reconcile_grok_catalog(&binary, session, plan.cwd.as_deref());
        if !status.success() {
            return Ok(());
        }
    }
    if !status.success() {
        stderr.rewind().context("reading session deletion error")?;
        let mut output = Vec::new();
        stderr
            .take(16 * 1024)
            .read_to_end(&mut output)
            .context("reading session deletion error")?;
        let detail = safe_terminal_line(&redact_secrets(&String::from_utf8_lossy(&output)));
        let detail = (!detail.trim().is_empty()).then(|| format!(": {detail}"));
        bail!(
            "{} session deletion exited with {status}{}; source session was not deleted",
            provider_name(session.provider),
            detail.as_deref().unwrap_or_default()
        );
    }
    Ok(())
}

fn reconcile_grok_catalog(binary: &Path, session: &SessionRef, workspace: Option<&Path>) {
    let Ok(mut command) = provider_process(binary) else {
        return;
    };
    command
        .args(["sessions", "search", &session.id, "--limit", "1"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(workspace) = workspace {
        command.current_dir(workspace);
    }
    let Ok(mut child) = command.spawn() else {
        return;
    };
    if let Ok(None) = child.wait_timeout(Duration::from_secs(10)) {
        let _ = child.kill();
        let _ = child.wait();
    }
}

fn write_private_handoff(document: &str) -> Result<NamedTempFile> {
    let _store = Store::open_default().context("validating OmniSession state root")?;
    let directory = state_root()
        .context("resolving OmniSession state")?
        .join("handoffs");
    if directory
        .symlink_metadata()
        .is_ok_and(|metadata| metadata.file_type().is_symlink())
    {
        bail!(
            "refusing symlink handoff directory `{}`",
            directory.display()
        );
    }
    fs::create_dir_all(&directory)
        .with_context(|| format!("creating `{}`", directory.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))
            .context("securing handoff directory")?;
    }
    let mut file = tempfile::Builder::new()
        .prefix("handoff-")
        .suffix(".md")
        .tempfile_in(&directory)
        .context("creating private handoff")?;
    file.as_file_mut()
        .write_all(document.as_bytes())
        .context("writing private handoff")?;
    file.as_file()
        .sync_all()
        .context("syncing private handoff")?;
    Ok(file)
}

fn create_private_import_file() -> Result<NamedTempFile> {
    let _store = Store::open_default().context("validating OmniSession state root")?;
    let directory = state_root()
        .context("resolving OmniSession state")?
        .join("imports");
    if directory
        .symlink_metadata()
        .is_ok_and(|metadata| metadata.file_type().is_symlink())
    {
        bail!(
            "refusing symlink import directory `{}`",
            directory.display()
        );
    }
    fs::create_dir_all(&directory)
        .with_context(|| format!("creating `{}`", directory.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))
            .context("securing import directory")?;
    }
    tempfile::Builder::new()
        .prefix("opencode-")
        .suffix(".json")
        .tempfile_in(&directory)
        .context("creating private import")
}

fn sanitize_snapshot(mut snapshot: CanonicalSnapshot) -> CanonicalSnapshot {
    snapshot.events.retain(|event| {
        event.sensitivity != Sensitivity::Secret && event.replay_policy != ReplayPolicy::Secret
    });
    if let Some(title) = &mut snapshot.title {
        *title = redact_secrets(title);
    }
    for event in &mut snapshot.events {
        if redact_json_secrets(&mut event.payload) && event.sensitivity == Sensitivity::Normal {
            event.sensitivity = Sensitivity::PotentialSecret;
        }
        event.raw_blob_hash = None;
    }
    snapshot.workspace.root = redact_path(&snapshot.workspace.root);
    snapshot.workspace.current_dir = redact_path(&snapshot.workspace.current_dir);
    snapshot.workspace.git.worktree = snapshot.workspace.git.worktree.as_deref().map(redact_path);
    if let Some(branch) = &mut snapshot.workspace.git.branch {
        *branch = redact_secrets(branch);
    }
    snapshot.workspace.instruction_files = snapshot
        .workspace
        .instruction_files
        .iter()
        .map(|path| redact_path(path))
        .collect();
    snapshot.workspace.git.untracked_files = snapshot
        .workspace
        .git
        .untracked_files
        .iter()
        .map(|path| redact_path(path))
        .collect();
    snapshot.workspace.environment_names.clear();
    snapshot
}

fn redact_path(path: &Path) -> PathBuf {
    path.to_str().map_or_else(
        || PathBuf::from("[REDACTED: NON_UTF8_PATH]"),
        |path| PathBuf::from(redact_secrets(path)),
    )
}

fn write_bundle(path: &Path, bundle: &PortableBundle) -> Result<()> {
    let mut encoded = serde_json::to_vec_pretty(bundle).context("encoding bundle")?;
    encoded.push(b'\n');
    if encoded.len() as u64 > MAX_BUNDLE_SIZE {
        bail!("bundle exceeds {MAX_BUNDLE_SIZE} byte limit after redaction");
    }
    write_new_file(path, &encoded, "bundle")
}

fn write_markdown(path: &Path, document: &str) -> Result<()> {
    if document.len() as u64 > MAX_MARKDOWN_SIZE {
        bail!("Markdown export exceeds {MAX_MARKDOWN_SIZE} byte limit after redaction");
    }
    write_new_file(path, document.as_bytes(), "Markdown export")
}

fn write_new_file(path: &Path, contents: &[u8], description: &str) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).with_context(|| format!("creating `{}`", parent.display()))?;
    if path.exists() {
        bail!("refusing to overwrite existing `{}`", path.display());
    }
    let mut temporary = NamedTempFile::new_in(parent)
        .with_context(|| format!("creating temporary {description} in `{}`", parent.display()))?;
    temporary
        .as_file_mut()
        .write_all(contents)
        .with_context(|| format!("writing {description}"))?;
    temporary
        .as_file()
        .sync_all()
        .with_context(|| format!("syncing {description}"))?;
    temporary
        .persist_noclobber(path)
        .map_err(|error| error.error)
        .with_context(|| format!("persisting `{}`", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        path::{Path, PathBuf},
        process::Command,
    };

    use chrono::Utc;
    use clap::Parser;
    use omnis_adapters::CodexAdapter;
    use omnis_ir::{
        BundleManifest, CanonicalSnapshot, GitState, PortableBundle, SCHEMA_VERSION,
        WorkspaceSnapshot,
    };
    use serde_json::json;
    use uuid::Uuid;

    use super::{
        Cli, Commands, DELETE_PROVIDERS, NativeSession, Provider, ProviderInstallation,
        ProviderStatus, ResolvedResumeRequest, SessionRef, can_resume_without_snapshot,
        command_or_resume, cross_provider_import_ready, doctor_import_lines, doctor_provider_lines,
        grok_session_directory_exists, may_attempt_native_import_on, native_delete_plan,
        redact_json_secrets, requires_materialized_fork, resume_project, select_discovered_session,
        select_exact_session, selected_native_workspace, session_discovery_report,
        session_discovery_status, unique_native_session, validate_bundle,
    };
    use crate::provider_compatibility::{
        Capability, PROVIDER_PRIORITY, Platform, supports_capability_on,
    };
    use crate::session_picker::PickerSelection;
    use crate::transfer::reject_unsupported_target;

    const COMPATIBILITY_MANIFEST: &str = include_str!("../provider-compatibility.json");

    #[test]
    fn search_limit_preserves_rank_ties_and_conversation_overflow() {
        let temporary = tempfile::tempdir().expect("temporary search store");
        let store = super::Store::open(temporary.path().join("store.sqlite3")).expect("store");
        let timestamp = "2026-01-01T00:00:00Z".parse().expect("timestamp");
        let sessions = [
            ("weak", "Prefix needle", Some(timestamp)),
            ("first", "needle", Some(timestamp)),
            ("second", "needle", Some(timestamp)),
            (
                "newer",
                "needle",
                Some(timestamp + chrono::Duration::seconds(1)),
            ),
            ("conversation", "unrelated", None),
        ]
        .map(|(id, title, updated_at)| NativeSession {
            session: SessionRef::new(Provider::Claude, id),
            title: Some(title.to_owned()),
            project_path: None,
            git_branch: None,
            created_at: None,
            updated_at,
            updated_at_approximate: false,
            event_count: 0,
            source_path: None,
            source_revision: None,
        });
        for id in [
            "weak",
            "first",
            "second",
            "newer",
            "conversation",
            "outside",
        ] {
            store
                .upsert_session_trajectory(
                    &SessionRef::new(Provider::Claude, id),
                    "needle",
                    timestamp,
                    true,
                )
                .expect("synthetic conversation");
        }
        let expected = ["newer", "first", "second", "weak", "conversation"];
        for limit in 0..=6 {
            let (hits, has_more) = store
                .read_trajectory_search(|snapshot| {
                    super::rank_search_hits(
                        snapshot,
                        "needle",
                        &sessions,
                        &super::search_selections(&sessions, true),
                        Vec::new(),
                        limit,
                    )
                })
                .expect("rank matches");
            assert_eq!(
                hits.iter()
                    .map(|hit| hit.session.session.id.as_str())
                    .collect::<Vec<_>>(),
                expected[..limit.min(expected.len())],
                "limit {limit}",
            );
            assert_eq!(has_more, limit < expected.len(), "limit {limit}");
            for (position, hit) in hits.iter().enumerate() {
                assert_eq!(hit.conversation.is_some(), position == 4);
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn cached_search_distinguishes_local_and_network_path_spellings() {
        let temporary = tempfile::tempdir().expect("temporary cached store");
        let project = std::fs::canonicalize(temporary.path()).expect("local project");
        let network = PathBuf::from(format!("/{}", project.display()));
        let cli = Cli::try_parse_from([
            "omni",
            "search",
            "synthetic",
            "--cached",
            "--provider",
            "codex",
        ])
        .expect("cached search arguments");
        let Commands::Search(args) = cli.command.expect("search command") else {
            panic!("expected search arguments");
        };

        for local_first in [true, false] {
            let store = super::Store::open(
                temporary
                    .path()
                    .join(format!("state-{local_first}.sqlite3")),
            )
            .expect("synthetic store");
            let entries = [true, false].map(|local| super::IndexedSession {
                source_revision: None,
                session: SessionRef::new(
                    Provider::Codex,
                    if local == local_first { "a" } else { "b" },
                ),
                title: None,
                project_path: Some(if local {
                    project.clone()
                } else {
                    network.clone()
                }),
                git_branch: None,
                created_at: None,
                updated_at: None,
                updated_at_approximate: false,
                event_count: 0,
            });
            store
                .replace_indexed_sessions(Provider::Codex, &entries)
                .expect("cache both path spellings");

            let (sessions, _) = store
                .read_trajectory_search(|snapshot| {
                    super::cached_native_sessions(snapshot, &args, &project)
                })
                .expect("search local workspace");
            assert_eq!(sessions.len(), 1, "local first: {local_first}");
            assert_eq!(sessions[0].session, entries[0].session);
        }
    }

    fn compatibility_manifest() -> serde_json::Value {
        serde_json::from_str(COMPATIBILITY_MANIFEST).expect("compatibility manifest JSON")
    }

    fn manifest_provider(id: &serde_json::Value) -> Provider {
        let id = id.as_str().expect("manifest provider id");
        id.parse()
            .unwrap_or_else(|error| panic!("manifest provider `{id}`: {error}"))
    }

    #[test]
    fn provider_priority_matches_manifest() {
        let manifest = compatibility_manifest();
        let priority = manifest["provider_priority"]
            .as_array()
            .expect("manifest provider_priority")
            .iter()
            .map(manifest_provider)
            .collect::<Vec<_>>();

        assert_eq!(PROVIDER_PRIORITY.as_slice(), priority.as_slice());
    }

    #[test]
    fn provider_capability_platform_scope_matches_manifest() {
        const CAPABILITIES: [(&str, Capability); 4] = [
            ("read_index", Capability::ReadIndex),
            ("clean_start", Capability::CleanStart),
            ("same_provider_resume", Capability::SameProviderResume),
            ("cross_provider_import", Capability::CrossProviderImport),
        ];
        const PLATFORMS: [(&str, Platform); 3] = [
            ("linux", Platform::Linux),
            ("macos", Platform::Macos),
            ("windows", Platform::Windows),
        ];
        let manifest = compatibility_manifest();
        let entries = manifest["providers"]
            .as_array()
            .expect("manifest providers");
        let providers = entries
            .iter()
            .map(|entry| manifest_provider(&entry["id"]))
            .collect::<Vec<_>>();
        assert_eq!(providers.len(), PROVIDER_PRIORITY.len());
        assert!(
            PROVIDER_PRIORITY
                .iter()
                .all(|provider| providers.contains(provider)),
            "manifest providers must cover the runtime provider table"
        );

        for (entry, provider) in entries.iter().zip(providers) {
            let capabilities = entry["capabilities"]
                .as_object()
                .expect("manifest capabilities");
            assert_eq!(
                capabilities.len(),
                CAPABILITIES.len(),
                "{provider} declares unknown capabilities"
            );
            for (key, capability) in CAPABILITIES {
                let declared = capabilities
                    .get(key)
                    .and_then(serde_json::Value::as_array)
                    .unwrap_or_else(|| panic!("{provider} omits capability {key}"))
                    .iter()
                    .map(|platform| platform.as_str().expect("platform name"))
                    .collect::<Vec<_>>();
                assert!(
                    declared
                        .iter()
                        .all(|name| PLATFORMS.iter().any(|(known, _)| known == name)),
                    "{provider} {key} lists an unknown platform"
                );
                for (name, platform) in PLATFORMS {
                    assert_eq!(
                        supports_capability_on(provider, capability, platform),
                        declared.contains(&name),
                        "{provider} {key} on {name} differs from the manifest"
                    );
                }
            }
        }
    }

    #[test]
    fn explicit_native_import_uses_runtime_platform_policy() {
        // Codex and Grok roll back interrupted imports on Windows and declare cross-provider import
        // there. Explicit targets still attempt runtime-validated imports that stay undeclared.
        for (provider, declared) in [
            (Provider::Codex, true),
            (Provider::Grok, true),
            (Provider::OpenCode, false),
            (Provider::Pi, false),
            (Provider::CursorCli, false),
            (Provider::Hermes, false),
        ] {
            assert!(may_attempt_native_import_on(provider, Platform::Windows));
            assert_eq!(
                supports_capability_on(
                    provider,
                    Capability::CrossProviderImport,
                    Platform::Windows
                ),
                declared,
                "{provider} Windows cross-provider import declaration"
            );
        }
        for provider in [Provider::Claude, Provider::CursorIde, Provider::Antigravity] {
            assert!(!may_attempt_native_import_on(provider, Platform::Windows));
        }
        for provider in [
            Provider::AntigravityIde,
            Provider::GenericAcp,
            Provider::Imported,
        ] {
            assert!(!may_attempt_native_import_on(provider, Platform::Linux));
            assert!(!may_attempt_native_import_on(provider, Platform::Windows));
        }
    }

    #[test]
    fn provider_status_merges_evidence_and_separates_declaration_from_readiness() {
        let status = |source_detected, launcher| ProviderStatus {
            installation: ProviderInstallation {
                provider: Provider::Codex,
                installed: source_detected,
                executable: None,
                data_root: None,
            },
            launcher,
            source_detected,
            read_index: false,
            clean_start: false,
            same_provider_resume: false,
            cross_provider_import_declared: false,
            cross_provider_import: false,
            native_writer_ready: false,
        };
        assert!(status(true, None).installed());
        assert!(status(false, Some(PathBuf::from("/synthetic/codex"))).installed());
        assert!(!status(false, None).installed());
        assert_eq!(
            status(false, None).native_writer_readiness(),
            "not_declared"
        );

        let mut supported_without_launcher = status(false, None);
        supported_without_launcher.cross_provider_import_declared = true;
        assert!(!supported_without_launcher.cross_provider_import);
        assert_eq!(
            supported_without_launcher.native_writer_readiness(),
            "launcher_not_detected"
        );
        assert_eq!(
            supported_without_launcher.cross_provider_route(Provider::Codex),
            "unavailable"
        );

        let mut semantic = status(false, Some(PathBuf::from("/synthetic/agy")));
        semantic.clean_start = true;
        assert_eq!(
            semantic.cross_provider_route(Provider::Antigravity),
            "semantic_handoff"
        );

        let mut cursor = status(false, Some(PathBuf::from("/synthetic/cursor")));
        cursor.cross_provider_import_declared = true;
        cursor.cross_provider_import = true;
        assert_eq!(
            cursor.cross_provider_route(Provider::CursorIde),
            "native_materialization"
        );
        assert_eq!(
            cursor.native_writer_readiness(),
            "runtime_validation_required"
        );
    }

    #[test]
    fn cursor_ide_cross_import_readiness_requires_supported_static_version() {
        let temporary = tempfile::tempdir().expect("temporary Cursor applications");
        let older = temporary.path().join("Cursor-3.12.16-x86_64.AppImage");
        let supported = temporary.path().join("Cursor-3.12.17-x86_64.AppImage");
        std::fs::write(&older, b"synthetic older Cursor desktop").expect("older Cursor fixture");
        std::fs::write(&supported, b"synthetic supported Cursor desktop")
            .expect("supported Cursor fixture");

        assert!(!cross_provider_import_ready(
            Provider::CursorIde,
            true,
            Some(&older)
        ));
        assert!(cross_provider_import_ready(
            Provider::CursorIde,
            true,
            Some(&supported)
        ));
        assert!(cross_provider_import_ready(
            Provider::Codex,
            true,
            Some(Path::new("/synthetic/codex"))
        ));
        assert!(!cross_provider_import_ready(
            Provider::CursorIde,
            false,
            Some(&supported)
        ));
    }

    #[test]
    fn doctor_reports_blocked_gates_without_claiming_schema_compatibility() {
        let lines = doctor_import_lines(&json!({
            "native_import_check": {
                "ready": false,
                "blocker": "Pi 0.79.2 is too old\u{1b}[2J"
            }
        }));
        assert!(lines[0].contains("warning: native import blocked: Pi 0.79.2 is too old"));
        assert!(!lines[0].contains('\u{1b}'));

        let lines = doctor_import_lines(&json!({
            "native_import_check": {
                "ready": true,
                "version": "0.155.1",
                "minimum_version": "0.146.0",
                "tested_version": "0.147.0",
                "version_matches_tested": false
            }
        }));
        assert!(lines[0].contains("version gate passed (0.155.1; minimum 0.146.0)"));
        assert!(lines[0].contains("runtime validation required"));
        assert!(lines[1].contains("not evidence of breaking compatibility"));

        let lines = doctor_import_lines(&json!({
            "native_import_check": { "ready": true, "version": null }
        }));
        assert!(lines[0].contains("no version gate (provider-owned import)"));
        let lines = doctor_import_lines(&json!({
            "native_import_check": null,
            "native_writer_readiness": "not_declared"
        }));
        assert_eq!(lines, ["             native import: not_declared"]);
    }

    #[test]
    fn doctor_status_distinguishes_declaration_and_installation_evidence() {
        assert_eq!(session_discovery_status(true, false), "not_declared");
        assert_eq!(session_discovery_status(false, false), "not_installed");
        assert_eq!(session_discovery_status(true, true), "no_source");
    }

    #[test]
    fn doctor_sessions_report_workspace_mismatch_and_prints_errors() {
        let temporary = tempfile::tempdir().expect("temporary Codex home");
        let sessions = temporary.path().join("sessions/2026/09/09");
        std::fs::create_dir_all(&sessions).expect("session directory");
        let id = "44444444-4444-4444-8444-444444444444";
        std::fs::write(
            sessions.join(format!("rollout-{id}.jsonl")),
            format!(
                "{{\"timestamp\":\"2026-09-09T00:00:00Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"{id}\",\"cwd\":\"/workspace/other\"}}}}\n"
            ),
        )
        .expect("Codex fixture");
        let adapter = CodexAdapter::with_root(temporary.path());
        let report = session_discovery_report(&adapter, Path::new("/workspace/omnisession"));
        assert_eq!(report["status"], "ok");
        assert_eq!(report["count"], 0);
        assert_eq!(report["all_projects_count"], 1);
        let notes = report["notes"].as_array().expect("notes");
        assert!(
            notes.iter().any(|note| note
                .as_str()
                .is_some_and(|text| text.contains("all workspaces") && text.contains("Tab"))),
            "{notes:?}"
        );

        let lines = doctor_provider_lines(&json!({
            "provider": "codex",
            "sessions": {
                "status": "degraded",
                "count": 0,
                "error": "permission denied",
                "notes": ["Codex skipped 1 unreadable session directory."]
            }
        }));
        assert!(
            lines
                .iter()
                .any(|line| line.contains("error: permission denied")),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("note: Codex skipped")),
            "{lines:?}"
        );
    }

    #[test]
    fn bare_command_opens_resume_picker() {
        let cli = Cli::try_parse_from(["omni"]).expect("bare command");
        let Commands::Resume(args) = command_or_resume(cli.command) else {
            panic!("default resume command");
        };
        assert!(args.source.is_none());
        assert!(args.target.is_none());
        assert!(!args.all_projects);
    }

    #[test]
    fn native_delete_plans_ignore_missing_workspaces_and_reject_private_stores() {
        let workspace_root = tempfile::tempdir().expect("workspace");
        let workspace = workspace_root.path();
        let missing = native_delete_plan(
            &SessionRef::new(Provider::Codex, "019fa3c6-0000-7000-8000-000000000000"),
            Some(Path::new("/workspace/that/no/longer/exists")),
        )
        .expect("missing-workspace delete plan");
        assert!(missing.cwd.is_none());

        assert!(
            native_delete_plan(
                &SessionRef::new(Provider::Claude, "synthetic"),
                Some(workspace)
            )
            .is_err()
        );
    }

    #[test]
    fn private_store_deletion_requires_verified_writer_detection() {
        let verified = cfg!(any(target_os = "linux", target_os = "macos"));
        for provider in [
            Provider::Claude,
            Provider::Antigravity,
            Provider::Pi,
            Provider::CursorCli,
            Provider::CursorIde,
        ] {
            assert_eq!(
                DELETE_PROVIDERS.contains(&provider),
                verified,
                "{provider} private-store deletion platform gate"
            );
        }
    }

    #[test]
    fn deletion_refuses_duplicate_native_session_ids() {
        let selected = SessionRef::new(Provider::Pi, "duplicate");
        let native = |source_path: &str| NativeSession {
            session: selected.clone(),
            title: None,
            project_path: None,
            git_branch: None,
            created_at: None,
            updated_at: None,
            updated_at_approximate: false,
            event_count: 0,
            source_path: Some(PathBuf::from(source_path)),
            source_revision: None,
        };
        let error = unique_native_session(
            vec![native("/sessions/one.jsonl"), native("/sessions/two.jsonl")],
            &selected,
        )
        .expect_err("duplicate ID must be ambiguous");

        assert!(error.to_string().contains("ambiguous"));
    }

    #[test]
    fn grok_delete_verification_checks_exact_native_directory() {
        let root = tempfile::tempdir().expect("Grok sessions root");
        let workspace = root.path().join("workspace-key");
        let id = "019fa3c6-0000-7000-8000-000000000000";
        std::fs::create_dir_all(workspace.join(id)).expect("Grok session directory");

        assert!(
            grok_session_directory_exists(Some(root.path()), id)
                .expect("verify existing Grok session")
        );
        assert!(
            !grok_session_directory_exists(
                Some(root.path()),
                "019fa3c6-0000-7000-8000-000000000001"
            )
            .expect("verify missing Grok session")
        );
    }

    #[test]
    fn grok_delete_rejects_non_canonical_ids_before_running_provider() {
        for id in [
            "--all",
            "../019fa3c6-0000-7000-8000-000000000000",
            "019fa3c6000070008000000000000000",
            "{019fa3c6-0000-7000-8000-000000000000}",
            "urn:uuid:019fa3c6-0000-7000-8000-000000000000",
            "019FA3C6-0000-7000-8000-000000000000",
        ] {
            assert!(
                native_delete_plan(&SessionRef::new(Provider::Grok, id), None).is_err(),
                "Grok delete plan accepted `{id}`"
            );
        }

        // Another spelling of an existing session's UUID must not verify its directory as gone.
        let root = tempfile::tempdir().expect("Grok sessions root");
        let id = "019fa3c6-0000-7000-8000-000000000000";
        std::fs::create_dir_all(root.path().join("workspace-key").join(id))
            .expect("Grok session directory");
        assert!(
            grok_session_directory_exists(Some(root.path()), "019fa3c6000070008000000000000000")
                .is_err()
        );
    }

    #[test]
    fn workspace_projection_keeps_git_metadata_only_for_the_recorded_directory() {
        use omnis_adapters::{CodexWorkspaceProjection, ProviderAdapter};

        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        std::fs::create_dir(&sessions).unwrap();
        let id = "019fae41-ace0-7000-8000-123456789abc";
        let path = sessions.join(format!("rollout-{id}.jsonl"));
        let created = root.path().join("creation");
        let moved = root.path().join("continuation");
        let header = json!({"type":"session_meta","payload":{"id":id,"cwd":created,"git":{"branch":"creation-branch"}}});
        std::fs::write(&path, format!("{header}\n")).unwrap();
        let read = CodexAdapter::with_root(root.path())
            .index_session(&SessionRef::new(Provider::Codex, id), Some(&path), u64::MAX)
            .unwrap();
        for (directory, expected_branch) in [
            (Some(created), Some("creation-branch")),
            (Some(moved), None),
            (None, None),
        ] {
            let projection = CodexWorkspaceProjection::Observed(directory.clone());
            let update = super::codex_workspace_update(
                &read.snapshot,
                read.source_revision,
                false,
                &projection,
            )
            .unwrap();
            let omnis_store::CodexWorkspaceUpdate::Verified(workspace) = update else {
                panic!("a stamped workspace projection must be accepted");
            };
            assert_eq!(workspace.project_path, directory);
            assert_eq!(workspace.git_branch.as_deref(), expected_branch);
        }
    }

    #[test]
    fn bundle_validation_rejects_network_workspace_roots() {
        let bundle = |root: &str| {
            let root = PathBuf::from(root);
            let snapshot = CanonicalSnapshot {
                schema_version: SCHEMA_VERSION.to_owned(),
                session: SessionRef::new(Provider::Codex, "session"),
                thread_id: Uuid::nil(),
                branch_id: Uuid::nil(),
                title: None,
                captured_at: Utc::now(),
                workspace: WorkspaceSnapshot {
                    schema_version: SCHEMA_VERSION.to_owned(),
                    captured_at: Utc::now(),
                    root: root.clone(),
                    current_dir: root.clone(),
                    git: GitState {
                        worktree: (!root.as_os_str().is_empty()).then_some(root),
                        ..GitState::default()
                    },
                    instruction_files: Vec::new(),
                    environment_names: Vec::new(),
                    available_tools: Vec::new(),
                },
                events: Vec::new(),
            };
            PortableBundle {
                manifest: BundleManifest {
                    schema_version: SCHEMA_VERSION.to_owned(),
                    bundle_id: Uuid::from_u128(7),
                    created_at: Utc::now(),
                    source: snapshot.session.clone(),
                    event_count: 0,
                    redactions: Vec::new(),
                },
                snapshot,
                fidelity: None,
            }
        };

        for root in [
            "",
            "/workspace/repo",
            r"C:\workspace\repo",
            r"\\?\C:\workspace\repo",
        ] {
            assert!(validate_bundle(&bundle(root)).is_ok(), "rejected `{root}`");
        }
        for root in [
            r"\\attacker.invalid\share\repo",
            "//attacker.invalid/share/repo",
            r"\\?\UNC\attacker.invalid\share\repo",
            "/net/attacker.invalid/repo",
            "/NET/attacker.invalid/repo",
            "/System/Volumes/Data/net/attacker.invalid/repo",
            "/Network/Servers/attacker.invalid/repo",
            "/workspace/../net/attacker.invalid/repo",
        ] {
            let error = validate_bundle(&bundle(root)).expect_err(root);
            assert!(error.to_string().contains("network"), "{root}: {error:#}");
        }
    }

    #[test]
    fn resume_contract_parses_target_provider() {
        let cli =
            Cli::try_parse_from(["omni", "resume", "claude:abc", "--in", "codex", "--dry-run"])
                .expect("valid command");
        let Commands::Resume(args) = cli.command.expect("subcommand") else {
            panic!("resume command");
        };
        assert_eq!(args.source.as_deref(), Some("claude:abc"));
        assert_eq!(args.target, Some(Provider::Codex.into()));
        assert!(args.dry_run);
        assert!(!args.allow_workspace_mismatch);
    }

    #[test]
    fn resume_accepts_bare_id_and_optional_target() {
        let cli = Cli::try_parse_from(["omni", "resume", "abc", "--dry-run"])
            .expect("valid bare session ID");
        let Commands::Resume(args) = cli.command.expect("subcommand") else {
            panic!("resume command");
        };
        assert_eq!(args.source.as_deref(), Some("abc"));
        assert_eq!(args.target, None);
        assert!(!args.materialize_only);
    }

    #[test]
    fn resume_accepts_explicit_fork() {
        let cli =
            Cli::try_parse_from(["omni", "resume", "abc", "--fork"]).expect("valid fork request");
        let Commands::Resume(args) = cli.command.expect("subcommand") else {
            panic!("resume command");
        };
        assert!(args.fork);
        assert!(!args.no_fork);

        assert!(Cli::try_parse_from(["omni", "resume", "abc", "--fork", "--no-fork"]).is_err());
    }

    #[test]
    fn fork_accepts_bare_id_and_optional_target() {
        let cli = Cli::try_parse_from(["omni", "fork", "abc", "--in", "claude-code", "--dry-run"])
            .expect("valid fork request");
        let Commands::Fork(args) = cli.command.expect("subcommand") else {
            panic!("fork command");
        };
        assert_eq!(args.source, "abc");
        assert_eq!(args.target, Some(Provider::Claude.into()));
        assert!(args.dry_run);
        assert!(!args.allow_workspace_mismatch);

        let cli = Cli::try_parse_from(["omni", "fork", "claude-code:abc"])
            .expect("interactive fork request");
        let Commands::Fork(args) = cli.command.expect("subcommand") else {
            panic!("fork command");
        };
        assert_eq!(args.target, None);
    }

    #[test]
    fn resume_accepts_interactive_source_filters() {
        let cli = Cli::try_parse_from([
            "omni", "resume", "--in", "codex", "--from", "claude", "--all",
        ])
        .expect("valid interactive resume request");
        let Commands::Resume(args) = cli.command.expect("subcommand") else {
            panic!("resume command");
        };
        assert_eq!(args.source, None);
        assert_eq!(args.target, Some(Provider::Codex.into()));
        assert_eq!(args.source_provider, Some(Provider::Claude));
        assert!(args.all_projects);
    }

    #[test]
    fn native_resume_never_requires_full_transcript() {
        let codex = SessionRef::new(Provider::Codex, "019fa3c6-0000-7000-8000-000000000000");
        let request = ResolvedResumeRequest {
            source: codex.clone(),
            target: Provider::Codex,
            profile: None,
            resume_in_place: true,
            mode: None,
            picked_target: false,
            picker_selection: Some(PickerSelection {
                session: codex,
                source_revision: None,
                project_path: Some(PathBuf::from("/workspace/project")),
                across_projects: false,
                target: Provider::Codex.into(),
                fork: false,
                mode: None,
                workspace_override: None,
            }),
        };

        assert!(can_resume_without_snapshot(&request));

        let cross_provider = ResolvedResumeRequest {
            target: Provider::Claude,
            ..request
        };
        assert!(!can_resume_without_snapshot(&cross_provider));

        let cursor = ResolvedResumeRequest {
            source: SessionRef::new(Provider::CursorCli, "cursor-session"),
            target: Provider::CursorCli,
            profile: None,
            resume_in_place: false,
            mode: None,
            picker_selection: None,
            picked_target: false,
        };
        assert!(requires_materialized_fork(&cursor));
        assert!(!can_resume_without_snapshot(&cursor));
    }

    #[test]
    fn picker_workspace_override_replaces_missing_recorded_path() {
        let chosen = tempfile::tempdir().expect("chosen workspace");
        let current = tempfile::tempdir().expect("current workspace");
        let selection = PickerSelection {
            session: SessionRef::new(Provider::Codex, "session"),
            source_revision: None,
            project_path: Some(chosen.path().join("missing")),
            across_projects: false,
            target: Provider::Codex.into(),
            fork: false,
            mode: None,
            workspace_override: Some(chosen.path().to_path_buf()),
        };

        assert_eq!(
            selected_native_workspace(&selection, current.path()).expect("selected workspace"),
            omnis_core::canonicalize_path(chosen.path()).expect("canonical workspace")
        );
    }

    #[test]
    fn picker_workspace_override_wins_over_current_workspace() {
        let chosen = tempfile::tempdir().expect("chosen workspace");
        let current = tempfile::tempdir().expect("current workspace");
        let current_path = omnis_core::canonicalize_path(current.path()).expect("current path");
        let chosen_path = omnis_core::canonicalize_path(chosen.path()).expect("chosen path");
        let service = chosen_path.join("service");
        std::fs::create_dir(&service).expect("destination directory");
        assert!(
            Command::new("git")
                .args(["init", "--quiet"])
                .current_dir(&chosen_path)
                .status()
                .expect("initialize destination repository")
                .success()
        );
        let snapshot = CanonicalSnapshot {
            schema_version: SCHEMA_VERSION.to_owned(),
            session: SessionRef::new(Provider::Codex, "session"),
            thread_id: Uuid::nil(),
            branch_id: Uuid::nil(),
            title: None,
            captured_at: Utc::now(),
            workspace: WorkspaceSnapshot {
                schema_version: SCHEMA_VERSION.to_owned(),
                captured_at: Utc::now(),
                root: current_path.clone(),
                current_dir: current_path.clone(),
                git: GitState::default(),
                instruction_files: Vec::new(),
                environment_names: Vec::new(),
                available_tools: Vec::new(),
            },
            events: Vec::new(),
        };
        let selection = PickerSelection {
            session: snapshot.session.clone(),
            source_revision: None,
            project_path: None,
            across_projects: true,
            target: Provider::Codex.into(),
            fork: false,
            mode: None,
            workspace_override: Some(service.clone()),
        };

        assert_eq!(
            resume_project(
                &snapshot.session,
                &snapshot,
                &current_path,
                false,
                Some(&selection)
            )
            .expect("selected workspace"),
            chosen_path
        );
        assert_eq!(
            resume_project(
                &snapshot.session,
                &snapshot,
                &current_path,
                true,
                Some(&selection)
            )
            .expect("selected workspace with mismatch allowed"),
            chosen_path
        );
        let cursor = PickerSelection {
            target: Provider::CursorIde.into(),
            ..selection
        };
        assert_eq!(
            resume_project(
                &snapshot.session,
                &snapshot,
                &current_path,
                false,
                Some(&cursor)
            )
            .expect("selected Cursor IDE folder"),
            service
        );
    }

    #[test]
    fn resume_project_accepts_nested_path_only_in_same_repository() {
        let temp = tempfile::tempdir().expect("temporary workspaces");
        let repo = temp.path().join("repo");
        let sibling_repo = temp.path().join("sibling-repo");
        std::fs::create_dir(&repo).expect("repository directory");
        std::fs::create_dir(&sibling_repo).expect("sibling repository directory");
        for path in [&repo, &sibling_repo] {
            let status = Command::new("git")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env_remove("GIT_CONFIG_COUNT")
                .env_remove("GIT_CONFIG_PARAMETERS")
                .arg("-C")
                .arg(path)
                .args(["init", "--quiet"])
                .status()
                .expect("run git init");
            assert!(status.success());
        }
        let nested = repo.join("crates/component");
        std::fs::create_dir_all(&nested).expect("nested repository directory");
        let repo = omnis_core::canonicalize_path(repo).expect("repository root");
        let nested = omnis_core::canonicalize_path(nested).expect("nested directory");
        let sibling_repo =
            omnis_core::canonicalize_path(sibling_repo).expect("sibling repository root");
        let snapshot = CanonicalSnapshot {
            schema_version: SCHEMA_VERSION.to_owned(),
            session: SessionRef::new(Provider::Codex, "session"),
            thread_id: Uuid::nil(),
            branch_id: Uuid::nil(),
            title: None,
            captured_at: Utc::now(),
            workspace: WorkspaceSnapshot {
                schema_version: SCHEMA_VERSION.to_owned(),
                captured_at: Utc::now(),
                root: nested.clone(),
                current_dir: nested,
                git: GitState::default(),
                instruction_files: Vec::new(),
                environment_names: Vec::new(),
                available_tools: Vec::new(),
            },
            events: Vec::new(),
        };

        assert_eq!(
            resume_project(&snapshot.session, &snapshot, &repo, false, None)
                .expect("same repository"),
            repo
        );
        assert!(resume_project(&snapshot.session, &snapshot, &sibling_repo, false, None).is_err());
    }

    #[test]
    fn resume_accepts_materialize_only() {
        let cli = Cli::try_parse_from([
            "omni",
            "resume",
            "claude:abc",
            "--in",
            "opencode",
            "--materialize-only",
        ])
        .expect("valid materialize-only request");
        let Commands::Resume(args) = cli.command.expect("subcommand") else {
            panic!("resume command");
        };
        assert!(args.materialize_only);
    }

    #[test]
    fn markdown_accepts_bare_id_and_optional_output() {
        let cli = Cli::try_parse_from(["omni", "markdown", "abc", "-o", "session.md"])
            .expect("valid Markdown export");
        let Commands::Markdown(args) = cli.command.expect("subcommand") else {
            panic!("markdown command");
        };
        assert_eq!(args.source, "abc");
        assert_eq!(args.output.as_deref(), Some(Path::new("session.md")));
    }

    #[test]
    fn session_commands_accept_bare_ids() {
        for arguments in [
            vec!["omni", "show", "abc"],
            vec!["omni", "verify", "abc"],
            vec!["omni", "inspect", "abc", "--target", "codex"],
            vec!["omni", "export", "abc", "-o", "session.omnisession"],
        ] {
            Cli::try_parse_from(arguments).expect("bare session ID");
        }
    }

    #[test]
    fn exact_session_selection_requires_one_match() {
        let codex = SessionRef::new(Provider::Codex, "abc");
        assert_eq!(
            select_exact_session("abc", vec![codex.clone()]).expect("unique match"),
            codex
        );
        assert!(select_exact_session("abc", Vec::new()).is_err());
        assert!(
            select_exact_session(
                "abc",
                vec![
                    SessionRef::new(Provider::Claude, "abc"),
                    SessionRef::new(Provider::Codex, "abc")
                ]
            )
            .is_err()
        );
    }

    #[test]
    fn bare_session_selection_uses_known_match_when_another_provider_fails() {
        let claude = SessionRef::new(Provider::Claude, "abc");
        let failures =
            vec!["cursor-ide: provider SQLite file exceeds safe snapshot limit".to_owned()];

        assert_eq!(
            select_discovered_session("abc", vec![claude.clone()], &failures)
                .expect("known provider match"),
            claude
        );
        assert!(select_discovered_session("missing", Vec::new(), &failures).is_err());
    }

    #[test]
    fn provider_alias_is_normalized_by_cli() {
        let cli =
            Cli::try_parse_from(["omni", "list", "--provider", "cursor"]).expect("valid alias");
        let Commands::List(args) = cli.command.expect("subcommand") else {
            panic!("list command");
        };
        assert_eq!(args.provider, Some(Provider::CursorCli));

        let cli_agy =
            Cli::try_parse_from(["omni", "list", "--provider", "agy"]).expect("valid alias");
        let Commands::List(args_agy) = cli_agy.command.expect("subcommand") else {
            panic!("list command");
        };
        assert_eq!(args_agy.provider, Some(Provider::Antigravity));

        let cli_antigravity_cli =
            Cli::try_parse_from(["omni", "list", "--provider", "antigravity-cli"])
                .expect("valid alias");
        let Commands::List(args_cli) = cli_antigravity_cli.command.expect("subcommand") else {
            panic!("list command");
        };
        assert_eq!(args_cli.provider, Some(Provider::Antigravity));

        let cli_antigravity_ide =
            Cli::try_parse_from(["omni", "list", "--provider", "antigravity-ide"])
                .expect("valid alias");
        let Commands::List(args_ide) = cli_antigravity_ide.command.expect("subcommand") else {
            panic!("list command");
        };
        assert_eq!(args_ide.provider, Some(Provider::AntigravityIde));
    }

    #[test]
    fn antigravity_ide_is_rejected_as_a_native_target() {
        let error = reject_unsupported_target(Provider::AntigravityIde)
            .expect_err("Antigravity IDE has no native target");
        assert!(error.to_string().contains("antigravity-cli"), "{error}");
        reject_unsupported_target(Provider::Antigravity).expect("Antigravity CLI is supported");
        assert_eq!(
            crate::transfer::provider_name(Provider::Antigravity),
            "Antigravity CLI"
        );
        assert_eq!(
            crate::transfer::provider_name(Provider::AntigravityIde),
            "Antigravity IDE"
        );
        assert_eq!(
            crate::transfer::provider_name(Provider::CursorCli),
            "Cursor CLI"
        );
        assert_eq!(
            crate::transfer::provider_name(Provider::CursorIde),
            "Cursor IDE"
        );
    }

    #[test]
    fn structured_sensitive_fields_are_redacted() {
        let mut value = json!({
            "nested": {
                "refresh_token": "arbitrary-value",
                "x-api-key": "another-value"
            },
            "safe": "visible"
        });

        assert!(redact_json_secrets(&mut value));
        assert_eq!(
            value["nested"]["refresh_token"],
            "[REDACTED: SENSITIVE_FIELD]"
        );
        assert_eq!(value["nested"]["x-api-key"], "[REDACTED: SENSITIVE_FIELD]");
        assert_eq!(value["safe"], "visible");
    }
}

#[cfg(test)]
mod launch_environment_tests {
    use omnis_adapters::{EnvChange, LaunchPlan};

    use super::{display_command, launch_json, redacted_env_value};

    fn set(name: &str, value: &str) -> EnvChange {
        EnvChange::Set {
            name: name.to_owned(),
            value: value.to_owned(),
        }
    }

    fn plan(env: Vec<EnvChange>) -> LaunchPlan {
        LaunchPlan {
            program: "claude".to_owned(),
            args: vec!["--resume".to_owned(), "abc".to_owned()],
            cwd: None,
            env,
        }
    }

    #[test]
    fn plan_without_environment_changes_displays_as_before() {
        let plan = plan(Vec::new());
        assert_eq!(display_command(&plan), r#""claude" "--resume" "abc""#);
        assert_eq!(launch_json(&plan)["env"], serde_json::json!([]));
    }

    #[test]
    fn display_and_json_list_environment_changes_in_order() {
        let plan = plan(vec![
            set("ANTHROPIC_BASE_URL", "http://127.0.0.1:11435"),
            EnvChange::Remove("ANTHROPIC_API_KEY".to_owned()),
        ]);
        assert_eq!(
            display_command(&plan),
            r#""env" "ANTHROPIC_BASE_URL=http://127.0.0.1:11435" "-u" "ANTHROPIC_API_KEY" "claude" "--resume" "abc""#
        );
        assert_eq!(
            launch_json(&plan)["env"],
            serde_json::json!([
                {"name": "ANTHROPIC_BASE_URL", "value": "http://127.0.0.1:11435"},
                {"name": "ANTHROPIC_API_KEY", "unset": true},
            ])
        );
    }

    #[test]
    fn credential_values_never_reach_display_or_json() {
        let credential = "abcdef0123456789abcdef0123456789";
        let plan = plan(vec![
            set("ANTHROPIC_AUTH_TOKEN", credential),
            set("DEPLOY_PASSWORD", credential),
            set(
                "UPSTREAM_URL",
                &format!("https://user:{credential}@example.com/v1"),
            ),
        ]);
        let shown = format!("{} {}", display_command(&plan), launch_json(&plan));
        assert!(!shown.contains(credential), "{shown}");
        assert!(shown.contains("ANTHROPIC_AUTH_TOKEN"));
        assert_eq!(
            redacted_env_value("MODEL", "gateway/model-name"),
            "gateway/model-name"
        );
    }

    #[test]
    fn value_that_changes_its_own_name_is_hidden_entirely() {
        // A value that starts a new assignment would otherwise leak through the name prefix.
        let credential = "abcdef0123456789abcdef0123456789";
        let shown = redacted_env_value("X", &format!("\nAPI_KEY={credential}"));
        assert!(!shown.contains(credential), "{shown}");
    }

    #[cfg(unix)]
    mod launched_process {
        use std::{fs, path::Path};

        use omnis_adapters::{EnvChange, LaunchPlan};
        use tempfile::tempdir;

        use super::set;
        use crate::{spawn_launch, wait_for_launch};

        /// Writes `NAME`'s value to a file, or `unset` when the launched program has no such variable.
        fn probe(directory: &Path, name: &str, env: Vec<EnvChange>) -> String {
            let output = directory.join("output");
            let plan = LaunchPlan {
                program: "/bin/sh".to_owned(),
                args: vec![
                    "-c".to_owned(),
                    format!("printf '%s' \"${{{name}-unset}}\" > \"$1\""),
                    "probe".to_owned(),
                    output.to_string_lossy().into_owned(),
                ],
                cwd: None,
                env,
            };
            wait_for_launch(spawn_launch(&plan).expect("launch probe"), &plan)
                .expect("probe exits successfully");
            fs::read_to_string(output).expect("probe output")
        }

        #[test]
        fn set_and_remove_apply_to_the_launched_process_in_order() {
            let directory = tempdir().expect("temporary directory");
            let directory = directory.path();
            assert_eq!(probe(directory, "OMNI_PROBE_VALUE", Vec::new()), "unset");
            assert_eq!(
                probe(
                    directory,
                    "OMNI_PROBE_VALUE",
                    vec![set("OMNI_PROBE_VALUE", "a b")]
                ),
                "a b"
            );
            // The last change to a name decides, as in a shell that exports and unsets in turn.
            assert_eq!(
                probe(
                    directory,
                    "OMNI_PROBE_VALUE",
                    vec![
                        set("OMNI_PROBE_VALUE", "first"),
                        EnvChange::Remove("OMNI_PROBE_VALUE".to_owned()),
                    ]
                ),
                "unset"
            );
            assert_eq!(
                probe(
                    directory,
                    "OMNI_PROBE_VALUE",
                    vec![
                        EnvChange::Remove("OMNI_PROBE_VALUE".to_owned()),
                        set("OMNI_PROBE_VALUE", "second"),
                    ]
                ),
                "second"
            );
            // A variable that the parent process has is removed from the launched process.
            assert_eq!(
                probe(
                    directory,
                    "HOME",
                    vec![EnvChange::Remove("HOME".to_owned())]
                ),
                "unset"
            );
        }
    }
}
