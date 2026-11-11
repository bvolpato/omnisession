pub(super) mod chunks;
mod coverage;
pub(super) mod desktop_receipts;
mod text;

use std::borrow::Cow;

use super::{
    CatalogObservation, OptionalExtension, PreparedTrajectoryWrite, Result,
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
             derived_title TEXT,
             source_updated_at INTEGER NOT NULL DEFAULT 0,
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
    desktop_receipts::initialize(tx)?;
    chunks::initialize(tx)
}

pub(super) enum WriteResult {
    Primary(bool),
    Handled(usize),
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
        return Ok(WriteResult::Primary(false));
    };
    if !TrajectorySourceGuard::uses_catalog_home(write.session.provider)
        || document.origin == SessionTrajectoryOrigin::ImportedBundle
        || observed.document.is_none()
    {
        return Ok(WriteResult::Primary(false));
    }
    let provider = write.session.provider.to_string();
    let id = &write.session.id;
    let Some(home) = selected_home(tx, context, &provider, id, observed.catalog)? else {
        return Ok(WriteResult::Primary(false));
    };
    let (Some(file), Some(contents)) = (
        document.source_revision.file,
        document.source_revision.contents,
    ) else {
        return Ok(WriteResult::Primary(false));
    };
    let Some(primary) = coverage::Primary::read(tx, write)? else {
        return Ok(WriteResult::Primary(false));
    };
    let same_owner = primary.verified_home.as_deref() == Some(home.as_ref());
    let same_source = same_owner && primary.file == Some(file);
    let legacy_file = primary.verified_home.is_none() && primary.file == Some(file);
    let has_gap = observed.incomplete_native_history;
    if !write.source_complete {
        let accepts = if same_source || legacy_file {
            primary.accepts_partial(tx, write, rewritten, has_gap)?
        } else {
            coverage::scoped_accepts_partial(tx, write, home.as_ref(), file, has_gap)?
        };
        if !accepts {
            publish_read(tx, write, context, &home)?;
            return Ok(WriteResult::Handled(0));
        }
    }
    if same_source || legacy_file || (primary.verified_home.is_none() && !has_gap) {
        return Ok(WriteResult::Primary(
            same_source && has_gap && write.session.provider == crate::Provider::AntigravityDesktop,
        ));
    }
    if (!has_gap || write.source_complete)
        && let Some(text) = primary.text(tx, rewritten)?
    {
        archive_primary(tx, primary.id, &text)?;
        return Ok(WriteResult::Primary(false));
    }
    let changed = save_scoped(tx, write, &home, file, contents)?;
    publish_read(tx, write, context, &home)?;
    Ok(WriteResult::Handled(changed))
}

fn save_scoped(
    tx: &Transaction<'_>,
    write: &PreparedTrajectoryWrite<'_, '_>,
    home: &[u8],
    file: uuid::Uuid,
    contents: uuid::Uuid,
) -> Result<usize> {
    let provider = write.session.provider.to_string();
    let id = &write.session.id;
    let document = &write.document;
    let changed = tx.prepare_cached(
        "INSERT INTO scoped_native_documents(provider,session_id,source_home,source_file,source_contents,
             indexed_byte_count,source_byte_count,truncation_strategy,stored_text,source_complete,complete,provider_limited,derived_title,source_updated_at)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)
         ON CONFLICT(provider,session_id,source_home,source_file) DO UPDATE SET
             source_contents=excluded.source_contents,
             indexed_byte_count=excluded.indexed_byte_count,source_byte_count=excluded.source_byte_count,
             truncation_strategy=excluded.truncation_strategy,stored_text=excluded.stored_text,
             source_complete=excluded.source_complete,complete=excluded.complete,provider_limited=excluded.provider_limited,
             derived_title=excluded.derived_title,source_updated_at=excluded.source_updated_at
         WHERE (source_contents,stored_text,source_complete,complete,provider_limited,derived_title,source_updated_at,
                indexed_byte_count,source_byte_count,truncation_strategy) IS NOT
               (excluded.source_contents,excluded.stored_text,excluded.source_complete,excluded.complete,excluded.provider_limited,excluded.derived_title,excluded.source_updated_at,
                excluded.indexed_byte_count,excluded.source_byte_count,excluded.truncation_strategy)",
    ).and_then(|mut statement| {
        statement.execute(params![
            provider, id, home, file.as_bytes(), contents.as_bytes(),
            write.indexed_byte_count, write.source_byte_count, document.truncation_strategy,
            document.redacted_text,write.source_complete,write.complete,document.provider_limited,document.derived_title,document.source_updated_at.timestamp_millis(),
        ])
    }).map_err(database_error)?;
    if write.session.provider == crate::Provider::AntigravityDesktop {
        desktop_receipts::save(
            tx,
            id,
            home,
            file,
            contents,
            &write.content_hash,
            document.desktop_step_proof,
        )?;
    }
    Ok(changed)
}

fn publish_read(
    tx: &Transaction<'_>,
    write: &PreparedTrajectoryWrite<'_, '_>,
    context: TrajectoryWriteContext<'_>,
    home: &[u8],
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
            file,
            steps,
            observed.desktop_generation,
            None,
        )?;
    }
    context.publish_locator(tx, write, true)?;
    Ok(())
}

fn archive_primary(tx: &Transaction<'_>, trajectory_id: i64, text: &str) -> Result<()> {
    tx.prepare_cached(
        "INSERT INTO scoped_native_documents(provider,session_id,source_home,source_file,source_contents,
             indexed_byte_count,source_byte_count,truncation_strategy,stored_text,source_complete,complete,provider_limited,derived_title,source_updated_at)
         SELECT doc.provider,doc.session_id,origin.source_home,origin.source_file,doc.source_contents_id,
             doc.indexed_byte_count,doc.source_byte_count,doc.truncation_strategy,?2,doc.source_complete,doc.complete,doc.provider_limited,doc.derived_title,doc.source_updated_at
         FROM session_trajectories doc JOIN native_search_document_homes origin ON origin.trajectory_id=doc.id
         WHERE doc.id=?1 AND doc.source_contents_id IS NOT NULL
         ON CONFLICT(provider,session_id,source_home,source_file) DO UPDATE SET
             source_contents=excluded.source_contents,indexed_byte_count=excluded.indexed_byte_count,
             source_byte_count=excluded.source_byte_count,truncation_strategy=excluded.truncation_strategy,
             stored_text=excluded.stored_text,source_complete=excluded.source_complete,complete=excluded.complete,
             provider_limited=excluded.provider_limited,derived_title=excluded.derived_title,source_updated_at=excluded.source_updated_at",
    ).and_then(|mut statement| statement.execute(params![trajectory_id, text])).map_err(database_error)?;
    Ok(())
}

fn selected_home<'a>(
    tx: &Transaction<'_>,
    context: TrajectoryWriteContext<'a>,
    provider: &str,
    id: &str,
    catalog: CatalogObservation,
) -> Result<Option<Cow<'a, [u8]>>> {
    let home = match context {
        TrajectoryWriteContext::Locator {
            home: Some(home), ..
        } => Cow::Borrowed(home.as_os_str().as_encoded_bytes()),
        TrajectoryWriteContext::Discovered(_)
            if matches!(catalog, CatalogObservation::Verified { .. }) =>
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
