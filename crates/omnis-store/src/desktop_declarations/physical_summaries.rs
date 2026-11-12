use super::{Result, Transaction, database_error};

pub(super) fn initialize(tx: &Transaction<'_>) -> Result<()> {
    let exists = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='physical_database_summaries')",
        [], |row| row.get::<_, bool>(0),
    ).map_err(database_error)?;
    if !exists {
        create(tx)?;
    }
    if tx.execute(
        "INSERT OR IGNORE INTO session_trajectory_migrations(name) VALUES('recover_physical_summary_fields')",
        [],
    ).map_err(database_error)? != 0 {
        tx.execute_batch(include_str!("summary_fields.sql")).map_err(database_error)?;
    }
    Ok(())
}

fn create(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch(
        "CREATE TABLE physical_database_summaries(
            session_id TEXT NOT NULL, source_home BLOB NOT NULL, source_file BLOB NOT NULL,
            summary TEXT NOT NULL, PRIMARY KEY(session_id,source_home,source_file)
         ) WITHOUT ROWID;",
    )
    .map_err(database_error)?;
    for (table, peer) in [
        ("desktop_step_declarations", "ide_step_declarations"),
        ("ide_step_declarations", "desktop_step_declarations"),
    ] {
        tx.execute_batch(&format!(
            "CREATE TRIGGER save_summary_{table} AFTER INSERT ON {table} WHEN new.summary IS NOT NULL BEGIN
                INSERT OR REPLACE INTO physical_database_summaries VALUES(new.session_id,new.source_home,new.source_file,new.summary);
             END;
             CREATE TRIGGER forget_summary_{table} AFTER DELETE ON {table}
             WHEN NOT EXISTS(SELECT 1 FROM {peer} WHERE session_id=old.session_id AND source_home=old.source_home AND source_file=old.source_file)
             BEGIN DELETE FROM physical_database_summaries WHERE session_id=old.session_id AND source_home=old.source_home AND source_file=old.source_file;
             END;"
        )).map_err(database_error)?;
    }
    Ok(())
}
