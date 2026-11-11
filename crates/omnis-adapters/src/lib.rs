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
mod sqlite_wal;
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
    OpenCodeAdapter, canonicalize_opencode_export, fork_opencode_session_with_binary,
    installed_opencode_model, installed_opencode_model_with_binary, is_opencode_v2,
    opencode_command_args, read_opencode_session_with_binary, read_opencode_session_with_binary_at,
};
pub use pi::PiAdapter;
pub use provider_process::provider_process;
pub use source_file::source_file_revision;
pub use sqlite_wal::UnverifiedSqliteWal;
pub use support::IncompleteTranscript;

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
    pub source_revision: Option<SourceRevision>,
}

/// Native catalog rows and the metadata quality observed during the same read.
#[derive(Default)]
pub struct NativeListing {
    pub sessions: Vec<NativeSession>,
    pub notes: Vec<String>,
    pub desktop_summaries_unavailable: bool,
}

/// A durable source revision and the native path metadata that verified this read.
#[derive(Debug)]
pub struct SearchSourceObservation {
    pub revision: SourceRevision,
    pub session: NativeSession,
    /// The desktop database was readable, but its optional summary metadata was unavailable.
    pub desktop_summaries_unavailable: bool,
    /// Canonical native home verified together with this session.
    pub home: PathBuf,
    selected_home: PathBuf,
    paths: Vec<(PathBuf, Option<support::SnapshotFileStamp>)>,
}

impl SearchSourceObservation {
    fn capture(
        root: &Path,
        paths: Vec<PathBuf>,
        read: impl FnOnce() -> Result<NativeSession>,
    ) -> Result<Self> {
        let home = root.canonicalize()?;
        let paths = paths
            .into_iter()
            .map(|path| {
                let stamp = optional_stamp(&path)?;
                Ok((path, stamp))
            })
            .collect::<Result<_>>()?;
        let session = read()?;
        let revision = session
            .source_revision
            .ok_or_else(|| anyhow!("provider source revision is unavailable"))?;
        let observed = Self {
            revision,
            session,
            desktop_summaries_unavailable: false,
            home,
            selected_home: root.to_path_buf(),
            paths,
        };
        anyhow::ensure!(
            observed.is_current(),
            "provider source changed during search read"
        );
        Ok(observed)
    }

    /// Checks the selected source before publishing a captured document or failure.
    #[must_use]
    pub fn is_current(&self) -> bool {
        self.selected_home
            .canonicalize()
            .is_ok_and(|home| home == self.home)
            && self.paths.iter().all(|(path, before)| {
                optional_stamp(path).is_ok_and(|after| after.as_ref() == before.as_ref())
            })
    }
}

fn optional_stamp(path: &Path) -> std::io::Result<Option<support::SnapshotFileStamp>> {
    match support::SnapshotFileStamp::read(path) {
        Ok(stamp) => Ok(Some(stamp)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // An unresolved link cannot verify that a companion file is absent.
            match std::fs::symlink_metadata(path) {
                Ok(_) => Err(error),
                Err(missing) if missing.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(other) => Err(other),
            }
        }
        Err(error) => Err(error),
    }
}

/// A search indexing read and whether it covered the full provider source.
pub struct IndexRead {
    pub snapshot: CanonicalSnapshot,
    pub source_complete: bool,
    /// A provider limit made a full read impossible, even without an indexer byte budget.
    pub provider_limited: bool,
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

pub trait ProviderAdapter: Send + Sync {
    fn provider(&self) -> Provider;

    fn probe(&self) -> ProviderInstallation;

    /// Returns the canonical home whose native catalog this adapter uses for discovery.
    ///
    /// # Errors
    ///
    /// Returns an error if the configured home cannot be resolved.
    fn discovery_cache_home(&self) -> std::io::Result<Option<PathBuf>> {
        Ok(None)
    }

    /// Lists native session metadata, optionally filtered by exact project path.
    ///
    /// # Errors
    ///
    /// Returns provider discovery or data-read failures.
    fn list_sessions(&self, project: Option<&Path>) -> Result<Vec<NativeSession>>;

