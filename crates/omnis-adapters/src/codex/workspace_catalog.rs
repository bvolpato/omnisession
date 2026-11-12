use std::{
    collections::HashMap,
    fmt,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};

use omnis_ir::{Provider, SessionRef};
use omnis_store::{IndexedCodexWorkspace, IndexedSession, Store};
use sha2::{Digest, Sha256};

use super::{CodexAdapter, CodexSession, RolloutStamp};
use crate::{NativeSession, support::paths_match};

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

    pub(super) fn indexed_sessions(&self) -> HashMap<String, IndexedSession> {
        self.with_store(|store| store.indexed_sessions_for_provider(Provider::Codex))
            .and_then(Result::ok)
            .unwrap_or_default()
            .into_iter()
            .map(|row| (row.session.id.clone(), row))
            .collect()
    }

    pub(super) fn refresh_replaced(
        &self,
        adapter: &CodexAdapter,
        raw: &CodexSession,
        listed: &NativeSession,
        previous: &IndexedSession,
    ) {
        let Some((old, selected)) = previous.source_revision.zip(raw.observed_revision) else {
            return;
        };
        if old.file.is_none() || old.contents.is_none() || old.matches_file_contents(selected) {
            return;
        }
        let metadata =
            raw.indexed_workspace(listed.project_path.as_deref(), listed.git_branch.as_deref());
        let _ = self.with_store(|store| {
            store.refresh_replaced_codex_session(previous, &metadata, || {
                adapter
                    .selected_rollout(&raw.id, Some(&raw.path))
                    .is_ok_and(|chosen| {
                        chosen.is_some_and(|chosen| {
                            chosen.session.path == raw.path
                                && chosen.session.observed_revision == Some(selected)
                        })
                    })
            })
        });
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
                Some(stamp) == raw.stamp
                    && Self::source_key(
                        stamp,
                        &raw.path,
                        observation.project_path.as_deref(),
                        observation.git_branch.as_deref(),
                    )
                    .as_ref()
                        == Some(&observation.source_stamp)
            })
    }

    #[cfg(unix)]
    fn unchanged_unknown(
        raw: &CodexSession,
        observation: &IndexedCodexWorkspace,
        current: Option<RolloutStamp>,
    ) -> bool {
        let Some(stamp) = current else { return false };
        if observation.project_path.is_some()
            || raw.stamp != current
            || observation.source_path.as_os_str() != raw.path.as_os_str()
        {
            return false;
        }
        // A former parser can have recorded an unresolved location for this same physical file.
        let key = stamp.catalog_key(&raw.path);
        key.split_once('/')
            .zip(observation.source_stamp.split_once('/'))
            .is_some_and(|((_, current), (_, previous))| current == previous)
    }

    #[cfg_attr(
        not(windows),
        allow(
            clippy::unnecessary_wraps,
            reason = "Only Windows needs to open a handle to read the file identity."
        )
    )]
    fn source_key(
        stamp: RolloutStamp,
        path: &Path,
        directory: Option<&Path>,
        branch: Option<&str>,
    ) -> Option<String> {
        let key = stamp.catalog_key(path);
        if cfg!(unix) {
            return Some(key);
        }
        #[cfg(windows)]
        let key = {
            let handle = std::fs::File::open(path).ok()?;
            match winapi_util::file::information(&handle) {
                Ok(info) if info.file_index() != 0 => format!(
                    "{key}/{}/{}",
                    info.volume_serial_number(),
                    info.file_index()
                ),
                _ => format!("{key}/unknown-file-id"),
            }
        };
        let component = |value: Option<&[u8]>| {
            value.map_or_else(
                || "-".to_owned(),
                |bytes| hex::encode(Sha256::digest(bytes)),
            )
        };
        Some(format!(
            "{key}/{}/{}",
            component(directory.map(|path| path.as_os_str().as_encoded_bytes())),
            component(branch.map(str::as_bytes))
        ))
    }

    pub(super) fn apply(
        &self,
        raw: &CodexSession,
        session: &mut NativeSession,
        observation: &IndexedCodexWorkspace,
        project: Option<&Path>,
    ) {
        // A preferred copy can disappear and make this exact archived document current again.
        if observation.source_path.as_os_str() != raw.path.as_os_str() {
            return;
        }
        if !cfg!(unix)
            && project.is_some_and(|project| {
                ![
                    session.project_path.as_deref(),
                    observation.project_path.as_deref(),
                ]
                .into_iter()
                .flatten()
                .any(|path| paths_match(path, project))
            })
        {
            return;
        }
        let current = RolloutStamp::read(&raw.path);
        if current.is_none() || current != raw.stamp {
            return;
        }
        if cfg!(unix) {
            if Self::matches(raw, observation, current) {
                session.project_path.clone_from(&observation.project_path);
                session.git_branch.clone_from(&observation.git_branch);
                return;
            }
            #[cfg(unix)]
            if Self::unchanged_unknown(raw, observation, current) {
                session.project_path = None;
                session.git_branch = None;
                return;
            }
        } else {
            // A size and a restored write time do not identify an in-place rewrite.
            let Ok(live) = raw.live_workspace() else {
                session.project_path = None;
                session.git_branch = None;
                return;
            };
            session.project_path = live.directory;
            session.git_branch = live.git.branch;
            if session.project_path != observation.project_path
                || session.git_branch != observation.git_branch
                || !Self::matches(raw, observation, current)
            {
                self.record(
                    raw,
                    session.project_path.as_deref(),
                    session.git_branch.as_deref(),
                );
            }
            return;
        }
        let _ = self.with_store(|store| {
            store.forget_codex_workspace(&self.home, &raw.id, &observation.source_stamp)
        });
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
            let metadata = raw.indexed_workspace(directory, branch);
            let Some(key) = Self::source_key(stamp, &raw.path, directory, branch) else {
                return;
            };
            if store
                .record_codex_workspace(&metadata, &self.home, &raw.path, &key)
                .is_ok()
                && (RolloutStamp::read(&raw.path) != Some(stamp)
                    || (!cfg!(unix)
                        && Self::source_key(stamp, &raw.path, directory, branch).as_ref()
                            != Some(&key)))
            {
                let _ = store.forget_codex_workspace(&self.home, &raw.id, &key);
            }
        });
    }
}

impl CodexSession {
    fn indexed_workspace(&self, directory: Option<&Path>, branch: Option<&str>) -> IndexedSession {
        let millis = |time: Option<chrono::DateTime<chrono::Utc>>| {
            time.and_then(|time| chrono::DateTime::from_timestamp_millis(time.timestamp_millis()))
        };
        IndexedSession {
            source_path: None,
            session: SessionRef::new(Provider::Codex, self.id.clone()),
            source_revision: self.observed_revision,
            title: self.title.as_deref().map(omnis_core::redact_secrets),
            project_path: directory.map(Path::to_path_buf),
            git_branch: branch.map(str::to_owned),
            created_at: millis(self.created_at),
            updated_at: millis(self.updated_at),
            updated_at_approximate: false,
            event_count: 0,
        }
    }
}
