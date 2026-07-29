//! Read-only provider discovery and canonicalization.

mod antigravity;
mod claude;
mod codex;
mod cursor;
mod grok;
mod opencode;
mod pi;
mod support;

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use anyhow::{Result, anyhow};
use chrono::{DateTime, Utc};
use omnis_ir::{CanonicalSnapshot, Provider, SessionRef};

pub use antigravity::AntigravityAdapter;
pub use claude::ClaudeAdapter;
pub use codex::CodexAdapter;
pub use cursor::{CursorCliAdapter, CursorIdeAdapter};
pub use grok::GrokAdapter;
pub use opencode::{
    OpenCodeAdapter, canonicalize_opencode_export, installed_opencode_model,
    installed_opencode_model_with_binary, read_opencode_session_with_binary,
    read_opencode_session_with_binary_at,
};
pub use pi::PiAdapter;

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
    pub event_count: usize,
    pub source_path: Option<PathBuf>,
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

/// Command description. Callers decide whether and how to execute it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LaunchPlan {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
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

    /// Reads one provider-native session into canonical events.
    ///
    /// # Errors
    ///
    /// Returns an error for mismatched providers, missing sessions, or unreadable data.
    fn read_session(&self, session: &SessionRef) -> Result<CanonicalSnapshot>;

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
        registry.register(ClaudeAdapter::default());
        registry.register(CodexAdapter::default());
        registry.register(GrokAdapter::default());
        registry.register(CursorCliAdapter::default());
        registry.register(CursorIdeAdapter::default());
        registry.register(OpenCodeAdapter);
        registry.register(PiAdapter::default());
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

    /// Dispatches native session read to provider adapter.
    ///
    /// # Errors
    ///
    /// Returns missing-adapter or provider read failures.
    pub fn read_session(&self, session: &SessionRef) -> Result<CanonicalSnapshot> {
        self.adapter(session.provider)?.read_session(session)
    }

    /// Dispatches a bounded native session read for interactive previews.
    ///
    /// # Errors
    ///
    /// Returns missing-adapter or provider preview failures.
    pub fn preview_session(&self, session: &SessionRef) -> Result<CanonicalSnapshot> {
        self.adapter(session.provider)?.preview_session(session)
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
