use rusqlite::{OptionalExtension, Transaction};

use crate::{FIRST_PRIMARY_LEGACY_CHUNK, Result, database_error, insert_chunks_into};

pub(super) fn initialize(tx: &Transaction<'_>) -> Result<()> {
    let installed = tx
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='primary_legacy_chunk_updates')",
            [],
            |row| row.get::<_, bool>(0),
        )
        .map_err(database_error)?;
    tx.execute_batch(&format!(
        "CREATE TABLE IF NOT EXISTS primary_legacy_chunk_updates (
             trajectory_id INTEGER PRIMARY KEY REFERENCES session_trajectories(id) ON DELETE CASCADE
         );
         DROP TRIGGER IF EXISTS primary_legacy_chunks_after_update;
         CREATE TRIGGER IF NOT EXISTS queue_primary_legacy_text_change
         AFTER UPDATE OF legacy_search_text ON session_trajectories
         WHEN old.legacy_search_text IS NOT new.legacy_search_text BEGIN
             INSERT INTO primary_legacy_chunk_updates SELECT new.id WHERE NOT EXISTS(SELECT 1 FROM primary_legacy_chunk_updates WHERE trajectory_id=new.id);
         END;
         CREATE TRIGGER IF NOT EXISTS primary_legacy_chunks_after_insert
         AFTER INSERT ON session_trajectories WHEN new.legacy_search_text IS NOT NULL BEGIN
             INSERT OR IGNORE INTO primary_legacy_chunk_updates VALUES(new.id);
         END;
         CREATE TRIGGER IF NOT EXISTS requeue_removed_primary_legacy_chunks
         AFTER DELETE ON session_trajectory_chunk_rows WHEN old.chunk_index={FIRST_PRIMARY_LEGACY_CHUNK} BEGIN
             INSERT INTO primary_legacy_chunk_updates
                 SELECT id FROM session_trajectories WHERE id=old.trajectory_id AND legacy_search_text IS NOT NULL
                   AND NOT EXISTS(SELECT 1 FROM primary_legacy_chunk_updates WHERE trajectory_id=old.trajectory_id);
         END;"
    )).map_err(database_error)?;
    if !installed {
        tx.execute(
            "INSERT INTO primary_legacy_chunk_updates SELECT id FROM session_trajectories WHERE legacy_search_text IS NOT NULL",
            [],
        ).map_err(database_error)?;
    }
    Ok(())
}

pub(super) fn flush(tx: &Transaction<'_>) -> Result<()> {
    let mut pending = tx
        .prepare_cached(
            "SELECT doc.id, doc.legacy_search_text FROM primary_legacy_chunk_updates q
         JOIN session_trajectories doc ON doc.id=q.trajectory_id LIMIT 1",
        )
        .map_err(database_error)?;
    while let Some((id, text)) = pending
        .query_row([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, Option<String>>(1)?))
        })
        .optional()
        .map_err(database_error)?
    {
        tx.execute(
            "DELETE FROM session_trajectory_chunk_rows WHERE trajectory_id=?1 AND chunk_index>=?2",
            [id, FIRST_PRIMARY_LEGACY_CHUNK],
        )
        .map_err(database_error)?;
        if let Some(text) = text {
            insert_chunks_into(
                tx,
                &[(id, &text)],
                FIRST_PRIMARY_LEGACY_CHUNK,
                "INSERT INTO session_trajectory_chunk_rows(trajectory_id, chunk_index, stored_text) VALUES ",
            )?;
        }
        tx.execute(
            "DELETE FROM primary_legacy_chunk_updates WHERE trajectory_id=?1",
            [id],
        )
        .map_err(database_error)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        initialize_trajectory_chunk_schema, register_search_functions, replace_trajectory_chunks,
    };

    #[test]
    fn an_open_reader_hides_retired_chunks_after_a_plain_sqlite_write() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("store.db");
        let store = crate::Store::open(&path).unwrap();
        let session = crate::SessionRef::new(crate::Provider::Antigravity, "synthetic");
        store
            .upsert_session_trajectory(&session, "currentmarker", chrono::Utc::now(), false)
            .unwrap();
        let raw = rusqlite::Connection::open(&path).unwrap();
        raw.execute(
            "UPDATE session_trajectories SET legacy_search_text='oldmarker'",
            [],
        )
        .unwrap();
        drop(store);
        let store = crate::Store::open(&path).unwrap();
        let hits = |word| store.search_session_trajectories(word, 10).unwrap();
        assert_eq!(hits("oldmarker"), std::slice::from_ref(&session));
        raw.execute(
            "UPDATE session_trajectories SET source_file_id=zeroblob(16)",
            [],
        )
        .unwrap();
        assert!(hits("oldmarker").is_empty());
        assert_eq!(hits("currentmarker"), [session]);
    }

    #[test]
    fn retained_primary_text_is_searched_and_retired_independently() {
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        register_search_functions(&conn).unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON").unwrap();
        let tx = conn.transaction().unwrap();
        tx.execute_batch(
            "CREATE TABLE session_trajectories(id INTEGER PRIMARY KEY, redacted_text TEXT, content_hash BLOB, legacy_search_text TEXT);
             INSERT INTO session_trajectories VALUES(1,'currentmarker',NULL,NULL);"
        ).unwrap();
        initialize_trajectory_chunk_schema(&tx).unwrap();
        initialize(&tx).unwrap();
        let hits = |query| {
            tx.query_row(
            "SELECT count(*) FROM session_trajectory_chunks_fts WHERE session_trajectory_chunks_fts MATCH ?1",
            [query], |r| r.get::<_, i64>(0),
        ).unwrap()
        };
        tx.execute(
            "UPDATE session_trajectories SET legacy_search_text='oldmarker'",
            [],
        )
        .unwrap();
        flush(&tx).unwrap();
        assert_eq!(hits("currentmarker"), 1);
        assert_eq!(hits("oldmarker"), 1);
        assert_eq!(hits("\"currentmarker oldmarker\""), 0);
        replace_trajectory_chunks(&tx, &[(1, "newmarker")]).unwrap();
        assert_eq!(hits("currentmarker"), 0);
        assert_eq!(hits("newmarker"), 1);
        assert_eq!(hits("oldmarker"), 1);
        tx.execute(
            "DELETE FROM session_trajectory_chunk_rows WHERE trajectory_id=1 AND chunk_index>=?1",
            [FIRST_PRIMARY_LEGACY_CHUNK],
        )
        .unwrap();
        flush(&tx).unwrap();
        assert_eq!(hits("oldmarker"), 1);
        tx.execute(
            "UPDATE session_trajectories SET legacy_search_text=NULL",
            [],
        )
        .unwrap();
        flush(&tx).unwrap();
        assert_eq!(hits("oldmarker"), 0);
        assert_eq!(hits("newmarker"), 1);
        tx.execute_batch("INSERT INTO session_trajectory_chunks_fts(session_trajectory_chunks_fts,rank) VALUES('integrity-check',1)").unwrap();
    }
}
