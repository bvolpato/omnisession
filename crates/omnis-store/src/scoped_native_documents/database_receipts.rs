use rusqlite::{OptionalExtension, Transaction, params};
use uuid::Uuid;

use crate::{Provider, Result, SessionRef, StoreError, database_error};

const MAX_DATABASE_STEPS: usize = 100_000;

const RECEIPT_TABLES: [(Provider, &str); 3] = [
    (Provider::AntigravityDesktop, "desktop_search_receipts"),
    (Provider::Antigravity, "antigravity_cli_search_receipts"),
    (Provider::AntigravityIde, "antigravity_ide_search_receipts"),
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
             INSERT INTO session_trajectories VALUES('antigravity-desktop','session'), ('antigravity','session'), ('antigravity-ide','session');",
        )
        .unwrap();
        initialize(&tx).unwrap();
        let session = &SessionRef::new(Provider::AntigravityDesktop, "session");
        let cli = &SessionRef::new(Provider::Antigravity, "session");
        let ide = &SessionRef::new(Provider::AntigravityIde, "session");
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
        assert_eq!(read(&tx, ide, b"a", file, contents, &hash).unwrap(), None);
        save(&tx, ide, b"a", file, contents, &hash, Some(&proof[..1])).unwrap();
        assert_eq!(
            read(&tx, ide, b"a", file, contents, &hash).unwrap(),
            Some(proof[..1].to_vec())
        );
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
