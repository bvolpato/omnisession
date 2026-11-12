use super::{Result, Transaction, database_error};

pub(super) fn initialize(tx: &Transaction<'_>) -> Result<()> {
    let exists = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='physical_database_summaries')",
        [], |row| row.get::<_, bool>(0),
    ).map_err(database_error)?;
    if exists {
        return Ok(());
    }
    tx.execute_batch(
        "CREATE TABLE physical_database_summaries(
            session_id TEXT NOT NULL, source_home BLOB NOT NULL, source_file BLOB NOT NULL,
            summary TEXT NOT NULL, PRIMARY KEY(session_id,source_home,source_file)
         ) WITHOUT ROWID;
         WITH summaries AS (
             SELECT 'antigravity-ide' AS provider,session_id,source_home,source_file,steps,summary FROM ide_step_declarations
             UNION ALL SELECT 'antigravity-desktop',session_id,source_home,source_file,steps,summary FROM desktop_step_declarations
         ), candidates AS (
             SELECT s.*,coalesce(c.source_file_id=p.source_file AND c.source_contents_id=p.source_contents AND h.verified AND h.home=p.source_home,0) AS matches_revision
             FROM summaries s JOIN physical_step_declarations p USING(session_id,source_home,source_file,steps)
             LEFT JOIN session_index c USING(provider,session_id)
             LEFT JOIN native_catalog_homes h USING(provider,session_id)
             WHERE s.summary IS NOT NULL
         ), ranked AS (
             SELECT *, max(matches_revision) OVER (PARTITION BY session_id,source_home,source_file) AS best FROM candidates
         )
         INSERT INTO physical_database_summaries
             SELECT session_id,source_home,source_file,min(summary) FROM ranked WHERE matches_revision=best
             GROUP BY session_id,source_home,source_file HAVING min(summary)=max(summary);",
    ).map_err(database_error)?;
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
