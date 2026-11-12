//! Workspaces established by reading a specific Codex rollout.

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
};

use omnis_ir::{Provider, SourceRevision};
use rusqlite::{Connection, OpenFlags, Transaction, params};
use uuid::Uuid;

use crate::{
    DATABASE_FILE_NAME, PreparedTrajectoryWrite, Result, SessionTrajectoryOrigin, StoreError,
    TrajectorySourceGuard, active_codex_home, database_error, reject_symlink, state_root,
};

const UNCERTAIN_LEGACY_SOURCE: &str =
    "(NOT t.source_complete OR t.truncation_strategy GLOB 'source_incomplete*'
    OR t.truncation_strategy IN ('legacy_unknown','legacy_bounded'))";

// Older search documents did not record directory changes between turns. A stored
// workspace can also belong to another physical copy of the same thread.
const LEGACY_RECHECKS: &str = "SELECT t.trajectory_id,t.source_file_id,t.source_contents_id FROM (
    SELECT id AS trajectory_id,source_file_id,source_contents_id FROM session_trajectories
      WHERE provider='codex' AND origin='native' AND document_version=6
        AND source_file_id IS NOT NULL AND source_contents_id IS NOT NULL
    UNION ALL SELECT trajectory_id,source_file_id,source_contents_id FROM codex_indexed_workspaces
    ) AS t WHERE 1";

/// The workspace observations and pending reads for the selected rollout revisions.
#[derive(Debug, Default)]
pub struct CodexWorkspaceIndex {
    pub verified: HashMap<Uuid, CodexWorkspace>,
    pub ancestor_revisions: HashMap<Uuid, Vec<(Uuid, Uuid)>>,
    pub pending: HashSet<SourceRevision>,
}

impl CodexWorkspaceIndex {
    /// Returns whether a fresh read must establish this physical rollout’s working directory.
    #[must_use]
    pub fn needs_recheck(&self, revision: SourceRevision) -> bool {
        self.pending.contains(&SourceRevision {
            missing_ancestor: None,
            ..revision
        })
    }
}

/// A workspace and branch read from the selected rollout revision.
#[derive(Debug, Clone)]
pub struct CodexWorkspace {
    pub source_revision: SourceRevision,
    pub project_path: Option<PathBuf>,
    pub git_branch: Option<String>,
}

/// A workspace change established by a physical read of a native Codex rollout.
#[derive(Debug, Clone)]
pub enum CodexWorkspaceUpdate {
    /// The reader verified the effective workspace.
    Verified(CodexWorkspace),
    /// A full read found unreadable records. The workspace for this revision is unknown.
    Withdrawn(SourceRevision),
}

impl CodexWorkspace {
    /// Reads workspace observations without opening any provider rollouts.
    /// A present entry with no project path means this revision has an unknown workspace.
    ///
    /// # Errors
    ///
    /// Returns an error if the workspace index does not exist or cannot be read.
    pub fn read_index(file: Option<Uuid>) -> Result<HashMap<Uuid, Self>> {
        Ok(Self::discovery_index(file)?.verified)
    }

    /// Reads trusted workspaces and identifies old revisions that need a fresh workspace check.
    ///
    /// # Errors
    ///
    /// Returns an error if the workspace index cannot be read.
    pub fn discovery_index(file: Option<Uuid>) -> Result<CodexWorkspaceIndex> {
        with_index(|connection| Self::discovery_index_from(connection, file))
    }

