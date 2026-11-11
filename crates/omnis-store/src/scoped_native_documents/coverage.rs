use std::borrow::Cow;

use rusqlite::{OptionalExtension, Transaction, params};
use uuid::Uuid;

use crate::{PreparedTrajectoryWrite, Result, StoreError, database_error, optional_uuid};

pub(super) struct Primary {
    pub id: i64,
    pub verified_home: Option<Vec<u8>>,
    pub file: Option<Uuid>,
    coverage: PriorCoverage,
    bytes: i64,
    hash: Vec<u8>,
}

impl Primary {
    pub(super) fn read(
        tx: &Transaction<'_>,
        write: &PreparedTrajectoryWrite<'_, '_>,
    ) -> Result<Option<Self>> {
        let mut statement = tx.prepare_cached(
            "SELECT doc.id, origin.source_home, doc.source_file_id, doc.source_contents_id,
                    doc.source_complete OR doc.provider_limited, doc.indexed_byte_count, doc.content_hash
             FROM session_trajectories doc LEFT JOIN native_search_document_homes origin
               ON doc.id=origin.trajectory_id AND doc.source_file_id=origin.source_file AND origin.verification=1
             WHERE doc.provider=?1 AND doc.session_id=?2 AND doc.origin='native'",
        ).map_err(database_error)?;
        statement
            .query_row(
                params![write.session.provider.to_string(), write.session.id],
                |row| {
                    Ok(Self {
                        id: row.get(0)?,
                        verified_home: row.get(1)?,
                        file: optional_uuid(row, 2)?,
                        coverage: PriorCoverage::from_row(row, 3)?,
                        bytes: row.get(5)?,
                        hash: row.get(6)?,
                    })
                },
            )
            .optional()
            .map_err(database_error)
    }

    pub(super) fn text<'a>(
        &self,
        tx: &Transaction<'_>,
        rewritten: &'a [(i64, &str)],
    ) -> Result<Option<Cow<'a, str>>> {
        if let Some((_, value)) = rewritten.iter().find(|(id, _)| *id == self.id) {
            return Ok(Some(Cow::Borrowed(*value)));
        }
        match super::text::archived_text(tx, self.id, self.bytes, &self.hash) {
            Ok(text) => Ok(Some(Cow::Owned(text))),
            Err(StoreError::CorruptStore | StoreError::TrajectoryBatchTooLarge) => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub(super) fn accepts_partial(
        &self,
        tx: &Transaction<'_>,
        write: &PreparedTrajectoryWrite<'_, '_>,
        rewritten: &[(i64, &str)],
        has_gap: bool,
    ) -> Result<bool> {
        self.coverage.accepts_partial(write, has_gap, || {
            if self.hash == write.content_hash {
                return Ok(true);
            }
            Ok(self
                .text(tx, rewritten)?
                .is_some_and(|text| retains_text(&text, write.document.redacted_text)))
        })
    }
}

pub(super) fn scoped_accepts_partial(
    tx: &Transaction<'_>,
    write: &PreparedTrajectoryWrite<'_, '_>,
    home: &[u8],
    file: Uuid,
    has_gap: bool,
) -> Result<bool> {
    let mut statement = tx.prepare_cached(
        "SELECT source_contents, source_complete OR provider_limited, stored_text FROM scoped_native_documents
         WHERE provider=?1 AND session_id=?2 AND source_home=?3 AND source_file=?4",
    ).map_err(database_error)?;
    let accepts = statement
        .query_row(
            params![
                write.session.provider.to_string(),
                write.session.id,
                home,
                file.as_bytes()
            ],
            |row| {
                PriorCoverage::from_row(row, 0)?.accepts_partial(write, has_gap, || {
                    Ok(retains_text(
                        row.get_ref(2)?.as_str()?,
                        write.document.redacted_text,
                    ))
                })
            },
        )
        .optional()
        .map_err(database_error)?;
    Ok(accepts.unwrap_or(true))
}

struct PriorCoverage {
    contents: Option<Uuid>,
    full_or_limited: bool,
}

impl PriorCoverage {
    fn from_row(row: &rusqlite::Row<'_>, column: usize) -> rusqlite::Result<Self> {
        Ok(Self {
            contents: optional_uuid(row, column)?,
            full_or_limited: row.get(column + 1)?,
        })
    }

    fn accepts_partial<E>(
        &self,
        write: &PreparedTrajectoryWrite<'_, '_>,
        has_gap: bool,
        retains_text: impl FnOnce() -> std::result::Result<bool, E>,
    ) -> std::result::Result<bool, E> {
        if !has_gap && self.contents != write.document.source_revision.contents {
            return Ok(true);
        }
        if self.full_or_limited {
            return Ok(false);
        }
        retains_text()
    }
}

fn retains_text(previous: &str, incoming: &str) -> bool {
    if incoming.starts_with(previous) {
        return true;
    }
    if incoming.len() < previous.len() {
        return false;
    }
    // Search separates visible events with blank lines. A restore can fill any gap between them.
    let mut restored = incoming.split("\n\n");
    previous
        .split("\n\n")
        .all(|old| restored.any(|current| current == old))
}

#[cfg(test)]
mod tests {
    use super::retains_text;

    #[test]
    fn a_restore_can_add_paragraphs_without_dropping_or_reordering_old_ones() {
        let previous = "first\n\nrepeat\n\nrepeat\n\nlast";
        assert!(retains_text(
            previous,
            "new\n\nfirst\n\nrepeat\n\nadded\n\nrepeat\n\nlast"
        ));
        assert!(!retains_text(
            previous,
            "first\n\nrepeat\n\ninserted longer paragraph\n\nlast"
        ));
        assert!(!retains_text(
            previous,
            "first\n\nlast\n\nrepeat\n\nrepeat\n\nmore"
        ));
    }
}
