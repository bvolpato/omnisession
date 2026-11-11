use super::{
    IndexedSession, Result, Store, StoreError, database_error, immediate_transaction,
    matches_catalog_row, now_timestamp, timestamp_from_db,
};
use chrono::{DateTime, Utc};
use omnis_ir::{Provider, SessionRef};
use rusqlite::{OptionalExtension, Transaction, params};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};
use uuid::Uuid;

impl Store {
    /// Returns the last completed provider-wide discovery for this reader and native home.
    ///
    /// # Errors
    ///
    /// Returns an error when the completed check cannot be read.
    pub fn completed_discovery_at(
        &self,
        provider: Provider,
        version: u32,
        home: &Path,
    ) -> Result<Option<DateTime<Utc>>> {
        let milliseconds = self
            .connection
            .borrow()
            .query_row(
                "SELECT lease.checked_at FROM session_index_reader_checks AS lease
             JOIN session_index_discovery_results USING (provider, reader_version, attempt_id)
             WHERE provider = ?1 AND reader_version = ?2 AND source_home = ?3 AND succeeded = 1",
                params![
                    provider.to_string(),
                    version,
                    home.as_os_str().as_encoded_bytes()
                ],
                |row| row.get(0),
            )
            .optional()
            .map_err(database_error)?;
        milliseconds.map(timestamp_from_db).transpose()
    }

    /// Records a completed provider-wide read. A newer discovery can supersede an older reader.
    ///
    /// # Errors
    ///
    /// Returns an error when the check cannot be published atomically.
    pub fn record_completed_discovery<'a>(
        &self,
        provider: Provider,
        version: u32,
        home: &Path,
        started_at: DateTime<Utc>,
        sessions: impl IntoIterator<Item = (IndexedSession, Option<&'a Path>)>,
    ) -> Result<()> {
        let mut connection = self.connection.borrow_mut();
        let transaction = immediate_transaction(&mut connection)?;
        let attempt = Uuid::new_v4().into_bytes();
        let accepted = transaction.execute(
            "INSERT INTO session_index_reader_checks(provider,reader_version,source_home,checked_at,attempt_id)
             VALUES (?1,?2,?3,?4,?5)
             ON CONFLICT(provider,reader_version) DO UPDATE SET source_home=excluded.source_home,
               checked_at=excluded.checked_at, attempt_id=excluded.attempt_id
             WHERE session_index_reader_checks.checked_at < ?6",
            params![provider.to_string(), version, home.as_os_str().as_encoded_bytes(), now_timestamp(), attempt, started_at.timestamp_millis()],
        ).map_err(database_error)?;
        if accepted != 0 {
            transaction.execute("DELETE FROM session_index_upgrade_sources WHERE provider=?1 AND reader_version=?2",
                params![provider.to_string(), version]).map_err(database_error)?;
            for (metadata, path) in sessions {
                if metadata.session.provider != provider
                    || !matches_catalog_row(&transaction, &metadata)?
                {
                    return Err(StoreError::TrajectorySourceChanged);
                }
                record_discovery_home(&transaction, &metadata.session, home)?;
                if let Some(path) = path {
                    transaction.execute(
                        "INSERT INTO session_index_upgrade_sources(provider,reader_version,session_id,source_path)
                         SELECT ?1,?2,?3,?4 WHERE NOT EXISTS(SELECT 1 FROM session_trajectories
                             WHERE provider=?1 AND session_id=?3 AND document_version>=?2)
                         AND NOT EXISTS(SELECT 1 FROM trajectory_index_failures
                             WHERE provider=?1 AND session_id=?3 AND document_version>=?2)",
                         params![provider.to_string(), version, metadata.session.id, path.to_str()],
                    ).map_err(database_error)?;
                }
            }
            transaction.execute(
                "INSERT INTO session_index_discovery_results(provider,reader_version,attempt_id,succeeded) VALUES (?1,?2,?3,1)
                 ON CONFLICT(provider,reader_version) DO UPDATE SET attempt_id=excluded.attempt_id,succeeded=1",
                params![provider.to_string(), version, attempt],
            ).map_err(database_error)?;
        }
        transaction.commit().map_err(database_error)
    }

