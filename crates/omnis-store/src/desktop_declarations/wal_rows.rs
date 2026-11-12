use rusqlite::{Connection, OptionalExtension, Transaction, params};
use uuid::Uuid;

use crate::{PreparedTrajectoryWrite, Result, StoreError, TrajectorySourceGuard, database_error};

pub(super) fn initialize(tx: &Transaction<'_>) -> Result<()> {
    crate::migrate_trajectory_provider_key(tx, "desktop_wal_row_counts")?;
    crate::migrate_trajectory_provider_key(tx, "desktop_wal_committed_rows")?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS desktop_wal_row_counts(
             provider TEXT NOT NULL, session_id TEXT NOT NULL, source_home BLOB NOT NULL,
             source_file BLOB NOT NULL, checkpoint BLOB NOT NULL, steps INTEGER NOT NULL CHECK(steps>0),
             PRIMARY KEY(session_id,source_home,source_file),
             FOREIGN KEY(provider,session_id) REFERENCES session_trajectories(provider,non_codex_session_id) ON DELETE CASCADE
         ) WITHOUT ROWID;
         CREATE TABLE IF NOT EXISTS desktop_wal_committed_rows(
             provider TEXT NOT NULL, session_id TEXT NOT NULL, source_home BLOB NOT NULL,
             source_file BLOB NOT NULL, checkpoint BLOB NOT NULL, commits BLOB NOT NULL,
             typed_rows BLOB NOT NULL, summary_steps INTEGER, summary_revision BLOB,
             PRIMARY KEY(session_id,source_home,source_file),
             FOREIGN KEY(provider,session_id) REFERENCES session_trajectories(provider,non_codex_session_id) ON DELETE CASCADE
         ) WITHOUT ROWID;
         CREATE TRIGGER IF NOT EXISTS retire_changed_desktop_wal_rows AFTER INSERT ON physical_step_declarations
         BEGIN
             DELETE FROM desktop_wal_committed_rows WHERE session_id=NEW.session_id
                 AND source_home=NEW.source_home AND source_file=NEW.source_file
                 AND summary_steps IS NOT NEW.steps;
         END;
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
    let (Some(checkpoint), Some(file), Some(rows)) = (
        observed.database_checkpoint,
        write.document.source_revision.file,
        write.document.database_step_proof,
    ) else {
        return Ok(());
    };
    let id = &write.session.id;
    let summary = write.document.database_summary_revision;
    let commits = write
        .document
        .database_wal_commits
        .filter(|proof| !proof.is_empty());
    let typed_wal = commits.filter(|_| checkpoint.1.is_some() && !rows.is_empty());
    let partial = !write.source_complete || write.document.provider_limited;
    if partial && typed_wal.is_none() {
        return Ok(());
    }
    if restored_rows(tx, id, home, file, Some(checkpoint), commits, summary)?.is_some() {
        return Ok(());
    }
    let encoded = typed_wal.map(encode).transpose()?;
    if partial
        && let Some(commits) = &encoded
        && loses_prior_rows(tx, id, home, file, checkpoint.0, commits, rows)?
    {
        return Ok(());
    }
    let key = params![id, home, file.as_bytes()];
    tx.execute(
        "DELETE FROM desktop_wal_committed_rows WHERE session_id=?1 AND source_home=?2 AND source_file=?3",
        key,
    ).map_err(database_error)?;
    tx.execute(
        "DELETE FROM desktop_wal_row_counts WHERE session_id=?1 AND source_home=?2 AND source_file=?3",
        key,
    ).map_err(database_error)?;
    if checkpoint.1.is_none() || rows.is_empty() {
        return Ok(());
    }
    let Some(commits) = encoded else {
        if observed.desktop_steps.is_none() {
            tx.execute(
                "INSERT INTO desktop_wal_row_counts(provider,session_id,source_home,source_file,checkpoint,steps)
                 VALUES(?1,?2,?3,?4,?5,?6)",
                params![write.session.provider.to_string(), id, home, file.as_bytes(), checkpoint.0.as_bytes(),
                    i64::try_from(rows.len()).map_err(|_| StoreError::InvalidSessionReference)?],
            ).map_err(database_error)?;
        }
        return Ok(());
    };
    let declared = observed
        .desktop_steps
        .map(i64::try_from)
        .transpose()
        .map_err(|_| StoreError::InvalidSessionReference)?;
    tx.execute(
        "INSERT INTO desktop_wal_committed_rows(
             provider, session_id, source_home, source_file, checkpoint, commits, typed_rows, summary_steps, summary_revision)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
        params![write.session.provider.to_string(), id, home, file.as_bytes(), checkpoint.0.as_bytes(),
                commits, encode(rows)?, declared, summary.map(Uuid::into_bytes)],
    ).map_err(database_error)?;
    Ok(())
}

const MAX_DIGESTS: usize = 100_000;

fn encode(ids: &[Uuid]) -> Result<Vec<u8>> {
    if ids.len() > MAX_DIGESTS {
        return Err(StoreError::TrajectoryBatchTooLarge);
    }
    Ok(ids.iter().flat_map(Uuid::as_bytes).copied().collect())
}

