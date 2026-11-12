use rusqlite::{OptionalExtension, Transaction, params};

use crate::{PreparedTrajectoryWrite, Result, TrajectorySourceGuard, database_error};

pub(super) fn initialize(tx: &Transaction<'_>) -> Result<()> {
    crate::migrate_trajectory_provider_key(tx, "physical_database_body_reads")?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS physical_database_body_reads(
             session_id TEXT NOT NULL, source_home BLOB NOT NULL, source_file BLOB NOT NULL,
             database_contents BLOB NOT NULL CHECK(length(database_contents)=16),
             provider TEXT, source_contents BLOB, content_hash BLOB, declaration_generation INTEGER,
             PRIMARY KEY(session_id,source_home,source_file),
             FOREIGN KEY(provider,session_id) REFERENCES session_trajectories(provider,non_codex_session_id) ON DELETE CASCADE
         ) WITHOUT ROWID;",
    ).map_err(database_error)?;
    if !tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('physical_database_body_reads') WHERE name='observed_contents')",
        [], |row| row.get::<_, bool>(0),
    ).map_err(database_error)? {
        tx.execute_batch("ALTER TABLE physical_database_body_reads ADD COLUMN observed_contents BLOB CHECK(length(observed_contents)=16)")
            .map_err(database_error)?;
    }
    if !tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('physical_database_body_reads') WHERE name='incomplete_contents')",
        [], |row| row.get::<_, bool>(0),
    ).map_err(database_error)? {
        tx.execute_batch("ALTER TABLE physical_database_body_reads ADD COLUMN incomplete_contents BLOB CHECK(length(incomplete_contents)=16)")
            .map_err(database_error)?;
    }
    tx.execute_batch(
        "CREATE INDEX IF NOT EXISTS incomplete_database_bodies ON physical_database_body_reads(provider,session_id)
         WHERE incomplete_contents=observed_contents;",
    ).map_err(database_error)?;
    Ok(())
}

pub(super) fn contains_rows(
    tx: &Transaction<'_>,
    write: &PreparedTrajectoryWrite<'_, '_>,
    home: &[u8],
    file: uuid::Uuid,
    incoming: &[uuid::Uuid],
) -> Result<bool> {
    let owner = tx
        .query_row(
            "SELECT provider, source_contents, content_hash FROM physical_database_body_reads
         WHERE session_id=?1 AND source_home=?2 AND source_file=?3 AND provider IS NOT NULL",
            params![write.session.id, home, file.as_bytes()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    crate::optional_uuid(row, 1)?,
                    row.get::<_, Vec<u8>>(2)?,
                ))
            },
        )
        .optional()
        .map_err(database_error)?;
    let owner = if owner.is_none() {
        tx.query_row(
            "SELECT provider,source_contents_id,content_hash FROM session_trajectories
             WHERE provider=?1 AND session_id=?2 AND source_file_id=?3 AND origin='native'",
            params![
                write.session.provider.to_string(),
                write.session.id,
                file.as_bytes()
            ],
            |row| Ok((row.get(0)?, crate::optional_uuid(row, 1)?, row.get(2)?)),
        )
        .optional()
        .map_err(database_error)?
    } else {
        owner
    };
    let Some((provider, Some(contents), hash)) = owner else {
        return Ok(false);
    };
    let owner = crate::SessionRef::new(
        provider
            .parse()
            .map_err(|_| crate::StoreError::CorruptStore)?,
        write.session.id.clone(),
    );
    let previous = crate::scoped_native_documents::database_receipts::read(
        tx, &owner, home, file, contents, &hash,
    )?;
    Ok(previous.is_some_and(|steps| {
        let mut prior = steps.into_iter();
        incoming
            .iter()
            .all(|current| prior.any(|step| step == *current))
    }))
}

pub(super) fn record(
    tx: &Transaction<'_>,
    write: &PreparedTrajectoryWrite<'_, '_>,
    home: &[u8],
    accepted: bool,
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
    if !accepted {
        tx.prepare_cached(
            "WITH current_generation AS (SELECT generation FROM physical_step_declarations
                 WHERE session_id=?1 AND source_home=?2 AND source_file=?3)
             UPDATE physical_database_body_reads SET observed_contents=?4, incomplete_contents=?5,
                 declaration_generation=(SELECT generation FROM current_generation)
             WHERE session_id=?1 AND source_home=?2 AND source_file=?3 AND provider IS NOT NULL
               AND (observed_contents IS NOT ?4 OR incomplete_contents IS NOT ?5 OR declaration_generation IS NOT (SELECT generation FROM current_generation))",
        )
        .and_then(|mut statement| {
            statement.execute(params![
                write.session.id,
                home,
                file.as_bytes(),
                contents.as_bytes(),
                observed.incomplete_native_history.then_some(contents.as_bytes())
            ])
        })
        .map_err(database_error)?;
        return Ok(());
    }
    crate::desktop_declarations::wal_rows::record(tx, write, home)?;
    let owner = write.session.provider.to_string();
    let typed = write.document.database_step_proof.is_some();
    let complete = write.source_complete && !write.document.provider_limited;
    // A preview can extend the document already selected for both references.
    let certified = typed
        && (complete
            || tx
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM physical_database_body_reads
         WHERE session_id=?1 AND source_home=?2 AND source_file=?3 AND provider=?4)",
                    params![write.session.id, home, file.as_bytes(), owner],
                    |row| row.get::<_, bool>(0),
                )
                .map_err(database_error)?);
    tx.prepare_cached(
        "INSERT INTO physical_database_body_reads(session_id,source_home,source_file,database_contents,
             provider,source_contents,content_hash,observed_contents,declaration_generation)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,(SELECT generation FROM physical_step_declarations
              WHERE session_id=?1 AND source_home=?2 AND source_file=?3))
         ON CONFLICT(session_id,source_home,source_file) DO UPDATE SET
             database_contents=excluded.database_contents,provider=excluded.provider,
             source_contents=excluded.source_contents,content_hash=excluded.content_hash,
             incomplete_contents=NULL,observed_contents=excluded.observed_contents,declaration_generation=excluded.declaration_generation
         WHERE (excluded.provider IS NOT NULL OR physical_database_body_reads.database_contents<>excluded.database_contents)
           AND (physical_database_body_reads.incomplete_contents IS NOT NULL OR
               (database_contents,provider,source_contents,content_hash,observed_contents,declaration_generation) IS NOT
               (excluded.database_contents,excluded.provider,excluded.source_contents,excluded.content_hash,excluded.observed_contents,excluded.declaration_generation))",
    ).and_then(|mut statement| statement.execute(params![write.session.id,home,file.as_bytes(),database.as_bytes(),
        certified.then_some(owner),certified.then_some(contents.as_bytes()),certified.then_some(&write.content_hash),contents.as_bytes()]))
        .map(|_| ()).map_err(database_error)
}
