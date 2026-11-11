use rusqlite::{OptionalExtension, Transaction, params};

use crate::{Result, database_error, insert_chunks_into};

pub(super) fn initialize(tx: &Transaction<'_>) -> Result<()> {
    let existing = tx
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='scoped_native_chunk_rows')",
            [],
            |row| row.get::<_, bool>(0),
        )
        .map_err(database_error)?;
    tx.execute_batch(
        "DROP TRIGGER IF EXISTS index_scoped_native_document;
         DROP TRIGGER IF EXISTS remove_scoped_native_document;
         DROP TRIGGER IF EXISTS update_scoped_native_document;
         DROP TABLE IF EXISTS scoped_native_document_fts;

         CREATE TABLE IF NOT EXISTS scoped_native_chunk_rows (
             id INTEGER PRIMARY KEY CHECK (id > 0),
             document_id INTEGER NOT NULL REFERENCES scoped_native_documents(id) ON DELETE CASCADE,
             chunk_index INTEGER NOT NULL CHECK (chunk_index >= 0),
             stored_text TEXT NOT NULL,
             UNIQUE (document_id, chunk_index)
         );
         CREATE TABLE IF NOT EXISTS scoped_native_chunk_updates (
             document_id INTEGER PRIMARY KEY REFERENCES scoped_native_documents(id) ON DELETE CASCADE
         );
         CREATE TRIGGER IF NOT EXISTS queue_scoped_chunks_after_insert
         AFTER INSERT ON scoped_native_documents BEGIN
             INSERT INTO scoped_native_chunk_updates VALUES(new.id);
         END;
         CREATE TRIGGER IF NOT EXISTS queue_scoped_chunks_after_update
         AFTER UPDATE OF stored_text ON scoped_native_documents WHEN old.stored_text <> new.stored_text BEGIN
             DELETE FROM scoped_native_chunk_rows WHERE document_id = new.id;
             INSERT INTO scoped_native_chunk_updates SELECT new.id
             WHERE NOT EXISTS (SELECT 1 FROM scoped_native_chunk_updates WHERE document_id=new.id);
         END;

         CREATE TRIGGER IF NOT EXISTS scoped_native_chunks_after_insert
         AFTER INSERT ON scoped_native_chunk_rows BEGIN
             INSERT INTO session_trajectory_chunks_fts(rowid, redacted_text)
             VALUES(-new.id, omnis_chunk_text(new.stored_text));
         END;
         CREATE TRIGGER IF NOT EXISTS scoped_native_chunks_after_delete
         AFTER DELETE ON scoped_native_chunk_rows BEGIN
             INSERT INTO session_trajectory_chunks_fts(session_trajectory_chunks_fts, rowid, redacted_text)
             VALUES('delete', -old.id, omnis_chunk_text(old.stored_text));
             INSERT OR IGNORE INTO session_trajectory_chunk_repairs(id)
             SELECT 1 WHERE NOT omnis_chunk_readable(old.stored_text);
         END;
         CREATE TRIGGER IF NOT EXISTS scoped_native_chunks_after_update
         AFTER UPDATE ON scoped_native_chunk_rows BEGIN
             INSERT INTO session_trajectory_chunks_fts(session_trajectory_chunks_fts, rowid, redacted_text)
             VALUES('delete', -old.id, omnis_chunk_text(old.stored_text));
             INSERT INTO session_trajectory_chunks_fts(rowid, redacted_text)
             VALUES(-new.id, omnis_chunk_text(new.stored_text));
             INSERT OR IGNORE INTO session_trajectory_chunk_repairs(id)
             SELECT 1 WHERE NOT omnis_chunk_readable(old.stored_text);
         END;",
    ).map_err(database_error)?;
    let shared = tx
        .query_row(
            "SELECT instr(sql,'scoped_native_chunk_rows') > 0 FROM sqlite_schema
         WHERE type='view' AND name='session_trajectory_chunks'",
            [],
            |row| row.get::<_, bool>(0),
        )
        .map_err(database_error)?;
    if !shared {
        // Primary chunks keep their positive FTS row IDs. Scoped chunks use the negative range.
        tx.execute_batch(
            "DROP VIEW session_trajectory_chunks;
             CREATE VIEW session_trajectory_chunks AS
                 SELECT id,trajectory_id,chunk_index,omnis_chunk_text(stored_text) AS redacted_text
                 FROM session_trajectory_chunk_rows
                 UNION ALL
                 SELECT -id,-document_id,chunk_index,omnis_chunk_text(stored_text)
                 FROM scoped_native_chunk_rows;",
        )
        .map_err(database_error)?;
        if existing {
            tx.execute_batch("INSERT INTO session_trajectory_chunks_fts(session_trajectory_chunks_fts) VALUES('rebuild')").map_err(database_error)?;
        }
    }
    if !existing {
        tx.execute_batch(
            "INSERT INTO scoped_native_chunk_updates SELECT id FROM scoped_native_documents",
        )
        .map_err(database_error)?;
    }
    flush(tx)
}

