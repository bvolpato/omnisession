pub(super) mod chunks;
mod coverage;
pub(super) mod database_receipts;
mod text;

use std::borrow::Cow;

use super::{
    CatalogHomeObservation, OptionalExtension, PreparedTrajectoryWrite, Result,
    SessionTrajectoryOrigin, StoreError, TrajectorySourceGuard, TrajectoryWriteContext,
    Transaction, database_error, params,
};

pub(super) fn initialize(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS scoped_native_documents (
             id INTEGER PRIMARY KEY,
             provider TEXT NOT NULL,
             session_id TEXT NOT NULL,
             source_home BLOB NOT NULL,
             source_file BLOB NOT NULL,
             source_contents BLOB NOT NULL,
             indexed_byte_count INTEGER NOT NULL,
             source_byte_count INTEGER NOT NULL,
             truncation_strategy TEXT NOT NULL,
             stored_text TEXT NOT NULL,
             legacy_search_text TEXT,
             derived_title TEXT,
             source_updated_at INTEGER NOT NULL DEFAULT 0,
             document_version INTEGER,
             source_complete INTEGER NOT NULL DEFAULT 0,
             complete INTEGER NOT NULL DEFAULT 0,
             provider_limited INTEGER NOT NULL DEFAULT 0,
             UNIQUE(provider, session_id, source_home, source_file),
             FOREIGN KEY(provider, session_id) REFERENCES session_trajectories(provider, session_id) ON DELETE CASCADE
         );",
    )
    .map_err(database_error)?;
    let has_title = tx
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('scoped_native_documents') WHERE name='derived_title')",
            [],
            |row| row.get::<_, bool>(0),
        )
        .map_err(database_error)?;
    if !has_title {
        tx.execute_batch("ALTER TABLE scoped_native_documents ADD COLUMN derived_title TEXT;
             ALTER TABLE scoped_native_documents ADD COLUMN source_updated_at INTEGER NOT NULL DEFAULT 0")
            .map_err(database_error)?;
    }
    let has_document_version = tx
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('scoped_native_documents') WHERE name='document_version')",
            [],
            |row| row.get::<_, bool>(0),
        )
        .map_err(database_error)?;
    if !has_document_version {
        // Stores without Desktop receipts predate the UTF-8 fix. Zero marks that legacy generation.
        tx.execute_batch(
            "ALTER TABLE scoped_native_documents ADD COLUMN document_version INTEGER;
             UPDATE scoped_native_documents SET document_version=0
             WHERE NOT EXISTS (SELECT 1 FROM sqlite_schema
                               WHERE type='table' AND name='desktop_search_receipts');",
        )
        .map_err(database_error)?;
    }
    let has_legacy_text = tx
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('scoped_native_documents') WHERE name='legacy_search_text')",
            [],
            |row| row.get::<_, bool>(0),
        )
        .map_err(database_error)?;
    if !has_legacy_text {
        tx.execute_batch("ALTER TABLE scoped_native_documents ADD COLUMN legacy_search_text TEXT")
            .map_err(database_error)?;
    }
    tx.execute_batch(
        "CREATE TRIGGER IF NOT EXISTS clear_replaced_scoped_legacy_text
         AFTER UPDATE OF source_file, source_home, source_complete, source_contents, stored_text ON scoped_native_documents
         WHEN new.legacy_search_text IS NOT NULL AND
              (new.source_file IS NOT old.source_file OR new.source_home IS NOT old.source_home
               OR (new.source_complete AND (NOT old.source_complete OR new.stored_text IS NOT old.stored_text
                                            OR new.source_contents IS NOT old.source_contents)))
         BEGIN
             UPDATE scoped_native_documents SET legacy_search_text=NULL WHERE id=new.id;
         END;
         CREATE TRIGGER IF NOT EXISTS invalidate_scoped_document_version
         AFTER UPDATE OF stored_text, source_contents ON scoped_native_documents
         WHEN new.document_version IS NOT NULL AND
              (new.stored_text IS NOT old.stored_text OR new.source_contents IS NOT old.source_contents)
         BEGIN
             UPDATE scoped_native_documents SET document_version=NULL WHERE id=new.id;
         END;",
    )
    .map_err(database_error)?;
    database_receipts::initialize(tx)?;
    super::physical_database_bodies::initialize(tx)?;
    chunks::initialize(tx)
}

