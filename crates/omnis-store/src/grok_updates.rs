//! Ownership checks for Grok's update logs, whose contents are independent of catalog titles.
use super::{
    Connection, Provider, Result, SessionRef, SourceRevision, Store, StoreError, database_error,
    params,
};
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

pub(super) fn initialize(connection: &Connection) -> Result<()> {
    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS grok_rejected_updates (
                session_id TEXT NOT NULL CHECK (length(session_id) > 0),
                file_id BLOB NOT NULL CHECK (length(file_id) = 16),
                contents_id BLOB NOT NULL CHECK (length(contents_id) = 16),
                PRIMARY KEY(session_id, file_id, contents_id)
            ) WITHOUT ROWID;",
        )
        .map_err(database_error)
}

impl Store {
    /// Loads physical Grok log rejections for one indexing pass.
    ///
    /// # Errors
    ///
    /// Returns a database error if the receipts cannot be read.
    pub fn rejected_grok_updates(&self) -> Result<HashMap<String, HashSet<SourceRevision>>> {
        let connection = self.connection.borrow();
        let mut statement = connection
            .prepare("SELECT session_id, file_id, contents_id FROM grok_rejected_updates")
            .map_err(database_error)?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    SourceRevision {
                        file: Some(Uuid::from_bytes(row.get(1)?)),
                        contents: Some(Uuid::from_bytes(row.get(2)?)),
                        ..SourceRevision::default()
                    },
                ))
            })
            .map_err(database_error)?;
        let mut receipts = HashMap::<String, HashSet<SourceRevision>>::new();
        for row in rows {
            let (id, revision) = row.map_err(database_error)?;
            receipts.entry(id).or_default().insert(revision);
        }
        Ok(receipts)
    }

    /// Checks whether the selected update log contains an update from a different session.
    ///
    /// # Errors
    ///
    /// Returns a database error if the receipt cannot be read.
    pub fn grok_updates_rejected(
        &self,
        session: &SessionRef,
        log: Option<SourceRevision>,
    ) -> Result<bool> {
        rejected(&self.connection.borrow(), session, log)
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
    Ok(())
}

pub(super) fn rejected(
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
             WHERE session_id = ?1 AND file_id = ?2 AND contents_id = ?3)",
            params![session.id, file.into_bytes(), contents.into_bytes()],
            |row| row.get(0),
        )
        .map_err(database_error)
}

pub(super) fn check_publication(
    connection: &Connection,
    session: &SessionRef,
    log: Option<SourceRevision>,
    complete: bool,
) -> Result<()> {
    let Some(log) = log else {
        return Ok(());
    };
    if !rejected(connection, session, Some(log))? {
        return Ok(());
    }
    if !complete {
        return Err(StoreError::TrajectorySourceChanged);
    }
    connection
        .execute(
            "DELETE FROM grok_rejected_updates
             WHERE session_id = ?1 AND file_id = ?2 AND contents_id = ?3",
            params![
                session.id,
                log.file.map(Uuid::into_bytes),
                log.contents.map(Uuid::into_bytes)
            ],
        )
        .map_err(database_error)?;
    Ok(())
}
