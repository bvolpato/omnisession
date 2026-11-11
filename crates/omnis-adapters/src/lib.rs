//! Read-only provider discovery and canonicalization.

mod antigravity;
mod antigravity_desktop;
mod antigravity_ide;
mod claude;
mod codex;
mod cursor;
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
    collections::HashMap,
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
pub use provider_process::{bounded_provider_output, provider_process};
pub use source_file::source_file_revision;
pub use support::TranscriptReadLimit;
pub use support::{IncompleteTranscript, MAX_STREAMED_TRANSCRIPT_FILE_SIZE};

/// Reads the revision of the file selected for this session in the configured provider store.
#[must_use]
pub fn selected_search_file_revision(session: &SessionRef, path: &Path) -> Option<SourceRevision> {
    if session.provider == Provider::Antigravity {
        return antigravity::selected_transcript_revision(session, path);
    }
    source_file_revision(path)
}

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

/// A search indexing read and whether it covered the full provider source.
pub struct IndexRead {
    /// Catalog metadata verified by the reader of the selected native transcript.
    pub native_session: Option<NativeSession>,
    pub snapshot: CanonicalSnapshot,
    pub source_complete: bool,
    /// A provider limit made a full read impossible, even without an indexer byte budget.
    pub provider_limited: bool,
    /// The source revision observed while reading the selected native transcript.
    pub source_revision: SourceRevision,
}

/// The searchable result of a provider read, including its source time and coverage.
pub struct IndexedSearchDocument {
    pub document: omnis_core::SearchDocument,
    /// Native metadata and the visible message used to derive a search title.
    pub read: IndexRead,
}

impl From<IndexRead> for IndexedSearchDocument {
    fn from(read: IndexRead) -> Self {
        Self {
            document: omnis_core::trajectory_search_document(&read.snapshot),
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

/// Command description. Callers decide whether and how to execute it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LaunchPlan {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    /// Changes to the inherited environment, applied in order. Built-in adapters leave it empty.
    /// A launch profile sets it.
    pub env: Vec<EnvChange>,
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
        snapshot: read(true)?,
        source_complete: false,
        provider_limited: within_budget,
        source_revision: SourceRevision::default(),
    })
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
        _prior_search_bytes: Option<usize>,
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
