use std::{collections::HashMap, path::Path};

use omnis_ir::{Provider, SessionRef};
use omnis_store::{
    CachedDatabaseAliasSource, IndexedSession, IndexedTrajectoryTitle, Result,
    SessionTrajectorySearchPage, TrajectorySearchSnapshot,
};

#[derive(Default)]
pub(crate) struct CachedDatabaseAliases {
    sources: HashMap<SessionRef, CachedDatabaseAliasSource>,
}

impl CachedDatabaseAliases {
    pub(crate) fn read(
        snapshot: &TrajectorySearchSnapshot<'_>,
        provider: Option<Provider>,
        ide_home: Option<&Path>,
    ) -> Result<Self> {
        let sources = if provider.is_none_or(|provider| {
            matches!(
                provider,
                Provider::AntigravityIde | Provider::AntigravityDesktop
            )
        }) {
            snapshot.cached_database_alias_sources(provider, ide_home)?
        } else {
            HashMap::new()
        };
        Ok(Self { sources })
    }

    pub(crate) fn project_catalog(
        &self,
        sessions: &mut [IndexedSession],
        titles: &mut Vec<IndexedTrajectoryTitle>,
    ) {
        titles.retain(|row| !self.sources.contains_key(&row.session));
        for row in sessions {
            if let Some(source) = self.sources.get(&row.session) {
                let session = row.session.clone();
                row.clone_from(&source.catalog);
                row.session.clone_from(&session);
                if let Some(title) = &source.title {
                    titles.push(IndexedTrajectoryTitle {
                        session,
                        ..title.clone()
                    });
                }
            }
        }
    }

    pub(crate) fn contains(&self, session: &SessionRef) -> bool {
        self.sources.contains_key(session)
    }

    pub(crate) fn search(
        &self,
        snapshot: &TrajectorySearchSnapshot<'_>,
        query: &str,
        limit: usize,
        selections: &super::ScopedSearchSelections,
    ) -> Result<SessionTrajectorySearchPage> {
        if self.sources.is_empty() {
            return snapshot.search_selected_source_documents(
                query,
                limit,
                &selections.primary,
                &selections.scoped_documents,
            );
        }
        let mut projected = Vec::new();
        let mut direct = |session: &SessionRef| {
            if let Some(source) = self.sources.get(session) {
                projected.push(source);
                false
            } else {
                true
            }
        };
        let primary = selections
            .primary
            .iter()
            .filter(|row| direct(&row.session))
            .cloned()
            .collect::<Vec<_>>();
        let scoped = selections
            .scoped_documents
            .iter()
            .filter(|(session, _, _)| direct(session))
            .cloned()
            .collect::<Vec<_>>();
        snapshot.search_projected_source_documents(query, limit, &primary, &scoped, &projected)
    }
}
