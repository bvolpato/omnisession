use rusqlite::{OptionalExtension, Transaction, params};
use uuid::Uuid;

use crate::{FIRST_PRIMARY_LEGACY_CHUNK, Provider, Result, SessionRef, StoreError, database_error};

use omnis_ir::ANTIGRAVITY_DATABASE_STEP_LIMIT as MAX_DATABASE_STEPS;

const RECEIPT_TABLES: [(Provider, &str); 3] = [
    (Provider::AntigravityDesktop, "desktop_search_receipts"),
    (Provider::Antigravity, "antigravity_cli_search_receipts"),
    (Provider::AntigravityIde, "antigravity_ide_search_receipts"),
];

fn receipt_table(provider: Provider) -> Option<&'static str> {
    RECEIPT_TABLES
        .into_iter()
        .find_map(|(owner, table)| (owner == provider).then_some(table))
}

pub(crate) fn initialize(tx: &Transaction<'_>) -> Result<()> {
    let old_cleanup = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='trigger' AND name='antigravity_gap_projections_removed'
         AND instr(sql,'old.first_chunk>=')=0)",
        [], |row| row.get::<_, bool>(0),
    ).map_err(database_error)?;
    if old_cleanup {
        tx.execute_batch("DROP TRIGGER antigravity_gap_projections_removed;")
            .map_err(database_error)?;
    }
    tx.execute_batch(&format!(
        "CREATE TABLE IF NOT EXISTS antigravity_read_catalogs (
            trajectory_id INTEGER PRIMARY KEY REFERENCES session_trajectories(id) ON DELETE CASCADE,
            source_home BLOB NOT NULL, source_file BLOB NOT NULL, source_contents BLOB NOT NULL,
            catalog_hash TEXT NOT NULL, document_version INTEGER NOT NULL, content_hash BLOB NOT NULL
        );
        CREATE TABLE IF NOT EXISTS antigravity_search_gaps (
            trajectory_id INTEGER PRIMARY KEY REFERENCES session_trajectories(id) ON DELETE CASCADE,
            source_home BLOB NOT NULL, source_file BLOB NOT NULL, indexed_contents BLOB NOT NULL,
            indexed_hash BLOB NOT NULL, observed_contents BLOB NOT NULL
        );
        CREATE TABLE IF NOT EXISTS antigravity_gap_projections (
            trajectory_id INTEGER PRIMARY KEY REFERENCES antigravity_search_gaps(trajectory_id) ON DELETE CASCADE,
            first_chunk INTEGER NOT NULL, source_byte_count INTEGER NOT NULL,
            indexed_byte_count INTEGER NOT NULL, content_hash BLOB NOT NULL, document_version INTEGER NOT NULL,
            derived_title TEXT, source_updated_at INTEGER NOT NULL
        );
        CREATE TRIGGER IF NOT EXISTS antigravity_gap_revision_changed AFTER UPDATE ON antigravity_search_gaps
        WHEN (old.source_home,old.source_file,old.indexed_contents,old.indexed_hash,old.observed_contents)
          IS NOT (new.source_home,new.source_file,new.indexed_contents,new.indexed_hash,new.observed_contents)
        BEGIN
            DELETE FROM antigravity_gap_projections WHERE trajectory_id=old.trajectory_id;
        END;
        CREATE TRIGGER IF NOT EXISTS antigravity_gap_chunks_replaced
        AFTER DELETE ON session_trajectory_chunk_rows
        WHEN old.chunk_index=0 AND EXISTS (
            SELECT 1 FROM antigravity_gap_projections WHERE trajectory_id=old.trajectory_id
        )
        BEGIN
            DELETE FROM antigravity_search_gaps WHERE trajectory_id=old.trajectory_id;
        END;
        CREATE TRIGGER IF NOT EXISTS antigravity_gap_projections_removed AFTER DELETE ON antigravity_gap_projections
        BEGIN
            DELETE FROM session_trajectory_chunk_rows WHERE trajectory_id=old.trajectory_id AND chunk_index>=old.first_chunk
              AND (old.first_chunk>={FIRST_PRIMARY_LEGACY_CHUNK} OR chunk_index<{FIRST_PRIMARY_LEGACY_CHUNK});
        END;",
    ))
    .map_err(database_error)?;
    for (provider, table) in RECEIPT_TABLES {
        let legacy_key: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_foreign_key_list(?1) WHERE \"table\"='session_trajectories' AND \"to\"='session_id')",
            [table], |row| row.get(0),
        ).map_err(database_error)?;
        if legacy_key {
            tx.execute_batch(&format!("ALTER TABLE {table} RENAME TO previous_{table}"))
                .map_err(database_error)?;
        }
        tx.execute_batch(&format!(
        "CREATE TABLE IF NOT EXISTS {table} (
             provider TEXT NOT NULL DEFAULT '{provider}' CHECK(provider='{provider}'),
             session_id TEXT NOT NULL,
             trajectory_id INTEGER NOT NULL REFERENCES session_trajectories(id) ON DELETE CASCADE,
             source_home BLOB NOT NULL,
             source_file BLOB NOT NULL CHECK(length(source_file)=16),
             source_contents BLOB NOT NULL CHECK(length(source_contents)=16),
             content_hash BLOB NOT NULL CHECK(length(content_hash)=32),
             step_proof BLOB NOT NULL CHECK(length(step_proof)%16=0 AND length(step_proof)<=1600000),
             PRIMARY KEY(session_id, source_home, source_file)
         ) WITHOUT ROWID;",
    )).map_err(database_error)?;
        if legacy_key {
            tx.execute_batch(&format!(
                "INSERT INTO {table}(provider,session_id,trajectory_id,source_home,source_file,source_contents,content_hash,step_proof)
                 SELECT r.provider,r.session_id,(SELECT id FROM session_trajectories WHERE provider=r.provider AND session_id=r.session_id),
                        r.source_home,r.source_file,r.source_contents,r.content_hash,r.step_proof FROM previous_{table} r;
                 DROP TABLE previous_{table};"
            )).map_err(database_error)?;
        }
    }
    Ok(())
}