// A batch may rewrite a document more than once. Index its final text in the same transaction.
pub(crate) fn flush(tx: &Transaction<'_>) -> Result<()> {
    let mut pending = tx
        .prepare_cached(
            "SELECT doc.id, doc.stored_text FROM scoped_native_chunk_updates u
         JOIN scoped_native_documents doc ON doc.id = u.document_id LIMIT 1",
        )
        .map_err(database_error)?;
    while let Some((id, text)) = pending
        .query_row([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })
        .optional()
        .map_err(database_error)?
    {
        insert_chunks_into(
            tx,
            &[(id, &text)],
            "INSERT INTO scoped_native_chunk_rows(document_id, chunk_index, stored_text) VALUES ",
        )?;
        tx.execute(
            "DELETE FROM scoped_native_chunk_updates WHERE document_id=?1",
            params![id],
        )
        .map_err(database_error)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{initialize_trajectory_chunk_schema, register_search_functions};

    #[test]
    fn archived_text_migrates_and_tracks_document_transactions() {
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        register_search_functions(&conn).unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON").unwrap();
        let tx = conn.transaction().unwrap();
        tx.execute_batch(
            "CREATE TABLE session_trajectories(id INTEGER PRIMARY KEY, redacted_text TEXT, content_hash BLOB);
             INSERT INTO session_trajectories VALUES(1,'primaryquartz',NULL);
             CREATE TABLE scoped_native_documents(id INTEGER PRIMARY KEY, stored_text TEXT);",
        ).unwrap();
        initialize_trajectory_chunk_schema(&tx).unwrap();
        let text = format!("{} archiveddiamond", "漢字 ".repeat(25000));
        tx.execute("INSERT INTO scoped_native_documents VALUES(1, ?1)", [&text])
            .unwrap();
        initialize(&tx).unwrap();
        let hits = |term| {
            tx.query_row(
            "SELECT count(*) FROM session_trajectory_chunks_fts WHERE session_trajectory_chunks_fts MATCH ?1",
            [term], |row| row.get::<_, i64>(0),
        ).unwrap()
        };
        let check = || {
            tx.execute_batch("INSERT INTO session_trajectory_chunks_fts(session_trajectory_chunks_fts, rank) VALUES('integrity-check',1)").unwrap();
        };
        check();
        assert_eq!(hits("archiveddiamond"), 1);
        assert_eq!(hits("primaryquartz"), 1);
        tx.execute_batch(
            "UPDATE scoped_native_documents SET stored_text='intermediatequartz';
                          UPDATE scoped_native_documents SET stored_text='scopedsapphire';",
        )
        .unwrap();
        flush(&tx).unwrap();
        assert_eq!(hits("archiveddiamond"), 0);
        assert_eq!(hits("intermediatequartz"), 0);
        assert_eq!(hits("scopedsapphire"), 1);
        check();
        tx.execute_batch("SAVEPOINT removal; DELETE FROM scoped_native_documents;")
            .unwrap();
        assert_eq!(hits("scopedsapphire"), 0);
        check();
        tx.execute_batch("ROLLBACK TO removal; RELEASE removal;")
            .unwrap();
        assert_eq!(hits("scopedsapphire"), 1);
        check();
    }
}
