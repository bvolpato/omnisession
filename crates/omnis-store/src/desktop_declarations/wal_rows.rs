use rusqlite::{Connection, OptionalExtension, Transaction, params};
use uuid::Uuid;

use crate::{PreparedTrajectoryWrite, Result, StoreError, TrajectorySourceGuard, database_error};

pub(super) fn initialize(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS desktop_wal_row_counts(
             provider TEXT NOT NULL, session_id TEXT NOT NULL, source_home BLOB NOT NULL,
             source_file BLOB NOT NULL, checkpoint BLOB NOT NULL, steps INTEGER NOT NULL CHECK(steps>0),
             PRIMARY KEY(session_id,source_home,source_file),
             FOREIGN KEY(provider,session_id) REFERENCES session_trajectories(provider,session_id) ON DELETE CASCADE
         ) WITHOUT ROWID;
         CREATE TRIGGER IF NOT EXISTS retire_desktop_wal_count AFTER INSERT ON physical_step_declarations
         BEGIN
             DELETE FROM desktop_wal_row_counts WHERE session_id=NEW.session_id
                 AND source_home=NEW.source_home AND source_file=NEW.source_file;
         END;",
    ).map_err(database_error)
}

pub(crate) fn record(
    tx: &Transaction<'_>,
    write: &PreparedTrajectoryWrite<'_, '_>,
    home: &[u8],
) -> Result<()> {
    let TrajectorySourceGuard::ObservedDocument(observed) = write.guard else {
        return Ok(());
    };
    let (Some((checkpoint, Some(_))), Some(file), Some(rows)) = (
        observed.database_checkpoint,
        write.document.source_revision.file,
        write.document.database_step_proof,
    ) else {
        return Ok(());
    };
    if observed.desktop_steps.is_some()
        || !write.source_complete
        || write.document.provider_limited
        || rows.is_empty()
    {
        return Ok(());
    }
    tx.execute(
        "INSERT INTO desktop_wal_row_counts(provider,session_id,source_home,source_file,checkpoint,steps)
         VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(session_id,source_home,source_file) DO UPDATE SET
         provider=excluded.provider,checkpoint=excluded.checkpoint,steps=excluded.steps",
        params![write.session.provider.to_string(),write.session.id,home,file.as_bytes(),checkpoint.as_bytes(),
            i64::try_from(rows.len()).map_err(|_|StoreError::InvalidSessionReference)?]
    ).map_err(database_error)?;
    Ok(())
}

pub(super) fn missing(
    connection: &Connection,
    id: &str,
    home: &[u8],
    file: Uuid,
    checkpoint: Option<(Uuid, Option<Uuid>)>,
) -> Result<Option<usize>> {
    let Some((base, None)) = checkpoint else {
        return Ok(None);
    };
    connection.query_row(
        "SELECT steps FROM desktop_wal_row_counts WHERE session_id=?1 AND source_home=?2 AND source_file=?3 AND checkpoint=?4",
        params![id,home,file.as_bytes(),base.as_bytes()],|row|row.get::<_,i64>(0)
    ).optional().map_err(database_error)?.map(usize::try_from).transpose().map_err(|_|StoreError::CorruptStore)
}