pub(super) fn promote_gap_projections(tx: &Transaction<'_>) -> Result<()> {
    let pending: bool = tx
        .query_row(
            "SELECT EXISTS (SELECT 1 FROM antigravity_gap_projections)",
            [],
            |row| row.get(0),
        )
        .map_err(database_error)?;
    if !pending {
        return clear_unverified_preview_catalogs(tx);
    }
    tx.execute_batch(&format!(
        "CREATE TEMP TABLE promoted_gap_documents AS
         SELECT trajectories.id, active_projection.first_chunk, gap.observed_contents,
                active_projection.content_hash,active_projection.source_byte_count,
                active_projection.indexed_byte_count,active_projection.derived_title,
                active_projection.source_updated_at,active_projection.document_version
         FROM antigravity_gap_projections p
         JOIN session_trajectories trajectories ON trajectories.id=p.trajectory_id
         {}
         WHERE active_projection.trajectory_id IS NOT NULL AND trajectories.origin='native'
           AND trajectories.provider IN ('antigravity','antigravity-desktop');
         CREATE UNIQUE INDEX temp.promoted_gap_ids ON promoted_gap_documents(id);
         CREATE TEMP TABLE promoted_gap_chunks AS
         SELECT d.id, c.chunk_index-d.first_chunk AS chunk_index, c.stored_text
         FROM promoted_gap_documents d JOIN session_trajectory_chunk_rows c
           ON c.trajectory_id=d.id AND c.chunk_index>=d.first_chunk
          AND (d.first_chunk>={FIRST_PRIMARY_LEGACY_CHUNK} OR c.chunk_index<{FIRST_PRIMARY_LEGACY_CHUNK});
         DELETE FROM antigravity_search_gaps WHERE trajectory_id IN (
             SELECT trajectory_id FROM antigravity_gap_projections
         );
         DELETE FROM antigravity_gap_projections;
         DELETE FROM session_trajectory_chunk_rows WHERE chunk_index<{FIRST_PRIMARY_LEGACY_CHUNK}
           AND trajectory_id IN (SELECT id FROM promoted_gap_documents);
         INSERT INTO session_trajectory_chunk_rows (trajectory_id,chunk_index,stored_text)
         SELECT id,chunk_index,stored_text FROM promoted_gap_chunks;
         UPDATE session_trajectories AS doc SET
             source_contents_id=d.observed_contents, content_hash=d.content_hash,
             source_updated_at=d.source_updated_at,source_updated_submillis=0,
             source_complete=0,complete=0,provider_limited=0,title_authoritative=0,
             source_byte_count=d.source_byte_count,indexed_byte_count=d.indexed_byte_count,
             truncation_strategy='source_incomplete',derived_title=d.derived_title,
             document_version=d.document_version,projected_byte_count=NULL,
             projected_prefix_hash=NULL,projected_record_prefix_hash=NULL,sampled_file_bytes=NULL,
             fully_scanned_bytes=NULL,verified_record_time=NULL,recovered_anchor_bytes=NULL,
             recovered_gap_start=NULL,observed_text_bytes=NULL,observed_text_hash=NULL
         FROM promoted_gap_documents d WHERE doc.id=d.id;
         DROP TABLE temp.promoted_gap_chunks;
         DROP TABLE temp.promoted_gap_documents;",
        crate::SEARCH_GAP_JOIN,
    )).map_err(database_error)?;
    clear_unverified_preview_catalogs(tx)
}

