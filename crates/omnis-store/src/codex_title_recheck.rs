use super::{
    LegacyCodexOpenerCandidate, LocalOpenerEvidence, PreparedTrajectoryWrite, Result,
    SelectedMetadata, SessionTrajectoryOrigin, StoreError, TrajectorySourceGuard, database_error,
};
use omnis_ir::{Provider, SessionRef, SourceRevision};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use std::collections::HashMap;
use uuid::Uuid;

// The catalog can advance to an appended revision while the old document still awaits its read.
const MATCHING_PROOF: &str = "
    FROM legacy_codex_title_proofs p
    JOIN session_trajectories t ON t.id=p.trajectory_id
    JOIN scoped_session_catalog c ON c.provider=t.provider AND c.session_id=t.session_id AND c.name_observable
    LEFT JOIN legacy_codex_name_retries r ON r.trajectory_id=p.trajectory_id
    WHERE t.provider='codex' AND t.origin='native' AND NOT t.protected_by_bundle
      AND (r.trajectory_id IS NULL OR r.codex_home=(SELECT home FROM current_codex_home))
      AND t.source_file_id=p.source_file_id AND t.source_contents_id=p.source_contents_id
      AND t.missing_ancestor_id IS NULL AND t.truncation_strategy!='legacy_unknown'
      AND ((t.derived_title_is_verified AND t.derived_title_is_local_opener IS 0 AND t.derived_title=p.previous_title)
           OR (r.trajectory_id IS NOT NULL AND t.derived_title IS NULL))
      AND c.title IS NULL AND c.source_file_id=p.source_file_id
      AND c.source_contents_id IS NOT NULL";

pub(super) fn schedule_legacy_codex_title_proofs(transaction: &Transaction<'_>) -> Result<()> {
    transaction
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS legacy_codex_title_proofs(
            trajectory_id INTEGER PRIMARY KEY REFERENCES session_trajectories(id) ON DELETE CASCADE,
            source_file_id BLOB NOT NULL CHECK(length(source_file_id)=16),
            source_contents_id BLOB NOT NULL CHECK(length(source_contents_id)=16),
            previous_title TEXT NOT NULL);",
        )
        .map_err(database_error)?;
    let has_origin: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('session_trajectories') WHERE name='derived_title_is_local_opener')",
        [], |row| row.get(0),
    ).map_err(database_error)?;
    if has_origin {
        transaction.execute_batch(
            "INSERT OR IGNORE INTO legacy_codex_title_proofs(trajectory_id,source_file_id,source_contents_id,previous_title)
             SELECT t.id,t.source_file_id,t.source_contents_id,t.derived_title
             FROM session_trajectories t JOIN session_index c ON c.provider=t.provider AND c.session_id=t.session_id
             WHERE t.provider='codex' AND t.origin='native' AND NOT t.protected_by_bundle AND t.document_version=6
               AND t.source_file_id=c.source_file_id AND t.source_contents_id=c.source_contents_id AND c.title IS NULL
               AND t.missing_ancestor_id IS NULL AND t.truncation_strategy!='legacy_unknown' AND t.derived_title_is_verified AND t.derived_title_is_local_opener IS 0
               AND length(t.derived_title)>0;"
        ).map_err(database_error)?;
    }
    for (name, operation) in [
        ("insert", "INSERT"),
        (
            "update",
            "UPDATE OF title,source_file_id,source_contents_id",
        ),
    ] {
        transaction.execute_batch(&format!(
            "CREATE TRIGGER IF NOT EXISTS legacy_codex_title_catalog_{name} AFTER {operation} ON session_index
             WHEN NEW.provider='codex' AND EXISTS(SELECT 1 FROM legacy_codex_title_proofs)
             BEGIN DELETE FROM legacy_codex_title_proofs WHERE trajectory_id=(
                 SELECT id FROM session_trajectories WHERE provider=NEW.provider AND session_id=NEW.session_id)
               AND (NEW.title IS NOT NULL OR NEW.source_file_id IS NOT source_file_id OR NEW.source_contents_id IS NULL);
             END;"
        )).map_err(database_error)?;
    }
    Ok(())
}

