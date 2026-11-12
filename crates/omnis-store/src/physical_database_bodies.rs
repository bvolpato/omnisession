use rusqlite::{Transaction, params};

use crate::{PreparedTrajectoryWrite, Result, TrajectorySourceGuard, database_error};

pub(super) fn initialize(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS physical_database_body_reads(
             session_id TEXT NOT NULL, source_home BLOB NOT NULL, source_file BLOB NOT NULL,
             database_contents BLOB NOT NULL CHECK(length(database_contents)=16),
             provider TEXT, source_contents BLOB, content_hash BLOB, declaration_generation INTEGER,
             PRIMARY KEY(session_id,source_home,source_file),
             FOREIGN KEY(provider,session_id) REFERENCES session_trajectories(provider,session_id) ON DELETE CASCADE
         ) WITHOUT ROWID;",
    ).map_err(database_error)
}

pub(super) fn record(
    tx: &Transaction<'_>,
    write: &PreparedTrajectoryWrite<'_, '_>,
    home: &[u8],
) -> Result<()> {
    let TrajectorySourceGuard::ObservedDocument(observed) = write.guard else {
        return Ok(());
    };
    let (Some(database), Some(file), Some(contents)) = (
        observed.database_contents,
        write.document.source_revision.file,
        write.document.source_revision.contents,
    ) else {
        return Ok(());
    };
    let certified = write.source_complete
        && !write.document.provider_limited
        && write.document.database_step_proof.is_some();
    tx.prepare_cached(
        "INSERT INTO physical_database_body_reads(session_id,source_home,source_file,database_contents,
             provider,source_contents,content_hash,declaration_generation)
         VALUES(?1,?2,?3,?4,?5,?6,?7,(SELECT generation FROM physical_step_declarations
              WHERE session_id=?1 AND source_home=?2 AND source_file=?3))
         ON CONFLICT(session_id,source_home,source_file) DO UPDATE SET
             database_contents=excluded.database_contents,provider=excluded.provider,
             source_contents=excluded.source_contents,content_hash=excluded.content_hash,
             declaration_generation=excluded.declaration_generation
         WHERE (excluded.provider IS NOT NULL OR physical_database_body_reads.database_contents<>excluded.database_contents)
           AND (database_contents,provider,source_contents,content_hash,declaration_generation) IS NOT
               (excluded.database_contents,excluded.provider,excluded.source_contents,excluded.content_hash,excluded.declaration_generation)",
    ).and_then(|mut statement| statement.execute(params![write.session.id,home,file.as_bytes(),database.as_bytes(),
        certified.then(|| write.session.provider.to_string()),certified.then_some(contents.as_bytes()),certified.then_some(&write.content_hash)]))
        .map(|_| ()).map_err(database_error)
}
