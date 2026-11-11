//! Workspaces established by reading a specific Codex rollout.

use std::{collections::HashMap, path::PathBuf};

use omnis_ir::{Provider, SessionRef, SourceRevision};
use rusqlite::{Connection, OpenFlags, Transaction, params};
use uuid::Uuid;

use crate::{DATABASE_FILE_NAME, Result, StoreError, database_error, reject_symlink, state_root};

/// A workspace and branch read from the selected rollout revision.
#[derive(Debug, Clone)]
pub struct CodexWorkspace {
    pub source_revision: SourceRevision,
    pub project_path: Option<PathBuf>,
    pub git_branch: Option<String>,
}

impl CodexWorkspace {
    /// Reads verified workspaces without walking or opening any provider rollouts.
    ///
    /// # Errors
    ///
    /// Returns an error if the workspace index does not exist or cannot be read.
    pub fn read_index(file: Option<Uuid>) -> Result<HashMap<Uuid, Self>> {
        let root = state_root()?;
        reject_symlink(&root)?;
        let path = root.join(DATABASE_FILE_NAME);
        reject_symlink(&path)?;
        let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(database_error)?;
        let mut sql = "SELECT source_file_id, source_contents_id, project_path, git_branch FROM codex_indexed_workspaces".to_owned();
        if file.is_some() {
            sql.push_str(" WHERE source_file_id=?1");
        }
        let filter = file.map(Uuid::into_bytes);
        let mut statement = connection.prepare(&sql).map_err(database_error)?;
        statement
            .query_map(rusqlite::params_from_iter(filter.as_slice()), |row| {
                let file = Uuid::from_bytes(row.get(0)?);
                Ok((
                    file,
                    Self {
                        source_revision: SourceRevision {
                            file: Some(file),
                            contents: Some(Uuid::from_bytes(row.get(1)?)),
                            missing_ancestor: None,
                        },
                        project_path: row.get::<_, Option<String>>(2)?.map(PathBuf::from),
                        git_branch: row.get(3)?,
                    },
                ))
            })
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
             ON CONFLICT(trajectory_id) DO UPDATE SET source_file_id=excluded.source_file_id,
                source_contents_id=excluded.source_contents_id,project_path=excluded.project_path,git_branch=excluded.git_branch",
            params![session.id, file, contents, project_path, self.git_branch, content_hash]
        ).map_err(database_error)?;
        if saved == 0 {
            return Err(StoreError::TrajectorySourceChanged);
        }
        transaction.execute(
            "UPDATE session_index SET project_path=?4,git_branch=?5
             WHERE provider='codex' AND session_id=?1 AND source_file_id=?2 AND source_contents_id IS ?3
               AND (project_path IS NOT ?4 OR git_branch IS NOT ?5)",
            params![session.id, file, contents, project_path,self.git_branch],
        ).map_err(database_error)?;
        Ok(())
    }
}

pub(super) fn initialize(transaction: &Transaction<'_>) -> Result<()> {
    let exists: bool = transaction
        .query_row(
            "SELECT EXISTS (SELECT 1 FROM sqlite_schema WHERE type='table' AND name='codex_indexed_workspaces')",
            [], |row| row.get(0),
        )
        .map_err(database_error)?;
    if exists {
        return Ok(());
    }
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
        .map_err(database_error)
}
