//! Local `SQLite` persistence for task selection, session lineage, and bundles.

use std::{
    cell::RefCell,
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use chrono::{DateTime, Utc};
use directories::BaseDirs;
use omnis_ir::{PortableBundle, Provider, SessionRef, TransferMode};
use rusqlite::{
    Connection, OptionalExtension, Transaction, TransactionBehavior, params, types::Type,
};
use serde_json::Value;
use thiserror::Error;
use uuid::Uuid;

const DATABASE_FILE_NAME: &str = "store.sqlite3";

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("database operation failed")]
    Database,
    #[error("default data directory is unavailable")]
    DefaultDirectoryUnavailable,
    #[error("invalid task name")]
    InvalidTaskName,
    #[error("invalid branch name")]
    InvalidBranchName,
    #[error("invalid session reference")]
    InvalidSessionReference,
    #[error("workspace root must have a lossless UTF-8 representation")]
    InvalidWorkspaceRoot,
    #[error("task not found")]
    TaskNotFound,
    #[error("stored data is invalid")]
    CorruptStore,
    #[error("bundle encoding failed")]
    BundleEncoding,
    #[error("bundle already exists")]
    BundleAlreadyExists,
}

pub type Result<T> = std::result::Result<T, StoreError>;

/// A task scoped to one workspace root.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskRecord {
    pub id: i64,
    pub name: String,
    pub workspace_root: PathBuf,
    pub created_at: DateTime<Utc>,
}

/// One entry in a task branch's session lineage.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BindingRecord {
    pub id: i64,
    pub task_id: i64,
    pub branch_name: String,
    pub session: SessionRef,
    pub bound_at: DateTime<Utc>,
    pub is_current: bool,
}

/// A source-to-target session handoff, ordered by creation time in lineage views.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HandoffRecord {
    pub source: SessionRef,
    pub target: SessionRef,
    pub mode: TransferMode,
    pub created_at: DateTime<Utc>,
}

/// Cached provider metadata used to render session discovery without transcript reads.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IndexedSession {
    pub session: SessionRef,
    pub title: Option<String>,
    pub project_path: Option<PathBuf>,
    pub git_branch: Option<String>,
    pub created_at: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
    pub event_count: usize,
}

/// `SQLite`-backed state for one local `OmniSession` installation.
pub struct Store {
    connection: RefCell<Connection>,
}

impl Store {
    /// Opens a store at `path`, creating its schema when needed.
    ///
    /// # Errors
    ///
    /// Returns an error when `SQLite` cannot open, configure, or initialize the store.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        reject_symlink(path)?;
        let connection = Connection::open(path).map_err(|_| StoreError::Database)?;
        connection
            .busy_timeout(Duration::from_secs(5))
            .map_err(|_| StoreError::Database)?;
        connection
            .pragma_update(None, "foreign_keys", "ON")
            .map_err(|_| StoreError::Database)?;
        connection
            .pragma_update(None, "journal_mode", "WAL")
            .map_err(|_| StoreError::Database)?;

