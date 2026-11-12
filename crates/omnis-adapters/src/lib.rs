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
use omnis_ir::{CanonicalSnapshot, GitState, Provider, SessionRef, SourceRevision};

pub use antigravity::AntigravityAdapter;
pub use antigravity_desktop::AntigravityDesktopAdapter;
pub use antigravity_ide::AntigravityIdeAdapter;
pub use claude::ClaudeAdapter;
pub use codex::CodexAdapter;
pub use cursor::{CursorCliAdapter, CursorIdeAdapter};
pub use cursor_identity::{
    cursor_workspace_id, is_cursor_workspace_id, validate_cursor_workspace_identity,
};
pub use grok::{GrokAdapter, GrokReadRejection};
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
pub use support::{IncompleteTranscript, MAX_STREAMED_TRANSCRIPT_FILE_SIZE};

/// Installation state discovered without reading provider credentials.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderInstallation {
    pub provider: Provider,
    pub installed: bool,
    pub executable: Option<PathBuf>,
    pub data_root: Option<PathBuf>,
}

pub use omnis_core::NativeSessionName;

/// Name assignments that still agree with the native index used for a listing.
pub struct ObservedSessionNames {
    /// Confirmed assignments, including readable rows after the last damaged row.
    pub names: std::sync::Arc<HashMap<String, String>>,
    /// Whether an omitted ID confirms that the thread has no assignment.
    pub complete: bool,
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
    /// The source is an ordinary local session. Its opening title remains unconfirmed.
    UnknownLocal,
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
            RollbackTitle::UnknownLocal
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

/// A canonical snapshot and the source state observed while building it.
pub struct IndexRead {
    /// Physical ancestor rollout IDs and the inventory revisions observed during this projection.
    pub ancestor_revisions: Vec<(uuid::Uuid, uuid::Uuid)>,
    /// Physical file whose ownership was checked independently of display metadata.
    pub ownership_revision: Option<SourceRevision>,
    pub database_aliases: std::sync::Arc<omnis_ir::antigravity_receipts::DatabaseAliases>,
    /// Exclusive event sequence at the first gap in an otherwise sequential read.
    pub verified_prefix_end: Option<u64>,
    /// Sequence ranges for native records that produced multiple canonical events.
    pub search_record_groups: Vec<std::ops::Range<u64>>,
    /// Catalog metadata verified by the reader of the selected native transcript.
    pub native_session: Option<NativeSession>,
    pub snapshot: CanonicalSnapshot,
    pub rollback_title: RollbackTitle,
    /// The workspace projection belongs to this read's source revision.
    pub workspace_projection: CodexWorkspaceProjection,
    /// A provider limit made a full read impossible, even without an indexer byte budget.
    pub provider_limited: bool,
    /// The source revision observed while reading the selected native transcript.
    pub source_revision: SourceRevision,
    /// True when the read retained all available canonical events. The snapshot can still report missing history.
    pub source_complete: bool,
}

/// Reads a session with the source selection shared by one indexing pass.
pub type IndexSessionReader<'a> =
    dyn Fn(&SessionRef, Option<&Path>, u64) -> Result<IndexRead> + Send + Sync + 'a;

/// Search text previously indexed from the same physical transcript.
#[derive(Clone, Copy)]
pub enum PriorSearchDocument {
    RequireCompleteTail(usize),
    Complete {
        byte_count: usize,
        indexed_hash: [u8; 32],
    },
    Partial {
        byte_count: usize,
        indexed_hash: [u8; 32],
        sampled_file_bytes: Option<u64>,
        /// An older partial index lacks the location of its sampled windows.
        recover_from_full_scan: bool,
    },
}

/// The searchable result of a provider read, including its source time and coverage.
pub struct IndexedSearchDocument {
    pub document: omnis_core::SearchDocument,
    /// Native metadata and the visible message used to derive a search title.
    pub read: IndexRead,
}

impl IndexRead {
    #[must_use]
    pub fn search_document(&self) -> omnis_core::SearchDocument {
        let mut document = omnis_core::trajectory_search_document_in_records(
            &self.snapshot,
            &self.search_record_groups,
        );
        if let Some(projection) = document.projection.as_mut() {
            projection.database_aliases = std::sync::Arc::clone(&self.database_aliases);
        }
        document
    }
}

impl From<IndexRead> for IndexedSearchDocument {
    fn from(read: IndexRead) -> Self {
        Self {
            document: read.search_document(),
            read,
        }
    }
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

/// Workspace evidence for a session whose active directory can change between turns.
pub struct NativeWorkspace {
    pub directory: Option<PathBuf>,
    /// Directory the native client can safely infer when it opens the session.
    pub startup_directory: Option<PathBuf>,
    pub git: GitState,
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
                    ancestor_revisions: Vec::new(),
                    ownership_revision: None,
                    verified_prefix_end: None,
                    database_aliases: std::sync::Arc::default(),
                    search_record_groups: Vec::new(),
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
        ancestor_revisions: Vec::new(),
        ownership_revision: None,
        verified_prefix_end: None,
        database_aliases: std::sync::Arc::default(),
        search_record_groups: Vec::new(),
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