    pub(super) fn discovery_index_from(
        connection: &Connection,
        file: Option<Uuid>,
    ) -> Result<CodexWorkspaceIndex> {
        let (stored_filter, legacy_filter, workspace_filter) = if file.is_some() {
            (
                " AND source_file_id=?1",
                " AND t.source_file_id=?1",
                " AND w.source_file_id=?1",
            )
        } else {
            ("", "", "")
        };
        let settings_checked = has_settings_generation(connection)?;
        let mut pending_queries = Vec::new();
        if has_rechecks(connection)? {
            pending_queries.push(format!("SELECT trajectory_id,source_file_id,source_contents_id FROM codex_workspace_rechecks WHERE 1{stored_filter}"));
        }
        if !settings_checked {
            pending_queries.push(format!("{LEGACY_RECHECKS}{legacy_filter}"));
        }
        let pending_rows = pending_queries.join(" UNION ");
        let has_ancestry: bool = connection
            .query_row(
                "SELECT count(*)=2 FROM pragma_table_info('session_trajectories')
             WHERE name IN ('ancestor_revisions','missing_ancestor_id')",
                [],
                |row| row.get(0),
            )
            .map_err(database_error)?;
        let ancestry = if has_ancestry {
            "CASE WHEN t.missing_ancestor_id IS NULL AND t.source_file_id=w.source_file_id
              AND t.source_contents_id=w.source_contents_id THEN coalesce(t.ancestor_revisions,x'5b5d') END"
        } else {
            "NULL"
        };
        let workspace_rows = if settings_checked {
            format!(" UNION ALL SELECT w.trajectory_id,w.source_file_id,w.source_contents_id,w.project_path,w.git_branch,0,{ancestry}
                FROM codex_indexed_workspaces w JOIN session_trajectories t ON t.id=w.trajectory_id
                  AND t.document_version >= {version}
                WHERE NOT EXISTS(SELECT 1 FROM pending p WHERE p.trajectory_id=w.trajectory_id
                    AND p.source_file_id=w.source_file_id AND p.source_contents_id=w.source_contents_id)
                {workspace_filter}", version = omnis_core::CODEX_DISPLAY_DOCUMENT_VERSION)
        } else {
            String::new()
        };
        let sql = format!(
            "WITH pending AS ({pending_rows}), observations AS (
                SELECT trajectory_id,source_file_id,source_contents_id,NULL AS project_path,NULL AS git_branch,1 AS pending,NULL AS ancestry
                FROM pending{workspace_rows})
             SELECT w.source_file_id,w.source_contents_id,w.project_path,w.git_branch,w.pending,w.ancestry
             FROM observations w JOIN visible_trajectories t ON t.id=w.trajectory_id"
        );
        let filter = file.map(Uuid::into_bytes);
        let mut statement = connection.prepare(&sql).map_err(database_error)?;
        let rows = statement
            .query_map(rusqlite::params_from_iter(filter.as_slice()), |row| {
                Ok((
                    workspace_row(row)?,
                    row.get::<_, bool>(4)?,
                    row.get::<_, Option<Vec<u8>>>(5)?,
                ))
            })
            .map_err(database_error)?;
        let mut index = CodexWorkspaceIndex::default();
        for row in rows {
            let ((file, workspace), pending, ancestry) = row.map_err(database_error)?;
            if pending {
                index.pending.insert(workspace.source_revision);
            } else {
                if let Some(bytes) = ancestry {
                    let revisions =
                        serde_json::from_slice(&bytes).map_err(|_| StoreError::CorruptStore)?;
                    index.ancestor_revisions.insert(file, revisions);
                }
                index.verified.insert(file, workspace);
            }
        }
        Ok(index)
    }

    /// Finds previous full documents whose saved workspace can schedule a verification read.
    /// Search must verify the selected file before it uses this workspace to rank a result.
    ///
    /// # Errors
    ///
    /// Returns an error if the search catalog cannot be read.
    pub fn refresh_hints() -> Result<HashMap<Uuid, Self>> {
        let sql = "SELECT t.source_file_id,t.source_contents_id,c.project_path,c.git_branch
             FROM visible_trajectories t JOIN scoped_session_index c USING(provider,session_id)
             WHERE t.provider='codex' AND t.origin='native' AND t.source_complete=1
               AND t.document_version < ?1 AND c.project_path IS NOT NULL
               AND c.source_file_id=t.source_file_id AND c.source_contents_id=t.source_contents_id";
        with_index(|connection| {
            let mut statement = connection.prepare(sql).map_err(database_error)?;
            statement
                .query_map(
                    [omnis_core::search_document_version(Provider::Codex)],
                    workspace_row,
                )
                .map_err(database_error)?
                .collect::<rusqlite::Result<_>>()
                .map_err(database_error)
        })
    }