    /// Returns the session listing with its non-fatal observations.
    ///
    /// # Errors
    ///
    /// Returns provider discovery or data-read failures.
    fn list_catalog(&self, project: Option<&Path>) -> Result<NativeListing> {
        Ok(NativeListing {
            sessions: self.list_sessions(project)?,
            notes: self.discovery_notes(),
            ..NativeListing::default()
        })
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

    /// Returns the source time used by discovery for the data that produced this snapshot.
    /// An adapter with no recorded time can use the Unix epoch as a stable search placeholder.
    ///
    /// # Errors
    ///
    /// Returns an error if the selected source can no longer be verified.
    fn search_source_time(
        &self,
        snapshot: &CanonicalSnapshot,
        _source_path: Option<&Path>,
    ) -> Result<DateTime<Utc>> {
        Ok(snapshot.captured_at)
    }

    /// Reads a snapshot and the provider's update time for that read.
    ///
    /// # Errors
    ///
    /// Returns an error if the provider cannot read the session or verify its update time.
    fn read_for_search(
        &self,
        session: &SessionRef,
        source_path: Option<&Path>,
        full_read: bool,
    ) -> Result<(CanonicalSnapshot, DateTime<Utc>)> {
        let snapshot = if full_read {
            self.read_session_at(session, source_path)?
        } else {
            self.preview_session_at(session, source_path)?
        };
        let time = self.search_source_time(&snapshot, source_path)?;
        Ok((snapshot, time))
    }

    /// Reads the search projection within its budget and records the update time of that source.
    ///
    /// # Errors
    ///
    /// Returns an error if the provider cannot read the source or verify its update time.
    fn index_for_search(
        &self,
        session: &SessionRef,
        source_path: Option<&Path>,
        full_read_bytes: u64,
    ) -> Result<(IndexRead, DateTime<Utc>)> {
        let read = self.index_session(session, source_path, full_read_bytes)?;
        let time = self.search_source_time(&read.snapshot, source_path)?;
        Ok((read, time))
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

    /// Returns the selected source files and directories when a search read uses several paths.
    /// The registry checks their identities before and after reading.
    ///
    /// # Errors
    ///
    /// Returns an error if the selected source cannot be resolved.
    fn search_source_files(
        &self,
        _session: &SessionRef,
        _source_path: Option<&Path>,
    ) -> Result<Vec<PathBuf>> {
        Ok(Vec::new())
    }

    /// Observes the provider files selected for this search read.
    ///
    /// # Errors
    ///
    /// Returns an error when the provider cannot verify the selected source.
    fn observe_search_source(
        &self,
        _session: &SessionRef,
        _source_path: Option<&Path>,
    ) -> Result<Option<SearchSourceObservation>> {
        Ok(None)
    }

    /// Returns whether this read covers an empty session recorded only in the native catalog.
    ///
    /// # Errors
    ///
    /// Returns an error if the catalog entry or its selected source changed.
    fn search_source_is_summary(
        &self,
        _session: &SessionRef,
        _source_path: Option<&Path>,
    ) -> Result<bool> {
        Ok(false)
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
            .is_some_and(|metadata| metadata.is_file() && metadata.len() > full_read_bytes)
            && !self.search_source_is_summary(session, source_path)?;
        if !over_budget {
            match self.read_session_at(session, source_path) {
                Ok(snapshot) => {
                    return Ok(IndexRead {
                        snapshot,
                        source_complete: true,
                        provider_limited: false,
                    });
                }
                Err(error) if error.is::<support::TranscriptReadLimit>() => {}
                Err(error) => return Err(error),
            }
        }
        Ok(IndexRead {
            snapshot: self.preview_session_at(session, source_path)?,
            source_complete: false,
            provider_limited: !over_budget,
        })
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
        let listing = self.adapter(provider)?.list_catalog(project)?;
        Ok((listing.sessions, listing.notes))
    }

    /// Returns the selected native home for providers with home-specific catalog caches.
    ///
    /// # Errors
    ///
    /// Returns the resolution error for an inaccessible or invalid native home.
    pub fn discovery_cache_home(&self, provider: Provider) -> Result<Option<PathBuf>> {
        Ok(self.adapter(provider)?.discovery_cache_home()?)
    }

    /// Reads catalog metadata from the selected native home.
    ///
    /// # Errors
    ///
    /// Returns a discovery error if the provider home changed or the listing failed.
    pub fn list_sessions_in_home(
        &self,
        provider: Provider,
        project: Option<&Path>,
        home: Option<&Path>,
    ) -> Result<NativeListing> {
        let adapter = self.adapter(provider)?;
        anyhow::ensure!(
            adapter.discovery_cache_home()?.as_deref() == home,
            "provider home changed during discovery"
        );
        let listing = adapter.list_catalog(project)?;
        anyhow::ensure!(
            adapter.discovery_cache_home()?.as_deref() == home,
            "provider home changed during discovery"
        );
        Ok(listing)
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
            let stamps = || -> Result<Vec<_>> {
                adapter
                    .search_source_files(session, source_path)?
                    .into_iter()
                    .map(|path| {
                        let stamp = optional_stamp(&path)?;
                        Ok((path, stamp))
                    })
                    .collect()
            };
            let before = stamps()?;
            let (value, time) = read(adapter, source_path)?;
            return Ok((
                value,
                stamps().is_ok_and(|after| after == before).then_some(time),
            ));
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