    /// Returns known catalog rows that the selected home cannot supply.
    ///
    /// # Errors
    ///
    /// Returns an error if the stored home assignments cannot be read.
    pub fn excluded_catalog_ids(&self, provider: Provider, home: &Path) -> Result<HashSet<String>> {
        let connection = self.connection.borrow();
        let mut statement = connection.prepare(
            "SELECT session_id FROM native_catalog_homes WHERE provider=?1 AND (home<>?2 OR NOT verified)"
        ).map_err(database_error)?;
        statement
            .query_map(
                params![provider.to_string(), home.as_os_str().as_encoded_bytes()],
                |row| row.get(0),
            )
            .map_err(database_error)?
            .collect::<rusqlite::Result<_>>()
            .map_err(database_error)
    }

    /// Returns the files from the last listing whose search upgrade has not finished.
    ///
    /// # Errors
    ///
    /// Returns an error if the recorded sources cannot be read.
    pub fn pending_discovery_sources(
        &self,
        provider: Provider,
        version: u32,
        home: &Path,
    ) -> Result<Vec<(String, Option<PathBuf>)>> {
        let connection = self.connection.borrow();
        let mut statement = connection.prepare(
            "SELECT pending.session_id, pending.source_path FROM session_index_upgrade_sources pending
             JOIN session_index_reader_checks lease USING(provider, reader_version)
             JOIN session_index_discovery_results result USING(provider,reader_version,attempt_id)
             JOIN native_catalog_homes origin USING(provider,session_id)
             WHERE pending.provider=?1 AND pending.reader_version=?2 AND lease.source_home=?3
               AND origin.home=lease.source_home AND origin.verified AND result.succeeded
               AND NOT EXISTS(SELECT 1 FROM session_trajectories d WHERE d.provider=pending.provider
                              AND d.session_id=pending.session_id AND d.document_version>=pending.reader_version)
               AND NOT EXISTS(SELECT 1 FROM trajectory_index_failures f WHERE f.provider=pending.provider
                              AND f.session_id=pending.session_id AND f.document_version>=pending.reader_version)"
        ).map_err(database_error)?;
        statement
            .query_map(
                params![
                    provider.to_string(),
                    version,
                    home.as_os_str().as_encoded_bytes()
                ],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get::<_, Option<String>>(1)?.map(PathBuf::from),
                    ))
                },
            )
            .map_err(database_error)?
            .collect::<rusqlite::Result<_>>()
            .map_err(database_error)
    }
}

pub(super) fn record_discovery_home(
    transaction: &Transaction<'_>,
    session: &SessionRef,
    home: &Path,
) -> Result<()> {
    if !matches!(
        session.provider,
        Provider::Antigravity | Provider::AntigravityDesktop
    ) {
        return Err(StoreError::InvalidSessionReference);
    }
    transaction
        .prepare_cached(
            "INSERT INTO native_catalog_homes(provider,session_id,home) VALUES(?1,?2,?3)
         ON CONFLICT(provider,session_id) DO UPDATE SET home=excluded.home, verified=1
         WHERE native_catalog_homes.home IS NOT excluded.home OR NOT native_catalog_homes.verified",
        )
        .and_then(|mut statement| {
            statement.execute(params![
                session.provider.to_string(),
                session.id,
                home.as_os_str().as_encoded_bytes()
            ])
        })
        .map_err(database_error)?;
    Ok(())
}

pub(super) fn initialize_session_index_checks(transaction: &Transaction<'_>) -> Result<()> {
    transaction
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS session_index_checks (
            provider TEXT PRIMARY KEY,
            checked_at INTEGER NOT NULL
        ) WITHOUT ROWID;",
        )
        .map_err(database_error)
}