        let store = Self {
            connection: RefCell::new(connection),
        };
        store.initialize_schema()?;
        set_private_file(path)?;
        for suffix in ["-wal", "-shm"] {
            let mut auxiliary = path.as_os_str().to_os_string();
            auxiliary.push(suffix);
            let auxiliary = PathBuf::from(auxiliary);
            if auxiliary.exists() {
                set_private_file(&auxiliary)?;
            }
        }
        Ok(store)
    }

    /// Opens `OMNISESSION_HOME` or `~/.omnisession` as state root.
    ///
    /// # Errors
    ///
    /// Returns an error when state-root creation, validation, or store initialization fails.
    pub fn open_default() -> Result<Self> {
        Self::open_state_root(&state_root()?)
    }

    /// Creates, selects, and optionally binds a task in one transaction.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid input or failed persistence.
    pub fn start_task(
        &self,
        name: impl AsRef<str>,
        workspace_root: impl AsRef<Path>,
        branch_name: impl AsRef<str>,
        session: Option<&SessionRef>,
    ) -> Result<TaskRecord> {
        let name = name.as_ref();
        let branch_name = branch_name.as_ref();
        if name.trim().is_empty() {
            return Err(StoreError::InvalidTaskName);
        }
        if branch_name.trim().is_empty() {
            return Err(StoreError::InvalidBranchName);
        }
        if let Some(session) = session {
            validate_session_ref(session)?;
        }
        let workspace_root = workspace_root_to_string(workspace_root.as_ref())?;
        let timestamp = now_timestamp();
        let mut connection = self.connection.borrow_mut();
        let transaction = immediate_transaction(&mut connection)?;
        transaction
            .execute(
                "INSERT INTO tasks (name, workspace_root, created_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT (workspace_root, name) DO NOTHING",
                params![name, workspace_root, timestamp],
            )
            .map_err(|_| StoreError::Database)?;
        let task =
            query_task(&transaction, &workspace_root, name)?.ok_or(StoreError::CorruptStore)?;
        transaction
            .execute(
                "INSERT INTO task_selections (workspace_root, task_id, selected_at)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT (workspace_root) DO UPDATE SET
                   task_id = excluded.task_id, selected_at = excluded.selected_at",
                params![workspace_root, task.id, timestamp],
            )
            .map_err(|_| StoreError::Database)?;
        if let Some(session) = session {
            replace_branch_head(&transaction, task.id, branch_name, session, timestamp)?;
        }
        transaction.commit().map_err(|_| StoreError::Database)?;
        Ok(task)
    }

    /// Creates a task or returns its existing record for this workspace.
    ///
    /// # Errors
    ///
    /// Returns an error for an empty task name or a failed database operation.
    pub fn create_or_get_task(
        &self,
        name: impl AsRef<str>,
        workspace_root: impl AsRef<Path>,
    ) -> Result<TaskRecord> {
        let name = name.as_ref();
        if name.trim().is_empty() {
            return Err(StoreError::InvalidTaskName);
        }
        let workspace_root = workspace_root_to_string(workspace_root.as_ref())?;
        let created_at = now_timestamp();
        let mut connection = self.connection.borrow_mut();
        let transaction = immediate_transaction(&mut connection)?;
        transaction
            .execute(
                "
                INSERT INTO tasks (name, workspace_root, created_at)
                VALUES (?1, ?2, ?3)
                ON CONFLICT (workspace_root, name) DO NOTHING
                ",
                params![name, workspace_root, created_at],
            )
            .map_err(|_| StoreError::Database)?;
        let task =
            query_task(&transaction, &workspace_root, name)?.ok_or(StoreError::CorruptStore)?;
        transaction.commit().map_err(|_| StoreError::Database)?;
        Ok(task)
    }

    /// Makes a workspace task selected and returns it.
    ///
    /// # Errors
    ///
    /// Returns an error when the named workspace task does not exist or persistence fails.
    pub fn select_task(
        &self,
        workspace_root: impl AsRef<Path>,
        name: impl AsRef<str>,
    ) -> Result<TaskRecord> {
        let workspace_root = workspace_root_to_string(workspace_root.as_ref())?;
        let name = name.as_ref();
        let selected_at = now_timestamp();
        let mut connection = self.connection.borrow_mut();
        let transaction = immediate_transaction(&mut connection)?;
        let task =
            query_task(&transaction, &workspace_root, name)?.ok_or(StoreError::TaskNotFound)?;
        transaction
            .execute(
                "
                INSERT INTO task_selections (workspace_root, task_id, selected_at)
                VALUES (?1, ?2, ?3)
                ON CONFLICT (workspace_root) DO UPDATE SET
                    task_id = excluded.task_id,
                    selected_at = excluded.selected_at
                ",
                params![workspace_root, task.id, selected_at],
            )
            .map_err(|_| StoreError::Database)?;
        transaction.commit().map_err(|_| StoreError::Database)?;
        Ok(task)
    }

    /// Returns the task currently selected for a workspace, if any.
    ///
    /// # Errors
    ///
    /// Returns an error when stored task data cannot be read.
    pub fn selected_task(&self, workspace_root: impl AsRef<Path>) -> Result<Option<TaskRecord>> {
        let workspace_root = workspace_root_to_string(workspace_root.as_ref())?;
        let connection = self.connection.borrow();
        connection
            .query_row(
                "
                SELECT t.id, t.name, t.workspace_root, t.created_at
                FROM task_selections AS s
                INNER JOIN tasks AS t ON t.id = s.task_id
                WHERE s.workspace_root = ?1
                ",
                params![workspace_root],
                task_from_row,
            )
            .optional()
            .map_err(|_| StoreError::Database)
    }

    /// Adds a new head for a task branch while preserving previous bindings.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid input or a failed transactional write.
    pub fn bind_session(
        &self,
        task_id: i64,
        branch_name: impl AsRef<str>,
        session: &SessionRef,
    ) -> Result<BindingRecord> {
        let branch_name = branch_name.as_ref();
        if branch_name.trim().is_empty() {
            return Err(StoreError::InvalidBranchName);
        }
        validate_session_ref(session)?;

        let bound_at = now_timestamp();
        let mut connection = self.connection.borrow_mut();
        let transaction = immediate_transaction(&mut connection)?;
        let id = replace_branch_head(&transaction, task_id, branch_name, session, bound_at)?;
        transaction.commit().map_err(|_| StoreError::Database)?;

        Ok(BindingRecord {
            id,
            task_id,
            branch_name: branch_name.to_owned(),
            session: session.clone(),
            bound_at: timestamp_from_db(bound_at)?,
            is_current: true,
        })
    }

    /// Returns the active head of a task branch, if one exists.
    ///
    /// # Errors
    ///
    /// Returns an error when stored binding data cannot be read.
    pub fn current_binding(
        &self,
        task_id: i64,
        branch_name: impl AsRef<str>,
    ) -> Result<Option<BindingRecord>> {
        let connection = self.connection.borrow();
        connection
            .query_row(
                "
                SELECT id, task_id, branch_name, provider, session_id, bound_at, is_current
                FROM session_bindings
                WHERE task_id = ?1 AND branch_name = ?2 AND is_current = 1
                ",
                params![task_id, branch_name.as_ref()],
                binding_from_row,
            )
            .optional()
            .map_err(|_| StoreError::Database)
    }

    /// Records a provider-to-provider handoff and its JSON fidelity report.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid session references, invalid JSON, or persistence failure.
    pub fn record_handoff(
        &self,
        source: &SessionRef,
        target: &SessionRef,
        mode: TransferMode,
        fidelity: &Value,
    ) -> Result<()> {
        validate_session_ref(source)?;
        validate_session_ref(target)?;
        let fidelity_json =
            serde_json::to_string(fidelity).map_err(|_| StoreError::BundleEncoding)?;
        let created_at = now_timestamp();
        let mut connection = self.connection.borrow_mut();
        let transaction = immediate_transaction(&mut connection)?;
        insert_handoff(
            &transaction,
            source,
            target,
            mode,
            &fidelity_json,
            created_at,
        )?;
        transaction.commit().map_err(|_| StoreError::Database)
    }

    /// Records a handoff and advances the task branch head in one transaction.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid input or when either write cannot be committed.
    pub fn record_handoff_and_bind(
        &self,
        task_id: i64,
        branch_name: impl AsRef<str>,
        source: &SessionRef,
        target: &SessionRef,
        mode: TransferMode,
        fidelity: &Value,
    ) -> Result<BindingRecord> {
        let branch_name = branch_name.as_ref();
        if branch_name.trim().is_empty() {
            return Err(StoreError::InvalidBranchName);
        }
        validate_session_ref(source)?;
        validate_session_ref(target)?;
        let fidelity_json =
            serde_json::to_string(fidelity).map_err(|_| StoreError::BundleEncoding)?;
        let created_at = now_timestamp();
        let mut connection = self.connection.borrow_mut();
        let transaction = immediate_transaction(&mut connection)?;
        insert_handoff(
            &transaction,
            source,
            target,
            mode,
            &fidelity_json,
            created_at,
        )?;
        let id = replace_branch_head(&transaction, task_id, branch_name, target, created_at)?;
        transaction.commit().map_err(|_| StoreError::Database)?;

        Ok(BindingRecord {
            id,
            task_id,
            branch_name: branch_name.to_owned(),
            session: target.clone(),
            bound_at: timestamp_from_db(created_at)?,
            is_current: true,
        })
    }

    /// Returns handoffs in source-to-target creation order for lineage displays.
    ///
    /// # Errors
    ///
    /// Returns an error when stored handoff metadata cannot be read.
    pub fn handoff_lineage(&self) -> Result<Vec<HandoffRecord>> {
        let connection = self.connection.borrow();
        let mut statement = connection
            .prepare(
                "
                SELECT source_provider, source_session_id, target_provider, target_session_id,
                       mode, created_at
                FROM handoffs
                ORDER BY created_at, id
                ",
            )
            .map_err(|_| StoreError::Database)?;
        let rows = statement
            .query_map([], handoff_from_row)
            .map_err(|_| StoreError::Database)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|_| StoreError::CorruptStore)
    }

    /// Stores a complete portable bundle, replacing a prior copy with its UUID.
    ///
    /// # Errors
    ///
    /// Returns an error when bundle serialization or persistence fails.
    pub fn save_bundle(&self, bundle: &PortableBundle) -> Result<()> {
        let bundle_json = serde_json::to_string(bundle).map_err(|_| StoreError::BundleEncoding)?;
        let mut connection = self.connection.borrow_mut();
        let transaction = immediate_transaction(&mut connection)?;
        transaction
            .execute(
                "
                INSERT INTO bundles (bundle_id, bundle_json, saved_at)
                VALUES (?1, ?2, ?3)
                ON CONFLICT (bundle_id) DO UPDATE SET
                    bundle_json = excluded.bundle_json,
                    saved_at = excluded.saved_at
                ",
                params![
                    bundle.manifest.bundle_id.to_string(),
                    bundle_json,
                    now_timestamp()
                ],
            )
            .map_err(|_| StoreError::Database)?;
        transaction.commit().map_err(|_| StoreError::Database)
    }

    /// Stores a new bundle without replacing an existing UUID.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::BundleAlreadyExists`] when UUID already exists.
    pub fn save_new_bundle(&self, bundle: &PortableBundle) -> Result<()> {
        let bundle_json = serde_json::to_string(bundle).map_err(|_| StoreError::BundleEncoding)?;
        let connection = self.connection.borrow_mut();
        connection
            .execute(
                "INSERT INTO bundles (bundle_id, bundle_json, saved_at) VALUES (?1, ?2, ?3)",
                params![
                    bundle.manifest.bundle_id.to_string(),
                    bundle_json,
                    now_timestamp()
                ],
            )
            .map_err(|error| {
                if matches!(
                    error,
                    rusqlite::Error::SqliteFailure(ref failure, _)
                        if failure.code == rusqlite::ErrorCode::ConstraintViolation
                ) {
                    StoreError::BundleAlreadyExists
                } else {
                    StoreError::Database
                }
            })?;
        Ok(())
    }

    /// Loads a portable bundle by UUID, if it has been stored.
    ///
    /// # Errors
    ///
    /// Returns an error when the stored bundle cannot be read or decoded.
    pub fn load_bundle(&self, bundle_id: Uuid) -> Result<Option<PortableBundle>> {
        let connection = self.connection.borrow();
        let bundle_json = connection
            .query_row(
                "SELECT bundle_json FROM bundles WHERE bundle_id = ?1",
                params![bundle_id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|_| StoreError::Database)?;
        bundle_json
            .map(|json| serde_json::from_str(&json).map_err(|_| StoreError::CorruptStore))
            .transpose()
    }

    /// Returns cached native-session metadata ordered by most recent update.
    ///
    /// # Errors
    ///
    /// Returns an error when cached metadata cannot be read.
    pub fn indexed_sessions(&self) -> Result<Vec<IndexedSession>> {
        let connection = self.connection.borrow();
        let mut statement = connection
            .prepare(
                "
                SELECT provider, session_id, title, project_path, git_branch,
                       created_at, updated_at, event_count
                FROM session_index
                ORDER BY updated_at IS NULL, updated_at DESC, provider, session_id
                ",
            )
            .map_err(|_| StoreError::Database)?;
        let rows = statement
            .query_map([], indexed_session_from_row)
            .map_err(|_| StoreError::Database)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|_| StoreError::CorruptStore)
    }

    /// Returns cached metadata for one provider, ordered by most recent update.
    ///
    /// # Errors
    ///
    /// Returns an error when cached metadata cannot be read.
    pub fn indexed_sessions_for_provider(&self, provider: Provider) -> Result<Vec<IndexedSession>> {
        let connection = self.connection.borrow();
        let mut statement = connection
            .prepare(
                "
                SELECT provider, session_id, title, project_path, git_branch,
                       created_at, updated_at, event_count
                FROM session_index
                WHERE provider = ?1
                ORDER BY updated_at IS NULL, updated_at DESC, session_id
                ",
            )
            .map_err(|_| StoreError::Database)?;
        let rows = statement
            .query_map(params![provider.to_string()], indexed_session_from_row)
            .map_err(|_| StoreError::Database)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|_| StoreError::CorruptStore)
    }

    /// Returns when one provider's cached metadata was last refreshed.
    ///
    /// Empty provider snapshots retain refresh state, allowing repeated discovery
    /// to avoid rescanning stores that still contain no sessions.
    ///
    /// # Errors
    ///
    /// Returns an error when cached refresh metadata cannot be read.
    pub fn session_index_refreshed_at(&self, provider: Provider) -> Result<Option<DateTime<Utc>>> {
        let connection = self.connection.borrow();
        let timestamp = connection
            .query_row(
                "SELECT indexed_at FROM session_index_state WHERE provider = ?1",
                params![provider.to_string()],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map_err(|_| StoreError::Database)?;
        timestamp.map(timestamp_from_db).transpose()
    }

    /// Returns when one provider store was last checked, including failed checks.
    ///
    /// # Errors
    ///
    /// Returns an error when cached check metadata cannot be read.
    pub fn session_index_checked_at(&self, provider: Provider) -> Result<Option<DateTime<Utc>>> {
        let connection = self.connection.borrow();
        let timestamp = connection
            .query_row(
                "SELECT checked_at FROM session_index_checks WHERE provider = ?1",
                params![provider.to_string()],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map_err(|_| StoreError::Database)?;
        timestamp.map(timestamp_from_db).transpose()
    }

    /// Records a provider-store check without replacing its last valid snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error when check metadata cannot be persisted.
    pub fn mark_session_index_checked(&self, provider: Provider) -> Result<()> {
        let connection = self.connection.borrow_mut();
        connection
            .execute(
                "
                INSERT INTO session_index_checks (provider, checked_at)
                VALUES (?1, ?2)
                ON CONFLICT (provider) DO UPDATE SET checked_at = excluded.checked_at
                ",
                params![provider.to_string(), now_timestamp()],
            )
            .map_err(|_| StoreError::Database)?;
        Ok(())
    }

    /// Atomically replaces cached metadata for one provider.
    ///
    /// # Errors
    ///
    /// Returns an error for mismatched session providers or failed persistence.
    pub fn replace_indexed_sessions(
        &self,
        provider: Provider,
        sessions: &[IndexedSession],
    ) -> Result<()> {
        if sessions
            .iter()
            .any(|session| session.session.provider != provider || session.session.id.is_empty())
        {
            return Err(StoreError::InvalidSessionReference);
        }
        if sessions.iter().any(|session| {
            session
                .project_path
                .as_deref()
                .is_some_and(|path| path.to_str().is_none())
        }) {
            return Err(StoreError::InvalidWorkspaceRoot);
        }
        let mut connection = self.connection.borrow_mut();
        let transaction = immediate_transaction(&mut connection)?;
        let provider_name = provider.to_string();
        transaction
            .execute(
                "DELETE FROM session_index WHERE provider = ?1",
                params![provider_name],
            )
            .map_err(|_| StoreError::Database)?;
        let indexed_at = now_timestamp();
        {
            let mut statement = transaction
                .prepare(
                    "
                    INSERT INTO session_index (
                        provider, session_id, title, project_path, git_branch,
                        created_at, updated_at, event_count, indexed_at
                    ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                    ",
                )
                .map_err(|_| StoreError::Database)?;
            for session in sessions {
                let event_count = i64::try_from(session.event_count)
                    .map_err(|_| StoreError::InvalidSessionReference)?;
                let project_path = session.project_path.as_deref().and_then(Path::to_str);
                statement
                    .execute(params![
                        provider_name,
                        session.session.id.as_str(),
                        session.title.as_deref(),
                        project_path,
                        session.git_branch.as_deref(),
                        session.created_at.as_ref().map(DateTime::timestamp_millis),
                        session.updated_at.as_ref().map(DateTime::timestamp_millis),
                        event_count,
                        indexed_at,
                    ])
                    .map_err(|_| StoreError::Database)?;
            }
        }
        transaction
            .execute(
                "
                INSERT INTO session_index_state (provider, indexed_at)
                VALUES (?1, ?2)
                ON CONFLICT (provider) DO UPDATE SET indexed_at = excluded.indexed_at
                ",
                params![provider_name, indexed_at],
            )
            .map_err(|_| StoreError::Database)?;
        transaction
            .execute(
                "
                INSERT INTO session_index_checks (provider, checked_at)
                VALUES (?1, ?2)
                ON CONFLICT (provider) DO UPDATE SET checked_at = excluded.checked_at
                ",
                params![provider_name, indexed_at],
            )
            .map_err(|_| StoreError::Database)?;
        transaction.commit().map_err(|_| StoreError::Database)
    }

    /// Creates or upgrades the schema required by this store.
    ///
    /// # Errors
    ///
    /// Returns an error when schema creation or upgrade fails.
    pub fn initialize_schema(&self) -> Result<()> {
        let mut connection = self.connection.borrow_mut();
        let transaction = immediate_transaction(&mut connection)?;
        transaction
            .execute_batch(
                "
                CREATE TABLE IF NOT EXISTS tasks (
                    id INTEGER PRIMARY KEY,
                    name TEXT NOT NULL CHECK (length(trim(name)) > 0),
                    workspace_root TEXT NOT NULL,
                    created_at INTEGER NOT NULL,
                    UNIQUE (workspace_root, name)
                );

                CREATE TABLE IF NOT EXISTS task_selections (
                    workspace_root TEXT PRIMARY KEY,
                    task_id INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
                    selected_at INTEGER NOT NULL
                );

                CREATE TABLE IF NOT EXISTS session_bindings (
                    id INTEGER PRIMARY KEY,
                    task_id INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
                    branch_name TEXT NOT NULL CHECK (length(trim(branch_name)) > 0),
                    provider TEXT NOT NULL,
                    session_id TEXT NOT NULL CHECK (length(session_id) > 0),
                    bound_at INTEGER NOT NULL,
                    is_current INTEGER NOT NULL DEFAULT 1 CHECK (is_current IN (0, 1))
                );

                CREATE UNIQUE INDEX IF NOT EXISTS current_session_binding_per_branch
                    ON session_bindings (task_id, branch_name)
                    WHERE is_current = 1;

                CREATE INDEX IF NOT EXISTS session_binding_lineage
                    ON session_bindings (task_id, branch_name, id);

                CREATE TABLE IF NOT EXISTS handoffs (
                    id INTEGER PRIMARY KEY,
                    source_provider TEXT NOT NULL,
                    source_session_id TEXT NOT NULL CHECK (length(source_session_id) > 0),
                    target_provider TEXT NOT NULL,
                    target_session_id TEXT NOT NULL CHECK (length(target_session_id) > 0),
                    mode TEXT NOT NULL,
                    fidelity_json TEXT NOT NULL,
                    created_at INTEGER NOT NULL
                );

                CREATE TABLE IF NOT EXISTS bundles (
                    bundle_id TEXT PRIMARY KEY,
                    bundle_json TEXT NOT NULL,
                    saved_at INTEGER NOT NULL
                );

                CREATE TABLE IF NOT EXISTS session_index (
                    provider TEXT NOT NULL,
                    session_id TEXT NOT NULL CHECK (length(session_id) > 0),
                    title TEXT,
                    project_path TEXT,
                    git_branch TEXT,
                    created_at INTEGER,
                    updated_at INTEGER,
                    event_count INTEGER NOT NULL CHECK (event_count >= 0),
                    indexed_at INTEGER NOT NULL,
                    PRIMARY KEY (provider, session_id)
                ) WITHOUT ROWID;

                CREATE INDEX IF NOT EXISTS session_index_updated
                    ON session_index (updated_at DESC);
                CREATE INDEX IF NOT EXISTS session_index_provider_updated
                    ON session_index (provider, updated_at DESC);

                CREATE TABLE IF NOT EXISTS session_index_state (
                    provider TEXT PRIMARY KEY,
                    indexed_at INTEGER NOT NULL
                ) WITHOUT ROWID;

                CREATE TABLE IF NOT EXISTS session_index_checks (
                    provider TEXT PRIMARY KEY,
                    checked_at INTEGER NOT NULL
                ) WITHOUT ROWID;
                ",
            )
            .map_err(|_| StoreError::Database)?;
        transaction.commit().map_err(|_| StoreError::Database)
    }

    fn open_state_root(state_root: &Path) -> Result<Self> {
        if state_root.as_os_str().is_empty() {
            return Err(StoreError::DefaultDirectoryUnavailable);
        }
        fs::create_dir_all(state_root).map_err(|_| StoreError::Database)?;
        if !fs::metadata(state_root)
            .map_err(|_| StoreError::Database)?
            .is_dir()
        {
            return Err(StoreError::Database);
        }
        reject_symlink(state_root)?;
        set_private_directory(state_root)?;
        Self::open(state_root.join(DATABASE_FILE_NAME))
    }
}

fn reject_symlink(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(StoreError::Database),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(StoreError::Database),
    }
}

#[cfg(unix)]
fn set_private_directory(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|_| StoreError::Database)
}

