use omnis_ir::{Provider, SessionRef, SourceRevision};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use uuid::Uuid;

use crate::{IndexedSession, MissingDesktopSummaries, Result, StoreError, database_error};
use std::path::Path;

pub(super) fn initialize(tx: &Transaction<'_>) -> Result<()> {
    initialize_provider_counts(tx)?;
    initialize_physical_counts(tx)
}

fn initialize_provider_counts(tx: &Transaction<'_>) -> Result<()> {
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

fn initialize_physical_counts(tx: &Transaction<'_>) -> Result<()> {
    if tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='physical_step_declarations')",
        [], |row| row.get::<_, bool>(0),
    ).map_err(database_error)? {
        return Ok(());
    }
    tx.execute_batch(
        "CREATE TABLE physical_step_declarations(
             generation INTEGER PRIMARY KEY AUTOINCREMENT,
             session_id TEXT NOT NULL, source_home BLOB NOT NULL, source_file BLOB NOT NULL,
             source_contents BLOB CHECK(length(source_contents)=16),
             steps INTEGER NOT NULL CHECK(steps>=0),
             UNIQUE(session_id,source_home,source_file)
         );
         INSERT OR IGNORE INTO ide_step_declarations(session_id,source_home,source_file,steps)
         SELECT r.session_id,r.source_home,r.source_file,c.event_count
         FROM antigravity_ide_search_receipts r
         JOIN session_trajectories d ON d.provider=r.provider AND d.session_id=r.session_id
             AND d.origin='native' AND d.source_complete AND NOT d.provider_limited
             AND d.source_file_id=r.source_file AND d.source_contents_id=r.source_contents AND d.content_hash=r.content_hash
         JOIN native_search_document_homes o ON o.trajectory_id=d.id AND o.verification=1
             AND o.source_home=r.source_home AND o.source_file=r.source_file
         JOIN session_index c ON c.provider=r.provider AND c.session_id=r.session_id
             AND c.source_file_id=r.source_file AND c.source_contents_id=r.source_contents
         JOIN native_catalog_homes h ON h.provider=c.provider AND h.session_id=c.session_id
             AND h.verified AND h.home=r.source_home
         WHERE c.event_count>0 AND length(r.step_proof)=16*c.event_count
             AND NOT EXISTS(SELECT 1 FROM desktop_step_declarations
                 WHERE session_id=r.session_id AND source_home=r.source_home AND source_file=r.source_file)
             AND NOT EXISTS(SELECT 1 FROM ide_step_declarations
                 WHERE session_id=r.session_id AND source_home=r.source_home AND source_file=r.source_file);
         INSERT INTO sqlite_sequence(name,seq)
         SELECT 'physical_step_declarations',coalesce(max(seq),0) FROM sqlite_sequence
         WHERE name IN ('desktop_step_declarations','ide_step_declarations');
         INSERT INTO physical_step_declarations(session_id,source_home,source_file,steps)
         SELECT session_id,source_home,source_file,min(steps) FROM (
             SELECT session_id,source_home,source_file,steps FROM desktop_step_declarations
             UNION ALL SELECT session_id,source_home,source_file,steps FROM ide_step_declarations
         ) GROUP BY session_id,source_home,source_file HAVING min(steps)=max(steps);",
    ).map_err(database_error)?;
    for (table, peer) in [
        ("desktop_step_declarations", "ide_step_declarations"),
        ("ide_step_declarations", "desktop_step_declarations"),
    ] {
        tx.execute_batch(&format!(
            "CREATE TRIGGER publish_{table} AFTER INSERT ON {table} BEGIN
                 INSERT OR REPLACE INTO physical_step_declarations(session_id,source_home,source_file,steps)
                     VALUES(new.session_id,new.source_home,new.source_file,new.steps);
             END;
             CREATE TRIGGER forget_{table} AFTER DELETE ON {table}
             WHEN NOT EXISTS(SELECT 1 FROM {peer} WHERE session_id=old.session_id
                      AND source_home=old.source_home AND source_file=old.source_file)
             BEGIN
                 DELETE FROM physical_step_declarations WHERE session_id=old.session_id
                     AND source_home=old.source_home AND source_file=old.source_file;
             END;"
        )).map_err(database_error)?;
    }
    Ok(())
}

pub(super) fn verified(
    connection: &Connection,
    id: &str,
    home: &[u8],
    file: Uuid,
) -> Result<Option<usize>> {
    connection
        .prepare_cached(
            "SELECT steps FROM physical_step_declarations
         WHERE session_id=?1 AND source_home=?2 AND source_file=?3",
        )
        .and_then(|mut query| {
            query
                .query_row(params![id, home, file.as_bytes()], |row| {
                    row.get::<_, i64>(0)
                })
                .optional()
        })
        .map_err(database_error)?
        .map(usize::try_from)
        .transpose()
        .map_err(|_| StoreError::CorruptStore)
}