fn clear_unverified_preview_catalogs(tx: &Transaction<'_>) -> Result<()> {
    const PROMOTED_PREVIEW: &str = "projected_byte_count IS NULL AND source_complete=0
        AND truncation_strategy='source_incomplete'
        AND ((provider='antigravity' AND document_version=10)
          OR (provider='antigravity-desktop' AND document_version=8))";
    tx.execute_batch(&format!(
        "CREATE INDEX IF NOT EXISTS antigravity_legacy_preview_catalogs
         ON session_trajectories(provider,session_id) WHERE {PROMOTED_PREVIEW}"
    ))
    .map_err(database_error)?;
    let sql = format!(
        "SELECT d.provider,d.session_id,d.source_file_id,d.source_contents_id,h.source_home
         FROM (SELECT * FROM session_trajectories WHERE {PROMOTED_PREVIEW}) d
         JOIN native_search_document_homes h ON h.trajectory_id=d.id AND h.verification=1
             AND h.source_file=d.source_file_id
         JOIN session_index c ON c.provider=d.provider AND c.session_id=d.session_id
         JOIN native_catalog_homes scope ON scope.provider=d.provider AND scope.session_id=d.session_id
             AND scope.home=h.source_home
         WHERE d.origin='native' AND d.source_contents_id IS NOT NULL
           AND (c.source_file_id IS NULL OR c.source_file_id=d.source_file_id)
           AND (c.source_contents_id IS NOT d.source_contents_id OR NOT scope.verified)"
    );
    let mut selected = tx.prepare(&sql).map_err(database_error)?;
    let mut rows = selected.query([]).map_err(database_error)?;
    let mut affected = Vec::new();
    while let Some(row) = rows.next().map_err(database_error)? {
        affected.push((
            row.get::<_, String>(0).map_err(database_error)?,
            row.get::<_, String>(1).map_err(database_error)?,
            row.get::<_, Vec<u8>>(2).map_err(database_error)?,
            row.get::<_, Vec<u8>>(3).map_err(database_error)?,
            row.get::<_, Vec<u8>>(4).map_err(database_error)?,
        ));
    }
    for (provider, session, file, contents, home) in affected {
        tx.execute(
            "UPDATE session_index SET title=NULL,project_path=NULL,git_branch=NULL,created_at=NULL,
             updated_at=NULL,updated_at_approximate=0,event_count=0,source_updated_submillis=NULL,
             source_file_id=?3,source_contents_id=?4 WHERE provider=?1 AND session_id=?2",
            params![provider, session, file, contents],
        )
        .map_err(database_error)?;
        tx.execute(
            "UPDATE native_catalog_homes SET verified=1 WHERE provider=?1 AND session_id=?2 AND home=?3",
            params![provider,session,home],
        ).map_err(database_error)?;
    }
    Ok(())
}

pub(crate) fn read(
    tx: &Transaction<'_>,
    session: &SessionRef,
    home: &[u8],
    file: Uuid,
    contents: Uuid,
    content_hash: &[u8],
) -> Result<Option<Vec<Uuid>>> {
    let Some(table) = receipt_table(session.provider) else {
        return Ok(None);
    };
    tx.prepare_cached(&format!(
        "SELECT step_proof FROM {table}
         WHERE session_id=?1 AND source_home=?2 AND source_file=?3
           AND source_contents=?4 AND content_hash=?5",
    ))
    .and_then(|mut statement| {
        statement
            .query_row(
                params![
                    session.id,
                    home,
                    file.as_bytes(),
                    contents.as_bytes(),
                    content_hash
                ],
                |row| {
                    let proof = row.get_ref(0)?.as_blob()?;
                    let (steps, tail) = proof.as_chunks::<16>();
                    Ok((tail.is_empty() && steps.len() <= MAX_DATABASE_STEPS)
                        .then(|| steps.iter().copied().map(Uuid::from_bytes).collect()))
                },
            )
            .optional()
            .map(Option::flatten)
    })
    .map_err(database_error)
}