    pub(super) fn record(
        &self,
        transaction: &Transaction<'_>,
        write: &PreparedTrajectoryWrite<'_, '_>,
        require_full_source: bool,
    ) -> Result<()> {
        let session = write.session;
        let revision = write.document.source_revision;
        if session.provider != Provider::Codex
            || !self.source_revision.matches_file_contents(revision)
        {
            return Err(StoreError::TrajectorySourceChanged);
        }
        let project_path = self
            .project_path
            .as_ref()
            .map(|p| p.to_str().ok_or(StoreError::InvalidWorkspaceRoot))
            .transpose()?;
        let file = revision.file.map(Uuid::into_bytes);
        let contents = revision
            .contents
            .ok_or(StoreError::TrajectorySourceChanged)?
            .into_bytes();
        let saved = transaction.execute(
            "INSERT INTO codex_indexed_workspaces(trajectory_id,source_file_id,source_contents_id,project_path,git_branch)
             SELECT id,source_file_id,source_contents_id,?4,?5 FROM visible_trajectories
             WHERE provider='codex' AND session_id=?1 AND origin='native'
               AND source_file_id=?2 AND source_contents_id IS ?3 AND content_hash=?6
               AND (NOT ?7 OR source_complete)
             ON CONFLICT(trajectory_id) DO UPDATE SET source_file_id=excluded.source_file_id,
                source_contents_id=excluded.source_contents_id,project_path=excluded.project_path,git_branch=excluded.git_branch",
            params![session.id, file, contents, project_path, self.git_branch, write.content_hash, require_full_source]
        ).map_err(database_error)?;
        if saved == 0 {
            // A preview can update a name while search keeps the full text and its verified workspace.
            if !write.source_complete
                && write.document.origin == SessionTrajectoryOrigin::Native
                && matches!(write.guard, TrajectorySourceGuard::Observed(_))
                && self.source_revision == revision
                && transaction.query_row(
                    "SELECT EXISTS (
                         SELECT 1 FROM visible_trajectories t JOIN codex_indexed_workspaces w ON w.trajectory_id=t.id
                         WHERE t.provider='codex' AND t.session_id=?1 AND t.origin='native' AND t.source_complete
                           AND t.source_file_id=?2 AND t.source_contents_id=?3
                           AND w.source_file_id=t.source_file_id AND w.source_contents_id=t.source_contents_id
                           AND w.project_path IS ?4 AND w.git_branch IS ?5
                           AND NOT EXISTS (SELECT 1 FROM codex_workspace_rechecks p WHERE p.trajectory_id=t.id
                               AND p.source_file_id=t.source_file_id AND p.source_contents_id=t.source_contents_id))",
                    params![session.id, file, contents, project_path, self.git_branch],
                    |row| row.get::<_, bool>(0),
                ).map_err(database_error)?
            {
                return Ok(());
            }
            return Err(StoreError::TrajectorySourceChanged);
        }
        transaction.execute(
            "DELETE FROM codex_workspace_rechecks WHERE source_file_id=?1 AND source_contents_id=?2
             AND trajectory_id=(SELECT id FROM visible_trajectories WHERE provider='codex' AND session_id=?3)",
            params![file, contents, session.id],
        ).map_err(database_error)?;
        transaction.execute(
            "UPDATE session_index SET project_path=?4,git_branch=?5
             WHERE provider='codex' AND session_id=?1 AND source_file_id=?2 AND source_contents_id IS ?3
               AND codex_home=(SELECT home FROM current_codex_home)
               AND (project_path IS NOT ?4 OR git_branch IS NOT ?5)",
            params![session.id, file, contents, project_path,self.git_branch],
        ).map_err(database_error)?;
        Ok(())
    }
}