pub(super) fn reconcile(
    connection: &Connection,
    id: &str,
    home: &[u8],
    file: Uuid,
    matches: impl Fn(usize, Uuid) -> bool,
) -> Result<Option<usize>> {
    let tx = connection.unchecked_transaction().map_err(database_error)?;
    if let Some(count) = verified(&tx, id, home, file)? {
        return Ok(Some(count));
    }
    let mut query = tx.prepare_cached(
        "WITH candidates(provider,steps) AS (
            SELECT 'antigravity-desktop',steps FROM desktop_step_declarations
              WHERE session_id=?1 AND source_home=?2 AND source_file=?3
            UNION ALL SELECT 'antigravity-ide',steps FROM ide_step_declarations
              WHERE session_id=?1 AND source_home=?2 AND source_file=?3)
         SELECT c.steps,d.source_contents_id FROM candidates c
         JOIN session_trajectories d ON d.provider=c.provider AND d.session_id=?1 AND d.source_file_id=?3
         JOIN native_search_document_homes o ON o.trajectory_id=d.id AND o.verification=1
             AND o.source_home=?2 AND o.source_file=?3
         WHERE d.source_contents_id IS NOT NULL
         UNION ALL SELECT c.steps,d.source_contents FROM candidates c JOIN scoped_native_documents d
             ON d.provider=c.provider AND d.session_id=?1 AND d.source_home=?2 AND d.source_file=?3",
    ).map_err(database_error)?;
    let mut rows = query
        .query(params![id, home, file.as_bytes()])
        .map_err(database_error)?;
    let mut proven = None;
    while let Some(row) = rows.next().map_err(database_error)? {
        let count = usize::try_from(row.get::<_, i64>(0).map_err(database_error)?)
            .map_err(|_| StoreError::CorruptStore)?;
        let contents = Uuid::from_bytes(row.get(1).map_err(database_error)?);
        if matches(count, contents) {
            if proven.is_some_and(|prior| prior != count) {
                return Ok(None);
            }
            proven = Some(count);
        }
    }
    drop(rows);
    drop(query);
    if let Some(count) = proven {
        tx.execute(
            "INSERT INTO physical_step_declarations(session_id,source_home,source_file,steps)
                    VALUES(?1,?2,?3,?4)",
            params![
                id,
                home,
                file.as_bytes(),
                i64::try_from(count).map_err(|_| StoreError::CorruptStore)?
            ],
        )
        .map_err(database_error)?;
    }
    tx.commit().map_err(database_error)?;
    Ok(proven)
}

pub(super) fn table(provider: Provider) -> Option<&'static str> {
    match provider {
        Provider::AntigravityDesktop => Some("desktop_step_declarations"),
        Provider::AntigravityIde => Some("ide_step_declarations"),
        _ => None,
    }
}

pub(super) fn generation(connection: &Connection, provider: Provider) -> Result<Option<i64>> {
    let Some(_) = table(provider) else {
        return Ok(None);
    };
    connection
        .query_row(
            "SELECT coalesce(max(generation),0) FROM physical_step_declarations",
            [],
            |row| row.get(0),
        )
        .map_err(database_error)
}

pub(super) fn record(
    tx: &Transaction<'_>,
    session: &SessionRef,
    home: &[u8],
    revision: SourceRevision,
    steps: usize,
    observed_generation: Option<i64>,
    metadata: Option<&IndexedSession>,
) -> Result<()> {
    let file = revision.file.ok_or(StoreError::InvalidSessionReference)?;
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
            "SELECT p.generation,p.steps,d.summary FROM physical_step_declarations p
         LEFT JOIN {table} d USING(session_id,source_home,source_file)
         WHERE p.session_id=?1 AND p.source_home=?2 AND p.source_file=?3"
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
    if let Some(contents) = revision.contents {
        tx.execute(
            "UPDATE physical_step_declarations SET source_contents=?4
             WHERE session_id=?1 AND source_home=?2 AND source_file=?3",
            params![session.id, home, file.as_bytes(), contents.as_bytes()],
        )
        .map_err(database_error)?;
    }
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
        && let Some(revision) = row
            .source_revision
            .filter(|revision| revision.file.is_some())
    {
        record(
            tx,
            &row.session,
            home.as_os_str().as_encoded_bytes(),
            revision,
            count,
            generation,
            Some(row),
        )?;
    }
    Ok(())
}
