//! Workspaces established by reading a specific Codex rollout.

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
};

use omnis_ir::{Provider, SessionRef, SourceRevision};
use rusqlite::{Connection, OpenFlags, Transaction, params};
use uuid::Uuid;

use crate::{DATABASE_FILE_NAME, Result, StoreError, database_error, reject_symlink, state_root};

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
        let mut connection = open_index()?;
        let transaction = connection.transaction().map_err(database_error)?;
        Self::discovery_index_from(&transaction, file)
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
        let workspace_rows = if settings_checked {
            format!(" UNION ALL SELECT w.source_file_id,w.source_contents_id,w.project_path,w.git_branch,0
                FROM codex_indexed_workspaces w
                WHERE NOT EXISTS(SELECT 1 FROM pending p WHERE p.trajectory_id=w.trajectory_id
                    AND p.source_file_id=w.source_file_id AND p.source_contents_id=w.source_contents_id)
                {workspace_filter}")
        } else {
            String::new()
        };
        let sql = format!(
            "WITH pending AS ({pending_rows})
            SELECT source_file_id,source_contents_id,NULL,NULL,1 FROM pending{workspace_rows}"
        );
        let filter = file.map(Uuid::into_bytes);
        let mut statement = connection.prepare(&sql).map_err(database_error)?;
        let rows = statement
            .query_map(rusqlite::params_from_iter(filter.as_slice()), |row| {
                Ok((workspace_row(row)?, row.get::<_, bool>(4)?))
            })
            .map_err(database_error)?;
        let mut index = CodexWorkspaceIndex::default();
        for row in rows {
            let ((file, workspace), pending) = row.map_err(database_error)?;
            if pending {
                index.pending.insert(workspace.source_revision);
            } else {
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
        let connection = open_index()?;
        let sql = "SELECT t.source_file_id,t.source_contents_id,c.project_path,c.git_branch
             FROM session_trajectories t JOIN session_index c USING(provider,session_id)
             WHERE t.provider='codex' AND t.origin='native' AND t.source_complete=1
               AND t.document_version < ?1 AND c.project_path IS NOT NULL
               AND c.source_file_id=t.source_file_id AND c.source_contents_id=t.source_contents_id";
        let mut statement = connection.prepare(sql).map_err(database_error)?;
        statement
            .query_map([omnis_core::SEARCH_DOCUMENT_VERSION], workspace_row)
            .map_err(database_error)?
            .collect::<rusqlite::Result<_>>()
            .map_err(database_error)
    }

    pub(super) fn record(
        &self,
        transaction: &Transaction<'_>,
        session: &SessionRef,
        revision: SourceRevision,
        content_hash: &[u8],
        require_full_source: bool,
    ) -> Result<()> {
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
             SELECT id,source_file_id,source_contents_id,?4,?5 FROM session_trajectories
             WHERE provider='codex' AND session_id=?1 AND origin='native'
               AND source_file_id=?2 AND source_contents_id IS ?3 AND content_hash=?6
               AND (NOT ?7 OR source_complete)
             ON CONFLICT(trajectory_id) DO UPDATE SET source_file_id=excluded.source_file_id,
                source_contents_id=excluded.source_contents_id,project_path=excluded.project_path,git_branch=excluded.git_branch",
            params![session.id, file, contents, project_path, self.git_branch, content_hash, require_full_source]
        ).map_err(database_error)?;
        if saved == 0 {
            return Err(StoreError::TrajectorySourceChanged);
        }
        transaction.execute(
            "DELETE FROM codex_workspace_rechecks WHERE source_file_id=?1 AND source_contents_id=?2
             AND trajectory_id=(SELECT id FROM session_trajectories WHERE provider='codex' AND session_id=?3)",
            params![file, contents, session.id],
        ).map_err(database_error)?;
        transaction.execute(
            "UPDATE session_index SET project_path=?4,git_branch=?5
             WHERE provider='codex' AND session_id=?1 AND source_file_id=?2 AND source_contents_id IS ?3
               AND (project_path IS NOT ?4 OR git_branch IS NOT ?5)",
            params![session.id, file, contents, project_path,self.git_branch],
        ).map_err(database_error)?;
        Ok(())
    }
}

fn open_index() -> Result<Connection> {
    let root = state_root()?;
    reject_symlink(&root)?;
    let path = root.join(DATABASE_FILE_NAME);
    reject_symlink(&path)?;
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(database_error)
}

fn workspace_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<(Uuid, CodexWorkspace)> {
    let file = Uuid::from_bytes(row.get(0)?);
    Ok((
        file,
        CodexWorkspace {
            source_revision: SourceRevision {
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
    if !exists {
        // Version 5 could cache either the creation directory or a later turn's directory.
        // Re-read the selected source once, even if discovery currently points at another copy.
        transaction
        .execute_batch(
            "CREATE TABLE codex_indexed_workspaces (
            trajectory_id INTEGER PRIMARY KEY REFERENCES session_trajectories(id) ON DELETE CASCADE,
            source_file_id BLOB NOT NULL UNIQUE CHECK(length(source_file_id)=16),
            source_contents_id BLOB NOT NULL CHECK(length(source_contents_id)=16),
            project_path TEXT, git_branch TEXT
        );
        UPDATE session_trajectories AS t SET document_version=4
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
             WHERE t.session_id=c.session_id AND p.source_file_id=c.source_file_id AND p.source_contents_id=c.source_contents_id);
         UPDATE trajectory_index_failures AS f SET document_version=5 WHERE f.provider='codex' AND f.document_version=6 AND EXISTS (
             SELECT 1 FROM codex_workspace_rechecks p JOIN session_trajectories t ON t.id=p.trajectory_id
             WHERE t.session_id=f.session_id AND t.source_updated_at IS f.source_updated_at
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
                 SELECT 1 FROM session_trajectories t WHERE t.session_id=f.session_id
                   AND t.source_file_id=f.source_file_id AND t.source_contents_id=f.source_contents_id
                   AND t.source_updated_at IS f.source_updated_at AND {affected});
             UPDATE session_trajectories AS t SET document_version=5 WHERE {affected};"
        ))
        .map_err(database_error)
}
