use std::{collections::HashMap, path::Path};

use rusqlite::{Connection, Row};
use sha2::{Digest, Sha256};

use crate::{
    IndexedSession, IndexedTrajectoryTitle, Provider, Result, SessionRef, StoreError,
    TrajectorySearchSnapshot, database_error, indexed_session_from_row,
};

/// A verified database document and its metadata for a cached alias search.
pub struct CachedDatabaseAliasSource {
    pub(super) document_id: i64,
    pub(super) provider: Provider,
    /// Metadata for the selected document.
    pub catalog: IndexedSession,
    /// Title derived from the selected document.
    pub title: Option<IndexedTrajectoryTitle>,
}

const ALIAS_SOURCES: &str = "
    WITH receipts AS (
        SELECT provider,session_id,source_home,source_file,source_contents,content_hash,length(step_proof)/16 AS steps
        FROM desktop_search_receipts
        UNION ALL
        SELECT provider,session_id,source_home,source_file,source_contents,content_hash,length(step_proof)/16
        FROM antigravity_ide_search_receipts
    ), primary_docs AS (
        SELECT r.*,d.id AS document_id,d.derived_title,d.source_complete AND d.complete AND NOT d.provider_limited AS complete
        FROM receipts r JOIN session_trajectories d USING(provider,session_id)
        JOIN native_search_document_homes o ON o.trajectory_id=d.id AND o.verification=1
            AND o.source_home=r.source_home AND o.source_file=r.source_file
        JOIN native_catalog_homes h USING(provider,session_id)
        JOIN session_index c USING(provider,session_id)
        WHERE d.origin='native' AND d.source_file_id=r.source_file AND d.source_contents_id=r.source_contents
            AND d.content_hash=r.content_hash AND h.verified AND h.home=r.source_home
            AND c.source_file_id=r.source_file AND c.source_contents_id=r.source_contents
    ), documents AS (
        SELECT * FROM primary_docs
        UNION ALL
        SELECT r.*,-s.id,s.derived_title,s.source_complete AND s.complete AND NOT s.provider_limited
        FROM receipts r JOIN scoped_native_documents s USING(provider,session_id,source_home,source_file,source_contents)
        WHERE NOT EXISTS(SELECT 1 FROM primary_docs p WHERE p.provider=r.provider AND p.session_id=r.session_id
            AND p.source_home=r.source_home AND p.source_file=r.source_file)
    ), summaries AS (
        SELECT 'antigravity-ide' AS provider,session_id,source_home,source_file,steps,summary FROM ide_step_declarations
        UNION ALL
        SELECT 'antigravity-desktop',session_id,source_home,source_file,steps,summary FROM desktop_step_declarations
    )
    SELECT cur.provider,cur.session_id,
        CASE WHEN cur.document_id>0 THEN c.title ELSE s.summary->>'title' END,
        CASE WHEN cur.document_id>0 THEN c.project_path ELSE s.summary->>'project_path' END,
        CASE WHEN cur.document_id>0 THEN c.git_branch ELSE s.summary->>'git_branch' END,
        CASE WHEN cur.document_id>0 THEN c.created_at ELSE s.summary->>'created_at' END,
        CASE WHEN cur.document_id>0 THEN c.updated_at ELSE s.summary->>'updated_at' END,
        CASE WHEN cur.document_id>0 THEN c.updated_at_approximate ELSE coalesce(s.summary->>'approximate',0) END,
        cur.steps,cur.source_file,cur.source_contents,cur.derived_title,
        cur.document_id,cur.content_hash,older.document_id,older.content_hash
    FROM documents older JOIN physical_step_declarations declaration USING(session_id,source_home,source_file)
    JOIN documents cur USING(session_id,source_home,source_file)
    JOIN native_catalog_homes selected ON selected.provider=older.provider AND selected.session_id=older.session_id
        AND selected.verified AND selected.home=older.source_home
    JOIN session_index selected_catalog ON selected_catalog.provider=older.provider AND selected_catalog.session_id=older.session_id
        AND selected_catalog.source_file_id=older.source_file AND selected_catalog.source_contents_id=older.source_contents
    LEFT JOIN session_index c ON cur.document_id>0 AND c.provider=cur.provider AND c.session_id=cur.session_id
    LEFT JOIN summaries s ON cur.document_id<0 AND s.provider=cur.provider AND s.session_id=cur.session_id
        AND s.source_file=cur.source_file AND s.source_home=cur.source_home AND s.steps=cur.steps
    WHERE (?1 IS NULL OR older.provider=?1) AND (older.provider<>'antigravity-ide' OR selected.home=?2)
        AND older.provider<>cur.provider AND older.steps>declaration.steps
        AND cur.source_contents=declaration.source_contents AND cur.complete AND cur.steps=declaration.steps";

fn archived_body_matches(
    connection: &Connection,
    row: &Row<'_>,
    column: usize,
) -> rusqlite::Result<bool> {
    let id: i64 = row.get(column)?;
    if id > 0 {
        return Ok(true);
    }
    let hash: [u8; 32] = row.get(column + 1)?;
    connection
        .prepare_cached("SELECT stored_text FROM scoped_native_documents WHERE id=-?1")?
        .query_row([id], |r| {
            Ok(Sha256::digest(r.get_ref(0)?.as_bytes()?)[..] == hash)
        })
}

impl TrajectorySearchSnapshot<'_> {
    /// Finds an alias document that covers the latest declared revision of the same database.
    ///
    /// # Errors
    ///
    /// Returns an error if the store cannot read the document receipts.
    pub fn cached_database_alias_sources(
        &self,
        provider: Option<Provider>,
        ide_home: Option<&Path>,
    ) -> Result<HashMap<SessionRef, CachedDatabaseAliasSource>> {
        let mut statement = self
            .connection
            .prepare_cached(ALIAS_SOURCES)
            .map_err(database_error)?;
        let rows = statement
            .query_map(
                rusqlite::params![
                    provider.map(|p| p.to_string()),
                    ide_home.map(|p| p.as_os_str().as_encoded_bytes())
                ],
                |row| {
                    if !archived_body_matches(self.connection, row, 12)?
                        || !archived_body_matches(self.connection, row, 14)?
                    {
                        return Ok(None);
                    }
                    let catalog = indexed_session_from_row(row)?;
                    let session = catalog.session.clone();
                    let title =
                        row.get::<_, Option<String>>(11)?
                            .map(|title| IndexedTrajectoryTitle {
                                session: session.clone(),
                                title: Some(title),
                                source_revision: catalog.source_revision.unwrap_or_default(),
                            });
                    let provider = if session.provider == Provider::AntigravityIde {
                        Provider::AntigravityDesktop
                    } else {
                        Provider::AntigravityIde
                    };
                    Ok(Some((
                        SessionRef::new(provider, session.id.clone()),
                        CachedDatabaseAliasSource {
                            provider,
                            document_id: row.get(12)?,
                            catalog,
                            title,
                        },
                    )))
                },
            )
            .map_err(database_error)?;
        rows.filter_map(std::result::Result::transpose)
            .collect::<rusqlite::Result<_>>()
            .map_err(|_| StoreError::CorruptStore)
    }
}