fn loses_prior_rows(
    connection: &Connection,
    id: &str,
    home: &[u8],
    file: Uuid,
    base: Uuid,
    commits: &[u8],
    rows: &[Uuid],
) -> Result<bool> {
    connection.query_row(
        "SELECT typed_rows FROM desktop_wal_committed_rows
         WHERE session_id=?1 AND source_home=?2 AND source_file=?3 AND checkpoint=?4
           AND length(commits)<=length(?5) AND substr(?5,length(commits)-15,16)=substr(commits,-16)",
        params![id,home,file.as_bytes(),base.as_bytes(),commits],
        |row| {
            let prior = row.get_ref(0)?.as_blob()?;
            let (steps, tail) = prior.as_chunks::<16>();
            let mut incoming = rows.iter();
            Ok(!tail.is_empty() || !steps.iter().all(|step| incoming.any(|new| new.as_bytes() == step)))
        },
    ).optional().map(|loses| loses.unwrap_or(false)).map_err(database_error)
}

pub(crate) fn restored_rows(
    connection: &Connection,
    id: &str,
    home: &[u8],
    file: Uuid,
    checkpoint: Option<(Uuid, Option<Uuid>)>,
    commits: Option<&[Uuid]>,
    current_summary: Option<Uuid>,
) -> Result<Option<Vec<Uuid>>> {
    let Some((base, wal)) = checkpoint else {
        return Ok(None);
    };
    let prefix = if wal.is_some() {
        let Some(commits) = commits.filter(|ids| !ids.is_empty() && ids.len() <= MAX_DIGESTS)
        else {
            return Ok(None);
        };
        // Each commit digest covers the WAL header and every preceding frame.
        let end =
            i64::try_from(commits.len() * 16).map_err(|_| StoreError::InvalidSessionReference)?;
        Some((end, commits[commits.len() - 1].as_bytes()))
    } else {
        None
    };
    connection
        .query_row(
            "SELECT typed_rows FROM desktop_wal_committed_rows
         WHERE session_id=?1 AND source_home=?2 AND source_file=?3 AND checkpoint=?4
           AND (?5 IS NULL OR summary_revision=?5)
           AND (?6 IS NULL OR (?6<length(commits) AND substr(commits,?6-15,16)=?7))",
            params![
                id,
                home,
                file.as_bytes(),
                base.as_bytes(),
                current_summary.map(Uuid::into_bytes),
                prefix.map(|(bytes, _)| bytes),
                prefix.map(|(_, tip)| tip),
            ],
            |row| {
                let bytes = row.get_ref(0)?.as_blob()?;
                let (rows, tail) = bytes.as_chunks::<16>();
                Ok(
                    (tail.is_empty() && !rows.is_empty() && rows.len() <= MAX_DIGESTS)
                        .then(|| rows.iter().copied().map(Uuid::from_bytes).collect()),
                )
            },
        )
        .optional()
        .map(Option::flatten)
        .map_err(database_error)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_only_the_saved_wal_prefix_and_summary() {
        let mut db = Connection::open_in_memory().unwrap();
        let tx = db.transaction().unwrap();
        tx.execute_batch(
            "CREATE TABLE session_trajectories(provider,session_id,
                 non_codex_session_id AS (CASE WHEN provider<>'codex' THEN session_id END),
                 UNIQUE(provider,non_codex_session_id));
             INSERT INTO session_trajectories VALUES('antigravity-desktop','s');
             CREATE TABLE physical_step_declarations(session_id,source_home,source_file,steps);",
        )
        .unwrap();
        initialize(&tx).unwrap();
        let file = Uuid::new_v4();
        let base = Uuid::new_v4();
        let tip = Uuid::new_v4();
        let summary = Uuid::new_v4();
        let commits = [Uuid::new_v4(), Uuid::new_v4()];
        let rows = [Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4()];
        tx.execute(
            "INSERT INTO desktop_wal_committed_rows VALUES('antigravity-desktop','s',?1,?2,?3,?4,?5,2,?6)",
            params![b"h",file.as_bytes(),base.as_bytes(),encode(&commits).unwrap(),encode(&rows).unwrap(),summary.as_bytes()],
        ).unwrap();
        let read = |wal, prefix: Option<&[Uuid]>, stamp| {
            restored_rows(&tx, "s", b"h", file, Some((base, wal)), prefix, stamp).unwrap()
        };
        let declare = |steps: i64| {
            tx.execute(
                "INSERT INTO physical_step_declarations VALUES('s',?1,?2,?3)",
                params![b"h", file.as_bytes(), steps],
            )
            .unwrap();
        };
        let extends = encode(&[commits[0], commits[1], Uuid::new_v4()]).unwrap();
        assert!(loses_prior_rows(&tx, "s", b"h", file, base, &extends, &rows[..2]).unwrap());
        assert!(!loses_prior_rows(&tx, "s", b"h", file, base, &extends, &rows).unwrap());
        declare(2);
        assert_eq!(
            read(Some(tip), Some(&commits[..1]), Some(summary)),
            Some(rows.to_vec())
        );
        assert_eq!(read(None, None, None), Some(rows.to_vec()));
        assert_eq!(read(Some(tip), Some(&commits), None), None);
        assert_eq!(read(Some(tip), Some(&commits[1..]), None), None);
        assert_eq!(read(Some(tip), Some(&[]), None), None);
        assert_eq!(read(None, None, Some(Uuid::new_v4())), None);
        declare(1);
        assert_eq!(read(None, None, None), None);
    }
}
