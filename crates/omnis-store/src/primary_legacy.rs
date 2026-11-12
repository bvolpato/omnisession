use rusqlite::{Transaction, params};

use crate::{Result, database_error};

pub(super) fn initialize(tx: &Transaction<'_>) -> Result<()> {
    let has_column: bool = tx
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('session_trajectories') WHERE name='legacy_search_text')",
            [],
            |row| row.get(0),
        )
        .map_err(database_error)?;
    if !has_column {
        tx.execute_batch("ALTER TABLE session_trajectories ADD COLUMN legacy_search_text TEXT")
            .map_err(database_error)?;
    }
    tx.execute_batch(
        "CREATE TRIGGER IF NOT EXISTS clear_replaced_primary_legacy_text
         AFTER UPDATE OF source_file_id, origin, source_complete ON session_trajectories
         WHEN new.legacy_search_text IS NOT NULL AND
              (new.source_file_id IS NOT old.source_file_id OR new.origin IS NOT old.origin
               OR (new.source_complete AND NOT new.provider_limited))
         BEGIN
             UPDATE session_trajectories SET legacy_search_text=NULL WHERE id=new.id;
         END;
         CREATE TRIGGER IF NOT EXISTS clear_rebound_primary_legacy_text
         AFTER UPDATE OF source_home,source_file ON native_search_document_homes
         WHEN (new.source_home,new.source_file) IS NOT (old.source_home,old.source_file)
         BEGIN
             UPDATE session_trajectories SET legacy_search_text=NULL
             WHERE id=old.trajectory_id AND legacy_search_text IS NOT NULL;
         END;
         CREATE TRIGGER IF NOT EXISTS clear_detached_primary_legacy_text
         AFTER DELETE ON native_search_document_homes
         BEGIN
             UPDATE session_trajectories SET legacy_search_text=NULL
             WHERE id=old.trajectory_id AND legacy_search_text IS NOT NULL;
         END;",
    )
    .map_err(database_error)
}

pub(super) fn retain(tx: &Transaction<'_>, id: Option<i64>, text: Option<&str>) -> Result<()> {
    if let (Some(id), Some(text)) = (id, text) {
        tx.execute(
            "UPDATE session_trajectories SET legacy_search_text=?2
             WHERE id=?1 AND legacy_search_text IS NULL AND NOT source_complete",
            params![id, text],
        )
        .map_err(database_error)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn older_writers_retire_companions_when_the_source_or_owner_changes() {
        let mut connection = rusqlite::Connection::open_in_memory().unwrap();
        let tx = connection.transaction().unwrap();
        tx.execute_batch(
            "CREATE TABLE session_trajectories(id INTEGER PRIMARY KEY, source_file_id BLOB, origin TEXT, source_complete INTEGER, provider_limited INTEGER);
             CREATE TABLE native_search_document_homes(trajectory_id INTEGER PRIMARY KEY, source_home BLOB, source_file BLOB);
             INSERT INTO session_trajectories VALUES(1,x'01','native',0,0);
             INSERT INTO native_search_document_homes VALUES(1,x'01',x'01');",
        ).unwrap();
        initialize(&tx).unwrap();
        retain(&tx, Some(1), Some("prior")).unwrap();
        let saved = || {
            tx.query_row(
                "SELECT legacy_search_text FROM session_trajectories",
                [],
                |row| row.get::<_, Option<String>>(0),
            )
            .unwrap()
        };
        tx.execute("UPDATE session_trajectories SET source_complete=0", [])
            .unwrap();
        retain(&tx, Some(1), Some("later")).unwrap();
        assert_eq!(saved().as_deref(), Some("prior"));
        tx.execute(
            "UPDATE session_trajectories SET source_complete=1, provider_limited=1",
            [],
        )
        .unwrap();
        assert_eq!(saved().as_deref(), Some("prior"));
        tx.execute(
            "UPDATE session_trajectories SET source_complete=0, provider_limited=0",
            [],
        )
        .unwrap();
        for change in [
            "UPDATE session_trajectories SET source_complete=1",
            "UPDATE session_trajectories SET source_file_id=x'02'",
            "UPDATE native_search_document_homes SET source_home=x'02'",
            "DELETE FROM native_search_document_homes",
        ] {
            tx.execute_batch("SAVEPOINT original").unwrap();
            tx.execute(change, []).unwrap();
            assert_eq!(saved(), None, "{change}");
            tx.execute_batch("ROLLBACK TO original; RELEASE original")
                .unwrap();
        }
    }
}