pub(super) enum WriteResult {
    Primary {
        verified_partial: bool,
        retain_text: Option<String>,
    },
    Handled(usize),
}

impl WriteResult {
    fn primary() -> Self {
        Self::Primary {
            verified_partial: false,
            retain_text: None,
        }
    }
}

/// Keeps each verified home’s search history when two homes publish the same session ID.
pub(super) fn write(
    tx: &Transaction<'_>,
    write: &PreparedTrajectoryWrite<'_, '_>,
    context: TrajectoryWriteContext<'_>,
    rewritten: &[(i64, &str)],
) -> Result<WriteResult> {
    let document = &write.document;
    let TrajectorySourceGuard::ObservedDocument(observed) = write.guard else {
        return Ok(WriteResult::primary());
    };
    if !TrajectorySourceGuard::uses_catalog_home(write.session.provider)
        || document.origin == SessionTrajectoryOrigin::ImportedBundle
        || observed.document.is_none()
    {
        return Ok(WriteResult::primary());
    }
    let provider = write.session.provider.to_string();
    let id = &write.session.id;
    let Some(home) = selected_home(tx, context, &provider, id, observed.catalog)? else {
        return Ok(WriteResult::primary());
    };
    let (Some(file), Some(contents)) = (
        document.source_revision.file,
        document.source_revision.contents,
    ) else {
        return Ok(WriteResult::primary());
    };
    let Some(primary) = coverage::Primary::read(tx, write)? else {
        return Ok(WriteResult::primary());
    };
    let same_owner = primary.verified_home.as_deref() == Some(home.as_ref());
    let same_source = same_owner && primary.file == Some(file);
    let legacy_file = primary.verified_home.is_none() && primary.file == Some(file);
    let has_gap = observed.incomplete_native_history;
    let mut retain_archived_text = false;
    let mut retain_text = None;
    if !write.source_complete {
        let on_primary = same_source || legacy_file;
        let decision = if on_primary {
            primary.partial_write(tx, write, rewritten, has_gap)?
        } else {
            coverage::scoped_partial_write(tx, write, home.as_ref(), file, has_gap)?
        };
        match decision {
            coverage::PartialWrite::Keep => {
                publish_read(tx, write, context, &home, false)?;
                return Ok(WriteResult::Handled(0));
            }
            coverage::PartialWrite::KeepBoth if on_primary => {
                retain_text = Some(
                    primary
                        .text(tx, rewritten)?
                        .ok_or(StoreError::CorruptStore)?
                        .into_owned(),
                );
            }
            coverage::PartialWrite::KeepBoth => retain_archived_text = true,
            coverage::PartialWrite::Replace => {}
        }
    }
    if !retain_archived_text
        && (same_source || legacy_file || (primary.verified_home.is_none() && !has_gap))
    {
        return Ok(WriteResult::Primary {
            retain_text,
            verified_partial: same_source
                && has_gap
                && (write.document.database_step_proof.is_some()
                    || write.session.provider == crate::Provider::AntigravityDesktop
                    || write.document.legacy_utf8_text.is_some()),
        });
    }
    if !retain_archived_text
        && (!has_gap || write.source_complete)
        && let Some(text) = primary.text(tx, rewritten)?
    {
        archive_primary(tx, primary.id, &text)?;
        return Ok(WriteResult::primary());
    }
    let changed = save_scoped(tx, write, &home, file, contents, retain_archived_text)?;
    publish_read(tx, write, context, &home, true)?;
    Ok(WriteResult::Handled(changed))
}

