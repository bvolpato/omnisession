use rusqlite::{OptionalExtension, Transaction, params};
use uuid::Uuid;

use crate::{Result, StoreError, database_error};

const MAX_DESKTOP_STEPS: usize = 100_000;

pub(crate) fn initialize(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS desktop_search_receipts (
             provider TEXT NOT NULL DEFAULT 'antigravity-desktop' CHECK(provider='antigravity-desktop'),
             session_id TEXT NOT NULL,
             source_home BLOB NOT NULL,
             source_file BLOB NOT NULL CHECK(length(source_file)=16),
             source_contents BLOB NOT NULL CHECK(length(source_contents)=16),
             content_hash BLOB NOT NULL CHECK(length(content_hash)=32),
             step_proof BLOB NOT NULL CHECK(length(step_proof)%16=0 AND length(step_proof)<=1600000),
             PRIMARY KEY(session_id, source_home, source_file),
             FOREIGN KEY(provider, session_id) REFERENCES session_trajectories(provider, session_id) ON DELETE CASCADE
         ) WITHOUT ROWID;",
    )
    .map_err(database_error)
}

pub(crate) fn read(
    tx: &Transaction<'_>,
    session_id: &str,
    home: &[u8],
    file: Uuid,
    contents: Uuid,
    content_hash: &[u8],
) -> Result<Option<Vec<Uuid>>> {
    tx.prepare_cached(
        "SELECT step_proof FROM desktop_search_receipts
         WHERE session_id=?1 AND source_home=?2 AND source_file=?3
           AND source_contents=?4 AND content_hash=?5",
    )
    .and_then(|mut statement| {
        statement
            .query_row(
                params![
                    session_id,
                    home,
                    file.as_bytes(),
                    contents.as_bytes(),
                    content_hash
                ],
                |row| {
                    let proof = row.get_ref(0)?.as_blob()?;
                    let (steps, tail) = proof.as_chunks::<16>();
                    Ok((tail.is_empty() && steps.len() <= MAX_DESKTOP_STEPS)
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
    session_id: &str,
    home: &[u8],
    file: Uuid,
    contents: Uuid,
    content_hash: &[u8],
    proof: Option<&[Uuid]>,
) -> Result<()> {
    if let Some(proof) = proof {
        if proof.len() > MAX_DESKTOP_STEPS {
            return Err(StoreError::TrajectoryBatchTooLarge);
        }
        let bytes: Vec<u8> = proof.iter().flat_map(Uuid::as_bytes).copied().collect();
        tx.prepare_cached(
            "INSERT INTO desktop_search_receipts(session_id, source_home, source_file, source_contents, content_hash, step_proof)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(session_id, source_home, source_file) DO UPDATE SET
                 source_contents=excluded.source_contents, content_hash=excluded.content_hash, step_proof=excluded.step_proof",
        )
        .and_then(|mut statement| {
            statement.execute(params![
                session_id, home, file.as_bytes(), contents.as_bytes(), content_hash, bytes
            ])
        })
    } else {
        tx.prepare_cached(
            "DELETE FROM desktop_search_receipts WHERE session_id=?1 AND source_home=?2 AND source_file=?3",
        )
        .and_then(|mut statement| statement.execute(params![session_id, home, file.as_bytes()]))
    }
    .map_err(database_error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{initialize, read, save};
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
             INSERT INTO session_trajectories VALUES('antigravity-desktop','session');",
        )
        .unwrap();
        initialize(&tx).unwrap();
        let session = "session";
        let (file, contents) = (Uuid::new_v4(), Uuid::new_v4());
        let hash = [1; 32];
        let proof = [Uuid::new_v4(), Uuid::new_v4()];
        let lookup = |home: &[u8], file, contents, hash: &[u8]| {
            read(&tx, session, home, file, contents, hash).unwrap()
        };
        assert_eq!(lookup(b"a", file, contents, &hash), None);
        save(&tx, session, b"a", file, contents, &hash, Some(&proof)).unwrap();
        save(&tx, session, b"b", file, contents, &hash, Some(&[])).unwrap();
        assert_eq!(lookup(b"a", file, contents, &hash), Some(proof.to_vec()));
        assert_eq!(lookup(b"a", file, contents, &[2; 32]), None);
        assert_eq!(lookup(b"a", Uuid::new_v4(), contents, &hash), None);
        assert_eq!(lookup(b"a", file, Uuid::new_v4(), &hash), None);
        save(&tx, session, b"a", file, contents, &hash, None).unwrap();
        assert_eq!(lookup(b"a", file, contents, &hash), None);
        assert_eq!(lookup(b"b", file, contents, &hash), Some(Vec::new()));
    }
}
