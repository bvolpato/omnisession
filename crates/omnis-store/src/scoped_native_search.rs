use std::path::PathBuf;

use omnis_ir::{Provider, SessionRef};
use rusqlite::params;
use uuid::Uuid;

use super::{
    DatabaseFailure, Result, SessionTrajectoryMatch, SessionTrajectorySearchPage, StoreError,
    TRAJECTORY_SEARCH_RESULT_LIMIT, TrajectorySearchSnapshot, database_error,
    empty_trajectory_search_page, exact_phrase_snippet, trajectory_clauses, validate_session_ref,
};

impl TrajectorySearchSnapshot<'_> {
    /// Searches native documents retained for a home with a conflicting primary document.
    /// Live sources identify the selected home and file. Cached searches use the verified catalog.
    ///
    /// # Errors
    ///
    /// Returns an error if a session reference is invalid or the scoped document index cannot be read.
    pub fn search_scoped_native_documents(
        &self,
        query: &str,
        limit: usize,
        sources: &[(SessionRef, Option<PathBuf>, Option<Uuid>)],
    ) -> Result<SessionTrajectorySearchPage> {
        let clauses = trajectory_clauses(query);
        if sources.is_empty() || limit == 0 || clauses.is_empty() {
            return Ok(empty_trajectory_search_page());
        }
        let mut selected = Vec::with_capacity(sources.len());
        for (session, home, file) in sources {
            validate_session_ref(session)?;
            selected.push((
                session.provider.to_string(),
                &session.id,
                home.as_ref()
                    .map(|path| hex::encode(path.as_os_str().as_encoded_bytes())),
                file.map(|id| id.simple().to_string()),
            ));
        }
        let selected = serde_json::to_string(&selected)
            .map_err(|_| StoreError::Database(DatabaseFailure::Interface))?;
        let expression = clauses
            .iter()
            .map(|clause| clause.match_expression.as_str())
            .collect::<Vec<_>>()
            .join(" AND ");
        let phrases = clauses
            .iter()
            .filter_map(|clause| clause.exact_phrase.as_deref())
            .collect::<Vec<_>>();
        let exact = serde_json::to_string(&phrases)
            .map_err(|_| StoreError::Database(DatabaseFailure::Interface))?;
        let mut statement = self.connection.prepare_cached(
            "SELECT p.provider, p.session_id, p.indexed_byte_count, p.source_byte_count, p.truncation_strategy,
                    CASE WHEN ?3 = '[]' THEN snippet(scoped_native_document_fts, 0, '', '', ' … ', 28)
                         ELSE p.stored_text END, p.source_complete, p.complete
             FROM scoped_native_document_fts JOIN scoped_native_documents p ON p.id = scoped_native_document_fts.rowid
             WHERE scoped_native_document_fts MATCH ?1 AND p.id IN (
                 SELECT scoped.id FROM json_each(?2) picked
                 JOIN scoped_native_documents scoped ON scoped.provider = (picked.value ->> 0)
                     AND scoped.session_id = (picked.value ->> 1)
                 LEFT JOIN native_catalog_homes native USING(provider,session_id)
                 LEFT JOIN session_index catalog USING(provider,session_id)
                 WHERE CASE WHEN (picked.value ->> 2) IS NULL THEN
                         native.verified AND scoped.source_home = native.home AND scoped.source_file = catalog.source_file_id
                     ELSE scoped.source_home = unhex(picked.value ->> 2)
                         AND scoped.source_file = unhex(picked.value ->> 3) END
             ) AND NOT EXISTS (SELECT 1 FROM json_each(?3) phrase
                               WHERE NOT omnis_folded_contains(p.stored_text, phrase.value))
             ORDER BY scoped_native_document_fts.rank, p.provider, p.session_id, p.id LIMIT ?4"
        ).map_err(database_error)?;
        let limit = limit.min(TRAJECTORY_SEARCH_RESULT_LIMIT);
        let take = i64::try_from(limit + 1).map_err(|_| StoreError::InvalidSessionReference)?;
        let mut rows = statement
            .query(params![expression, selected, exact, take])
            .map_err(database_error)?;
        let mut matches = Vec::new();
        while let Some(row) = rows.next().map_err(database_error)? {
            if matches.len() == limit {
                return Ok(SessionTrajectorySearchPage {
                    matches,
                    has_more: true,
                });
            }
            let provider = row
                .get::<_, String>(0)
                .map_err(database_error)?
                .parse::<Provider>()
                .map_err(|_| StoreError::CorruptStore)?;
            let text: String = row.get(5).map_err(database_error)?;
            matches.push(SessionTrajectoryMatch {
                session: SessionRef::new(
                    provider,
                    row.get::<_, String>(1).map_err(database_error)?,
                ),
                snippet: match phrases.first() {
                    Some(phrase) => exact_phrase_snippet(&text, phrase),
                    None => text,
                },
                source_complete: row.get(6).map_err(database_error)?,
                complete: row.get(7).map_err(database_error)?,
                indexed_byte_count: usize::try_from(row.get::<_, i64>(2).map_err(database_error)?)
                    .map_err(|_| StoreError::CorruptStore)?,
                source_byte_count: usize::try_from(row.get::<_, i64>(3).map_err(database_error)?)
                    .map_err(|_| StoreError::CorruptStore)?,
                truncation_strategy: row.get(4).map_err(database_error)?,
            });
        }
        Ok(SessionTrajectorySearchPage {
            matches,
            has_more: false,
        })
    }
}