    /// Reads the effective workspace when it can differ from the discovery metadata.
    ///
    /// # Errors
    ///
    /// Returns an error when the provider cannot determine the effective directory.
    fn native_workspace(&self, _session: &SessionRef) -> Result<Option<NativeWorkspace>> {
        Ok(None)
    }

    /// Describes whether the provider can continue the stored representation by its session ID.
    ///
    /// # Errors
    ///
    /// Returns an error if the adapter cannot identify the source representation.
    fn native_resume_support(&self, _session: &SessionRef) -> Result<NativeResumeSupport> {
        Ok(NativeResumeSupport::Direct)
    }

    /// IDs with a conflicting or unreadable native source in the last discovery pass.
    fn unavailable_session_ids(&self) -> Vec<String> {
        Vec::new()
    }

    /// Resolves the local Codex database directory for a continuation. An existing ID must select the displayed session there.
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

    /// Returns the canonical home whose native catalog this adapter uses for discovery.
    ///
    /// # Errors
    ///
    /// Returns an error if the configured home cannot be resolved.
    fn discovery_cache_home(&self) -> std::io::Result<Option<PathBuf>> {
        Ok(None)
    }

    /// Whether the last listing observed every eligible session across all projects.
    ///
    /// Return false for bounded, incomplete, or structurally unverified listings. An omitted
    /// session can be treated as deleted only after complete discovery. The default preserves it.
    fn discovery_is_complete(&self) -> bool {
        false
    }

    /// Returns whether a missing provider name confirms that the session has no name.
    fn session_titles_are_complete(&self) -> bool {
        false
    }

    /// Reads selected name observations for a batch of session IDs, paths, and file revisions.
    fn listed_session_names(
        &self,
        _sessions: &[(&str, &Path, SourceRevision)],
    ) -> HashMap<String, NativeSessionName> {
        HashMap::new()
    }

    /// Rechecks the named rows selected by discovery before their search documents are published.
    fn listed_session_names_still_current(
        &self,
        _sessions: &[(&str, &Path, SourceRevision)],
    ) -> HashMap<String, NativeSessionName> {
        HashMap::new()
    }

    /// Returns name assignments and clears confirmed for the selected paths and file revisions.
    fn observed_listed_names(
        &self,
        sessions: &[NativeSession],
    ) -> HashMap<String, NativeSessionName> {
        let requested: Vec<_> = sessions
            .iter()
            .filter(|s| s.session.provider == Provider::Codex)
            .filter_map(|s| {
                Some((
                    s.session.id.as_str(),
                    s.source_path.as_deref()?,
                    s.source_revision?,
                ))
            })
            .collect();
        let mut names = self.listed_session_names(&requested);
        sessions
            .iter()
            .filter(|session| session.session.provider == Provider::Codex)
            .filter_map(|session| {
                let id = &session.session.id;
                let name = names.remove(id)?;
                name.confirms(session.title.as_deref())
                    .then(|| (id.clone(), name))
            })
            .collect()
    }

