use std::path::PathBuf;

use omnis_ir::SessionRef;
use uuid::Uuid;

use crate::{DatabaseFailure, Result, validate_session_ref};

pub(super) fn scoped_selection_json(
    sources: &[(SessionRef, Option<PathBuf>, Option<Uuid>)],
) -> Result<String> {
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
    serde_json::to_string(&selected)
        .map_err(|_| crate::StoreError::Database(DatabaseFailure::Interface))
}

pub(super) fn scoped_selection_sql(parameter: usize) -> String {
    format!(
        "SELECT DISTINCT scoped.id FROM json_each(?{parameter}) picked
         JOIN scoped_native_documents scoped ON scoped.provider = (picked.value ->> 0)
             AND scoped.session_id = (picked.value ->> 1)
         LEFT JOIN native_catalog_homes native USING(provider,session_id)
         LEFT JOIN session_index catalog USING(provider,session_id)
         WHERE CASE WHEN (picked.value ->> 2) IS NULL THEN
                 native.verified AND scoped.source_home = native.home AND scoped.source_file = catalog.source_file_id
             ELSE scoped.source_home = unhex(picked.value ->> 2)
                 AND (scoped.source_file = unhex(picked.value ->> 3)
                     OR ((picked.value ->> 3) IS NULL AND scoped.provider = 'antigravity'
                         AND native.verified AND scoped.source_home = native.home
                         AND catalog.source_file_id IS NULL AND catalog.event_count > 0
                         AND NOT EXISTS (SELECT 1 FROM scoped_native_documents other
                             WHERE other.provider = scoped.provider AND other.session_id = scoped.session_id
                               AND other.source_home = scoped.source_home AND other.id <> scoped.id))) END"
    )
}

impl crate::TrajectorySearchSnapshot<'_> {
    /// Searches only the native documents selected by home and physical source file.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid references or an unreadable index.
    pub fn search_scoped_native_documents(
        &self,
        query: &str,
        limit: usize,
        sources: &[(SessionRef, Option<PathBuf>, Option<Uuid>)],
    ) -> Result<crate::SessionTrajectorySearchPage> {
        self.search_selected_source_documents(query, limit, &[], sources)
    }
}
