use std::{
    cell::OnceCell,
    collections::{HashMap, HashSet},
    path::Path,
};

use omnis_adapters::{AdapterRegistry, NativeSelectionInventory};
use omnis_ir::{Provider, SessionRef, SourceRevision};
use omnis_store::{IndexedSession, StoreError, TrajectorySourceGuard};

#[derive(Clone, Copy)]
pub(crate) struct FileCheck<'a> {
    pub(crate) session: &'a SessionRef,
    pub(crate) path: Option<&'a Path>,
    pub(crate) revision: SourceRevision,
    pub(crate) guard: TrajectorySourceGuard,
    pub(crate) catalog: Option<&'a IndexedSession>,
}

impl FileCheck<'_> {
    fn requires_catalog(self) -> bool {
        matches!(
            self.session.provider,
            Provider::Antigravity | Provider::AntigravityDesktop
        )
    }
}

/// One native file inventory per provider for the duration of an indexing operation.
pub(crate) struct FileSelections<'a> {
    pub(super) registry: &'a AdapterRegistry,
    inventories: HashMap<Provider, anyhow::Result<Option<Box<dyn NativeSelectionInventory>>>>,
}

impl<'a> FileSelections<'a> {
    pub(crate) fn new(registry: &'a AdapterRegistry) -> Self {
        Self {
            registry,
            inventories: HashMap::new(),
        }
    }

    fn observe(&mut self, provider: Provider) -> omnis_store::Result<()> {
        self.inventories
            .entry(provider)
            .or_insert_with(|| self.registry.adapter(provider)?.selection_inventory())
            .as_ref()
            .map(|_| ())
            .map_err(|_| StoreError::TrajectorySourceChanged)
    }

    pub(super) fn refresh(&mut self, provider: Provider) -> omnis_store::Result<()> {
        if let Some(Ok(Some(inventory))) = self.inventories.get_mut(&provider) {
            inventory
                .refresh()
                .map_err(|_| StoreError::TrajectorySourceChanged)?;
        } else if self.inventories.get(&provider).is_some_and(Result::is_err) {
            self.inventories.remove(&provider);
            self.observe(provider)?;
        }
        Ok(())
    }

    pub(super) fn selected(&mut self, check: &FileCheck<'_>) -> omnis_store::Result<bool> {
        self.observe(check.session.provider)?;
        let inventory = self.inventories[&check.session.provider]
            .as_ref()
            .map_err(|_| StoreError::TrajectorySourceChanged)?;
        let Some(inventory) = inventory else {
            return Ok(self.provider_selects(check));
        };
        Ok(check
            .path
            .and_then(|path| inventory.selected_copy(&check.session.id, path))
            .is_some_and(|native| {
                native.source_path.as_deref() == check.path
                    && native.source_revision == Some(check.revision)
            }))
    }

    pub(super) fn selected_session_at(
        &mut self,
        session: &SessionRef,
        path: &Path,
    ) -> anyhow::Result<Option<omnis_adapters::NativeSession>> {
        self.observe(session.provider)?;
        if let Ok(Some(inventory)) = &self.inventories[&session.provider] {
            return Ok(inventory.selected_copy(&session.id, path).cloned());
        }
        self.registry
            .adapter(session.provider)?
            .selected_session_at(&session.id, path)
    }

    fn provider_selects(&self, check: &FileCheck<'_>) -> bool {
        check.path.is_some_and(|path| {
            self.registry
                .adapter(check.session.provider)
                .and_then(|adapter| adapter.selected_session_at(&check.session.id, path))
                .is_ok_and(|native| {
                    native.is_some_and(|native| {
                        if check.requires_catalog() {
                            native.source_revision.is_some_and(|revision| {
                                revision.matches_file_contents(check.revision)
                            }) && check.catalog.is_some_and(|catalog| {
                                crate::indexed_session_metadata(&native) == *catalog
                            })
                        } else {
                            native.source_revision == Some(check.revision)
                        }
                    })
                })
        })
    }