pub(super) fn initialize_discovery_checks(transaction: &Transaction<'_>) -> Result<()> {
    let enrolled = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='session_index_upgrade_sources')",
        [], |row| row.get::<_, bool>(0),
    ).map_err(database_error)?;
    transaction
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS session_index_reader_checks (
            provider TEXT NOT NULL,
            reader_version INTEGER NOT NULL,
            source_home BLOB NOT NULL,
            checked_at INTEGER NOT NULL,
            attempt_id BLOB NOT NULL,
            PRIMARY KEY (provider, reader_version)
        ) WITHOUT ROWID;

        CREATE TABLE IF NOT EXISTS session_index_discovery_results (
            provider TEXT NOT NULL,
            reader_version INTEGER NOT NULL,
            attempt_id BLOB NOT NULL,
            succeeded INTEGER NOT NULL CHECK (succeeded IN (0, 1)),
            PRIMARY KEY (provider, reader_version),
            FOREIGN KEY (provider, reader_version)
                REFERENCES session_index_reader_checks(provider, reader_version) ON DELETE CASCADE
        ) WITHOUT ROWID;
        CREATE TABLE IF NOT EXISTS session_index_upgrade_sources(
            provider TEXT NOT NULL, reader_version INTEGER NOT NULL, session_id TEXT NOT NULL, source_path TEXT,
            PRIMARY KEY(provider, reader_version, session_id),
            FOREIGN KEY(provider, reader_version) REFERENCES session_index_reader_checks(provider, reader_version) ON DELETE CASCADE
        ) WITHOUT ROWID;
        CREATE TABLE IF NOT EXISTS native_catalog_homes (
                    provider TEXT NOT NULL,
                    session_id TEXT NOT NULL,
                    home BLOB NOT NULL,
                    verified INTEGER NOT NULL DEFAULT 1,
                    PRIMARY KEY (provider, session_id),
                    FOREIGN KEY (provider, session_id) REFERENCES session_index (provider, session_id)
                        ON DELETE CASCADE
                ) WITHOUT ROWID;
        CREATE TRIGGER IF NOT EXISTS forget_changed_catalog_home
                AFTER UPDATE ON session_index
                WHEN NEW.provider IN ('antigravity', 'antigravity-desktop')
                 AND (OLD.title, OLD.project_path, OLD.git_branch, OLD.created_at, OLD.updated_at, OLD.event_count, OLD.source_file_id, OLD.source_contents_id)
                     IS NOT (NEW.title, NEW.project_path, NEW.git_branch, NEW.created_at, NEW.updated_at, NEW.event_count, NEW.source_file_id, NEW.source_contents_id)
                BEGIN
                    UPDATE native_catalog_homes SET verified = 0
                    WHERE provider = NEW.provider AND session_id = NEW.session_id AND verified = 1;
                END;
        CREATE TRIGGER IF NOT EXISTS expire_changed_catalog_receipt
        AFTER UPDATE OF verified ON native_catalog_homes WHEN OLD.verified AND NOT NEW.verified
        BEGIN
            UPDATE session_index_discovery_results SET succeeded=0
            WHERE provider=OLD.provider AND EXISTS (
                SELECT 1 FROM session_index_reader_checks lease
                WHERE lease.provider=OLD.provider AND lease.source_home=OLD.home
                  AND lease.reader_version=session_index_discovery_results.reader_version);
        END;
        CREATE TRIGGER IF NOT EXISTS expire_rebound_catalog_home
                AFTER UPDATE OF home ON native_catalog_homes WHEN OLD.home IS NOT NEW.home
                BEGIN
                    DELETE FROM session_index_reader_checks WHERE provider = OLD.provider AND source_home = OLD.home;
                END;",
        )
        .map_err(database_error)?;
    if !enrolled {
        transaction.execute("DELETE FROM session_index_reader_checks WHERE provider IN ('antigravity','antigravity-desktop')", []).map_err(database_error)?;
    }
    Ok(())
}