#[cfg(not(unix))]
fn set_private_directory(path: &Path) -> Result<()> {
    fs::metadata(path)
        .map_err(|_| StoreError::Database)?
        .is_dir()
        .then_some(())
        .ok_or(StoreError::Database)
}

#[cfg(unix)]
fn set_private_file(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|_| StoreError::Database)
}

#[cfg(not(unix))]
fn set_private_file(path: &Path) -> Result<()> {
    fs::metadata(path)
        .map_err(|_| StoreError::Database)?
        .is_file()
        .then_some(())
        .ok_or(StoreError::Database)
}

fn immediate_transaction(connection: &mut Connection) -> Result<Transaction<'_>> {
    connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|_| StoreError::Database)
}

fn insert_handoff(
    transaction: &Transaction<'_>,
    source: &SessionRef,
    target: &SessionRef,
    mode: TransferMode,
    fidelity_json: &str,
    created_at: i64,
) -> Result<()> {
    transaction
        .execute(
            "
            INSERT INTO handoffs (
                source_provider, source_session_id, target_provider, target_session_id,
                mode, fidelity_json, created_at
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
            ",
            params![
                source.provider.to_string(),
                source.id,
                target.provider.to_string(),
                target.id,
                transfer_mode_name(mode),
                fidelity_json,
                created_at,
            ],
        )
        .map_err(|_| StoreError::Database)?;
    Ok(())
}

fn replace_branch_head(
    transaction: &Transaction<'_>,
    task_id: i64,
    branch_name: &str,
    session: &SessionRef,
    bound_at: i64,
) -> Result<i64> {
    transaction
        .execute(
            "
            UPDATE session_bindings
            SET is_current = 0
            WHERE task_id = ?1 AND branch_name = ?2 AND is_current = 1
            ",
            params![task_id, branch_name],
        )
        .map_err(|_| StoreError::Database)?;
    transaction
        .execute(
            "
            INSERT INTO session_bindings
                (task_id, branch_name, provider, session_id, bound_at, is_current)
            VALUES (?1, ?2, ?3, ?4, ?5, 1)
            ",
            params![
                task_id,
                branch_name,
                session.provider.to_string(),
                session.id,
                bound_at,
            ],
        )
        .map_err(|_| StoreError::Database)?;
    Ok(transaction.last_insert_rowid())
}

fn query_task(
    connection: &Connection,
    workspace_root: &str,
    name: &str,
) -> Result<Option<TaskRecord>> {
    connection
        .query_row(
            "
            SELECT id, name, workspace_root, created_at
            FROM tasks
            WHERE workspace_root = ?1 AND name = ?2
            ",
            params![workspace_root, name],
            task_from_row,
        )
        .optional()
        .map_err(|_| StoreError::Database)
}

fn task_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<TaskRecord> {
    let timestamp = row.get::<_, i64>(3)?;
    let created_at = DateTime::from_timestamp_millis(timestamp).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(3, Type::Integer, "invalid timestamp".into())
    })?;
    Ok(TaskRecord {
        id: row.get(0)?,
        name: row.get(1)?,
        workspace_root: PathBuf::from(row.get::<_, String>(2)?),
        created_at,
    })
}

