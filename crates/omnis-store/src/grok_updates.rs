//! Ownership checks for Grok's update logs, whose contents are independent of catalog titles.
use super::{
    Connection, Provider, Result, SessionRef, SourceRevision, Store, StoreError, database_error,
    params,
};
use std::collections::HashSet;

pub(super) fn initialize(connection: &Connection) -> Result<()> {
    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS grok_rejected_updates (
                session_id TEXT NOT NULL CHECK (length(session_id) > 0),
                file_id BLOB NOT NULL CHECK (length(file_id) = 16),
                contents_id BLOB NOT NULL CHECK (length(contents_id) = 16),
                PRIMARY KEY(session_id, file_id, contents_id)
            ) WITHOUT ROWID;
            CREATE TABLE IF NOT EXISTS grok_verified_updates (
                session_id TEXT PRIMARY KEY,
                file_id BLOB NOT NULL CHECK (length(file_id) = 16),
                contents_id BLOB NOT NULL CHECK (length(contents_id) = 16)
            ) WITHOUT ROWID;",
        )
        .map_err(database_error)
}

impl Store {
    /// Lists sessions whose update ownership must be rechecked after a source change.
    ///
    /// # Errors
    ///
    /// Returns a database error if the receipts cannot be read.
    pub fn grok_sessions_with_rejected_updates(&self) -> Result<HashSet<String>> {
        let connection = self.connection.borrow();
        let mut statement = connection
            .prepare("SELECT DISTINCT session_id FROM grok_rejected_updates")
            .map_err(database_error)?;
        statement
            .query_map([], |row| row.get(0))
            .map_err(database_error)?
            .collect::<rusqlite::Result<_>>()
            .map_err(database_error)
    }

    /// Records a completed ownership scan for the selected update file and revision.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid identity or a database failure.
    pub fn record_verified_grok_updates(
        &self,
        session: &SessionRef,
        log: SourceRevision,
    ) -> Result<()> {
        let (Some(file), Some(contents)) = (log.file, log.contents) else {
            return Err(StoreError::InvalidSessionReference);
        };
        if session.provider != Provider::Grok {
            return Err(StoreError::InvalidSessionReference);
        }
        self.connection
            .borrow()
            .execute(
                "INSERT INTO grok_verified_updates(session_id, file_id, contents_id)
             VALUES (?1, ?2, ?3) ON CONFLICT(session_id) DO UPDATE SET
             file_id = excluded.file_id, contents_id = excluded.contents_id",
                params![session.id, file.into_bytes(), contents.into_bytes()],
            )
            .map_err(database_error)?;
        Ok(())
    }

    /// Checks whether a previous ownership conflict requires a full check of this selected log.
    ///
    /// # Errors
    ///
    /// Returns a database error if the receipt cannot be read.
    pub fn grok_updates_need_verification(
        &self,
        session: &SessionRef,
        log: Option<SourceRevision>,
    ) -> Result<bool> {
        requires_verification(&self.connection.borrow(), session, log)
    }
}

pub(super) fn record(
    connection: &Connection,
    session: &SessionRef,
    log: SourceRevision,
) -> Result<()> {
    let (Some(file), Some(contents)) = (log.file, log.contents) else {
        return Err(StoreError::InvalidSessionReference);
    };
    if session.provider != Provider::Grok {
        return Err(StoreError::InvalidSessionReference);
    }
    connection
        .execute(
            "INSERT OR IGNORE INTO grok_rejected_updates(session_id, file_id, contents_id)
         VALUES (?1, ?2, ?3)",
            params![session.id, file.into_bytes(), contents.into_bytes()],
        )
        .map_err(database_error)?;
    connection
        .execute(
            "DELETE FROM grok_verified_updates WHERE session_id = ?1 AND file_id = ?2 AND contents_id = ?3",
            params![session.id, file.into_bytes(), contents.into_bytes()],
        )
        .map_err(database_error)?;
    Ok(())
}

pub(super) fn requires_verification(
    connection: &Connection,
    session: &SessionRef,
    log: Option<SourceRevision>,
) -> Result<bool> {
    let Some(SourceRevision {
        file: Some(file),
        contents: Some(contents),
        ..
    }) = log
    else {
        return Ok(false);
    };
    if session.provider != Provider::Grok {
        return Ok(false);
    }
    connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM grok_rejected_updates
             WHERE session_id = ?1)
             AND NOT EXISTS(SELECT 1 FROM grok_verified_updates
             WHERE session_id = ?1 AND file_id = ?2 AND contents_id = ?3)",
            params![session.id, file.into_bytes(), contents.into_bytes()],
            |row| row.get(0),
        )
        .map_err(database_error)
}