fn save_scoped(
    tx: &Transaction<'_>,
    write: &PreparedTrajectoryWrite<'_, '_>,
    home: &[u8],
    file: uuid::Uuid,
    contents: uuid::Uuid,
    preserve_previous: bool,
) -> Result<usize> {
    let provider = write.session.provider.to_string();
    let id = &write.session.id;
    let document = &write.document;
    let changed = tx.prepare_cached(
        "INSERT INTO scoped_native_documents(provider,session_id,source_home,source_file,source_contents,
             indexed_byte_count,source_byte_count,truncation_strategy,stored_text,source_complete,complete,provider_limited,derived_title,source_updated_at,document_version)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)
         ON CONFLICT(provider,session_id,source_home,source_file) DO UPDATE SET
             source_contents=excluded.source_contents,
             indexed_byte_count=excluded.indexed_byte_count,source_byte_count=excluded.source_byte_count,
             truncation_strategy=excluded.truncation_strategy,stored_text=excluded.stored_text,
             source_complete=excluded.source_complete,complete=excluded.complete,provider_limited=excluded.provider_limited,
             derived_title=excluded.derived_title,source_updated_at=excluded.source_updated_at,
             document_version=excluded.document_version,
             legacy_search_text=CASE WHEN excluded.source_complete THEN NULL
                                     WHEN ?16 THEN coalesce(legacy_search_text,stored_text)
                                     ELSE legacy_search_text END
         WHERE (excluded.source_complete AND legacy_search_text IS NOT NULL)
            OR (?16 AND NOT excluded.source_complete AND legacy_search_text IS NULL)
            OR (source_contents,stored_text,source_complete,complete,provider_limited,derived_title,source_updated_at,
                indexed_byte_count,source_byte_count,truncation_strategy,document_version) IS NOT
               (excluded.source_contents,excluded.stored_text,excluded.source_complete,excluded.complete,excluded.provider_limited,excluded.derived_title,excluded.source_updated_at,
                excluded.indexed_byte_count,excluded.source_byte_count,excluded.truncation_strategy,excluded.document_version)
         RETURNING id, document_version",
    ).and_then(|mut statement| {
        statement.query_row(params![
            provider, id, home, file.as_bytes(), contents.as_bytes(),
            write.indexed_byte_count, write.source_byte_count, document.truncation_strategy,
            document.redacted_text,write.source_complete,write.complete,document.provider_limited,document.derived_title,document.source_updated_at.timestamp_millis(),document.document_version,preserve_previous,
        ], |row| Ok((row.get(0)?, row.get(1)?))).optional()
    }).map_err(database_error)?;
    if TrajectorySourceGuard::uses_catalog_home(write.session.provider) {
        database_receipts::save(
            tx,
            write.session,
            home,
            file,
            contents,
            &write.content_hash,
            document.database_step_proof,
        )?;
    }
    confirm_document_version(tx, changed)
}

fn publish_read(
    tx: &Transaction<'_>,
    write: &PreparedTrajectoryWrite<'_, '_>,
    context: TrajectoryWriteContext<'_>,
    home: &[u8],
    accepted: bool,
) -> Result<()> {
    let TrajectorySourceGuard::ObservedDocument(observed) = write.guard else {
        return Err(StoreError::TrajectorySourceChanged);
    };
    let document = &write.document;
    let (Some(file), Some(contents)) = (
        document.source_revision.file,
        document.source_revision.contents,
    ) else {
        return Err(StoreError::TrajectorySourceChanged);
    };
    let provider = write.session.provider.to_string();
    let id = &write.session.id;
    if observed.failure.is_some()
        && super::native_failure_receipt(tx, write.session)? == observed.failure
    {
        tx.execute(
            "DELETE FROM trajectory_index_failures WHERE provider=?1 AND session_id=?2
             AND source_file_id=?3 AND source_contents_id=?4 AND NOT native_summary AND document_version<=?5",
            params![provider,id,file.as_bytes(),contents.as_bytes(),document.document_version],
        ).map_err(database_error)?;
    }
    if matches!(context, TrajectoryWriteContext::Discovered(_))
        && let Some(steps) = observed.desktop_steps
    {
        super::desktop_declarations::record(
            tx,
            write.session,
            home,
            document.source_revision,
            steps,
            observed.desktop_generation,
            None,
        )?;
    }
    context.publish_locator(tx, write, true)?;
    super::physical_database_bodies::record(tx, write, home, accepted)
}