fn binding_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<BindingRecord> {
    let provider = row
        .get::<_, String>(3)?
        .parse::<Provider>()
        .map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(3, Type::Text, Box::new(error))
        })?;
    let timestamp = row.get::<_, i64>(5)?;
    let bound_at = DateTime::from_timestamp_millis(timestamp).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(5, Type::Integer, "invalid timestamp".into())
    })?;
    Ok(BindingRecord {
        id: row.get(0)?,
        task_id: row.get(1)?,
        branch_name: row.get(2)?,
        session: SessionRef::new(provider, row.get::<_, String>(4)?),
        bound_at,
        is_current: row.get::<_, i64>(6)? != 0,
    })
}

fn handoff_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<HandoffRecord> {
    let source_provider = row
        .get::<_, String>(0)?
        .parse::<Provider>()
        .map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(0, Type::Text, Box::new(error))
        })?;
    let target_provider = row
        .get::<_, String>(2)?
        .parse::<Provider>()
        .map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(2, Type::Text, Box::new(error))
        })?;
    let mode_name = row.get::<_, String>(4)?;
    let mode = transfer_mode_from_name(&mode_name).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(4, Type::Text, "invalid transfer mode".into())
    })?;
    let timestamp = row.get::<_, i64>(5)?;
    let created_at = DateTime::from_timestamp_millis(timestamp).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(5, Type::Integer, "invalid timestamp".into())
    })?;
    Ok(HandoffRecord {
        source: SessionRef::new(source_provider, row.get::<_, String>(1)?),
        target: SessionRef::new(target_provider, row.get::<_, String>(3)?),
        mode,
        created_at,
    })
}