pub(crate) fn save(
    tx: &Transaction<'_>,
    session: &SessionRef,
    home: &[u8],
    file: Uuid,
    contents: Uuid,
    content_hash: &[u8],
    proof: Option<&[Uuid]>,
) -> Result<()> {
    let Some(table) = receipt_table(session.provider) else {
        return Ok(());
    };
    if let Some(proof) = proof {
        if proof.len() > MAX_DATABASE_STEPS {
            return Err(StoreError::TrajectoryBatchTooLarge);
        }
        let bytes: Vec<u8> = proof.iter().flat_map(Uuid::as_bytes).copied().collect();
        tx.prepare_cached(&format!(
            "INSERT INTO {table}(session_id, trajectory_id, source_home, source_file, source_contents, content_hash, step_proof)
             VALUES(?1, (SELECT id FROM session_trajectories WHERE provider=?7 AND session_id=?1), ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(session_id, source_home, source_file) DO UPDATE SET
                 trajectory_id=excluded.trajectory_id, source_contents=excluded.source_contents,
                 content_hash=excluded.content_hash, step_proof=excluded.step_proof",
        ))
        .and_then(|mut statement| {
            statement.execute(params![
                session.id, home, file.as_bytes(), contents.as_bytes(), content_hash, bytes, session.provider.to_string(),
            ])
        })
    } else {
        tx.prepare_cached(&format!(
            "DELETE FROM {table} WHERE session_id=?1 AND source_home=?2 AND source_file=?3",
        ))
        .and_then(|mut statement| statement.execute(params![session.id, home, file.as_bytes()]))
    }
    .map_err(database_error)?;
    Ok(())
}

pub(super) fn catalog_hash(metadata: &crate::IndexedSession) -> Result<String> {
    let fields = serde_json::to_vec(&(
        &metadata.title,
        &metadata.project_path,
        &metadata.git_branch,
        metadata.created_at,
        metadata.updated_at,
        metadata.updated_at_approximate,
        metadata.event_count,
    ))
    .map_err(|_| StoreError::InvalidSessionReference)?;
    Ok(Uuid::new_v5(&Uuid::NAMESPACE_OID, &fields)
        .simple()
        .to_string())
}

fn record_catalog(
    tx: &Transaction<'_>,
    id: i64,
    metadata: &crate::IndexedSession,
    home: &std::path::Path,
) -> Result<()> {
    tx.execute(
        "INSERT INTO antigravity_read_catalogs
         SELECT id,?1,source_file_id,source_contents_id,?2,document_version,content_hash FROM session_trajectories WHERE id=?3
         ON CONFLICT(trajectory_id) DO UPDATE SET source_home=excluded.source_home, source_file=excluded.source_file,
           source_contents=excluded.source_contents,catalog_hash=excluded.catalog_hash,document_version=excluded.document_version,content_hash=excluded.content_hash
         WHERE (source_home,source_file,source_contents,catalog_hash,document_version,content_hash) IS NOT
           (excluded.source_home,excluded.source_file,excluded.source_contents,excluded.catalog_hash,excluded.document_version,excluded.content_hash)",
        params![home.as_os_str().as_encoded_bytes(), catalog_hash(metadata)?, id]
    ).map_err(database_error)?;
    Ok(())
}

impl crate::Store {
    /// Checks whether a scoped document used the current catalog, native revision, and reader version.
    ///
    /// # Errors
    ///
    /// Returns an error if the store or catalog metadata cannot be read.
    pub fn current_scoped_database_catalog(
        &self,
        catalog: &crate::IndexedSession,
        home: &std::path::Path,
        version: u32,
    ) -> Result<bool> {
        let Some(revision) = catalog.source_revision else {
            return Ok(false);
        };
        let (Some(file), Some(contents)) = (revision.file, revision.contents) else {
            return Ok(false);
        };
        self.connection
            .borrow()
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM scoped_native_documents d
             JOIN scoped_database_catalogs r ON r.document_id=d.id AND r.catalog_hash=?7
             JOIN native_catalog_homes h ON h.provider=d.provider AND h.session_id=d.session_id
               AND h.verified=1 AND h.home=d.source_home
             JOIN session_index c ON c.provider=d.provider AND c.session_id=d.session_id
               AND c.source_file_id=d.source_file AND c.source_contents_id=d.source_contents
             WHERE d.provider=?1 AND d.session_id=?2 AND d.source_home=?3
               AND d.source_file=?4 AND d.source_contents=?5 AND d.document_version=?6)",
                params![
                    catalog.session.provider.to_string(),
                    catalog.session.id,
                    home.as_os_str().as_encoded_bytes(),
                    file.as_bytes(),
                    contents.as_bytes(),
                    version,
                    catalog_hash(catalog)?
                ],
                |row| row.get(0),
            )
            .map_err(database_error)
    }

    /// Returns database sessions whose verified read used this home, revision, catalog, and reader version.
    ///
    /// # Errors
    ///
    /// Returns an error if the store or the catalog metadata cannot be read.
    pub fn current_database_catalogs(
        &self,
        provider: Provider,
        home: &std::path::Path,
        candidates: &[(&crate::IndexedSession, u32)],
    ) -> Result<std::collections::HashSet<SessionRef>> {
        let mut selected = Vec::with_capacity(candidates.len());
        for &(catalog, version) in candidates {
            let Some(revision) = catalog.source_revision else {
                continue;
            };
            if let (Some(file), Some(contents)) = (revision.file, revision.contents) {
                selected.push((
                    &catalog.session.id,
                    file.simple().to_string(),
                    contents.simple().to_string(),
                    catalog_hash(catalog)?,
                    version,
                ));
            }
        }
        let selected =
            serde_json::to_string(&selected).map_err(|_| StoreError::InvalidSessionReference)?;
        let connection = self.connection.borrow();
        let mut statement = connection.prepare_cached(
            "SELECT doc.session_id FROM json_each(?1) picked
             JOIN session_trajectories doc ON doc.provider=?2 AND doc.session_id=(picked.value ->> 0)
             JOIN antigravity_read_catalogs r ON r.trajectory_id=doc.id AND r.source_home=?3
             JOIN native_search_document_homes h ON h.trajectory_id=doc.id AND h.verification=1 AND h.source_home=r.source_home AND h.source_file=r.source_file
             JOIN native_catalog_homes catalog_home ON catalog_home.provider=doc.provider AND catalog_home.session_id=doc.session_id
               AND catalog_home.verified=1 AND catalog_home.home=r.source_home
             JOIN session_index catalog ON catalog.provider=doc.provider AND catalog.session_id=doc.session_id
               AND catalog.source_file_id=r.source_file AND catalog.source_contents_id=r.source_contents
             WHERE r.source_file=doc.source_file_id AND r.source_contents=doc.source_contents_id AND r.document_version=doc.document_version AND r.content_hash=doc.content_hash
               AND hex(r.source_file)=upper(picked.value ->> 1) AND hex(r.source_contents)=upper(picked.value ->> 2)
               AND r.catalog_hash=(picked.value ->> 3) AND r.document_version=(picked.value ->> 4)"
        ).map_err(database_error)?;
        statement
            .query_map(
                params![
                    selected,
                    provider.to_string(),
                    home.as_os_str().as_encoded_bytes()
                ],
                |row| Ok(SessionRef::new(provider, row.get::<_, String>(0)?)),
            )
            .map_err(database_error)?
            .collect::<rusqlite::Result<_>>()
            .map_err(database_error)
    }
}

