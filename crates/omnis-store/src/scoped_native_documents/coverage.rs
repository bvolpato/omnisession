use std::borrow::Cow;

use rusqlite::{OptionalExtension, Transaction, params};
use sha2::{Digest, Sha256};
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
                    doc.source_complete OR doc.provider_limited, doc.truncation_strategy, doc.indexed_byte_count, doc.content_hash
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
                        bytes: row.get(6)?,
                        hash: row.get(7)?,
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
        self.coverage.accepts_partial(
            tx,
            write,
            has_gap,
            self.verified_home.as_deref(),
            &self.hash,
            || self.text(tx, rewritten),
        )
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
        "SELECT source_contents, source_complete OR provider_limited, truncation_strategy, stored_text
         FROM scoped_native_documents
         WHERE provider=?1 AND session_id=?2 AND source_home=?3 AND source_file=?4",
    ).map_err(database_error)?;
    let mut rows = statement
        .query(params![
            write.session.provider.to_string(),
            write.session.id,
            home,
            file.as_bytes()
        ])
        .map_err(database_error)?;
    let Some(row) = rows.next().map_err(database_error)? else {
        return Ok(true);
    };
    let prior = PriorCoverage::from_row(row, 0).map_err(database_error)?;
    let text = row
        .get_ref(3)
        .map_err(database_error)?
        .as_str()
        .map_err(|_| StoreError::CorruptStore)?;
    prior.accepts_partial(
        tx,
        write,
        has_gap,
        Some(home),
        &Sha256::digest(text),
        || Ok(Some(Cow::Borrowed(text))),
    )
}

struct PriorCoverage {
    contents: Option<Uuid>,
    full_or_limited: bool,
    document_truncated: bool,
}

impl PriorCoverage {
    fn from_row(row: &rusqlite::Row<'_>, column: usize) -> rusqlite::Result<Self> {
        Ok(Self {
            contents: optional_uuid(row, column)?,
            full_or_limited: row.get(column + 1)?,
            document_truncated: row
                .get_ref(column + 2)?
                .as_str()?
                .contains("document_head_tail"),
        })
    }

    fn accepts_partial<'a>(
        &self,
        tx: &Transaction<'_>,
        write: &PreparedTrajectoryWrite<'_, '_>,
        has_gap: bool,
        home: Option<&[u8]>,
        hash: &[u8],
        prior_text: impl FnOnce() -> Result<Option<Cow<'a, str>>>,
    ) -> Result<bool> {
        if !has_gap && self.contents != write.document.source_revision.contents {
            return Ok(true);
        }
        if write.session.provider != crate::Provider::AntigravityDesktop {
            if self.full_or_limited {
                return Ok(false);
            }
            return Ok(hash == write.content_hash
                || prior_text()?.is_some_and(|previous| {
                    retains_legacy_preview(&previous, write.document.redacted_text)
                }));
        }
        if has_gap
            && let (Some(home), Some(contents), Some(file), Some(incoming)) = (
                home,
                self.contents,
                write.document.source_revision.file,
                write.document.desktop_step_proof,
            )
            && let Some(previous) =
                super::desktop_receipts::read(tx, &write.session.id, home, file, contents, hash)?
        {
            if !retains_steps(&previous, incoming, self.full_or_limited) {
                return Ok(false);
            }
            // A whole-document limit can displace indexed text even when every old step survives.
            if !self.document_truncated
                && !write
                    .document
                    .truncation_strategy
                    .contains("document_head_tail")
            {
                return Ok(true);
            }
            if hash == write.content_hash {
                return Ok(true);
            }
            if let Some(projection) = write.document.desktop_projection {
                return Ok(prior_text()?.is_some_and(|text| {
                    projection.retains(&previous, &text, write.document.redacted_text)
                }));
            }
        } else if self.full_or_limited {
            return Ok(false);
        }
        if hash == write.content_hash {
            return Ok(true);
        }
        Ok(prior_text()?.is_some_and(|previous| {
            retains_standalone_text(&previous, write.document.redacted_text)
        }))
    }
}

fn retains_standalone_text(previous: &str, incoming: &str) -> bool {
    previous.is_empty()
        || incoming.match_indices(previous).any(|(start, matched)| {
            let end = start + matched.len();
            (start == 0
                || previous.starts_with(char::is_whitespace)
                || incoming[..start].ends_with(char::is_whitespace))
                && (end == incoming.len()
                    || previous.ends_with(char::is_whitespace)
                    || incoming[end..].starts_with(char::is_whitespace))
        })
}

fn retains_legacy_preview(previous: &str, incoming: &str) -> bool {
    if incoming.starts_with(previous) {
        return true;
    }
    if incoming.len() < previous.len() {
        return false;
    }
    let mut paragraphs = incoming.split("\n\n");
    previous
        .split("\n\n")
        .all(|old| paragraphs.any(|current| current == old))
}

fn retains_steps(previous: &[Uuid], incoming: &[Uuid], strict: bool) -> bool {
    if strict && previous.len() >= incoming.len() {
        return false;
    }
    let mut restored = incoming.iter();
    previous
        .iter()
        .all(|step| restored.any(|current| current == step))
}

#[cfg(test)]
mod tests {
    use super::{Uuid, retains_steps};

    #[test]
    fn partial_reads_preserve_order_and_need_a_new_step_to_replace_full_coverage() {
        let [a, b, c] = [1, 2, 3].map(Uuid::from_u128);
        assert!(retains_steps(&[a, c], &[a, b, c], true));
        assert!(!retains_steps(&[a, c], &[a, c], true));
        assert!(!retains_steps(&[a, c], &[c, b, a], false));
        assert!(!retains_steps(&[a, a], &[a, b, c], false));
    }
}
