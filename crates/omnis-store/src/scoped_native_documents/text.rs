use rusqlite::{Statement, Transaction};
use sha2::{Digest, Sha256};

use crate::{
    MAX_TRAJECTORY_WRITE_BATCH_BYTES, Result, SEARCH_QUERY_MAX_CHARS, StoreError,
    TRAJECTORY_CHUNK_MAX_OVERLAP_BYTES, chunk_text, database_error,
};

#[derive(Clone, Copy)]
enum Overlap {
    Characters,
    Bytes,
    None,
}

impl Overlap {
    fn suffix(self, previous: &str) -> &str {
        match self {
            Self::Characters => previous
                .char_indices()
                .rev()
                .nth(SEARCH_QUERY_MAX_CHARS - 1)
                .filter(|(offset, _)| *offset != 0)
                .map_or("", |(offset, _)| &previous[offset..]),
            Self::Bytes => {
                let mut start = previous
                    .len()
                    .saturating_sub(TRAJECTORY_CHUNK_MAX_OVERLAP_BYTES);
                while !previous.is_char_boundary(start) {
                    start += 1;
                }
                &previous[start..]
            }
            Self::None => "",
        }
    }
}

pub(super) fn archived_text(
    tx: &Transaction<'_>,
    trajectory_id: i64,
    indexed_bytes: i64,
    expected_hash: &[u8],
) -> Result<String> {
    let length = usize::try_from(indexed_bytes).map_err(|_| StoreError::CorruptStore)?;
    if length > MAX_TRAJECTORY_WRITE_BATCH_BYTES {
        return Err(StoreError::TrajectoryBatchTooLarge);
    }
    let mut statement = tx
        .prepare_cached(
            "SELECT chunk_index,stored_text FROM session_trajectory_chunk_rows
             WHERE trajectory_id=?1 AND chunk_index < ?2 ORDER BY chunk_index",
        )
        .map_err(database_error)?;
    // Older stores used byte overlap or disjoint chunks. The document digest identifies the format.
    for overlap in [Overlap::Characters, Overlap::Bytes, Overlap::None] {
        if let Some(text) = join_chunks(&mut statement, trajectory_id, length, overlap)?
            && Sha256::digest(text.as_bytes()).as_slice() == expected_hash
        {
            return Ok(text);
        }
    }
    Err(StoreError::CorruptStore)
}

fn join_chunks(
    statement: &mut Statement<'_>,
    trajectory_id: i64,
    length: usize,
    overlap: Overlap,
) -> Result<Option<String>> {
    let mut rows = statement
        .query([trajectory_id, crate::FIRST_PRIMARY_LEGACY_CHUNK])
        .map_err(database_error)?;
    let mut text = String::new();
    let mut scratch = Vec::new();
    let mut index = 0_i64;
    let mut previous_length = 0;
    while let Some(row) = rows.next().map_err(database_error)? {
        if row.get::<_, i64>(0).map_err(database_error)? != index {
            return Err(StoreError::CorruptStore);
        }
        let chunk = chunk_text(row.get_ref(1).map_err(database_error)?, &mut scratch)
            .ok_or(StoreError::CorruptStore)?;
        let previous = &text[text.len() - previous_length..];
        let Some(new_text) = chunk.strip_prefix(overlap.suffix(previous)) else {
            return Ok(None);
        };
        if new_text.len() > length - text.len() {
            return Ok(None);
        }
        text.push_str(new_text);
        previous_length = chunk.len();
        index += 1;
    }
    Ok((text.len() == length).then_some(text))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::{Connection, params};

    #[test]
    fn archive_verifies_unicode_overlap_in_all_stored_chunk_formats() {
        let mut connection = Connection::open_in_memory().unwrap();
        let tx = connection.transaction().unwrap();
        tx.execute_batch(
            "CREATE TABLE session_trajectory_chunk_rows(trajectory_id, chunk_index, stored_text)",
        )
        .unwrap();
        assert!(matches!(
            archived_text(
                &tx,
                1,
                i64::try_from(MAX_TRAJECTORY_WRITE_BATCH_BYTES + 1).unwrap(),
                &[0; 32]
            ),
            Err(StoreError::TrajectoryBatchTooLarge)
        ));
        let first = "青".repeat(12_001);
        let text = format!("{first}tail");
        let hash = Sha256::digest(text.as_bytes());
        let size = i64::try_from(text.len()).unwrap();
        tx.execute(
            "INSERT INTO session_trajectory_chunk_rows VALUES(1,0,?1)",
            [&first],
        )
        .unwrap();
        tx.execute(
            "INSERT INTO session_trajectory_chunk_rows VALUES(1,?1,'independent older search evidence')",
            [crate::FIRST_PRIMARY_LEGACY_CHUNK],
        ).unwrap();
        for repeated_chars in [4_096, 16_384 / 3, 0] {
            let second = format!("{}tail", "青".repeat(repeated_chars));
            tx.execute(
                "INSERT INTO session_trajectory_chunk_rows VALUES(1,1,?1)",
                params![second],
            )
            .unwrap();
            assert_eq!(archived_text(&tx, 1, size, &hash).unwrap(), text);
            assert!(archived_text(&tx, 1, size, &[0; 32]).is_err());
            tx.execute(
                "DELETE FROM session_trajectory_chunk_rows WHERE chunk_index=1",
                [],
            )
            .unwrap();
        }
        assert!(archived_text(&tx, 1, size, &hash).is_err());
    }
}
