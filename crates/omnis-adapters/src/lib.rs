//! Read-only provider discovery and canonicalization.

mod antigravity;
mod antigravity_desktop;
mod antigravity_ide;
mod claude;
mod codex;
mod cursor;
mod cursor_identity;
mod grok;
mod hermes;
mod omp;
mod opencode;
mod pi;
mod provider_process;
mod source_file;
mod support;
#[cfg(all(test, unix))]
mod test_support;

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};

use anyhow::{Result, anyhow};
use chrono::{DateTime, Utc};
use omnis_ir::{CanonicalSnapshot, Provider, SessionRef, SourceRevision};

pub use antigravity::AntigravityAdapter;
pub use antigravity_desktop::AntigravityDesktopAdapter;
pub use antigravity_ide::AntigravityIdeAdapter;
pub use claude::ClaudeAdapter;
pub use codex::CodexAdapter;
pub use cursor::{CursorCliAdapter, CursorIdeAdapter};
pub use cursor_identity::{
    cursor_workspace_id, is_cursor_workspace_id, validate_cursor_workspace_identity,
};
pub use grok::GrokAdapter;
pub use hermes::HermesAdapter;
pub use omp::oh_my_pi_sessions_root;
pub use opencode::{
    OpenCodeAdapter, OpenCodeGeneration, bounded_opencode_json_output,
    canonicalize_opencode_export, find_opencode_session_with_binary,
    fork_opencode_session_with_binary, installed_opencode_model,
    installed_opencode_model_with_binary, is_opencode_v2, opencode_command_args,
    read_opencode_export, read_opencode_session_with_binary, read_opencode_session_with_binary_at,
};
pub use pi::{InvalidPiTree, PiAdapter};
#[cfg(windows)]
pub use provider_process::WINDOWS_HELPER_CREATION_FLAGS;
pub use provider_process::{
    ProviderProbe, bounded_provider_help_output, bounded_provider_json_output,
    bounded_provider_line_output, bounded_provider_output, bounded_provider_text_output,
    inherit_provider_probes, provider_process, with_provider_probes,
};
pub use source_file::source_file_revision;
pub use support::TranscriptReadLimit;

/// Installation state discovered without reading provider credentials.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderInstallation {
    pub provider: Provider,
    pub installed: bool,
    pub executable: Option<PathBuf>,
    pub data_root: Option<PathBuf>,
}

/// Metadata returned by session discovery. Transcript content is intentionally absent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeSession {
    pub session: SessionRef,
    pub title: Option<String>,
    pub project_path: Option<PathBuf>,
    pub git_branch: Option<String>,
    pub created_at: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
    pub updated_at_approximate: bool,
    pub event_count: usize,
    pub source_path: Option<PathBuf>,
    /// The file revision observed with this session's metadata.
    pub source_revision: Option<SourceRevision>,
}

/// The opener confirmed by a full ordinary Codex read or by reconstructing an applied rollback.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum RollbackTitle {
    #[default]
    Unknown,
    /// An empty title means the surviving transcript has no eligible user opener.
    Known(Option<String>),
    /// An unread row followed one or more confirmed withdrawals. Their successor is unknown.
    UnknownAfterWithdrawal(WithdrawnOpeners),
}

/// Bounded evidence about local opening titles removed before the first unreadable row.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WithdrawnOpeners {
    hashes: Vec<[u8; 32]>,
    overflowed: bool,
}

impl WithdrawnOpeners {
    fn record(&mut self, hash: [u8; 32]) {
        if !self.hashes.contains(&hash) {
            if self.hashes.len() < 8 {
                self.hashes.push(hash);
            } else {
                self.overflowed = true;
            }
        }
    }

    fn reissued(&mut self, title: &str) {
        if !self.hashes.is_empty() {
            let hash = RollbackTitle::fingerprint(title);
            self.hashes.retain(|previous| *previous != hash);
        }
    }

    fn after_gap(&self) -> RollbackTitle {
        if self.hashes.is_empty() && !self.overflowed {
            RollbackTitle::Unknown
        } else {
            RollbackTitle::UnknownAfterWithdrawal(self.clone())
        }
    }
}