fn with_index<T>(read: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
    let root = state_root()?;
    reject_symlink(&root)?;
    let path = root.join(DATABASE_FILE_NAME);
    reject_symlink(&path)?;
    let mut connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(database_error)?;
    let snapshot = connection.transaction().map_err(database_error)?;
    let format: i64 = snapshot
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(database_error)?;
    if format > crate::STORE_FORMAT {
        return Err(StoreError::NewerStoreFormat);
    }
    if format < 2 {
        snapshot
            .execute_batch(
                "CREATE TEMP VIEW visible_trajectories AS SELECT * FROM main.session_trajectories;
                 CREATE TEMP VIEW scoped_session_index AS SELECT * FROM main.session_index;",
            )
            .map_err(database_error)?;
    } else {
        crate::initialize_trajectory_scope(&snapshot, active_codex_home().as_deref())?;
    }
    read(&snapshot)
}

fn workspace_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<(Uuid, CodexWorkspace)> {
    let file = Uuid::from_bytes(row.get(0)?);
    Ok((
        file,
        CodexWorkspace {
            source_revision: SourceRevision {
                legacy_file: None,
                file: Some(file),
                contents: Some(Uuid::from_bytes(row.get(1)?)),
                missing_ancestor: None,
            },
            project_path: row.get::<_, Option<String>>(2)?.map(PathBuf::from),
            git_branch: row.get(3)?,
        },
    ))
}

fn has_rechecks(connection: &Connection) -> Result<bool> {
    connection.query_row("SELECT EXISTS (SELECT 1 FROM sqlite_schema WHERE type='table' AND name='codex_workspace_rechecks')", [], |row| row.get(0)).map_err(database_error)
}

fn has_settings_generation(connection: &Connection) -> Result<bool> {
    connection.query_row(
        "SELECT EXISTS (SELECT 1 FROM pragma_table_info('codex_workspace_rechecks') WHERE name='source_contents_id' AND pk>0)",
        [], |row| row.get(0),
    ).map_err(database_error)
}

pub(super) fn initialize(transaction: &Transaction<'_>) -> Result<()> {
    let exists: bool = transaction
        .query_row(
            "SELECT EXISTS (SELECT 1 FROM sqlite_schema WHERE type='table' AND name='codex_indexed_workspaces')",
            [], |row| row.get(0),
        )
        .map_err(database_error)?;
    let unique_file: bool = transaction
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_index_list('codex_indexed_workspaces') WHERE origin='u')",
            [], |row| row.get(0),
        )
        .map_err(database_error)?;
    if unique_file {
        transaction
            .execute_batch(
                "ALTER TABLE codex_indexed_workspaces RENAME TO previous_codex_indexed_workspaces",
            )
            .map_err(database_error)?;
    }
    if !exists || unique_file {
        transaction.execute_batch(
            "CREATE TABLE codex_indexed_workspaces (
                trajectory_id INTEGER PRIMARY KEY REFERENCES session_trajectories(id) ON DELETE CASCADE,
                source_file_id BLOB NOT NULL CHECK(length(source_file_id)=16),
                source_contents_id BLOB NOT NULL CHECK(length(source_contents_id)=16),
                project_path TEXT, git_branch TEXT
             );
             CREATE INDEX codex_indexed_workspace_file ON codex_indexed_workspaces(source_file_id);",
        ).map_err(database_error)?;
    }
    if unique_file {
        // Reindexing a format 1 document gives the same physical file a new, home-scoped owner.
        transaction.execute_batch(
            "INSERT INTO codex_indexed_workspaces SELECT * FROM previous_codex_indexed_workspaces;
             DROP TABLE previous_codex_indexed_workspaces;",
        ).map_err(database_error)?;
    }
    if !exists {
        // Version 5 could cache either the creation directory or a later turn's directory.
        // Re-read the selected source once, even if discovery currently points at another copy.
        transaction
        .execute_batch(
            "UPDATE session_trajectories AS t SET document_version=4
        WHERE t.document_version=5 AND t.provider='codex' AND t.origin='native' AND t.source_complete
          AND t.source_file_id IS NOT NULL AND t.source_contents_id IS NOT NULL;",
        )
        .map_err(database_error)?;
    }
    if !has_settings_generation(transaction)? {
        schedule_legacy_workspace_rechecks(transaction)?;
    }
    Ok(())
}