    /// Returns GUI assignments and clears that match the selected sessions.
    fn gui_session_names(&self, sessions: &[NativeSession]) -> HashMap<String, Option<String>> {
        self.observed_listed_names(sessions)
            .into_iter()
            .filter_map(|(id, name)| {
                if let NativeSessionName::Gui(title) = name {
                    Some((id, title))
                } else {
                    None
                }
            })
            .collect()
    }

    /// Returns the individually confirmed assignments in the name index used for discovery.
    /// Only a complete index also confirms that omitted IDs have no assignment.
    fn observed_session_names(&self) -> Option<ObservedSessionNames> {
        None
    }

    /// Reads the latest name assignment for one session.
    fn native_session_name(&self, _id: &str) -> NativeSessionName {
        NativeSessionName::Unknown
    }

    /// Returns earlier readable assignments that can corroborate a saved name for the same rollout.
    /// An unreadable later row prevents these assignments from establishing the current name.
    fn prior_session_names(&self, _sessions: &[&NativeSession]) -> HashMap<String, String> {
        HashMap::new()
    }

    /// Checks whether the name still matches the provider's index after a session read.
    fn confirms_session_title(&self, id: &str, name: Option<&str>) -> bool {
        self.native_session_name(id).confirms(name)
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
            ancestor_revisions: Vec::new(),
            ownership_revision: None,
            verified_prefix_end: None,
            database_aliases: std::sync::Arc::default(),
            search_record_groups: Vec::new(),
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

    /// Prepares one reader for the given native session IDs. An adapter can pin its selected
    /// source paths for this pass. The caller drops the reader before the next pass.
    fn index_batch(&self, _ids: &[&str]) -> Option<Box<IndexSessionReader<'_>>> {
        None
    }

    /// Prepares a batch reader for search. Discovery supplies names for the listed sessions.
    fn index_search_batch(
        &self,
        ids: &[&str],
        _listed: &HashSet<&str>,
    ) -> Option<Box<IndexSessionReader<'_>>> {
        self.index_batch(ids)
    }

    fn apply_verified_workspace(
        &self,
        _session: &mut NativeSession,
        _index: &omnis_store::CodexWorkspaceIndex,
    ) -> bool {
        false
    }

    /// Observes the current physical copies of each ancestor used to index a descendant.
    fn index_ancestor_revisions(
        &self,
        _ids: &HashSet<uuid::Uuid>,
    ) -> HashMap<uuid::Uuid, uuid::Uuid> {
        HashMap::new()
    }

    /// Finds previously missing sources that can now contribute to a new index read.
    fn available_index_sources(&self, _ids: &HashSet<uuid::Uuid>) -> HashSet<uuid::Uuid> {
        HashSet::new()
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

    /// Builds the local search document with the provider's full-read or preview budget.
    ///
    /// # Errors
    ///
    /// Returns provider read failures.
    fn index_search_document(
        &self,
        session: &SessionRef,
        source_path: Option<&Path>,
        full_read_bytes: u64,
        _prior: Option<PriorSearchDocument>,
    ) -> Result<IndexedSearchDocument> {
        self.index_session(session, source_path, full_read_bytes)
            .map(Into::into)
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

    /// Returns the selected native home for providers with home-specific catalog caches.
    ///
    /// # Errors
    ///
    /// Returns the resolution error for an inaccessible or invalid native home.
    pub fn discovery_cache_home(&self, provider: Provider) -> Result<Option<PathBuf>> {
        Ok(self.adapter(provider)?.discovery_cache_home()?)
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
