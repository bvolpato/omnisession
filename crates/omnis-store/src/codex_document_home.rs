use rusqlite::{Connection, Transaction};

use super::{Result, StoreError, database_error, immediate_transaction};

/// Replaces the format 1 unique keys without changing the trajectory IDs that own FTS chunks.
pub(super) fn initialize(
    connection: &mut Connection,
    prepare_receipts: fn(&Transaction<'_>) -> Result<()>,
) -> Result<()> {
    let (foreign_keys, legacy_alter): (bool, bool) = connection
        .query_row(
            "SELECT foreign_keys, legacy_alter_table FROM pragma_foreign_keys, pragma_legacy_alter_table",
            [], |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(database_error)?;
    // Keep foreign keys in existing child tables pointed at the original names during the swap.
    // SQLite applies ON DELETE CASCADE even when foreign key constraints are deferred.
    let result = connection
        .execute_batch("PRAGMA foreign_keys=OFF; PRAGMA legacy_alter_table=ON;")
        .map_err(database_error)
        .and_then(|()| migrate(connection, prepare_receipts));
    let restore_alter = connection.pragma_update(None, "legacy_alter_table", legacy_alter);
    let restore_keys = connection.pragma_update(None, "foreign_keys", foreign_keys);
    result
        .and(restore_alter.map_err(database_error))
        .and(restore_keys.map_err(database_error))
}

fn migrate(
    connection: &mut Connection,
    prepare_receipts: fn(&Transaction<'_>) -> Result<()>,
) -> Result<()> {
    let transaction = immediate_transaction(connection)?;
    let version: i64 = transaction
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(database_error)?;
    if version >= 2 {
        return Ok(());
    }
    // Format 1 clients still use the old receipt keys if the parent-table swap rolls back.
    prepare_receipts(&transaction)?;
    for (table, key) in [
        ("session_trajectories", "unique"),
        ("session_index", "primarykey"),
        ("trajectory_index_failures", "primarykey"),
    ] {
        replace_global_key(&transaction, table, key)?;
    }
    let visible: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('session_trajectories') WHERE name='codex_workspace_current')",
        [], |row| row.get(0),
    ).map_err(database_error)?;
    if !visible {
        transaction.execute_batch(
            "ALTER TABLE session_trajectories ADD COLUMN codex_workspace_current INTEGER NOT NULL
             DEFAULT 1 CHECK(codex_workspace_current IN (0,1));"
        ).map_err(database_error)?;
    }
    transaction.execute_batch(
        "CREATE UNIQUE INDEX session_trajectory_codex_home ON session_trajectories
             (provider,session_id,provider='codex' AND origin='native',
              CASE WHEN provider='codex' AND origin='native' THEN coalesce(codex_home,'') ELSE '' END);
         CREATE UNIQUE INDEX session_index_home ON session_index
             (provider,session_id,CASE WHEN provider='codex' THEN coalesce(codex_home,'') ELSE '' END);
         CREATE UNIQUE INDEX trajectory_index_failure_home ON trajectory_index_failures
             (provider,session_id,CASE WHEN provider='codex' THEN coalesce(codex_home,'') ELSE '' END);"
    ).map_err(database_error)?;
    super::initialize_codex_document_triggers(&transaction)?;
    let broken_reference: bool = transaction
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_foreign_key_check
         WHERE parent IN ('session_trajectories','session_index','trajectory_index_failures'))",
            [],
            |row| row.get(0),
        )
        .map_err(database_error)?;
    if broken_reference {
        return Err(StoreError::CorruptStore);
    }
    transaction
        .pragma_update(None, "user_version", 2)
        .map_err(database_error)?;
    transaction.commit().map_err(database_error)
}