pub(super) fn record_verified(
    tx: &Transaction<'_>,
    write: &crate::PreparedTrajectoryWrite<'_, '_>,
    home: &std::path::Path,
) -> Result<()> {
    if receipt_table(write.session.provider).is_none() {
        return Ok(());
    }
    let revision = write.document.source_revision;
    if let (Some(file), Some(contents)) = (revision.file, revision.contents) {
        let stored = tx.query_row(
            "SELECT id,document_version FROM session_trajectories
             WHERE provider=?1 AND session_id=?2 AND origin='native' AND source_file_id=?3 AND source_contents_id=?4 AND content_hash=?5",
            params![write.session.provider.to_string(), write.session.id, file.as_bytes(), contents.as_bytes(), write.content_hash],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, u32>(1)?)),
        ).optional().map_err(database_error)?;
        if let Some((id, version)) = stored {
            if version == write.document.document_version
                && let Some(metadata) = write.verified_metadata
            {
                record_catalog(tx, id, metadata, home)?;
            }
            tx.execute(
                "DELETE FROM antigravity_search_gaps WHERE trajectory_id=?1",
                [id],
            )
            .map_err(database_error)?;
            let Some(projection) = write.document.projection else {
                return Ok(());
            };
            let Some(steps) = projection.database_steps.as_deref() else {
                return Ok(());
            };
            let canonical;
            let steps = if projection.database_aliases.is_empty() {
                steps
            } else {
                canonical = steps
                    .iter()
                    .map(|raw| {
                        projection
                            .database_aliases
                            .get(raw)
                            .map_or(*raw, |alias| alias.canonical)
                    })
                    .collect::<Vec<_>>();
                &canonical
            };
            save(
                tx,
                write.session,
                home.as_os_str().as_encoded_bytes(),
                file,
                contents,
                &write.content_hash,
                Some(steps),
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Provider, SessionRef, initialize, read, save};
    use rusqlite::Connection;
    use uuid::Uuid;

    #[test]
    fn a_format_one_database_keeps_its_receipts_until_the_session_is_removed() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("store.sqlite3");
        let old = Connection::open(&path).unwrap();
        old.execute_batch(
            "PRAGMA foreign_keys=ON;
             PRAGMA user_version=1;
             CREATE TABLE session_trajectories(id INTEGER PRIMARY KEY,provider TEXT NOT NULL,session_id TEXT NOT NULL,
                redacted_text TEXT NOT NULL,source_updated_at INTEGER NOT NULL,complete INTEGER NOT NULL,indexed_at INTEGER NOT NULL,
                UNIQUE(provider,session_id));
             CREATE TABLE session_index(provider TEXT NOT NULL,session_id TEXT NOT NULL,title TEXT,project_path TEXT,
                git_branch TEXT,created_at INTEGER,updated_at INTEGER,event_count INTEGER NOT NULL,indexed_at INTEGER NOT NULL,
                PRIMARY KEY(provider,session_id)) WITHOUT ROWID;
             CREATE TABLE native_catalog_homes(provider TEXT,session_id TEXT,home BLOB NOT NULL,verified INTEGER NOT NULL DEFAULT 1,
                PRIMARY KEY(provider,session_id),FOREIGN KEY(provider,session_id) REFERENCES session_index(provider,session_id) ON DELETE CASCADE) WITHOUT ROWID;
             CREATE TABLE desktop_search_receipts(provider TEXT,session_id TEXT,source_home BLOB,source_file BLOB,source_contents BLOB,content_hash BLOB,step_proof BLOB,
                PRIMARY KEY(session_id,source_home,source_file),FOREIGN KEY(provider,session_id) REFERENCES session_trajectories(provider,session_id) ON DELETE CASCADE) WITHOUT ROWID;
             CREATE TABLE session_index_reader_checks(provider TEXT,reader_version INTEGER,source_home BLOB,checked_at INTEGER,attempt_id BLOB,
                PRIMARY KEY(provider,reader_version)) WITHOUT ROWID;
             CREATE TABLE session_index_discovery_results(provider TEXT,reader_version INTEGER,attempt_id BLOB,succeeded INTEGER,
                PRIMARY KEY(provider,reader_version),FOREIGN KEY(provider,reader_version) REFERENCES session_index_reader_checks(provider,reader_version) ON DELETE CASCADE) WITHOUT ROWID;
             CREATE TABLE session_index_upgrade_sources(provider TEXT,reader_version INTEGER,session_id TEXT,source_path TEXT,
                PRIMARY KEY(provider,reader_version,session_id),FOREIGN KEY(provider,reader_version) REFERENCES session_index_reader_checks(provider,reader_version) ON DELETE CASCADE) WITHOUT ROWID;
             INSERT INTO session_index(provider,session_id,event_count,indexed_at) VALUES('antigravity-desktop','saved',1,1);
             INSERT INTO session_trajectories VALUES(81,'antigravity-desktop','saved','',1,1,1);
             INSERT INTO native_catalog_homes VALUES('antigravity-desktop','saved',X'61',1);
             INSERT INTO desktop_search_receipts VALUES('antigravity-desktop','saved',X'61',zeroblob(16),zeroblob(16),zeroblob(32),zeroblob(16));
             INSERT INTO session_index_reader_checks VALUES('antigravity-desktop',6,X'61',42,zeroblob(16));
             INSERT INTO session_index_discovery_results VALUES('antigravity-desktop',6,zeroblob(16),1);
             INSERT INTO session_index_upgrade_sources VALUES('antigravity-desktop',6,'saved','transcript');",
        ).unwrap();
        drop(old);

        let store = crate::Store::open(path).unwrap();
        let session = SessionRef::new(Provider::AntigravityDesktop, "saved");
        let proof = || {
            let mut connection = store.connection.borrow_mut();
            let tx = connection.transaction().unwrap();
            read(&tx, &session, b"a", Uuid::nil(), Uuid::nil(), &[0; 32]).unwrap()
        };
        let excluded = || {
            store
                .excluded_catalog_ids(session.provider, std::path::Path::new("b"))
                .unwrap()
        };
        assert_eq!(proof(), Some(vec![Uuid::nil()]));
        assert!(excluded().contains("saved"));
        let original = std::path::Path::new("a");
        assert_eq!(
            store
                .completed_discovery_at(session.provider, 6, original)
                .unwrap(),
            chrono::DateTime::from_timestamp_millis(42)
        );
        let pending = || {
            store
                .pending_discovery_sources(session.provider, 6, original)
                .unwrap()
        };
        assert_eq!(pending(), [("saved".to_owned(), Some("transcript".into()))]);
        store.forget_session(&session).unwrap();
        assert_eq!(proof(), None);
        assert!(excluded().is_empty());
        assert!(pending().is_empty());
    }

    #[test]
    fn receipt_follows_its_saved_projection_and_source() {
        let mut connection = Connection::open_in_memory().unwrap();
        let tx = connection.transaction().unwrap();
        tx.execute_batch(
            "CREATE TABLE session_trajectories (
                id INTEGER PRIMARY KEY, provider TEXT, session_id TEXT
             );
             CREATE TABLE session_trajectory_chunk_rows (trajectory_id INTEGER, chunk_index INTEGER);
             INSERT INTO session_trajectories(provider,session_id) VALUES('antigravity-desktop','session'), ('antigravity','session'), ('antigravity-ide','session');",
        )
        .unwrap();
        initialize(&tx).unwrap();
        let session = &SessionRef::new(Provider::AntigravityDesktop, "session");
        let cli = &SessionRef::new(Provider::Antigravity, "session");
        let ide = &SessionRef::new(Provider::AntigravityIde, "session");
        let (file, contents) = (Uuid::new_v4(), Uuid::new_v4());
        let hash = [1; 32];
        let proof = [Uuid::new_v4(), Uuid::new_v4()];
        let lookup = |home: &[u8], file, contents, hash: &[u8]| {
            read(&tx, session, home, file, contents, hash).unwrap()
        };
        assert_eq!(lookup(b"a", file, contents, &hash), None);
        save(&tx, session, b"a", file, contents, &hash, Some(&proof)).unwrap();
        assert_eq!(read(&tx, cli, b"a", file, contents, &hash).unwrap(), None);
        save(&tx, cli, b"a", file, contents, &hash, Some(&[])).unwrap();
        assert_eq!(read(&tx, ide, b"a", file, contents, &hash).unwrap(), None);
        save(&tx, ide, b"a", file, contents, &hash, Some(&proof[..1])).unwrap();
        assert_eq!(
            read(&tx, ide, b"a", file, contents, &hash).unwrap(),
            Some(proof[..1].to_vec())
        );
        save(&tx, session, b"b", file, contents, &hash, Some(&[])).unwrap();
        assert_eq!(lookup(b"a", file, contents, &hash), Some(proof.to_vec()));
        assert_eq!(lookup(b"a", file, contents, &[2; 32]), None);
        assert_eq!(lookup(b"a", Uuid::new_v4(), contents, &hash), None);
        assert_eq!(lookup(b"a", file, Uuid::new_v4(), &hash), None);
        save(&tx, session, b"a", file, contents, &hash, None).unwrap();
        assert_eq!(lookup(b"a", file, contents, &hash), None);
        assert_eq!(lookup(b"b", file, contents, &hash), Some(Vec::new()));
        assert_eq!(
            read(&tx, cli, b"a", file, contents, &hash).unwrap(),
            Some(Vec::new())
        );
    }
}
