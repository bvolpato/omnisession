use rusqlite::{OptionalExtension, Transaction, params};
use uuid::Uuid;

use crate::{Provider, Result, SessionRef, StoreError, database_error};

use omnis_ir::ANTIGRAVITY_DATABASE_STEP_LIMIT as MAX_DATABASE_STEPS;

const RECEIPT_TABLES: [(Provider, &str); 2] = [
    (Provider::AntigravityDesktop, "desktop_search_receipts"),
    (Provider::Antigravity, "antigravity_cli_search_receipts"),
];

fn receipt_table(provider: Provider) -> Option<&'static str> {
    RECEIPT_TABLES
        .into_iter()
        .find_map(|(owner, table)| (owner == provider).then_some(table))
}

pub(crate) fn initialize(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch(
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
            DELETE FROM session_trajectory_chunk_rows WHERE trajectory_id=old.trajectory_id AND chunk_index>=old.first_chunk;
        END;",
    )
    .map_err(database_error)?;
    for (provider, table) in RECEIPT_TABLES {
        tx.execute_batch(&format!(
        "CREATE TABLE IF NOT EXISTS {table} (
             provider TEXT NOT NULL DEFAULT '{provider}' CHECK(provider='{provider}'),
             session_id TEXT NOT NULL,
             source_home BLOB NOT NULL,
             source_file BLOB NOT NULL CHECK(length(source_file)=16),
             source_contents BLOB NOT NULL CHECK(length(source_contents)=16),
             content_hash BLOB NOT NULL CHECK(length(content_hash)=32),
             step_proof BLOB NOT NULL CHECK(length(step_proof)%16=0 AND length(step_proof)<=1600000),
             PRIMARY KEY(session_id, source_home, source_file),
             FOREIGN KEY(provider, session_id) REFERENCES session_trajectories(provider, session_id) ON DELETE CASCADE
         ) WITHOUT ROWID;",
    )).map_err(database_error)?;
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
        return Ok(());
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
           ON c.trajectory_id=d.id AND c.chunk_index>=d.first_chunk;
         DELETE FROM antigravity_search_gaps WHERE trajectory_id IN (
             SELECT trajectory_id FROM antigravity_gap_projections
         );
         DELETE FROM session_trajectory_chunk_rows WHERE trajectory_id IN (SELECT id FROM promoted_gap_documents);
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
    )).map_err(database_error)
}

#[cfg(test)]
fn read(
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
            "INSERT INTO {table}(session_id, source_home, source_file, source_contents, content_hash, step_proof)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(session_id, source_home, source_file) DO UPDATE SET
                 source_contents=excluded.source_contents, content_hash=excluded.content_hash, step_proof=excluded.step_proof",
        ))
        .and_then(|mut statement| {
            statement.execute(params![
                session.id, home, file.as_bytes(), contents.as_bytes(), content_hash, bytes
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

fn catalog_hash(metadata: &crate::IndexedSession) -> Result<String> {
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
    fn receipt_follows_its_saved_projection_and_source() {
        let mut connection = Connection::open_in_memory().unwrap();
        let tx = connection.transaction().unwrap();
        tx.execute_batch(
            "CREATE TABLE session_trajectories (
                provider TEXT, session_id TEXT, PRIMARY KEY(provider, session_id)
             );
             CREATE TABLE session_trajectory_chunk_rows (trajectory_id INTEGER, chunk_index INTEGER);
             INSERT INTO session_trajectories VALUES('antigravity-desktop','session'), ('antigravity','session');",
        )
        .unwrap();
        initialize(&tx).unwrap();
        let session = &SessionRef::new(Provider::AntigravityDesktop, "session");
        let cli = &SessionRef::new(Provider::Antigravity, "session");
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