fn indexed_session_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<IndexedSession> {
    let provider = row
        .get::<_, String>(0)?
        .parse::<Provider>()
        .map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(0, Type::Text, Box::new(error))
        })?;
    let created_at = optional_timestamp_from_row(row, 5)?;
    let updated_at = optional_timestamp_from_row(row, 6)?;
    let event_count = row.get::<_, i64>(7)?;
    let event_count = usize::try_from(event_count).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(7, Type::Integer, Box::new(error))
    })?;
    Ok(IndexedSession {
        session: SessionRef::new(provider, row.get::<_, String>(1)?),
        title: row.get(2)?,
        project_path: row.get::<_, Option<String>>(3)?.map(PathBuf::from),
        git_branch: row.get(4)?,
        created_at,
        updated_at,
        event_count,
    })
}

fn optional_timestamp_from_row(
    row: &rusqlite::Row<'_>,
    index: usize,
) -> rusqlite::Result<Option<DateTime<Utc>>> {
    row.get::<_, Option<i64>>(index)?
        .map(|timestamp| {
            DateTime::from_timestamp_millis(timestamp).ok_or_else(|| {
                rusqlite::Error::FromSqlConversionFailure(
                    index,
                    Type::Integer,
                    "invalid timestamp".into(),
                )
            })
        })
        .transpose()
}