/// The old index could stop following the opening message at a tool catalog update.
pub(super) fn schedule_tool_catalog_title_rechecks(transaction: &Transaction<'_>) -> Result<()> {
    let seen: bool = transaction.query_row(
        "SELECT EXISTS (SELECT 1 FROM pragma_table_info('legacy_codex_title_proofs') WHERE name='requires_tool_catalog')",
        [], |row| row.get(0),
    ).map_err(database_error)?;
    if seen {
        return Ok(());
    }
    transaction.execute_batch(
        "ALTER TABLE legacy_codex_title_proofs ADD COLUMN requires_tool_catalog INTEGER NOT NULL DEFAULT 0
             CHECK(requires_tool_catalog IN (0,1));
         CREATE TEMP TABLE tool_catalog_title_candidates(id INTEGER PRIMARY KEY);
         INSERT INTO tool_catalog_title_candidates
             SELECT t.id FROM session_trajectories t LEFT JOIN session_index c
               ON c.provider=t.provider AND c.session_id=t.session_id
             WHERE t.provider='codex' AND t.origin='native' AND NOT t.protected_by_bundle AND t.document_version=6
               AND t.source_file_id IS NOT NULL AND t.source_contents_id IS NOT NULL
               AND t.missing_ancestor_id IS NULL AND t.truncation_strategy!='legacy_unknown'
               AND t.derived_title_is_verified AND length(t.derived_title)>0
               AND ((t.derived_title_is_local_opener=1 AND (c.title IS NULL OR c.source_file_id IS NOT t.source_file_id))
                    OR (t.derived_title_is_local_opener=0 AND c.source_file_id=t.source_file_id
                        AND c.source_contents_id=t.source_contents_id AND c.title IS NULL));
         INSERT OR IGNORE INTO legacy_codex_title_proofs(trajectory_id,source_file_id,source_contents_id,previous_title,requires_tool_catalog)
             SELECT t.id,t.source_file_id,t.source_contents_id,t.derived_title,1
             FROM tool_catalog_title_candidates n JOIN session_trajectories t ON n.id=t.id
             WHERE t.derived_title_is_local_opener=0;
         DELETE FROM tool_catalog_title_candidates WHERE id IN (
             SELECT t.id FROM tool_catalog_title_candidates n JOIN session_trajectories t ON n.id=t.id
             WHERE t.source_complete AND t.truncation_strategy NOT GLOB 'source_incomplete*');
         UPDATE trajectory_index_failures AS f SET document_version=5 WHERE f.provider='codex' AND f.document_version=6
           AND EXISTS (SELECT 1 FROM session_trajectories t JOIN tool_catalog_title_candidates n ON t.id=n.id
               WHERE t.provider=f.provider AND t.session_id=f.session_id AND t.source_file_id=f.source_file_id
                 AND t.source_contents_id=f.source_contents_id AND t.source_updated_at IS f.source_updated_at);
         UPDATE session_trajectories SET document_version=5 WHERE id IN (SELECT id FROM tool_catalog_title_candidates);
         DROP TABLE tool_catalog_title_candidates;"
    ).map_err(database_error)
}

pub(super) fn name_retries(connection: &Connection) -> Result<HashMap<SessionRef, SourceRevision>> {
    let sql = format!(
        "SELECT doc.session_id,doc.source_file_id,doc.source_contents_id FROM legacy_codex_name_retries pending
         CROSS JOIN session_trajectories doc WHERE doc.id=pending.trajectory_id AND EXISTS (
             SELECT 1 {MATCHING_PROOF} AND p.trajectory_id=pending.trajectory_id AND c.source_contents_id=p.source_contents_id)"
    );
    let mut statement = connection.prepare_cached(&sql).map_err(database_error)?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                SessionRef::new(Provider::Codex, row.get::<_, String>(0)?),
                SourceRevision {
                    file: Some(Uuid::from_bytes(row.get(1)?)),
                    contents: Some(Uuid::from_bytes(row.get(2)?)),
                    missing_ancestor: None,
                },
            ))
        })
        .map_err(database_error)?;
    rows.collect::<rusqlite::Result<_>>()
        .map_err(database_error)
}

pub(super) fn candidate(
    connection: &Connection,
    session: &SessionRef,
    file: Uuid,
) -> Result<Option<LegacyCodexOpenerCandidate>> {
    if session.provider != Provider::Codex {
        return Ok(None);
    }
    connection
        .query_row(
            &format!("SELECT p.previous_title,p.requires_tool_catalog {MATCHING_PROOF} AND t.session_id=?1 AND t.source_file_id=?2"),
            params![session.id, file.into_bytes()],
            |row| {
                Ok(LegacyCodexOpenerCandidate {
                    title: row.get(0)?,
                    requires_tool_catalog: row.get(1)?,
                })
            },
        )
        .optional()
        .map_err(database_error)
}