fn archive_primary(tx: &Transaction<'_>, trajectory_id: i64, text: &str) -> Result<()> {
    let changed = tx.prepare_cached(
        "INSERT INTO scoped_native_documents(provider,session_id,source_home,source_file,source_contents,
             indexed_byte_count,source_byte_count,truncation_strategy,stored_text,source_complete,complete,provider_limited,derived_title,source_updated_at,document_version,legacy_search_text)
         SELECT doc.provider,doc.session_id,origin.source_home,origin.source_file,doc.source_contents_id,
             doc.indexed_byte_count,doc.source_byte_count,doc.truncation_strategy,?2,doc.source_complete,doc.complete,doc.provider_limited,doc.derived_title,doc.source_updated_at,doc.document_version,
             CASE WHEN NOT doc.source_complete AND origin.verification=1 AND origin.source_file=doc.source_file_id
                  THEN doc.legacy_search_text END
         FROM session_trajectories doc JOIN native_search_document_homes origin ON origin.trajectory_id=doc.id
         WHERE doc.id=?1 AND doc.source_contents_id IS NOT NULL
         ON CONFLICT(provider,session_id,source_home,source_file) DO UPDATE SET
             source_contents=excluded.source_contents,indexed_byte_count=excluded.indexed_byte_count,
             source_byte_count=excluded.source_byte_count,truncation_strategy=excluded.truncation_strategy,
             stored_text=excluded.stored_text,source_complete=excluded.source_complete,complete=excluded.complete,
             provider_limited=excluded.provider_limited,derived_title=excluded.derived_title,source_updated_at=excluded.source_updated_at,
             document_version=excluded.document_version,
             legacy_search_text=CASE WHEN excluded.source_complete THEN NULL
                                     ELSE coalesce(excluded.legacy_search_text,scoped_native_documents.legacy_search_text) END
         RETURNING id, document_version",
    )
    .and_then(|mut statement| {
        statement
            .query_row(params![trajectory_id, text], |row| Ok((row.get(0)?, row.get(1)?)))
            .optional()
    })
    .map_err(database_error)?;
    confirm_document_version(tx, changed)?;
    Ok(())
}

fn confirm_document_version(tx: &Transaction<'_>, changed: Option<(i64, u32)>) -> Result<usize> {
    let Some((id, version)) = changed else {
        return Ok(0);
    };
    // RETURNING gives the version that the writer supplied before the compatibility trigger ran.
    tx.execute(
        "UPDATE scoped_native_documents SET document_version=?2 WHERE id=?1 AND document_version IS NULL",
        params![id, version],
    )
    .map_err(database_error)?;
    Ok(1)
}

fn selected_home<'a>(
    tx: &Transaction<'_>,
    context: TrajectoryWriteContext<'a>,
    provider: &str,
    id: &str,
    catalog: CatalogHomeObservation,
) -> Result<Option<Cow<'a, [u8]>>> {
    let home = match context {
        TrajectoryWriteContext::Locator {
            home: Some(home), ..
        } => Cow::Borrowed(home.as_os_str().as_encoded_bytes()),
        TrajectoryWriteContext::Discovered(_)
            if matches!(catalog, CatalogHomeObservation::Verified { .. }) =>
        {
            let home = tx
                .query_row(
                    "SELECT home FROM native_catalog_homes
                     WHERE provider=?1 AND session_id=?2 AND verified",
                    params![provider, id],
                    |row| row.get(0),
                )
                .optional()
                .map_err(database_error)?
                .ok_or(StoreError::TrajectorySourceChanged)?;
            Cow::Owned(home)
        }
        _ => return Ok(None),
    };
    Ok(Some(home))
}