    pub(crate) fn write_documents<'s, 'd>(
        &mut self,
        store: &omnis_store::Store,
        checks: &[FileCheck<'_>],
        documents: impl ExactSizeIterator<Item = omnis_store::SelectedTrajectoryDocument<'s, 'd>>,
    ) -> omnis_store::Result<usize> {
        let registry = self.registry;
        let mut homes = HashMap::new();
        for check in checks.iter().filter(|check| check.requires_catalog()) {
            if let std::collections::hash_map::Entry::Vacant(entry) =
                homes.entry(check.session.provider)
            {
                entry.insert(
                    registry
                        .discovery_cache_home(check.session.provider)
                        .ok()
                        .flatten()
                        .ok_or(StoreError::TrajectorySourceChanged)?,
                );
            }
        }
        self.write(checks, |verify| {
            store.upsert_selected_trajectory_documents_in_homes(
                documents,
                &homes,
                |session, revision| {
                    verify(session, revision)
                        && homes.get(&session.provider).is_none_or(|home| {
                            registry
                                .discovery_cache_home(session.provider)
                                .ok()
                                .flatten()
                                .as_ref()
                                == Some(home)
                        })
                },
            )
        })
    }

    /// Discover any competing files before SQLite starts the write. Check the known paths
    /// for all disputed IDs together when the writer reaches its final verification.
    pub(crate) fn write<T>(
        &mut self,
        checks: &[FileCheck<'_>],
        write: impl FnOnce(&dyn Fn(&SessionRef, SourceRevision) -> bool) -> omnis_store::Result<T>,
    ) -> omnis_store::Result<T> {
        let mut by_session = HashMap::new();
        let mut disputed: HashMap<Provider, Vec<(&str, &Path, SourceRevision)>> = HashMap::new();
        for check in checks {
            by_session.insert(check.session, check);
            if check.guard.has_competing_file(check.revision.file) {
                self.observe(check.session.provider)?;
                disputed.entry(check.session.provider).or_default().push((
                    &check.session.id,
                    check.path.ok_or(StoreError::TrajectorySourceChanged)?,
                    check.revision,
                ));
            }
        }
        let validated = OnceCell::new();
        write(&|session, revision| {
            let Some(check) = by_session.get(session) else {
                return false;
            };
            if !check.revision.matches_file_contents(revision) {
                return false;
            }
            if check.requires_catalog() {
                return self.provider_selects(check);
            }
            if !check.guard.has_competing_file(revision.file) {
                return check
                    .path
                    .and_then(omnis_adapters::source_file_revision)
                    .is_some_and(|current| current.matches_file_contents(revision));
            }
            if matches!(self.inventories.get(&session.provider), Some(Ok(None))) {
                return self.provider_selects(check);
            }
            let selected = validated.get_or_init(|| {
                let mut selected = HashSet::new();
                for (&provider, requests) in &disputed {
                    let Some(Ok(Some(inventory))) = self.inventories.get(&provider) else {
                        continue;
                    };
                    for (request, valid) in requests.iter().zip(inventory.validate(requests)) {
                        if valid {
                            selected.insert((provider, request.0));
                        }
                    }
                }
                selected
            });
            selected.contains(&(session.provider, session.id.as_str()))
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use omnis_adapters::NativeSession;
    use uuid::Uuid;

    use super::*;

    struct ContestedSelection(Arc<Mutex<Vec<Vec<String>>>>);

    impl NativeSelectionInventory for ContestedSelection {
        fn refresh(&mut self) -> anyhow::Result<()> {
            Ok(())
        }

        fn selected(&self, _id: &str) -> Option<&NativeSession> {
            None
        }

        fn validate(&self, requests: &[(&str, &Path, SourceRevision)]) -> Vec<bool> {
            self.0
                .lock()
                .unwrap()
                .push(requests.iter().map(|(id, _, _)| (*id).to_owned()).collect());
            requests.iter().map(|(id, _, _)| *id != "changed").collect()
        }
    }

    #[test]
    fn disputed_files_share_one_validation_per_write() -> anyhow::Result<()> {
        let registry = AdapterRegistry::new();
        let mut selections = FileSelections::new(&registry);
        let validations = Arc::new(Mutex::new(Vec::new()));
        selections.inventories.insert(
            Provider::Pi,
            Ok(Some(Box::new(ContestedSelection(Arc::clone(&validations))))),
        );
        let sessions = ["first", "changed", "last"].map(|id| SessionRef::new(Provider::Pi, id));
        let revision = SourceRevision {
            file: Some(Uuid::from_u128(1)),
            contents: Some(Uuid::from_u128(2)),
            ..SourceRevision::default()
        };
        let checks = sessions.each_ref().map(|session| FileCheck {
            session,
            path: Some(Path::new(&session.id)),
            revision,
            catalog: None,
            guard: TrajectorySourceGuard::Verified {
                state: [0; 32],
                known_files: [Some(Uuid::nil()), None, None],
                source_byte_count: None,
                full_or_provider_limited: false,
                document_hash: None,
                sampled_file_bytes: None,
                fully_scanned: false,
            },
        });
        for batch in [&checks[..2], &checks[2..]] {
            selections.write(batch, |verify| {
                for check in batch.iter().rev() {
                    assert_eq!(
                        verify(check.session, revision),
                        check.session.id != "changed"
                    );
                }
                Ok(())
            })?;
        }
        assert_eq!(
            *validations.lock().unwrap(),
            [vec!["first", "changed"], vec!["last"]]
        );
        Ok(())
    }
}