pub(super) fn accept(
    transaction: &Transaction<'_>,
    write: &PreparedTrajectoryWrite<'_, '_>,
    selected: Option<SelectedMetadata<'_>>,
) -> Result<()> {
    let document = &write.document;
    let deferred = document.local_opener == LocalOpenerEvidence::DeferredLegacyName;
    if !deferred && !document.local_opener.rechecks_legacy_title() {
        return Ok(());
    }
    let TrajectorySourceGuard::ObservedRevision(Some(observed)) = write.guard else {
        return Err(StoreError::TrajectorySourceChanged);
    };
    let incoming = document.source_revision;
    let old = observed.source_revision;
    let (Some(file), Some(contents), Some(old_contents), Some(prior)) = (
        incoming.file,
        incoming.contents,
        old.contents,
        document.withdrawn_title,
    ) else {
        return Err(StoreError::TrajectorySourceChanged);
    };
    if write.session.provider != Provider::Codex
        || document.origin != SessionTrajectoryOrigin::Native
        || old.file != Some(file)
        || old.missing_ancestor.is_some()
        || incoming.missing_ancestor.is_some()
        || selected.is_some_and(|metadata| metadata.selected.title.is_some())
    {
        return Err(StoreError::TrajectorySourceChanged);
    }
    let sql = format!("SELECT p.trajectory_id {MATCHING_PROOF} AND t.session_id=?1 AND t.source_file_id=?2
                       AND p.source_contents_id=?3 AND p.previous_title=?4 AND t.source_updated_at=?5
                       AND t.document_version<=?6 AND (c.source_contents_id=p.source_contents_id OR c.source_contents_id=?7)");
    let id = transaction
        .query_row(
            &sql,
            params![
                write.session.id,
                file.into_bytes(),
                old_contents.into_bytes(),
                prior,
                observed.source_updated_at.timestamp_millis(),
                document.document_version,
                contents.into_bytes(),
            ],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(database_error)?
        .ok_or(StoreError::TrajectorySourceChanged)?;
    if deferred {
        transaction
            .execute(
                "INSERT INTO legacy_codex_name_retries SELECT ?1,home FROM current_codex_home
             WHERE home IS NOT NULL ON CONFLICT(trajectory_id) DO NOTHING",
                [id],
            )
            .map_err(database_error)?;
        transaction.execute(
            "UPDATE legacy_codex_title_proofs SET source_contents_id=?2 WHERE trajectory_id=?1 AND source_contents_id<>?2",
            params![id, contents.into_bytes()],
        ).map_err(database_error)?;
    } else {
        transaction
            .execute(
                "UPDATE session_trajectories SET derived_title_is_local_opener=NULL WHERE id=?1",
                [id],
            )
            .map_err(database_error)?;
    }
    Ok(())
}

pub(super) fn retire(
    transaction: &Transaction<'_>,
    write: &PreparedTrajectoryWrite<'_, '_>,
) -> Result<()> {
    let document = &write.document;
    if write.session.provider != Provider::Codex {
        return Ok(());
    }
    let named = document.local_opener == LocalOpenerEvidence::Unknown
        && document.title_is_verified
        && document.derived_title.is_some()
        && document.origin == SessionTrajectoryOrigin::Native;
    let revision = document.source_revision;
    transaction.prepare_cached(
        "DELETE FROM legacy_codex_title_proofs AS p WHERE trajectory_id=(
             SELECT id FROM session_trajectories WHERE provider='codex' AND session_id=?1)
         AND (?2 OR NOT EXISTS (SELECT 1 FROM session_trajectories t WHERE t.id=p.trajectory_id
              AND t.origin='native' AND NOT t.protected_by_bundle AND t.missing_ancestor_id IS NULL
              AND ((t.derived_title=p.previous_title AND t.derived_title_is_local_opener IS 0)
                   OR (t.derived_title IS NULL AND EXISTS (
                       SELECT 1 FROM legacy_codex_name_retries r WHERE r.trajectory_id=p.trajectory_id)))
              AND t.source_file_id=p.source_file_id AND t.source_contents_id=p.source_contents_id)
              OR (?3 AND p.source_file_id=?4 AND p.source_contents_id=?5))"
    ).map_err(database_error)?.execute(
        params![
            write.session.id,
            document.local_opener.rechecks_legacy_title(),
            named,
            revision.file.map(Uuid::into_bytes),
            revision.contents.map(Uuid::into_bytes),
        ],
    ).map_err(database_error)?;
    Ok(())
}
