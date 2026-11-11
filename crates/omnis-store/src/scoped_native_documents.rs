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
             source_complete INTEGER NOT NULL DEFAULT 0,
             complete INTEGER NOT NULL DEFAULT 0,
             provider_limited INTEGER NOT NULL DEFAULT 0,
             UNIQUE(provider, session_id, source_home, source_file),
             FOREIGN KEY(provider, session_id) REFERENCES session_trajectories(provider, session_id) ON DELETE CASCADE
         );
         CREATE VIRTUAL TABLE IF NOT EXISTS scoped_native_document_fts USING fts5(
             stored_text, content='scoped_native_documents', content_rowid='id',
             tokenize='unicode61 remove_diacritics 2'
         );
         CREATE TRIGGER IF NOT EXISTS index_scoped_native_document AFTER INSERT ON scoped_native_documents BEGIN
             INSERT INTO scoped_native_document_fts(rowid,stored_text) VALUES(new.id,new.stored_text);
         END;
         CREATE TRIGGER IF NOT EXISTS remove_scoped_native_document AFTER DELETE ON scoped_native_documents BEGIN
             INSERT INTO scoped_native_document_fts(scoped_native_document_fts,rowid,stored_text)
             VALUES('delete',old.id,old.stored_text);
         END;
         CREATE TRIGGER IF NOT EXISTS update_scoped_native_document AFTER UPDATE ON scoped_native_documents
         WHEN old.stored_text <> new.stored_text BEGIN
             INSERT INTO scoped_native_document_fts(scoped_native_document_fts,rowid,stored_text)
             VALUES('delete',old.id,old.stored_text);
             INSERT INTO scoped_native_document_fts(rowid,stored_text) VALUES(new.id,new.stored_text);
         END;",
    )
    .map_err(database_error)
}

/// Keeps each verified home’s search history when two homes publish the same session ID.
pub(super) fn write(
    tx: &Transaction<'_>,
    write: &PreparedTrajectoryWrite<'_, '_>,
    context: TrajectoryWriteContext<'_>,
    rewritten: &[(i64, &str)],
) -> Result<Option<usize>> {
    let document = &write.document;
    let TrajectorySourceGuard::ObservedDocument(observed) = write.guard else {
        return Ok(None);
    };
    if !TrajectorySourceGuard::uses_catalog_home(write.session.provider)
        || document.origin == SessionTrajectoryOrigin::ImportedBundle
        || observed.document.is_none()
    {
        return Ok(None);
    }
    let provider = write.session.provider.to_string();
    let id = &write.session.id;
    let Some(home) = selected_home(tx, context, &provider, id, observed.catalog)? else {
        return Ok(None);
    };
    let foreign = tx
        .prepare_cached(
            "SELECT doc.id,doc.indexed_byte_count,doc.content_hash FROM session_trajectories AS doc
             JOIN native_search_document_homes AS origin
               ON doc.id=origin.trajectory_id AND doc.source_file_id=origin.source_file
             WHERE doc.provider=?1 AND doc.session_id=?2 AND doc.origin='native'
               AND origin.verification=1 AND origin.source_home<>?3",
        )
        .and_then(|mut statement| {
            statement
                .query_row(params![provider, id, home.as_ref()], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get::<_, Vec<u8>>(2)?))
                })
                .optional()
        })
        .map_err(database_error)?;
    let Some((trajectory_id, bytes, hash)) = foreign else {
        return Ok(None);
    };
    if !observed.incomplete_native_history || write.source_complete {
        let text = if let Some((_, value)) = rewritten.iter().find(|(id, _)| *id == trajectory_id) {
            Some(Cow::Borrowed(*value))
        } else {
            match text::archived_text(tx, trajectory_id, bytes, &hash) {
                Ok(value) => Some(Cow::Owned(value)),
                Err(StoreError::CorruptStore | StoreError::TrajectoryBatchTooLarge) => None,
                Err(error) => return Err(error),
            }
        };
        if let Some(text) = text {
            archive_primary(tx, trajectory_id, &text)?;
            return Ok(None);
        }
        // Keep chunks that cannot be reconstructed in place. The new home still has a verified read.
    }
    let (Some(file), Some(contents)) = (
        document.source_revision.file,
        document.source_revision.contents,
    ) else {
        return Err(StoreError::TrajectorySourceChanged);
    };
    let changed = tx.prepare_cached(
        "INSERT INTO scoped_native_documents(provider,session_id,source_home,source_file,source_contents,
             indexed_byte_count,source_byte_count,truncation_strategy,stored_text,source_complete,complete,provider_limited)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)
         ON CONFLICT(provider,session_id,source_home,source_file) DO UPDATE SET
             source_contents=excluded.source_contents,
             indexed_byte_count=excluded.indexed_byte_count,source_byte_count=excluded.source_byte_count,
             truncation_strategy=excluded.truncation_strategy,stored_text=excluded.stored_text,
             source_complete=excluded.source_complete,complete=excluded.complete,provider_limited=excluded.provider_limited
         WHERE (source_contents,stored_text,source_complete,complete,provider_limited) IS NOT
               (excluded.source_contents,excluded.stored_text,excluded.source_complete,excluded.complete,excluded.provider_limited)
           AND (excluded.source_complete OR (NOT source_complete AND NOT provider_limited
               AND instr(excluded.stored_text,stored_text)=1))",
    ).and_then(|mut statement| {
        statement.execute(params![
            provider, id, home.as_ref(), file.as_bytes(), contents.as_bytes(),
            write.indexed_byte_count, write.source_byte_count, document.truncation_strategy,
            document.redacted_text,write.source_complete,write.complete,document.provider_limited,
        ])
    }).map_err(database_error)?;
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
            home.as_ref(),
            file,
            steps,
            observed.desktop_generation,
            None,
        )?;
    }
    context.publish_locator(tx, write, true)?;
    Ok(Some(changed))
}

fn archive_primary(tx: &Transaction<'_>, trajectory_id: i64, text: &str) -> Result<()> {
    tx.prepare_cached(
        "INSERT INTO scoped_native_documents(provider,session_id,source_home,source_file,source_contents,
             indexed_byte_count,source_byte_count,truncation_strategy,stored_text,source_complete,complete,provider_limited)
         SELECT doc.provider,doc.session_id,origin.source_home,origin.source_file,doc.source_contents_id,
             doc.indexed_byte_count,doc.source_byte_count,doc.truncation_strategy,?2,doc.source_complete,doc.complete,doc.provider_limited
         FROM session_trajectories doc JOIN native_search_document_homes origin ON origin.trajectory_id=doc.id
         WHERE doc.id=?1 AND doc.source_contents_id IS NOT NULL
         ON CONFLICT(provider,session_id,source_home,source_file) DO UPDATE SET
             source_contents=excluded.source_contents,indexed_byte_count=excluded.indexed_byte_count,
             source_byte_count=excluded.source_byte_count,truncation_strategy=excluded.truncation_strategy,
             stored_text=excluded.stored_text,source_complete=excluded.source_complete,complete=excluded.complete,
             provider_limited=excluded.provider_limited",
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