fn replace_global_key(transaction: &Transaction<'_>, table: &str, key: &str) -> Result<()> {
    let ddl: String = transaction
        .query_row(
            "SELECT sql FROM main.sqlite_schema WHERE type='table' AND name=?1",
            [table],
            |row| row.get(0),
        )
        .map_err(database_error)?;
    // Format 1 ended each table definition with the global key. SQLite inserts added columns
    // before that constraint. Keep their definitions, including constraints and generated values.
    let (prefix, key_columns) = ddl.rsplit_once('(').ok_or(StoreError::CorruptStore)?;
    let (prefix, constraint) = prefix.rsplit_once(',').ok_or(StoreError::CorruptStore)?;
    let compact = |text: &str| {
        text.split_ascii_whitespace()
            .collect::<String>()
            .to_ascii_lowercase()
    };
    if compact(constraint) != key
        || !matches!(
            compact(key_columns).as_str(),
            "provider,session_id))" | "provider,session_id))withoutrowid"
        )
    {
        return Err(StoreError::CorruptStore);
    }
    let (_, definition) = prefix.split_once('(').ok_or(StoreError::CorruptStore)?;
    let columns: String = transaction
        .query_row(
            r#"SELECT group_concat(printf('"%w"', name), ',') FROM
            (SELECT name FROM pragma_table_xinfo(?1) WHERE hidden=0 ORDER BY cid)"#,
            [table],
            |row| row.get(0),
        )
        .map_err(database_error)?;
    let mut objects = transaction.prepare(
        "SELECT sql FROM main.sqlite_schema WHERE tbl_name=?1 AND type IN ('index','trigger') AND sql IS NOT NULL"
    ).map_err(database_error)?;
    let objects = objects
        .query_map([table], |row| row.get::<_, String>(0))
        .map_err(database_error)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(database_error)?;
    transaction
        .execute_batch(&format!(
            "ALTER TABLE {table} RENAME TO previous_{table};
         CREATE TABLE {table} ({definition});
         INSERT INTO {table} ({columns}) SELECT {columns} FROM previous_{table};
         DROP TABLE previous_{table};"
        ))
        .map_err(database_error)?;
    for sql in objects {
        transaction.execute_batch(&sql).map_err(database_error)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Store;

    #[test]
    fn interrupted_home_upgrade_leaves_the_committed_compression_format_readable() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("cache");
        let original = "transitionalmarker survives ".repeat(64);
        let setup = Connection::open(&path).unwrap();
        setup.execute_batch(
            "CREATE TABLE session_trajectories(id INTEGER PRIMARY KEY, provider TEXT NOT NULL,
                 session_id TEXT NOT NULL, redacted_text TEXT NOT NULL, source_updated_at INTEGER NOT NULL,
                 complete INTEGER NOT NULL, indexed_at INTEGER NOT NULL, UNIQUE(provider,session_id));
             CREATE TABLE desktop_search_receipts(provider,session_id,source_home,source_file,source_contents,content_hash,step_proof,
                 PRIMARY KEY(session_id,source_home,source_file),
                 FOREIGN KEY(provider,session_id) REFERENCES session_trajectories(provider,session_id) ON DELETE CASCADE);
             INSERT INTO session_trajectories VALUES(42,'antigravity-desktop','desktop','',1,1,1);
             CREATE TABLE session_trajectory_codex_home(collision);"
        ).unwrap();
        setup
            .execute(
                "INSERT INTO session_trajectories VALUES(41,'claude','legacy',?1,1,1,1)",
                [&original],
            )
            .unwrap();
        drop(setup);

        assert!(Store::open(&path).is_err());
        let db = Connection::open(&path).unwrap();
        crate::register_search_functions(&db).unwrap();
        let intermediate: (i64, String, String) = db
            .query_row(
                "SELECT user_version,typeof(c.stored_text),v.redacted_text FROM pragma_user_version
             JOIN session_trajectory_chunk_rows c ON c.trajectory_id=41
             JOIN session_trajectory_chunks v ON v.id=c.id
             JOIN session_trajectory_chunks_fts f ON f.rowid=c.id
             WHERE session_trajectory_chunks_fts MATCH 'transitionalmarker'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(intermediate, (1, "blob".to_owned(), original));
        db.execute_batch(
            "INSERT INTO session_trajectories SELECT * FROM session_trajectories WHERE id=41
             ON CONFLICT(provider,session_id) DO NOTHING;
             INSERT INTO desktop_search_receipts VALUES('antigravity-desktop','desktop',X'01',zeroblob(16),zeroblob(16),zeroblob(32),X'');
             DROP TABLE session_trajectory_codex_home;",
        )
        .unwrap();
        drop(db);

        let store = Store::open(&path).unwrap();
        let format: i64 = store
            .connection
            .borrow()
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(format, crate::STORE_FORMAT);
        assert_eq!(
            store
                .connection
                .borrow()
                .query_row(
                    "SELECT trajectory_id FROM desktop_search_receipts WHERE session_id='desktop'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            42
        );
        assert_eq!(
            store
                .search_session_trajectories("transitionalmarker", 5)
                .unwrap(),
            [omnis_ir::SessionRef::new(
                omnis_ir::Provider::Claude,
                "legacy"
            )]
        );
    }

    #[test]
    fn upgrade_preserves_chunk_owners_and_separates_native_documents_from_portable_content() {
        let mut db = Connection::open_in_memory().unwrap();
        db.execute_batch(
            "PRAGMA user_version=1;
             PRAGMA foreign_keys=ON;
             CREATE TABLE session_trajectories(id INTEGER PRIMARY KEY, provider, session_id, origin,
                 protected_by_bundle, codex_home TEXT, UNIQUE(provider,session_id));
             ALTER TABLE session_trajectories ADD COLUMN source_contents_id BLOB;
             ALTER TABLE session_trajectories ADD COLUMN source_file_id BLOB;
             CREATE TABLE session_index(provider,session_id,codex_home, PRIMARY KEY(provider,session_id)) WITHOUT ROWID;
             CREATE TABLE trajectory_index_failures(provider,session_id,codex_home,source_file_id,source_contents_id,
                 PRIMARY KEY(provider,session_id)) WITHOUT ROWID;
             CREATE TABLE codex_workspace_observations(provider_home,session_id,source_stamp,source_file_id,source_contents_id);
             CREATE TABLE chunks(id INTEGER PRIMARY KEY, owner REFERENCES session_trajectories(id) ON DELETE CASCADE, text);
             CREATE VIRTUAL TABLE words USING fts5(text);
             CREATE TRIGGER index_words AFTER INSERT ON chunks BEGIN
                 INSERT INTO words(rowid,text) VALUES(new.id,new.text); END;
             CREATE TRIGGER delete_words AFTER DELETE ON chunks BEGIN DELETE FROM words WHERE rowid=old.id; END;
             CREATE TRIGGER preserve_manifest AFTER INSERT ON session_index BEGIN
                 DELETE FROM trajectory_index_failures WHERE provider=new.provider AND session_id=new.session_id
                     AND codex_home IS new.codex_home; END;
             CREATE INDEX metadata_by_home ON session_index(codex_home);
             INSERT INTO session_trajectories VALUES(41,'codex','shared','native',1,'home-a',X'0102',X'11');
             INSERT INTO session_trajectories VALUES(42,'codex','unowned','native',0,NULL,X'0304',NULL);
             INSERT INTO chunks VALUES(87,41,'preservedmarker');
             INSERT INTO trajectory_index_failures VALUES('codex','shared','home-a',X'11',X'0102');"
        ).unwrap();
        initialize(&mut db, |_| Ok(())).unwrap();
        let saved: (i64, i64, String, Option<String>) = db
            .query_row(
                "SELECT w.rowid,t.protected_by_bundle,hex(t.source_contents_id),t.codex_home
             FROM words w JOIN chunks c ON c.id=w.rowid JOIN session_trajectories t ON t.id=c.owner
             WHERE words MATCH 'preservedmarker' AND t.id=41",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(saved, (87, 1, "0102".to_owned(), Some("home-a".to_owned())));
        db.execute_batch(
            "INSERT INTO session_trajectories(provider,session_id,origin,protected_by_bundle,codex_home) VALUES
                 ('codex','shared','native',0,'home-b'), ('codex','shared','imported_bundle',1,NULL),
                 ('codex','unowned','native',0,'home-b');
             INSERT INTO trajectory_index_failures VALUES('codex','shared','home-b',NULL,NULL);
             INSERT INTO session_index VALUES('codex','shared','home-b');"
        ).unwrap();
        let remaining: (String, bool, bool) = db.query_row(
            "SELECT codex_home, EXISTS(SELECT 1 FROM session_trajectories WHERE id=42 AND codex_home IS NULL),
                 EXISTS(SELECT 1 FROM session_index INDEXED BY metadata_by_home WHERE codex_home='home-b')
             FROM trajectory_index_failures WHERE session_id='shared'", [],
            |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?))
        ).unwrap();
        assert_eq!(remaining, ("home-a".to_owned(), true, true));
        db.execute_batch(
            "INSERT INTO codex_workspace_observations VALUES('home-a','shared','legacy',NULL,NULL);
             UPDATE codex_workspace_observations SET source_stamp='different file',source_file_id=X'12',source_contents_id=X'0102';
             UPDATE codex_workspace_observations SET source_stamp='different contents',source_file_id=X'11',source_contents_id=X'1234';
             DELETE FROM codex_workspace_observations;",
        ).unwrap();
        let preserved: (bool, bool) = db.query_row(
            "SELECT codex_workspace_current,EXISTS(SELECT 1 FROM trajectory_index_failures WHERE codex_home='home-a')
             FROM session_trajectories WHERE id=41", [], |row| Ok((row.get(0)?,row.get(1)?)),
        ).unwrap();
        assert_eq!(preserved, (true, true));
        db.execute_batch(
            "INSERT INTO codex_workspace_observations VALUES('home-a','shared','before',X'11',X'0102');
             UPDATE codex_workspace_observations SET source_stamp='after' WHERE provider_home='home-a';"
        ).unwrap();
        let hidden: (bool, bool, bool) = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM session_trajectories WHERE id=41 AND codex_home='home-a' AND NOT codex_workspace_current),
                 EXISTS(SELECT 1 FROM session_trajectories WHERE session_id='shared' AND codex_home='home-b' AND codex_workspace_current),
                 EXISTS(SELECT 1 FROM trajectory_index_failures WHERE codex_home='home-a')",
            [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).unwrap();
        assert_eq!(hidden, (true, true, false));
        db.execute("DELETE FROM session_trajectories WHERE id=41", [])
            .unwrap();
        let hits: i64 = db
            .query_row(
                "SELECT count(*) FROM words WHERE words MATCH 'preservedmarker'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(hits, 0);
    }
}
