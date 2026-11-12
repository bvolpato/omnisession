use rusqlite::{OptionalExtension, Transaction, params};
use uuid::Uuid;

use crate::{Provider, Result, SessionRef, StoreError, database_error};

use omnis_ir::ANTIGRAVITY_DATABASE_STEP_LIMIT as MAX_DATABASE_STEPS;

const RECEIPT_TABLES: [(Provider, &str); 2] = [
    (Provider::AntigravityDesktop, "desktop_search_receipts"),
    (Provider::Antigravity, "antigravity_cli_search_receipts"),
];

fn receipt_table(provider: Provider) -> Option<&'static str> {
    RECEIPT_TABLES
        .into_iter()
        .find_map(|(owner, table)| (owner == provider).then_some(table))
}

pub(crate) fn initialize(tx: &Transaction<'_>) -> Result<()> {
    for (provider, table) in RECEIPT_TABLES {
        tx.execute_batch(&format!(
        "CREATE TABLE IF NOT EXISTS {table} (
             provider TEXT NOT NULL DEFAULT '{provider}' CHECK(provider='{provider}'),
             session_id TEXT NOT NULL,
             source_home BLOB NOT NULL,
             source_file BLOB NOT NULL CHECK(length(source_file)=16),
             source_contents BLOB NOT NULL CHECK(length(source_contents)=16),
             content_hash BLOB NOT NULL CHECK(length(content_hash)=32),
             step_proof BLOB NOT NULL CHECK(length(step_proof)%16=0 AND length(step_proof)<=1600000),
             PRIMARY KEY(session_id, source_home, source_file),
             FOREIGN KEY(provider, session_id) REFERENCES session_trajectories(provider, session_id) ON DELETE CASCADE
         ) WITHOUT ROWID;",
    )).map_err(database_error)?;
    }
    Ok(())
}

pub(crate) fn read(
    tx: &Transaction<'_>,
    session: &SessionRef,
    home: &[u8],
    file: Uuid,
    contents: Uuid,
    content_hash: &[u8],
) -> Result<Option<Vec<Uuid>>> {
    let Some(table) = receipt_table(session.provider) else {
        return Ok(None);
    };
    tx.prepare_cached(&format!(
        "SELECT step_proof FROM {table}
         WHERE session_id=?1 AND source_home=?2 AND source_file=?3
           AND source_contents=?4 AND content_hash=?5",
    ))
    .and_then(|mut statement| {
        statement
            .query_row(
                params![
                    session.id,
                    home,
                    file.as_bytes(),
                    contents.as_bytes(),
                    content_hash
                ],
                |row| {
                    let proof = row.get_ref(0)?.as_blob()?;
                    let (steps, tail) = proof.as_chunks::<16>();
                    Ok((tail.is_empty() && steps.len() <= MAX_DATABASE_STEPS)
                        .then(|| steps.iter().copied().map(Uuid::from_bytes).collect()))
                },
            )
            .optional()
            .map(Option::flatten)
    })
    .map_err(database_error)
}

pub(crate) fn save(
    tx: &Transaction<'_>,
    session: &SessionRef,
    home: &[u8],
    file: Uuid,
    contents: Uuid,
    content_hash: &[u8],
    proof: Option<&[Uuid]>,
) -> Result<()> {
    let Some(table) = receipt_table(session.provider) else {
        return Ok(());
    };
    if let Some(proof) = proof {
        if proof.len() > MAX_DATABASE_STEPS {
            return Err(StoreError::TrajectoryBatchTooLarge);
        }
        let bytes: Vec<u8> = proof.iter().flat_map(Uuid::as_bytes).copied().collect();
        tx.prepare_cached(&format!(
            "INSERT INTO {table}(session_id, source_home, source_file, source_contents, content_hash, step_proof)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(session_id, source_home, source_file) DO UPDATE SET
                 source_contents=excluded.source_contents, content_hash=excluded.content_hash, step_proof=excluded.step_proof",
        ))
        .and_then(|mut statement| {
            statement.execute(params![
                session.id, home, file.as_bytes(), contents.as_bytes(), content_hash, bytes
            ])
        })
    } else {
        tx.prepare_cached(&format!(
            "DELETE FROM {table} WHERE session_id=?1 AND source_home=?2 AND source_file=?3",
        ))
        .and_then(|mut statement| statement.execute(params![session.id, home, file.as_bytes()]))
    }
    .map_err(database_error)?;
    Ok(())
}

pub(super) fn record_verified(
    tx: &Transaction<'_>,
    write: &crate::PreparedTrajectoryWrite<'_, '_>,
    home: &std::path::Path,
) -> Result<()> {
    let Some(steps) = write
        .document
        .projection
        .and_then(|p| p.database_steps.as_deref())
    else {
        return Ok(());
    };
    let revision = write.document.source_revision;
    if let (Some(file), Some(contents)) = (revision.file, revision.contents) {
        let matches: bool = tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM session_trajectories
            WHERE provider=?1 AND session_id=?2 AND origin='native' AND source_file_id=?3 AND source_contents_id=?4 AND content_hash=?5)",
            params![write.session.provider.to_string(), write.session.id, file.as_bytes(), contents.as_bytes(), write.content_hash],
            |row| row.get(0),
        ).map_err(database_error)?;
        if matches {
            save(
                tx,
                write.session,
                home.as_os_str().as_encoded_bytes(),
                file,
                contents,
                &write.content_hash,
                Some(steps),
            )?;
        }
    }
    Ok(())
}