fn workspace_root_to_string(workspace_root: &Path) -> Result<String> {
    let canonical = fs::canonicalize(workspace_root).unwrap_or_else(|_| workspace_root.to_owned());
    canonical
        .to_str()
        .map(str::to_owned)
        .ok_or(StoreError::InvalidWorkspaceRoot)
}

/// Returns configured local state root without creating it.
///
/// # Errors
///
/// Returns an error when neither `OMNISESSION_HOME` nor a home directory is available.
pub fn state_root() -> Result<PathBuf> {
    if let Some(state_root) = std::env::var_os("OMNISESSION_HOME").filter(|value| !value.is_empty())
    {
        return Ok(PathBuf::from(state_root));
    }
    let base_dirs = BaseDirs::new().ok_or(StoreError::DefaultDirectoryUnavailable)?;
    Ok(base_dirs.home_dir().join(".omnisession"))
}

fn timestamp_from_db(timestamp: i64) -> Result<DateTime<Utc>> {
    DateTime::from_timestamp_millis(timestamp).ok_or(StoreError::CorruptStore)
}

fn now_timestamp() -> i64 {
    Utc::now().timestamp_millis()
}

fn validate_session_ref(session: &SessionRef) -> Result<()> {
    if session.id.is_empty() {
        return Err(StoreError::InvalidSessionReference);
    }
    Ok(())
}

const fn transfer_mode_name(mode: TransferMode) -> &'static str {
    match mode {
        TransferMode::NativeResume => "native_resume",
        TransferMode::OfficialImport => "official_import",
        TransferMode::NativeMaterialization => "native_materialization",
        TransferMode::SemanticHandoff => "semantic_handoff",
        TransferMode::PortableExport => "portable_export",
    }
}