pub(super) fn install_display_upgrade_trigger(connection: &Connection) -> Result<()> {
    connection
        .execute_batch(&format!(
            "CREATE TEMP TRIGGER IF NOT EXISTS refresh_codex_display_workspace
         AFTER UPDATE OF document_version ON session_trajectories
         WHEN OLD.provider='codex' AND OLD.origin='native' AND OLD.document_version < {floor}
              AND NEW.document_version >= {floor}
         BEGIN DELETE FROM codex_indexed_workspaces WHERE trajectory_id=OLD.id; END;",
            floor = omnis_core::CODEX_DISPLAY_DOCUMENT_VERSION,
        ))
        .map_err(database_error)?;
    Ok(())
}

fn schedule_legacy_workspace_rechecks(transaction: &Transaction<'_>) -> Result<()> {
    crate::schedule_legacy_codex_title_proofs(transaction)?;
    let previous_queue = has_rechecks(transaction)?;
    if previous_queue {
        transaction
            .execute_batch(
                "ALTER TABLE codex_workspace_rechecks RENAME TO previous_codex_workspace_rechecks",
            )
            .map_err(database_error)?;
    }
    transaction.execute_batch(&format!(
        "CREATE TABLE codex_workspace_rechecks(
            trajectory_id INTEGER NOT NULL REFERENCES session_trajectories(id) ON DELETE CASCADE,
            source_file_id BLOB NOT NULL,source_contents_id BLOB NOT NULL,
            PRIMARY KEY(trajectory_id,source_file_id,source_contents_id)) WITHOUT ROWID;
         INSERT OR IGNORE INTO codex_workspace_rechecks(trajectory_id,source_file_id,source_contents_id)
             {LEGACY_RECHECKS};"
    )).map_err(database_error)?;
    if previous_queue {
        transaction.execute_batch(
            "INSERT OR IGNORE INTO codex_workspace_rechecks(trajectory_id,source_file_id,source_contents_id)
             SELECT trajectory_id,source_file_id,source_contents_id FROM previous_codex_workspace_rechecks;
             DROP TABLE previous_codex_workspace_rechecks;"
        ).map_err(database_error)?;
    }
    transaction.execute_batch(
        "CREATE INDEX codex_workspace_rechecks_source ON codex_workspace_rechecks(source_file_id,source_contents_id);
         DELETE FROM codex_indexed_workspaces AS w WHERE EXISTS (SELECT 1 FROM codex_workspace_rechecks p
             WHERE p.trajectory_id=w.trajectory_id AND p.source_file_id=w.source_file_id AND p.source_contents_id=w.source_contents_id);
         UPDATE session_index AS c SET project_path=NULL,git_branch=NULL WHERE c.provider='codex' AND EXISTS (
             SELECT 1 FROM codex_workspace_rechecks p JOIN session_trajectories t ON t.id=p.trajectory_id
             WHERE t.session_id=c.session_id AND t.codex_home IS c.codex_home
               AND p.source_file_id=c.source_file_id AND p.source_contents_id=c.source_contents_id);
         UPDATE trajectory_index_failures AS f SET document_version=5 WHERE f.provider='codex' AND f.document_version=6 AND EXISTS (
             SELECT 1 FROM codex_workspace_rechecks p JOIN session_trajectories t ON t.id=p.trajectory_id
             WHERE t.session_id=f.session_id AND t.codex_home IS f.codex_home AND t.source_updated_at IS f.source_updated_at
             AND t.source_file_id=f.source_file_id AND t.source_contents_id=f.source_contents_id
             AND p.source_file_id=f.source_file_id AND p.source_contents_id=f.source_contents_id);
         UPDATE session_trajectories AS t SET document_version=5 WHERE t.provider='codex' AND t.origin='native'
             AND t.document_version=6 AND EXISTS (SELECT 1 FROM codex_workspace_rechecks p WHERE p.trajectory_id=t.id
             AND p.source_file_id=t.source_file_id AND p.source_contents_id=t.source_contents_id);"
    ).map_err(database_error)
}

