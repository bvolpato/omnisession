use super::{
    IndexedSession, Result, Store, StoreError, database_error, immediate_transaction,
    matches_catalog_row, now_timestamp, timestamp_from_db,
};
use chrono::{DateTime, Utc};
use omnis_ir::{Provider, SessionRef, SourceRevision};
use rusqlite::{OptionalExtension, Transaction, params};
use std::{
    collections::{HashMap, HashSet},
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
                    let revision = metadata.source_revision.unwrap_or_default();
                    transaction.execute(
                        "INSERT INTO session_index_upgrade_sources(provider,reader_version,session_id,source_path)
                         SELECT ?1,?2,?3,?4 WHERE NOT EXISTS(SELECT 1 FROM visible_trajectories
                             WHERE provider=?1 AND session_id=?3 AND (document_version>?2 OR (document_version=?2
                               AND (?5 IS NULL OR (source_file_id IS ?5 AND source_contents_id IS ?6)))))
                         AND NOT EXISTS(SELECT 1 FROM scoped_trajectory_failures
                             WHERE provider=?1 AND session_id=?3 AND (document_version>?2 OR (document_version=?2
                               AND (?5 IS NULL OR (source_file_id IS ?5 AND source_contents_id IS ?6)))))",
                         params![provider.to_string(), version, metadata.session.id, path.to_str(),
                            revision.file.map(Uuid::into_bytes), revision.contents.map(Uuid::into_bytes)],
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

    /// Returns verified search documents whose native catalog lost its home verification.
    ///
    /// These rows contain only the document identity. The catalog cannot establish a title or workspace.
    ///
    /// # Errors
    ///
    /// Returns an error if the document receipts cannot be read.
    pub fn uncatalogued_search_documents(
        &self,
        provider: Provider,
        home: &Path,
    ) -> Result<Vec<IndexedSession>> {
        let connection = self.connection.borrow();
        let mut statement = connection.prepare_cached(
            "SELECT doc.session_id, doc.source_file_id, doc.missing_ancestor_id, doc.source_contents_id
             FROM native_catalog_homes c
             JOIN scoped_session_index catalog USING(provider,session_id)
             JOIN visible_trajectories doc ON doc.provider=c.provider AND doc.session_id=c.session_id AND doc.origin='native'
             JOIN native_search_document_homes h ON h.trajectory_id=doc.id AND h.source_home=c.home
               AND h.source_file=doc.source_file_id AND h.verification=1
             JOIN antigravity_read_catalogs r ON r.trajectory_id=doc.id AND r.source_home=h.source_home AND r.source_file=h.source_file
               AND r.source_contents=doc.source_contents_id AND r.content_hash=doc.content_hash AND r.document_version=doc.document_version
             WHERE c.provider=?1 AND c.home=?2 AND NOT c.verified
               AND (catalog.source_file_id IS NULL OR catalog.source_file_id=doc.source_file_id)
               AND (catalog.source_contents_id IS NULL OR catalog.source_contents_id=doc.source_contents_id)"
        ).map_err(database_error)?;
        statement
            .query_map(
                params![provider.to_string(), home.as_os_str().as_encoded_bytes()],
                |row| {
                    Ok(IndexedSession {
                        source_path: None,
                        session: SessionRef::new(provider, row.get::<_, String>(0)?),
                        source_revision: Some(super::source_revision_from_row(row, 1)?),
                        title: None,
                        project_path: None,
                        git_branch: None,
                        created_at: None,
                        updated_at: None,
                        updated_at_approximate: true,
                        event_count: 0,
                    })
                },
            )
            .map_err(database_error)?
            .collect::<rusqlite::Result<_>>()
            .map_err(database_error)
    }

    /// Returns the stored revisions that belong to the selected native home.
    /// A selected file can identify a document from a reader that did not record its home.
    ///
    /// # Errors
    ///
    /// Returns an error if the document or its home cannot be read.
    pub fn selected_document_revisions(
        &self,
        provider: Provider,
        home: &Path,
        sessions: &[(&str, SourceRevision)],
    ) -> Result<HashMap<String, SourceRevision>> {
        let picked: Vec<_> = sessions
            .iter()
            .map(|(id, revision)| {
                (
                    id,
                    revision.file.map(|f| f.simple().to_string()),
                    revision.legacy_file.map(|f| f.simple().to_string()),
                )
            })
            .collect();
        let json =
            serde_json::to_string(&picked).map_err(|_| StoreError::InvalidSessionReference)?;
        let connection = self.connection.borrow();
        let sql = format!(
            "SELECT trajectories.session_id, trajectories.source_file_id, trajectories.missing_ancestor_id, {contents}
             FROM json_each(?1) AS picked
             JOIN visible_trajectories AS trajectories ON trajectories.provider=?2 AND trajectories.session_id=(picked.value ->> 0)
             LEFT JOIN native_search_document_homes AS origin ON origin.trajectory_id=trajectories.id AND origin.source_file=trajectories.source_file_id
             {gap_join}
             WHERE trajectories.source_file_id IS NOT NULL AND CASE WHEN origin.verification=1 THEN origin.source_home=?3 AND ((picked.value ->> 1) IS NULL OR hex(trajectories.source_file_id) IN (upper(picked.value ->> 1),upper(picked.value ->> 2)))
               ELSE hex(trajectories.source_file_id) IN (upper(picked.value ->> 1),upper(picked.value ->> 2)) END"
            ,contents = super::SEARCH_ACTIVE_CONTENTS, gap_join = super::SEARCH_GAP_JOIN
        );
        let mut statement = connection.prepare_cached(&sql).map_err(database_error)?;
        statement
            .query_map(
                params![
                    json,
                    provider.to_string(),
                    home.as_os_str().as_encoded_bytes()
                ],
                |row| Ok((row.get(0)?, super::source_revision_from_row(row, 1)?)),
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
             JOIN scoped_session_index catalog USING(provider,session_id)
             WHERE pending.provider=?1 AND pending.reader_version=?2 AND lease.source_home=?3
               AND origin.home=lease.source_home AND origin.verified AND result.succeeded
               AND NOT EXISTS(SELECT 1 FROM visible_trajectories d WHERE d.provider=pending.provider
                              AND d.session_id=pending.session_id AND (d.document_version>pending.reader_version
                              OR (d.document_version=pending.reader_version AND (catalog.source_file_id IS NULL
                                  OR (d.source_file_id IS catalog.source_file_id AND d.source_contents_id IS catalog.source_contents_id)))))
               AND NOT EXISTS(SELECT 1 FROM scoped_trajectory_failures f WHERE f.provider=pending.provider
                              AND f.session_id=pending.session_id AND (f.document_version>pending.reader_version
                              OR (f.document_version=pending.reader_version AND (catalog.source_file_id IS NULL
                                  OR (f.source_file_id IS catalog.source_file_id AND f.source_contents_id IS catalog.source_contents_id)))))"
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

pub(super) fn record_verified_document_home(
    transaction: &Transaction<'_>,
    session: &SessionRef,
    revision: SourceRevision,
    home: &Path,
) -> Result<()> {
    transaction.prepare_cached(
        "INSERT INTO native_search_document_homes(trajectory_id,source_file,source_home,verification)
         SELECT id,source_file_id,?5,1 FROM visible_trajectories
         WHERE provider=?1 AND session_id=?2 AND origin='native'
           AND source_file_id=?3 AND source_contents_id IS ?4
         ON CONFLICT(trajectory_id) DO UPDATE SET source_file=excluded.source_file,
           source_home=excluded.source_home, verification=1, declared_steps=NULL
         WHERE (source_file,source_home,verification) IS NOT (excluded.source_file,excluded.source_home,1)"
    ).and_then(|mut statement| statement.execute(params![
        session.provider.to_string(), session.id, revision.file.map(Uuid::into_bytes),
        revision.contents.map(Uuid::into_bytes), home.as_os_str().as_encoded_bytes()
    ])).map_err(database_error)?;
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
        CREATE TABLE IF NOT EXISTS native_search_document_homes (
            trajectory_id INTEGER PRIMARY KEY REFERENCES session_trajectories(id) ON DELETE CASCADE,
            source_file BLOB NOT NULL,
            source_home BLOB NOT NULL,
            verification INTEGER NOT NULL DEFAULT 0 CHECK(verification IN (-1,0,1)),
            declared_steps INTEGER CHECK(declared_steps >= 0)
        );",
        )
        .map_err(database_error)?;
    initialize_catalog_homes(transaction)?;
    let (verified, steps): (bool, bool) = transaction.query_row(
        "SELECT count(*) FILTER (WHERE name='verification'), count(*) FILTER (WHERE name='declared_steps')
         FROM pragma_table_info('native_search_document_homes')", [], |row| Ok((row.get(0)?, row.get(1)?)),
    ).map_err(database_error)?;
    if !verified {
        transaction.execute_batch("ALTER TABLE native_search_document_homes ADD COLUMN verification INTEGER NOT NULL DEFAULT 0 CHECK(verification IN (-1,0,1));").map_err(database_error)?;
    }
    if !steps {
        transaction.execute_batch("ALTER TABLE native_search_document_homes ADD COLUMN declared_steps INTEGER CHECK(declared_steps >= 0);").map_err(database_error)?;
    }
    if !enrolled {
        transaction.execute_batch(
            "DELETE FROM session_index_discovery_results WHERE provider IN ('antigravity','antigravity-desktop');
             DELETE FROM session_index_reader_checks WHERE provider IN ('antigravity','antigravity-desktop');"
        ).map_err(database_error)?;
    }
    Ok(())
}

fn initialize_catalog_homes(transaction: &Transaction<'_>) -> Result<()> {
    let legacy_home_key: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_foreign_key_list('native_catalog_homes') WHERE \"table\"='session_index')",
        [], |row| row.get(0),
    ).map_err(database_error)?;
    if legacy_home_key {
        transaction
            .execute_batch(
                "DROP TRIGGER IF EXISTS forget_changed_catalog_home;
             ALTER TABLE native_catalog_homes RENAME TO previous_native_catalog_homes;",
            )
            .map_err(database_error)?;
    }
    transaction
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS native_catalog_homes (
                    provider TEXT NOT NULL,
                    session_id TEXT NOT NULL,
                    home BLOB NOT NULL,
                    verified INTEGER NOT NULL DEFAULT 1,
                    PRIMARY KEY (provider, session_id)
                ) WITHOUT ROWID;",
        )
        .map_err(database_error)?;
    if legacy_home_key {
        transaction
            .execute_batch(
                "INSERT INTO native_catalog_homes(provider,session_id,home,verified)
             SELECT provider,session_id,home,verified FROM previous_native_catalog_homes;
             DROP TABLE previous_native_catalog_homes;",
            )
            .map_err(database_error)?;
    }
    transaction
        .execute_batch(
            "CREATE TRIGGER IF NOT EXISTS remove_discovery_home_after_catalog_delete
        AFTER DELETE ON session_index WHEN OLD.provider IN ('antigravity','antigravity-desktop')
        BEGIN
            DELETE FROM native_catalog_homes WHERE provider=OLD.provider AND session_id=OLD.session_id;
        END;
        CREATE TRIGGER IF NOT EXISTS require_catalog_for_discovery_home
        BEFORE INSERT ON native_catalog_homes
        WHEN NOT EXISTS(SELECT 1 FROM session_index WHERE provider=NEW.provider AND session_id=NEW.session_id)
        BEGIN
            SELECT RAISE(ABORT, 'missing discovery catalog row');
        END;
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
    Ok(())
}
