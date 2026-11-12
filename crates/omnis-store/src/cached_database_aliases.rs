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
    home: Vec<u8>,
    observed_contents: Option<uuid::Uuid>,
    /// Metadata for the selected document.
    pub catalog: IndexedSession,
    /// Title derived from the selected document.
    pub title: Option<IndexedTrajectoryTitle>,
}

impl CachedDatabaseAliasSource {
    /// Checks whether live discovery selected the same database and a revision verified by this read.
    #[must_use]
    pub fn matches_observation(&self, home: &Path, revision: omnis_ir::SourceRevision) -> bool {
        self.home == home.as_os_str().as_encoded_bytes()
            && self.catalog.source_revision.is_some_and(|source| {
                revision.file.is_some()
                    && revision.file == source.file
                    && revision.contents.is_some()
                    && (revision.contents == source.contents
                        || revision.contents == self.observed_contents)
            })
    }
}

fn alias_sources_sql() -> String {
    let gap_join = crate::SEARCH_GAP_JOIN;
    format!("
    WITH receipts AS (
        SELECT provider,session_id,source_home,source_file,source_contents,content_hash,length(step_proof)/16 AS steps
        FROM desktop_search_receipts
        UNION ALL
        SELECT provider,session_id,source_home,source_file,source_contents,content_hash,length(step_proof)/16
        FROM antigravity_ide_search_receipts
    ), primary_docs AS (
        SELECT r.*,trajectories.id AS document_id,trajectories.indexed_at,
            CASE WHEN active_projection.trajectory_id IS NULL THEN trajectories.derived_title ELSE active_projection.derived_title END AS derived_title,
            trajectories.complete AND gap.trajectory_id IS NULL AS projection_complete,
            coalesce(active_projection.document_version,trajectories.document_version) AS document_version,
            trajectories.source_complete AND NOT trajectories.provider_limited AND gap.trajectory_id IS NULL AS complete
        FROM receipts r JOIN session_trajectories trajectories USING(provider,session_id)
        {gap_join}
        JOIN native_search_document_homes o ON o.trajectory_id=trajectories.id AND o.verification=1
            AND o.source_home=r.source_home AND o.source_file=r.source_file
        JOIN native_catalog_homes h ON h.provider=r.provider AND h.session_id=r.session_id
        JOIN session_index c ON c.provider=r.provider AND c.session_id=r.session_id
        WHERE trajectories.origin='native' AND trajectories.source_file_id=r.source_file
            AND r.source_contents=CASE WHEN active_projection.trajectory_id IS NULL
                               THEN trajectories.source_contents_id ELSE gap.observed_contents END
            AND r.content_hash=coalesce(active_projection.content_hash,trajectories.content_hash)
            AND h.verified AND h.home=r.source_home AND c.source_file_id=r.source_file
    ), documents AS (
        SELECT * FROM primary_docs
        UNION ALL
        SELECT r.*,-s.id,NULL,s.derived_title,s.complete,coalesce(s.document_version,0),s.source_complete AND NOT s.provider_limited
        FROM receipts r JOIN scoped_native_documents s USING(provider,session_id,source_home,source_file,source_contents)
        WHERE NOT EXISTS(SELECT 1 FROM primary_docs p WHERE p.provider=r.provider AND p.session_id=r.session_id
            AND p.source_home=r.source_home AND p.source_file=r.source_file)
    ), summaries AS (
        SELECT 'antigravity-ide' AS provider,session_id,source_home,source_file,steps,summary FROM ide_step_declarations
        UNION ALL
        SELECT 'antigravity-desktop',session_id,source_home,source_file,steps,summary FROM desktop_step_declarations
    )
    SELECT cur.provider,cur.session_id,
        CASE WHEN shared.summary IS NOT NULL THEN shared.summary->>'title'
             WHEN cur.document_id>0 THEN c.title ELSE s.summary->>'title' END AS donor_title,
        CASE WHEN shared.summary IS NOT NULL THEN shared.summary->>'project_path'
             WHEN cur.document_id>0 THEN c.project_path ELSE s.summary->>'project_path' END AS donor_project_path,
        CASE WHEN shared.summary IS NOT NULL THEN shared.summary->>'git_branch'
             WHEN cur.document_id>0 THEN c.git_branch ELSE s.summary->>'git_branch' END AS donor_git_branch,
        CASE WHEN shared.summary IS NOT NULL THEN shared.summary->>'created_at'
             WHEN cur.document_id>0 THEN c.created_at ELSE s.summary->>'created_at' END AS donor_created_at,
        CASE WHEN shared.summary IS NOT NULL THEN shared.summary->>'updated_at'
             WHEN cur.document_id>0 THEN c.updated_at ELSE s.summary->>'updated_at' END AS donor_updated_at,
        CASE WHEN shared.summary IS NOT NULL THEN coalesce(shared.summary->>'approximate',0)
             WHEN cur.document_id>0 THEN c.updated_at_approximate ELSE coalesce(s.summary->>'approximate',0) END AS donor_approximate,
        cur.steps,NULL,cur.source_file,cur.source_contents,cur.derived_title,
        cur.document_id,cur.content_hash,older.document_id,older.content_hash,cur.document_version,
        cur.source_home,witness.observed_contents
    FROM documents older
    LEFT JOIN physical_database_body_reads witness USING(session_id,source_home,source_file)
    LEFT JOIN physical_step_declarations declaration USING(session_id,source_home,source_file)
    JOIN documents cur USING(session_id,source_home,source_file)
    JOIN native_catalog_homes selected ON selected.provider=older.provider AND selected.session_id=older.session_id
        AND selected.verified AND selected.home=older.source_home
    JOIN session_index selected_catalog ON selected_catalog.provider=older.provider AND selected_catalog.session_id=older.session_id
        AND selected_catalog.source_file_id=older.source_file
        AND (selected_catalog.source_contents_id=older.source_contents OR witness.provider=cur.provider)
    LEFT JOIN session_index c ON cur.document_id>0 AND c.provider=cur.provider AND c.session_id=cur.session_id
    LEFT JOIN physical_database_summaries shared ON shared.session_id=cur.session_id
        AND shared.source_home=cur.source_home AND shared.source_file=cur.source_file
    LEFT JOIN summaries s ON cur.document_id<0 AND s.provider=cur.provider AND s.session_id=cur.session_id
        AND s.source_file=cur.source_file AND s.source_home=cur.source_home AND s.steps=cur.steps
    WHERE (cur.document_id<0 OR c.source_contents_id=cur.source_contents OR witness.provider=cur.provider)
        AND (?1 IS NULL OR older.provider=?1) AND (older.provider<>'antigravity-ide' OR selected.home=?2)
        AND (older.content_hash,older.steps,older.complete,older.projection_complete,older.derived_title,
             selected_catalog.title,selected_catalog.project_path,selected_catalog.git_branch,
             selected_catalog.created_at,selected_catalog.updated_at,selected_catalog.updated_at_approximate,
             selected_catalog.event_count)
          IS NOT (cur.content_hash,cur.steps,cur.complete,cur.projection_complete,cur.derived_title,
             donor_title,donor_project_path,donor_git_branch,donor_created_at,donor_updated_at,donor_approximate,cur.steps)
        AND older.provider<>cur.provider AND (
            ((witness.declaration_generation IS declaration.generation
                  OR declaration.source_contents IN (witness.source_contents,witness.observed_contents))
                AND cur.provider=witness.provider
                AND cur.source_contents=witness.source_contents AND cur.content_hash=witness.content_hash)
            OR (witness.session_id IS NULL
                AND cur.source_contents=declaration.source_contents AND cur.steps=declaration.steps
                AND cur.complete AND cur.indexed_at>older.indexed_at)
        )")
}

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
    /// Finds the alias document from the latest verified read of the same database.
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
            .prepare_cached(&alias_sources_sql())
            .map_err(database_error)?;
        let rows = statement
            .query_map(
                rusqlite::params![
                    provider.map(|p| p.to_string()),
                    ide_home.map(|p| p.as_os_str().as_encoded_bytes())
                ],
                |row| {
                    if !archived_body_matches(self.connection, row, 13)?
                        || !archived_body_matches(self.connection, row, 15)?
                    {
                        return Ok(None);
                    }
                    let catalog = indexed_session_from_row(row)?;
                    let session = catalog.session.clone();
                    let document_version = row.get(17)?;
                    let title =
                        row.get::<_, Option<String>>(12)?
                            .map(|title| IndexedTrajectoryTitle {
                                session: session.clone(),
                                document_version,
                                verified_at: None,
                                cached_at: None,
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
                            home: row.get(18)?,
                            observed_contents: row
                                .get::<_, Option<[u8; 16]>>(19)?
                                .map(uuid::Uuid::from_bytes),
                            document_id: row.get(13)?,
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
