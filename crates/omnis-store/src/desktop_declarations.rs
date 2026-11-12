use omnis_ir::{Provider, SessionRef};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use uuid::Uuid;

use crate::{IndexedSession, MissingDesktopSummaries, Result, StoreError, database_error};
use std::path::Path;

pub(super) fn initialize(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS ide_step_declarations(
            generation INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id TEXT NOT NULL,
            source_home BLOB NOT NULL,
            source_file BLOB NOT NULL CHECK(length(source_file)=16),
            steps INTEGER NOT NULL CHECK(steps>=0),
            summary TEXT,
            UNIQUE(session_id,source_home,source_file)
         );",
    )
    .map_err(database_error)?;
    if tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='desktop_step_declarations')",
        [], |row| row.get::<_, bool>(0),
    ).map_err(database_error)? {
        return initialize_summary_column(tx);
    }
    tx.execute_batch(
        "CREATE TABLE desktop_step_declarations(
            generation INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id TEXT NOT NULL,
            source_home BLOB NOT NULL,
            source_file BLOB NOT NULL CHECK(length(source_file)=16),
            steps INTEGER NOT NULL CHECK(steps>=0),
            UNIQUE(session_id,source_home,source_file)
         );
         ALTER TABLE session_index_reader_checks ADD COLUMN desktop_generation INTEGER;
         INSERT INTO desktop_step_declarations(session_id,source_home,source_file,steps)
         SELECT catalog.session_id,native.home,catalog.source_file_id,catalog.event_count
         FROM session_index AS catalog JOIN native_catalog_homes AS native USING(provider,session_id)
         WHERE catalog.provider='antigravity-desktop' AND native.verified AND catalog.source_file_id IS NOT NULL
            AND catalog.event_count>0;
         INSERT OR IGNORE INTO desktop_step_declarations(session_id,source_home,source_file,steps)
         SELECT doc.session_id,origin.source_home,origin.source_file,origin.declared_steps
         FROM native_search_document_homes AS origin JOIN session_trajectories AS doc ON doc.id=origin.trajectory_id
         WHERE doc.provider='antigravity-desktop' AND doc.origin='native' AND origin.verification=1
            AND origin.source_file=doc.source_file_id AND origin.declared_steps IS NOT NULL;",
    ).map_err(database_error)?;
    initialize_summary_column(tx)
}

fn initialize_summary_column(tx: &Transaction<'_>) -> Result<()> {
    if tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('desktop_step_declarations') WHERE name='summary')",
        [], |row| row.get::<_,bool>(0),
    ).map_err(database_error)? {
        return Ok(());
    }
    tx.execute_batch(
        "ALTER TABLE desktop_step_declarations ADD COLUMN summary TEXT;
         UPDATE desktop_step_declarations AS declaration SET summary=json_object(
            'title',catalog.title,'project_path',catalog.project_path,'git_branch',catalog.git_branch,
            'created_at',catalog.created_at,'updated_at',catalog.updated_at,'approximate',catalog.updated_at_approximate)
         FROM session_index AS catalog JOIN native_catalog_homes AS native USING(provider,session_id)
         WHERE catalog.provider='antigravity-desktop' AND native.verified AND native.home=declaration.source_home
            AND catalog.session_id=declaration.session_id AND catalog.source_file_id=declaration.source_file;",
    ).map_err(database_error)
}

pub(super) fn table(provider: Provider) -> Option<&'static str> {
    match provider {
        Provider::AntigravityDesktop => Some("desktop_step_declarations"),
        Provider::AntigravityIde => Some("ide_step_declarations"),
        _ => None,
    }
}

pub(super) fn generation(connection: &Connection, provider: Provider) -> Result<Option<i64>> {
    let Some(table) = table(provider) else {
        return Ok(None);
    };
    connection
        .query_row(
            &format!("SELECT coalesce(max(generation),0) FROM {table}"),
            [],
            |row| row.get(0),
        )
        .map_err(database_error)
}

pub(super) fn record(
    tx: &Transaction<'_>,
    session: &SessionRef,
    home: &[u8],
    file: Uuid,
    steps: usize,
    observed_generation: Option<i64>,
    metadata: Option<&IndexedSession>,
) -> Result<()> {
    let table = table(session.provider).ok_or(StoreError::InvalidSessionReference)?;
    let summary = metadata
        .map(|row| -> Result<String> {
            let project = row
                .project_path
                .as_deref()
                .map(|path| path.to_str().ok_or(StoreError::InvalidWorkspaceRoot))
                .transpose()?;
            Ok(serde_json::json!({
                "title": row.title, "project_path": project, "git_branch": row.git_branch,
                "created_at": row.created_at.map(|time| time.timestamp_millis()),
                "updated_at": row.updated_at.map(|time| time.timestamp_millis()),
                "approximate": i64::from(row.updated_at_approximate)
            })
            .to_string())
        })
        .transpose()?;
    let steps = i64::try_from(steps).map_err(|_| StoreError::InvalidSessionReference)?;
    let previous = tx
        .prepare_cached(&format!(
            "SELECT generation,steps,summary FROM {table}
         WHERE session_id=?1 AND source_home=?2 AND source_file=?3"
        ))
        .map_err(database_error)?
        .query_row(params![session.id, home, file.as_bytes()], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })
        .optional()
        .map_err(database_error)?;
    if let Some((generation, stored, prior_summary)) = previous.as_ref()
        && observed_generation.is_some_and(|seen| seen < *generation)
    {
        return if *stored == steps && (summary.is_none() || summary == *prior_summary) {
            Ok(())
        } else {
            Err(StoreError::TrajectorySourceChanged)
        };
    }
    // A newer confirmation of the same count also supersedes an older listing in flight.
    let summary = summary.or_else(|| previous.and_then(|(_, _, summary)| summary));
    tx.prepare_cached(&format!(
        "INSERT OR REPLACE INTO {table}(session_id,source_home,source_file,steps,summary)
         VALUES(?1,?2,?3,?4,?5)"
    ))
    .and_then(|mut statement| {
        statement.execute(params![session.id, home, file.as_bytes(), steps, summary])
    })
    .map_err(database_error)?;
    Ok(())
}

pub(super) fn record_listing(
    tx: &Transaction<'_>,
    row: &IndexedSession,
    home: &Path,
    missing: &MissingDesktopSummaries,
    generation: Option<i64>,
    database_alias_count: Option<usize>,
) -> Result<()> {
    let count = match row.session.provider {
        Provider::AntigravityDesktop => Some(row.event_count),
        Provider::AntigravityIde => database_alias_count,
        _ => None,
    };
    if let Some(count) = count
        && !missing.contains(&row.session.id)
        && let Some(file) = row.source_revision.and_then(|revision| revision.file)
    {
        record(
            tx,
            &row.session,
            home.as_os_str().as_encoded_bytes(),
            file,
            count,
            generation,
            Some(row),
        )?;
    }
    Ok(())
}