pub(super) fn accept_gap(
    tx: &Transaction<'_>,
    write: &crate::PreparedTrajectoryWrite<'_, '_>,
    home: Option<&std::path::Path>,
) -> Result<Option<bool>> {
    use crate::{SessionTrajectoryOrigin, optional_uuid, stored_projection_text};
    let Some(projection) = write.document.projection.filter(|p| p.database_gap) else {
        return Ok(None);
    };
    if write.document.origin != SessionTrajectoryOrigin::Native || write.verified_metadata.is_none()
    {
        return Ok(None);
    }
    let revision = write.document.source_revision;
    if write.guard.matches_document_file(revision) != Some(true) {
        return Ok(None);
    }
    let (Some(home), Some(file)) = (home, revision.file) else {
        return Ok(Some(false));
    };
    let home = home.as_os_str().as_encoded_bytes();
    let prior = tx.query_row(
        "SELECT doc.id,doc.indexed_byte_count,doc.source_contents_id,doc.content_hash,doc.truncation_strategy
        FROM session_trajectories doc JOIN native_search_document_homes receipt ON doc.id=receipt.trajectory_id AND doc.source_file_id=receipt.source_file
        WHERE doc.provider=?1 AND doc.session_id=?2 AND doc.origin='native' AND doc.source_file_id=?3 AND receipt.verification=1 AND receipt.source_home=?4",
        params![write.session.provider.to_string(), write.session.id, file.as_bytes(), home],
        |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, u32>(1)? as usize,
                optional_uuid(row, 2)?,
                row.get::<_, Vec<u8>>(3)?,
                row.get::<_, String>(4)?,
            ))
        },
    ).optional().map_err(database_error)?;
    let Some((id, len, Some(contents), hash, strategy)) = prior else {
        return Ok(Some(false));
    };
    let Some(rows) = projection.database_steps.as_deref() else {
        return Ok(Some(false));
    };
    if let Some(previous) = read(tx, write.session, home, file, contents, &hash)? {
        let mut incoming = rows.iter();
        return Ok(Some(
            previous
                .iter()
                .all(|step| incoming.any(|next| step == next)),
        ));
    }
    if hash == write.content_hash {
        return Ok(Some(true));
    }
    if len > 2 * omnis_ir::SEARCH_DOCUMENT_EDGE_BYTE_LIMIT
        || strategy.contains("document_head_tail")
        || write
            .document
            .truncation_strategy
            .contains("document_head_tail")
    {
        return Ok(Some(false));
    }
    let prior = stored_projection_text(tx, id, len)?;
    Ok(Some(prior.is_some_and(|old| {
        let mut observed = write.document.redacted_text.split("\n\n");
        old.is_empty()
            || old
                .split("\n\n")
                .all(|part| observed.any(|current| current == part))
    })))
}

#[cfg(test)]
mod tests {
    use super::{Provider, SessionRef, initialize, read, save};
    use rusqlite::Connection;
    use uuid::Uuid;

    #[test]
    fn receipt_follows_its_saved_projection_and_source() {
        let mut connection = Connection::open_in_memory().unwrap();
        let tx = connection.transaction().unwrap();
        tx.execute_batch(
            "CREATE TABLE session_trajectories (
                provider TEXT, session_id TEXT, PRIMARY KEY(provider, session_id)
             );
             INSERT INTO session_trajectories VALUES('antigravity-desktop','session'), ('antigravity','session');",
        )
        .unwrap();
        initialize(&tx).unwrap();
        let session = &SessionRef::new(Provider::AntigravityDesktop, "session");
        let cli = &SessionRef::new(Provider::Antigravity, "session");
        let (file, contents) = (Uuid::new_v4(), Uuid::new_v4());
        let hash = [1; 32];
        let proof = [Uuid::new_v4(), Uuid::new_v4()];
        let lookup = |home: &[u8], file, contents, hash: &[u8]| {
            read(&tx, session, home, file, contents, hash).unwrap()
        };
        assert_eq!(lookup(b"a", file, contents, &hash), None);
        save(&tx, session, b"a", file, contents, &hash, Some(&proof)).unwrap();
        assert_eq!(read(&tx, cli, b"a", file, contents, &hash).unwrap(), None);
        save(&tx, cli, b"a", file, contents, &hash, Some(&[])).unwrap();
        save(&tx, session, b"b", file, contents, &hash, Some(&[])).unwrap();
        assert_eq!(lookup(b"a", file, contents, &hash), Some(proof.to_vec()));
        assert_eq!(lookup(b"a", file, contents, &[2; 32]), None);
        assert_eq!(lookup(b"a", Uuid::new_v4(), contents, &hash), None);
        assert_eq!(lookup(b"a", file, Uuid::new_v4(), &hash), None);
        save(&tx, session, b"a", file, contents, &hash, None).unwrap();
        assert_eq!(lookup(b"a", file, contents, &hash), None);
        assert_eq!(lookup(b"b", file, contents, &hash), Some(Vec::new()));
        assert_eq!(
            read(&tx, cli, b"a", file, contents, &hash).unwrap(),
            Some(Vec::new())
        );
    }
}