/// Rechecks version 6 documents whose workspace or opening message the reader could misidentify.
pub(super) fn recheck_legacy_projections(transaction: &Transaction<'_>) -> Result<()> {
    let stamped = "t.provider='codex' AND t.origin='native' AND t.document_version=6
                   AND t.source_file_id IS NOT NULL AND t.source_contents_id IS NOT NULL";
    let affected = format!(
        "{stamped} AND ({UNCERTAIN_LEGACY_SOURCE} OR (t.derived_title IS NOT NULL AND t.derived_title_is_local_opener IS NOT 0))"
    );
    transaction
        .execute_batch(&format!(
            "DELETE FROM codex_indexed_workspaces AS w WHERE EXISTS (
                 SELECT 1 FROM session_trajectories t WHERE t.id=w.trajectory_id
                   AND t.source_file_id=w.source_file_id AND t.source_contents_id=w.source_contents_id
                   AND {stamped} AND {UNCERTAIN_LEGACY_SOURCE});
             UPDATE trajectory_index_failures AS f SET document_version=5
             WHERE f.provider='codex' AND f.document_version=6 AND EXISTS (
                 SELECT 1 FROM session_trajectories t WHERE t.session_id=f.session_id AND t.codex_home IS f.codex_home
                   AND t.source_file_id=f.source_file_id AND t.source_contents_id=f.source_contents_id
                   AND t.source_updated_at IS f.source_updated_at AND {affected});
             UPDATE session_trajectories AS t SET document_version=5 WHERE {affected};"
        ))
        .map_err(database_error)
}

#[cfg(test)]
mod tests {
    use crate::{Store, tests::publish_workspace_fixture};

    #[test]
    fn reindexing_the_same_file_preserves_its_unowned_workspace_receipt() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("store.sqlite3");
        let store = Store::open(&path).unwrap();
        publish_workspace_fixture(&store);
        store.connection.borrow().execute_batch(
            "ALTER TABLE codex_indexed_workspaces RENAME TO current_workspaces;
             CREATE TABLE codex_indexed_workspaces (
                 trajectory_id INTEGER PRIMARY KEY REFERENCES session_trajectories(id) ON DELETE CASCADE,
                 source_file_id BLOB NOT NULL UNIQUE,source_contents_id BLOB NOT NULL,project_path TEXT,git_branch TEXT);
             INSERT INTO codex_indexed_workspaces SELECT trajectory_id,source_file_id,source_contents_id,'old receipt',NULL FROM current_workspaces;
             DROP TABLE current_workspaces;
             UPDATE session_trajectories SET codex_home=NULL;
             UPDATE session_index SET codex_home=NULL;",
        ).unwrap();
        drop(store);

        let store = Store::open(&path).unwrap();
        let (_, workspace, _) = publish_workspace_fixture(&store);
        let connection = store.connection.borrow();
        let refreshed = super::CodexWorkspace::discovery_index_from(
            &connection,
            workspace.source_revision.file,
        )
        .unwrap();
        assert_eq!(
            refreshed.verified[&workspace.source_revision.file.unwrap()].project_path,
            workspace.project_path
        );
        let legacy: String = connection.query_row(
            "SELECT w.project_path FROM codex_indexed_workspaces w JOIN session_trajectories t ON t.id=w.trajectory_id WHERE t.codex_home IS NULL",
            [], |row| row.get(0),
        ).unwrap();
        assert_eq!(legacy, "old receipt");
    }
}
