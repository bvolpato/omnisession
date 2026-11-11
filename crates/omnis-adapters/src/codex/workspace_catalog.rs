use std::{
    collections::HashMap,
    fmt,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};

use omnis_ir::{Provider, SessionRef};
use omnis_store::{IndexedCodexWorkspace, IndexedSession, Store};

use super::{CodexSession, RolloutStamp};
use crate::NativeSession;

pub(super) struct WorkspaceCatalog {
    home: PathBuf,
    store: OnceLock<Option<Mutex<Store>>>,
}

impl fmt::Debug for WorkspaceCatalog {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkspaceCatalog")
            .finish_non_exhaustive()
    }
}

impl WorkspaceCatalog {
    pub(super) fn new(home: &Path) -> Option<Self> {
        Some(Self {
            home: std::fs::canonicalize(home).ok()?,
            store: OnceLock::new(),
        })
    }

    fn with_store<T>(&self, operation: impl FnOnce(&Store) -> T) -> Option<T> {
        let store = self
            .store
            .get_or_init(|| Store::open_default().ok().map(Mutex::new))
            .as_ref()?;
        let store = store.lock().ok()?;
        Some(operation(&store))
    }

    pub(super) fn list(&self) -> HashMap<String, IndexedCodexWorkspace> {
        self.with_store(|store| store.indexed_codex_workspaces(&self.home))
            .and_then(Result::ok)
            .unwrap_or_default()
    }

    pub(super) fn find(&self, id: &str) -> Option<IndexedCodexWorkspace> {
        self.with_store(|store| store.indexed_codex_workspace(&self.home, id))
            .and_then(Result::ok)
            .flatten()
    }

    #[cfg(unix)]
    pub(super) fn verified_workspace(&self, raw: &CodexSession) -> Option<IndexedCodexWorkspace> {
        let observation = self.find(&raw.id)?;
        let current = RolloutStamp::read(&raw.path);
        Self::matches(raw, &observation, current).then_some(observation)
    }

    fn matches(
        raw: &CodexSession,
        observation: &IndexedCodexWorkspace,
        current: Option<RolloutStamp>,
    ) -> bool {
        observation.source_path.as_os_str() == raw.path.as_os_str()
            && current.is_some_and(|stamp| {
                Some(stamp) == raw.stamp && stamp.catalog_key(&raw.path) == observation.source_stamp
            })
    }

    pub(super) fn apply(
        &self,
        raw: &CodexSession,
        session: &mut NativeSession,
        observation: &IndexedCodexWorkspace,
    ) {
        let current = RolloutStamp::read(&raw.path);
        if Self::matches(raw, observation, current) {
            session.project_path.clone_from(&observation.project_path);
            session.git_branch.clone_from(&observation.git_branch);
        } else if current.is_some() && current == raw.stamp {
            let _ = self.with_store(|store| {
                store.forget_codex_workspace(&self.home, &raw.id, &observation.source_stamp)
            });
        }
    }

    pub(super) fn record(
        &self,
        raw: &CodexSession,
        directory: Option<&Path>,
        branch: Option<&str>,
    ) {
        let Some(stamp) = raw.stamp else { return };
        if raw.is_subagent {
            return;
        }
        let _ = self.with_store(|store| {
            if RolloutStamp::read(&raw.path) != Some(stamp) {
                return;
            }
            let metadata = IndexedSession {
                session: SessionRef::new(Provider::Codex, raw.id.clone()),
                title: raw.title.as_deref().map(omnis_core::redact_secrets),
                project_path: directory.map(Path::to_path_buf),
                git_branch: branch.map(str::to_owned),
                created_at: raw.created_at,
                updated_at: raw.updated_at,
                updated_at_approximate: false,
                event_count: 0,
            };
            let key = stamp.catalog_key(&raw.path);
            if store
                .record_codex_workspace(&metadata, &self.home, &raw.path, &key)
                .is_ok()
                && RolloutStamp::read(&raw.path) != Some(stamp)
            {
                let _ = store.forget_codex_workspace(&self.home, &raw.id, &key);
            }
        });
    }
}
