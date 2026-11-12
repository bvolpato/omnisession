use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use omnis_adapters::NativeSession;

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

    pub(crate) fn live(
        snapshot: &TrajectorySearchSnapshot<'_>,
        provider: Option<Provider>,
        homes: &HashMap<Provider, PathBuf>,
        sessions: &[NativeSession],
    ) -> Result<Self> {
        if !sessions.iter().any(|row| {
            matches!(
                row.session.provider,
                Provider::AntigravityIde | Provider::AntigravityDesktop
            )
        }) {
            return Ok(Self::default());
        }
        let mut aliases = Self::read(
            snapshot,
            provider,
            homes.get(&Provider::AntigravityIde).map(PathBuf::as_path),
        )?;
        if aliases.sources.is_empty() {
            return Ok(aliases);
        }
        let mut selected = HashMap::new();
        for row in sessions {
            if let Some(source) = aliases.sources.remove(&row.session)
                && let Some(home) = homes.get(&row.session.provider)
                && row
                    .source_revision
                    .is_some_and(|revision| source.matches_observation(home, revision))
            {
                selected.insert(row.session.clone(), source);
            }
        }
        Ok(Self { sources: selected })
    }

    pub(crate) fn project_titles(&self, titles: &mut Vec<IndexedTrajectoryTitle>) {
        if self.sources.is_empty() {
            return;
        }
        titles.retain(|row| !self.sources.contains_key(&row.session));
        titles.extend(self.sources.iter().filter_map(|(session, source)| {
            Some(IndexedTrajectoryTitle {
                session: session.clone(),
                ..source.title.as_ref()?.clone()
            })
        }));
    }

    pub(crate) fn project_catalog(
        &self,
        sessions: &mut [IndexedSession],
        titles: &mut Vec<IndexedTrajectoryTitle>,
    ) {
        self.project_titles(titles);
        for row in sessions {
            if let Some(source) = self.sources.get(&row.session) {
                let session = row.session.clone();
                row.clone_from(&source.catalog);
                row.session = session;
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