impl RollbackTitle {
    fn fingerprint(title: &str) -> [u8; 32] {
        use sha2::Digest;
        sha2::Sha256::digest(title.as_bytes()).into()
    }

    /// Whether this reconstruction witnessed the saved opener being removed before a gap.
    #[must_use]
    pub fn withdraws(&self, title: &str) -> bool {
        matches!(self, Self::UnknownAfterWithdrawal(witnesses) if witnesses.hashes.contains(&Self::fingerprint(title)))
    }

    /// Whether a single-title check can resolve an overflow in the bounded withdrawal history.
    #[must_use]
    pub fn needs_withdrawal_check(&self, title: &str) -> bool {
        matches!(self, Self::UnknownAfterWithdrawal(witnesses) if witnesses.overflowed && !self.withdraws(title))
    }
}

/// The effective workspace established independently of the amount of indexed conversation.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum CodexWorkspaceProjection {
    #[default]
    Unchecked,
    /// No directory means the selected rollout establishes that its workspace is unknown.
    Observed(Option<PathBuf>),
}

impl CodexWorkspaceProjection {
    fn from_reconstruction(verified: bool, directory: Option<&Path>) -> Self {
        if verified {
            Self::Observed(
                directory
                    .filter(|path| !path.as_os_str().is_empty())
                    .map(Path::to_path_buf),
            )
        } else {
            Self::Unchecked
        }
    }
}

/// A search indexing read and whether it covered the full provider source.
pub struct IndexRead {
    /// Catalog metadata verified by the reader of the selected native transcript.
    pub native_session: Option<NativeSession>,
    pub snapshot: CanonicalSnapshot,
    pub rollback_title: RollbackTitle,
    /// The workspace projection belongs to this read's source revision.
    pub workspace_projection: CodexWorkspaceProjection,
    pub source_complete: bool,
    /// A provider limit made a full read impossible, even without an indexer byte budget.
    pub provider_limited: bool,
    /// The source revision observed while reading the selected native transcript.
    pub source_revision: SourceRevision,
}

/// Provider-specific launch input.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LaunchTarget {
    pub cwd: Option<PathBuf>,
    pub fork: bool,
    pub prompt: Option<String>,
}

impl Default for LaunchTarget {
    fn default() -> Self {
        Self {
            cwd: None,
            fork: true,
            prompt: None,
        }
    }
}

/// One change to the environment that a launched program inherits.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EnvChange {
    Set { name: String, value: String },
    Remove(String),
}

/// State database settings that a Codex launch profile supplies.
#[derive(Debug, Default)]
pub struct CodexLaunchOverrides {
    pub project_root_markers: Option<Vec<String>>,
    pub profile: Option<String>,
    pub sqlite_home: Option<PathBuf>,
    pub environment: Option<EnvChange>,
}