fn transfer_mode_from_name(value: &str) -> Option<TransferMode> {
    match value {
        "native_resume" => Some(TransferMode::NativeResume),
        "official_import" => Some(TransferMode::OfficialImport),
        "native_materialization" => Some(TransferMode::NativeMaterialization),
        "semantic_handoff" => Some(TransferMode::SemanticHandoff),
        "portable_export" => Some(TransferMode::PortableExport),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use chrono::Utc;
    use directories::BaseDirs;
    use omnis_ir::{
        BundleManifest, CanonicalSnapshot, GitState, PortableBundle, Provider, SCHEMA_VERSION,
        SessionRef, TransferMode, WorkspaceSnapshot,
    };
    use rusqlite::params;
    use serde_json::json;
    use tempfile::tempdir;
    use uuid::Uuid;

    use super::{IndexedSession, Store, state_root};

    #[test]
    fn task_selection_is_scoped_to_its_workspace() {
        let temporary_directory = tempdir().expect("temporary directory");
        let store = Store::open(temporary_directory.path().join("store.sqlite3")).expect("store");
        let first_workspace = temporary_directory.path().join("first");
        let second_workspace = temporary_directory.path().join("second");

        let first = store
            .create_or_get_task("migration", &first_workspace)
            .expect("first task");
        let second = store
            .create_or_get_task("migration", &second_workspace)
            .expect("second task");
        assert_ne!(first.id, second.id);
        assert!(
            store
                .selected_task(&first_workspace)
                .expect("selection lookup")
                .is_none()
        );

        assert_eq!(
            store
                .select_task(&first_workspace, "migration")
                .expect("select first"),
            first
        );
        assert_eq!(
            store
                .selected_task(&first_workspace)
                .expect("first selection"),
            Some(first)
        );
        assert!(
            store
                .selected_task(&second_workspace)
                .expect("second selection")
                .is_none()
        );
    }

    #[test]
    fn session_index_replaces_only_refreshed_provider() {
        let temporary_directory = tempdir().expect("temporary directory");
        let store = Store::open(temporary_directory.path().join("store.sqlite3")).expect("store");
        let first_claude = indexed_session(Provider::Claude, "claude-old", "Old title");
        let codex = indexed_session(Provider::Codex, "codex-session", "Codex title");
        store
            .replace_indexed_sessions(Provider::Claude, &[first_claude])
            .expect("index Claude");
        store
            .replace_indexed_sessions(Provider::Codex, std::slice::from_ref(&codex))
            .expect("index Codex");

        let current_claude = indexed_session(Provider::Claude, "claude-new", "New title");
        store
            .replace_indexed_sessions(Provider::Claude, std::slice::from_ref(&current_claude))
            .expect("replace Claude index");
        let indexed = store.indexed_sessions().expect("read index");

        assert_eq!(indexed.len(), 2);
        assert!(indexed.contains(&current_claude));
        assert!(indexed.contains(&codex));
        assert!(
            indexed
                .iter()
                .all(|session| session.session.id != "claude-old")
        );
        assert_eq!(
            store
                .indexed_sessions_for_provider(Provider::Claude)
                .expect("read Claude index"),
            vec![current_claude]
        );
    }

    #[test]
    fn failed_session_index_refresh_keeps_previous_snapshot() {
        let temporary_directory = tempdir().expect("temporary directory");
        let store = Store::open(temporary_directory.path().join("store.sqlite3")).expect("store");
        let previous = indexed_session(Provider::Claude, "previous", "Previous title");
        store
            .replace_indexed_sessions(Provider::Claude, std::slice::from_ref(&previous))
            .expect("initial index");
        let duplicate = indexed_session(Provider::Claude, "duplicate", "Duplicate title");

        assert!(
            store
                .replace_indexed_sessions(Provider::Claude, &[duplicate.clone(), duplicate],)
                .is_err()
        );
        assert_eq!(
            store.indexed_sessions().expect("read index"),
            vec![previous]
        );
    }

    #[test]
    fn session_index_tracks_empty_provider_refreshes() {
        let temporary_directory = tempdir().expect("temporary directory");
        let store = Store::open(temporary_directory.path().join("store.sqlite3")).expect("store");

        assert_eq!(
            store
                .session_index_refreshed_at(Provider::Grok)
                .expect("initial refresh state"),
            None
        );
        store
            .replace_indexed_sessions(Provider::Grok, &[])
            .expect("empty refresh");

        assert!(
            store
                .session_index_refreshed_at(Provider::Grok)
                .expect("refresh state")
                .is_some()
        );
        assert!(
            store
                .indexed_sessions_for_provider(Provider::Grok)
                .expect("empty index")
                .is_empty()
        );
        assert!(
            store
                .session_index_checked_at(Provider::Grok)
                .expect("check state")
                .is_some()
        );
    }

    #[test]
    fn failed_provider_check_preserves_cached_snapshot() {
        let temporary_directory = tempdir().expect("temporary directory");
        let store = Store::open(temporary_directory.path().join("store.sqlite3")).expect("store");
        let cached = indexed_session(Provider::CursorIde, "cached", "Cached title");
        store
            .replace_indexed_sessions(Provider::CursorIde, std::slice::from_ref(&cached))
            .expect("initial index");

        store
            .mark_session_index_checked(Provider::CursorIde)
            .expect("record failed check");

        assert_eq!(
            store
                .indexed_sessions_for_provider(Provider::CursorIde)
                .expect("preserved index"),
            vec![cached]
        );
        assert!(
            store
                .session_index_checked_at(Provider::CursorIde)
                .expect("check state")
                .is_some()
        );
    }

    fn indexed_session(provider: Provider, id: &str, title: &str) -> IndexedSession {
        IndexedSession {
            session: SessionRef::new(provider, id),
            title: Some(title.to_owned()),
            project_path: Some(PathBuf::from("/workspace/project")),
            git_branch: Some("main".to_owned()),
            created_at: None,
            updated_at: None,
            event_count: 0,
        }
    }

    #[test]
    fn state_root_is_created_before_opening_its_store() {
        let temporary_directory = tempdir().expect("temporary directory");
        let state_root = temporary_directory.path().join("state");

        let _store = Store::open_state_root(&state_root).expect("store");

        assert!(state_root.is_dir());
        assert!(state_root.join("store.sqlite3").is_file());
    }

    #[cfg(unix)]
    #[test]
    fn state_root_and_database_are_private() {
        use std::os::unix::fs::PermissionsExt;

        let temporary_directory = tempdir().expect("temporary directory");
        let state_root = temporary_directory.path().join("state");
        let _store = Store::open_state_root(&state_root).expect("store");

        assert_eq!(
            std::fs::metadata(&state_root)
                .expect("state metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(state_root.join("store.sqlite3"))
                .expect("database metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[cfg(unix)]
    #[test]
    fn state_root_symlink_is_rejected() {
        use std::os::unix::fs::symlink;

        let temporary_directory = tempdir().expect("temporary directory");
        let real_root = temporary_directory.path().join("real");
        std::fs::create_dir(&real_root).expect("real state root");
        let linked_root = temporary_directory.path().join("linked");
        symlink(&real_root, &linked_root).expect("state-root symlink");

        assert!(Store::open_state_root(&linked_root).is_err());
    }

    #[test]
    fn default_state_root_is_user_home_dot_omnisession() {
        if std::env::var_os("OMNISESSION_HOME").is_some() {
            return;
        }
        let base_dirs = BaseDirs::new().expect("home directory");
        assert_eq!(
            state_root().expect("default state root"),
            base_dirs.home_dir().join(".omnisession")
        );
    }

    #[test]
    fn replacing_branch_head_preserves_prior_binding() {
        let temporary_directory = tempdir().expect("temporary directory");
        let store = Store::open(temporary_directory.path().join("store.sqlite3")).expect("store");
        let task = store
            .create_or_get_task("handoff", temporary_directory.path())
            .expect("task");
        let first = SessionRef::new(Provider::Codex, "first");
        let second = SessionRef::new(Provider::Claude, "second");

        store
            .bind_session(task.id, "main", &first)
            .expect("first binding");
        store
            .bind_session(task.id, "main", &second)
            .expect("second binding");

        assert_eq!(
            store
                .current_binding(task.id, "main")
                .expect("current binding")
                .expect("head")
                .session,
            second
        );
        let connection = store.connection.borrow();
        let lineage = connection
            .prepare(
                "SELECT session_id, is_current FROM session_bindings
                 WHERE task_id = ?1 AND branch_name = ?2 ORDER BY id",
            )
            .expect("lineage query")
            .query_map(params![task.id, "main"], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })
            .expect("lineage rows")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("lineage values");
        assert_eq!(
            lineage,
            vec![("first".to_owned(), 0), ("second".to_owned(), 1)]
        );
    }

    #[test]
    fn handoff_lineage_orders_source_to_target_metadata() {
        let temporary_directory = tempdir().expect("temporary directory");
        let store = Store::open(temporary_directory.path().join("store.sqlite3")).expect("store");
        let codex = SessionRef::new(Provider::Codex, "codex-1");
        let claude = SessionRef::new(Provider::Claude, "claude-1");
        let grok = SessionRef::new(Provider::Grok, "grok-1");

        store
            .record_handoff(
                &codex,
                &claude,
                TransferMode::SemanticHandoff,
                &json!({"status": "summarized"}),
            )
            .expect("first handoff");
        store
            .record_handoff(
                &claude,
                &grok,
                TransferMode::OfficialImport,
                &json!({"status": "preserved"}),
            )
            .expect("second handoff");

        let lineage = store.handoff_lineage().expect("handoff lineage");
        assert_eq!(lineage.len(), 2);
        assert_eq!(lineage[0].source, codex);
        assert_eq!(lineage[0].target, claude);
        assert_eq!(lineage[0].mode, TransferMode::SemanticHandoff);
        assert_eq!(lineage[1].source, claude);
        assert_eq!(lineage[1].target, grok);
        assert_eq!(lineage[1].mode, TransferMode::OfficialImport);
        assert!(lineage[0].created_at <= lineage[1].created_at);
    }

    #[test]
    fn combined_handoff_and_bind_commits_or_rolls_back_together() {
        let temporary_directory = tempdir().expect("temporary directory");
        let store = Store::open(temporary_directory.path().join("store.sqlite3")).expect("store");
        let task = store
            .create_or_get_task("handoff", temporary_directory.path())
            .expect("task");
        let source = SessionRef::new(Provider::Codex, "source");
        let target = SessionRef::new(Provider::Claude, "target");
        let rejected_target = SessionRef::new(Provider::Grok, "rejected-target");
        store
            .bind_session(task.id, "main", &source)
            .expect("source binding");

        let binding = store
            .record_handoff_and_bind(
                task.id,
                "main",
                &source,
                &target,
                TransferMode::SemanticHandoff,
                &json!({"status": "summarized"}),
            )
            .expect("combined handoff");
        assert_eq!(binding.session, target);
        assert_eq!(
            store
                .current_binding(task.id, "main")
                .expect("current binding")
                .expect("head")
                .session,
            target
        );

        store
            .connection
            .borrow()
            .execute_batch(
                "
                CREATE TRIGGER reject_test_binding
                BEFORE INSERT ON session_bindings
                WHEN NEW.session_id = 'rejected-target'
                BEGIN
                    SELECT RAISE(ABORT, 'synthetic test failure');
                END;
                ",
            )
            .expect("test trigger");
        assert!(
            store
                .record_handoff_and_bind(
                    task.id,
                    "main",
                    &target,
                    &rejected_target,
                    TransferMode::SemanticHandoff,
                    &json!({"status": "unsupported"}),
                )
                .is_err()
        );

        assert_eq!(
            store
                .current_binding(task.id, "main")
                .expect("current binding after rollback")
                .expect("head")
                .session,
            target
        );
        let lineage = store.handoff_lineage().expect("handoff lineage");
        assert_eq!(lineage.len(), 1);
        assert_eq!(lineage[0].source, source);
        assert_eq!(lineage[0].target, target);
    }

    #[test]
    fn portable_bundle_round_trips() {
        let temporary_directory = tempdir().expect("temporary directory");
        let store = Store::open(temporary_directory.path().join("store.sqlite3")).expect("store");
        let source = SessionRef::new(Provider::Codex, "session-1");
        let bundle = PortableBundle {
            manifest: BundleManifest {
                schema_version: SCHEMA_VERSION.to_owned(),
                bundle_id: Uuid::new_v4(),
                created_at: Utc::now(),
                source: source.clone(),
                event_count: 0,
                redactions: Vec::new(),
            },
            snapshot: CanonicalSnapshot {
                schema_version: SCHEMA_VERSION.to_owned(),
                session: source,
                thread_id: Uuid::new_v4(),
                branch_id: Uuid::new_v4(),
                title: Some("bundle test".to_owned()),
                captured_at: Utc::now(),
                workspace: WorkspaceSnapshot {
                    schema_version: SCHEMA_VERSION.to_owned(),
                    captured_at: Utc::now(),
                    root: temporary_directory.path().to_path_buf(),
                    current_dir: temporary_directory.path().to_path_buf(),
                    git: GitState::default(),
                    instruction_files: Vec::new(),
                    environment_names: Vec::new(),
                    available_tools: Vec::new(),
                },
                events: Vec::new(),
            },
            fidelity: None,
        };

        store.save_bundle(&bundle).expect("save bundle");
        assert_eq!(
            store
                .load_bundle(bundle.manifest.bundle_id)
                .expect("load bundle"),
            Some(bundle)
        );
    }
}