/// Command description. Callers decide whether and how to execute it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LaunchPlan {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    /// Changes to the inherited environment, applied in order before the child starts.
    pub env: Vec<EnvChange>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeResumeSupport {
    Direct,
    Since(&'static str),
    ImportOnly,
}

fn read_budgeted_transcript(
    within_budget: bool,
    mut read: impl FnMut(bool) -> Result<CanonicalSnapshot>,
) -> Result<IndexRead> {
    if within_budget {
        match read(false) {
            Ok(snapshot) => {
                return Ok(IndexRead {
                    native_session: None,
                    rollback_title: RollbackTitle::Unknown,
                    workspace_projection: CodexWorkspaceProjection::Unchecked,
                    snapshot,
                    source_complete: true,
                    provider_limited: false,
                    source_revision: SourceRevision::default(),
                });
            }
            Err(error) if error.is::<support::TranscriptReadLimit>() => {}
            Err(error) => return Err(error),
        }
    }
    Ok(IndexRead {
        native_session: None,
        rollback_title: RollbackTitle::Unknown,
        workspace_projection: CodexWorkspaceProjection::Unchecked,
        snapshot: read(true)?,
        source_complete: false,
        provider_limited: within_budget,
        source_revision: SourceRevision::default(),
    })
}

/// The files a native provider selects when more than one transcript can identify the same session.
pub trait NativeSelectionInventory: Send + Sync {
    /// Updates the inventory outside the search store's write transaction.
    ///
    /// # Errors
    ///
    /// Returns a discovery error when the provider directory cannot be inspected.
    fn refresh(&mut self) -> Result<()>;

    /// Returns the entry chosen by the provider's exact-ID rule.
    fn selected(&self, id: &str) -> Option<&NativeSession>;

    /// Checks known paths together and returns the result for each request in order.
    /// If a transcript changed, the adapter can check its previously observed header.
    fn validate(&self, requests: &[(&str, &Path, SourceRevision)]) -> Vec<bool>;
}

pub trait ProviderAdapter: Send + Sync {
    fn provider(&self) -> Provider;

    fn probe(&self) -> ProviderInstallation;

    /// Lists native session metadata, optionally filtered by exact project path.
    ///
    /// # Errors
    ///
    /// Returns provider discovery or data-read failures.
    fn list_sessions(&self, project: Option<&Path>) -> Result<Vec<NativeSession>>;

    /// Finds candidates for live indexing in one project, including recorded workspace changes.
    ///
    /// # Errors
    ///
    /// Returns provider discovery or data-read failures.
    fn project_search_candidates(
        &self,
        project: &Path,
    ) -> Result<(Vec<NativeSession>, Vec<String>)> {
        self.list_search_sessions_with_notes(Some(project))
    }

    /// Lists sessions for search in the requested project.
    ///
    /// Providers with duplicate IDs select the file used by an exact read before filtering by project.
    ///
    /// # Errors
    ///
    /// Returns provider discovery or data-read failures.
    fn list_search_sessions(&self, project: Option<&Path>) -> Result<Vec<NativeSession>> {
        self.list_sessions(project)
    }

    /// Lists searchable sessions together with any warnings from this invocation.
    ///
    /// # Errors
    ///
    /// Returns provider discovery or data-read failures.
    fn list_search_sessions_with_notes(
        &self,
        project: Option<&Path>,
    ) -> Result<(Vec<NativeSession>, Vec<String>)> {
        Ok((self.list_search_sessions(project)?, self.discovery_notes()))
    }

    /// Finds session metadata by exact native ID.
    ///
    /// An exact ID is an explicit selection. Adapters whose listings hide sessions that the
    /// provider can still resume must override this, so that those sessions stay reachable. The
    /// default searches the listing.
    ///
    /// # Errors
    ///
    /// Returns provider discovery or data-read failures. A missing session is `Ok(None)`.
    fn find_native_session(&self, id: &str) -> Result<Option<NativeSession>> {
        Ok(self
            .list_sessions(None)?
            .into_iter()
            .find(|session| session.session.id == id))
    }

    /// Describes whether the provider can continue the stored representation by its session ID.
    ///
    /// # Errors
    ///
    /// Returns an error if the adapter cannot identify the source representation.
    fn native_resume_support(&self, _session: &SessionRef) -> Result<NativeResumeSupport> {
        Ok(NativeResumeSupport::Direct)
    }

    /// Resolves the local SQLite home for a Codex continuation. An existing ID must select the displayed session there.
    ///
    /// # Errors
    ///
    /// Returns an error if the adapter cannot resolve the launch configuration.
    fn codex_continuation_sqlite_home(
        &self,
        _session: Option<&SessionRef>,
        _cwd: &Path,
        _overrides: &CodexLaunchOverrides,
        _expand_env_tilde: &dyn Fn() -> Result<bool>,
    ) -> Result<Option<PathBuf>> {
        Ok(None)
    }

    /// Revalidates the provider file selected for an exact session ID.
    ///
    /// # Errors
    ///
    /// Returns provider errors when the selected file cannot be inspected.
    fn selected_session_at(&self, id: &str, source_path: &Path) -> Result<Option<NativeSession>> {
        Ok(self
            .find_native_session(id)?
            .filter(|session| session.source_path.as_deref() == Some(source_path)))
    }

    /// Captures the current file selections for repeated checks during one index operation.
    ///
    /// # Errors
    ///
    /// Returns a discovery error when a provider with an inventory cannot inspect its files.
    fn selection_inventory(&self) -> Result<Option<Box<dyn NativeSelectionInventory>>> {
        Ok(None)
    }

    /// Non-fatal discovery observations from this adapter.
    ///
    /// Notes may be computed during the first `list_sessions` call and reused.
    /// They must not include transcript content.
    fn discovery_notes(&self) -> Vec<String> {
        Vec::new()
    }

    /// Whether the last listing observed every eligible session across all projects.
    ///
    /// Return false for bounded, incomplete, or structurally unverified listings. An omitted
    /// session can be treated as deleted only after complete discovery. The default preserves it.
    fn discovery_is_complete(&self) -> bool {
        false
    }

    /// Returns the cached Codex sessions that their recorded rollout selections rule out.
    ///
    /// # Errors
    ///
    /// Returns an error if the provider's current selections cannot be checked.
    fn unavailable_codex_rollouts(
        &self,
        _prior: &[(String, Option<SourceRevision>)],
    ) -> Result<HashSet<String>> {
        Ok(HashSet::new())
    }

    /// Reads one provider-native session into canonical events.
    ///
    /// # Errors
    ///
    /// Returns an error for mismatched providers, missing sessions, or unreadable data.
    fn read_session(&self, session: &SessionRef) -> Result<CanonicalSnapshot>;

    /// Reads a provider-native session, using a previously discovered path when available.
    ///
    /// Adapters must validate the hint against their configured provider root and the exact
    /// session identity before reading it. The default preserves adapters without path hints.
    ///
    /// # Errors
    ///
    /// Returns an error for mismatched providers, missing sessions, or unreadable data.
    fn read_session_at(
        &self,
        session: &SessionRef,
        _source_path: Option<&Path>,
    ) -> Result<CanonicalSnapshot> {
        self.read_session(session)
    }

    /// Reads a full transcript together with any catalog metadata verified by the same read.
    ///
    /// # Errors
    ///
    /// Returns provider read failures, including limits that prevent a full read.
    fn read_full_index_at(
        &self,
        session: &SessionRef,
        source_path: Option<&Path>,
    ) -> Result<IndexRead> {
        Ok(IndexRead {
            native_session: None,
            rollback_title: RollbackTitle::Unknown,
            workspace_projection: CodexWorkspaceProjection::Unchecked,
            snapshot: self.read_session_at(session, source_path)?,
            source_complete: true,
            provider_limited: false,
            source_revision: SourceRevision::default(),
        })
    }

    /// Reads enough provider-native history for bounded interactive previews.
    ///
    /// Adapters may sample large active sessions without weakening full-import limits.
    ///
    /// # Errors
    ///
    /// Returns an error for mismatched providers, missing sessions, or unreadable data.
    fn preview_session(&self, session: &SessionRef) -> Result<CanonicalSnapshot> {
        self.read_session(session)
    }

    /// Reads a bounded provider-native preview, using a previously discovered path when valid.
    ///
    /// # Errors
    ///
    /// Returns an error for mismatched providers, missing sessions, or unreadable data.
    fn preview_session_at(
        &self,
        session: &SessionRef,
        _source_path: Option<&Path>,
    ) -> Result<CanonicalSnapshot> {
        self.preview_session(session)
    }

    /// Returns the transcript file when discovery uses its modification time for search freshness.
    ///
    /// # Errors
    ///
    /// Returns an error when the session cannot be resolved to its provider-owned file.
    fn search_timestamp_file(
        &self,
        _session: &SessionRef,
        _source_path: Option<&Path>,
    ) -> Result<Option<PathBuf>> {
        Ok(None)
    }

    /// Reads history for indexing within a full-read byte budget.
    ///
    /// Shared-database adapters measure the selected session's content instead of the database.
    ///
    /// # Errors
    ///
    /// Returns provider read failures.
    fn index_session(
        &self,
        session: &SessionRef,
        source_path: Option<&Path>,
        full_read_bytes: u64,
    ) -> Result<IndexRead> {
        let over_budget = source_path
            .and_then(|path| path.metadata().ok())
            .is_some_and(|metadata| metadata.is_file() && metadata.len() > full_read_bytes);
        read_budgeted_transcript(!over_budget, |preview| {
            if preview {
                self.preview_session_at(session, source_path)
            } else {
                self.read_session_at(session, source_path)
            }
        })
    }

    /// Reads the conversation for its search document. Discovery supplies separately stored session names.
    ///
    /// # Errors
    ///
    /// Returns provider read failures.
    fn index_search_document(
        &self,
        session: &SessionRef,
        source_path: Option<&Path>,
        full_read_bytes: u64,
    ) -> Result<IndexRead> {
        self.index_session(session, source_path, full_read_bytes)
    }

    /// Describes a fresh interactive provider launch.
    ///
    /// # Errors
    ///
    /// Returns an error when provider has no supported interactive launcher.
    fn new_session_plan(&self, target: &LaunchTarget) -> Result<LaunchPlan>;

    /// Describes a native resume or fork without executing provider command.
    ///
    /// # Errors
    ///
    /// Returns an error for mismatched providers or unsupported native launches.
    fn launch_plan(&self, session: &SessionRef, target: &LaunchTarget) -> Result<LaunchPlan>;

    /// Finds native children of a fork started in the specified workspace.
    ///
    /// Adapters must exclude the source and unrelated sessions. Callers link lineage only when
    /// exactly one candidate is returned. Providers without a verified discovery contract leave
    /// lineage unbound.
    ///
    /// # Errors
    ///
    /// Returns provider metadata read or validation failures.
    fn fork_candidates_created_since(
        &self,
        _source: &SessionRef,
        _project: &Path,
        _started_at: DateTime<Utc>,
    ) -> Result<Vec<SessionRef>> {
        Ok(Vec::new())
    }
}

#[derive(Default)]
pub struct AdapterRegistry {
    adapters: HashMap<Provider, Box<dyn ProviderAdapter>>,
}

impl AdapterRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with_local_adapters() -> Self {
        let mut registry = Self::new();
        registry.register(AntigravityAdapter::default());
        registry.register(AntigravityDesktopAdapter::default());
        registry.register(AntigravityIdeAdapter::default());
        registry.register(ClaudeAdapter::default());
        registry.register(CodexAdapter::default());
        registry.register(GrokAdapter::default());
        registry.register(HermesAdapter::default());
        registry.register(CursorCliAdapter::default());
        registry.register(CursorIdeAdapter::default());
        registry.register(OpenCodeAdapter::default());
        registry.register(PiAdapter::default());
        registry.register(PiAdapter::oh_my_pi());
        registry
    }

    pub fn register(&mut self, adapter: impl ProviderAdapter + 'static) {
        self.adapters.insert(adapter.provider(), Box::new(adapter));
    }

    #[must_use]
    pub fn get(&self, provider: Provider) -> Option<&dyn ProviderAdapter> {
        self.adapters.get(&provider).map(Box::as_ref)
    }

    /// Returns registered provider adapter.
    ///
    /// # Errors
    ///
    /// Returns an error when no adapter is registered for provider.
    pub fn adapter(&self, provider: Provider) -> Result<&dyn ProviderAdapter> {
        self.get(provider)
            .ok_or_else(|| anyhow!("no adapter registered for provider `{provider}`"))
    }

    /// Dispatches metadata discovery to provider adapter.
    ///
    /// # Errors
    ///
    /// Returns missing-adapter or provider discovery failures.
    pub fn list_sessions(
        &self,
        provider: Provider,
        project: Option<&Path>,
    ) -> Result<Vec<NativeSession>> {
        self.adapter(provider)?.list_sessions(project)
    }

    /// Dispatches metadata discovery and returns non-fatal adapter notes.
    ///
    /// # Errors
    ///
    /// Returns missing-adapter or provider discovery failures.
    pub fn list_sessions_with_notes(
        &self,
        provider: Provider,
        project: Option<&Path>,
    ) -> Result<(Vec<NativeSession>, Vec<String>)> {
        let adapter = self.adapter(provider)?;
        let sessions = adapter.list_sessions(project)?;
        Ok((sessions, adapter.discovery_notes()))
    }

    /// Dispatches an exact-ID metadata lookup to provider adapter.
    ///
    /// # Errors
    ///
    /// Returns missing-adapter or provider discovery failures.
    pub fn find_native_session(
        &self,
        provider: Provider,
        id: &str,
    ) -> Result<Option<NativeSession>> {
        self.adapter(provider)?.find_native_session(id)
    }

    /// Dispatches native session read to provider adapter.
    ///
    /// # Errors
    ///
    /// Returns missing-adapter or provider read failures.
    pub fn read_session(&self, session: &SessionRef) -> Result<CanonicalSnapshot> {
        self.adapter(session.provider)?.read_session(session)
    }

    /// Dispatches a native session read with an optional path from prior discovery.
    ///
    /// # Errors
    ///
    /// Returns missing-adapter or provider read failures.
    pub fn read_session_at(
        &self,
        session: &SessionRef,
        source_path: Option<&Path>,
    ) -> Result<CanonicalSnapshot> {
        self.adapter(session.provider)?
            .read_session_at(session, source_path)
    }

    /// Dispatches a bounded native session read for interactive previews.
    ///
    /// # Errors
    ///
    /// Returns missing-adapter or provider preview failures.
    pub fn preview_session(&self, session: &SessionRef) -> Result<CanonicalSnapshot> {
        self.adapter(session.provider)?.preview_session(session)
    }

    /// Dispatches a bounded native preview with an optional path from prior discovery.
    ///
    /// # Errors
    ///
    /// Returns missing-adapter or provider preview failures.
    pub fn preview_session_at(
        &self,
        session: &SessionRef,
        source_path: Option<&Path>,
    ) -> Result<CanonicalSnapshot> {
        self.adapter(session.provider)?
            .preview_session_at(session, source_path)
    }

    /// Reads a source and its indexing time. A file change during the read leaves the time unset.
    ///
    /// For adapters without a file clock, the reader supplies the provider's timestamp.
    ///
    /// # Errors
    ///
    /// Returns an error when the session cannot be resolved or read.
    pub fn observe_search_read<T>(
        &self,
        session: &SessionRef,
        source_path: Option<&Path>,
        read: impl FnOnce(&dyn ProviderAdapter, Option<&Path>) -> Result<(T, DateTime<Utc>)>,
    ) -> Result<(T, Option<DateTime<Utc>>)> {
        let adapter = self.adapter(session.provider)?;
        let Some(path) = adapter.search_timestamp_file(session, source_path)? else {
            let (value, time) = read(adapter, source_path)?;
            return Ok((value, Some(time)));
        };
        let before = support::SnapshotFileStamp::read(&path).ok();
        let (value, _) = read(adapter, Some(&path))?;
        let modified = before
            .filter(|stamp| support::SnapshotFileStamp::read(&path).as_ref().ok() == Some(stamp))
            .map(|stamp| DateTime::from(stamp.modified));
        Ok((value, modified))
    }

    /// Dispatches a budgeted search indexing read.
    ///
    /// # Errors
    ///
    /// Returns missing-adapter or provider read failures.
    pub fn index_session(
        &self,
        session: &SessionRef,
        source_path: Option<&Path>,
        full_read_bytes: u64,
    ) -> Result<IndexRead> {
        self.adapter(session.provider)?
            .index_session(session, source_path, full_read_bytes)
    }

    /// Dispatches native resume or fork planning to provider adapter.
    ///
    /// # Errors
    ///
    /// Returns missing-adapter or unsupported-launch failures.
    pub fn launch_plan(&self, session: &SessionRef, target: &LaunchTarget) -> Result<LaunchPlan> {
        self.adapter(session.provider)?.launch_plan(session, target)
    }

    /// Dispatches fresh-session planning to provider adapter.
    ///
    /// # Errors
    ///
    /// Returns missing-adapter or unsupported-launch failures.
    pub fn new_session_plan(
        &self,
        provider: Provider,
        target: &LaunchTarget,
    ) -> Result<LaunchPlan> {
        self.adapter(provider)?.new_session_plan(target)
    }
}
