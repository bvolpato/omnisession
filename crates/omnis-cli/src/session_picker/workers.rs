use super::{
    AdapterRegistry, CachedPickerEntries, Command, DateTime, HandoffRecord, HashSet,
    IndexedSession, LATEST_RELEASE_URL, NativeSession, PICKER_WARNING_LIMIT, PROVIDERS, Path,
    PathBuf, PickerEntry, PickerState, PreviewKey, PreviewRequest, PreviewSource, PreviewValue,
    Provider, ProviderNameChange, Receiver, Result, SESSION_CACHE_TTL, Sender,
    SessionTrajectorySearchPage, Stdio, Store, SyncSender, TRAJECTORY_SEARCH_LIMIT,
    TrajectorySearchRequest, Utc, VecDeque, cached_picker_entries, env, first_user_message_after,
    has_scoped_home, mpsc, normalized_picker_entries, populate_approximate_updated_at,
    preview_revision, session_preview, thread,
};

use std::collections::HashMap;
use uuid::Uuid;

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use anyhow::Context;
use omnis_adapters::{IndexRead, ProviderAdapter};
use omnis_ir::{SessionRef, SourceRevision};
use omnis_store::{
    DiscoveryScope, IndexedTrajectoryTitle, SelectedSessionObservation, SessionDiscoveryAttempt,
    SessionDiscoveryClaim, SessionTrajectoryOrigin, TrajectorySourceGuard,
};

use crate::{
    read_native_search_snapshot,
    search_index::{self, IndexCandidate, IndexProgress, SearchSource},
};

pub(super) struct DiscoveryUpdate {
    pub(super) archived_ids: Vec<String>,
    pub(super) provider: Provider,
    pub(super) result: anyhow::Result<(Vec<PickerEntry>, bool)>,
    pub(super) names: Option<Arc<CatalogNames>>,
}

impl DiscoveryUpdate {
    fn apply(self, state: &mut PickerState, warnings: &mut Vec<String>) {
        match self.result {
            Ok((mut entries, complete)) => {
                if self.provider == Provider::Codex {
                    state.reconcile_discovered_names(&mut entries, self.names.as_deref());
                }
                state.update_provider_entries(self.provider, entries, complete, &self.archived_ids);
            }
            Err(error) => {
                record_picker_warning(warnings, format!("{}: {error}", self.provider));
            }
        }
    }
}

pub(super) struct CatalogNames {
    pub(super) inventory: omnis_store::CodexCatalogInventory,
    pub(super) prior_assignments: std::collections::HashMap<String, String>,
    pub(super) complete: bool,
}

pub(super) enum SessionCacheUpdate {
    Listed {
        provider: Provider,
        sessions: Vec<IndexedSession>,
        scope: DiscoveryScope,
        database_alias_counts: HashMap<String, usize>,
        archived_ids: Vec<String>,
        names: Option<Arc<CatalogNames>>,
        attempt: Option<SessionDiscoveryAttempt>,
        publication: Option<Sender<Option<bool>>>,
    },
    Failed(SessionDiscoveryAttempt),
}

impl SessionCacheUpdate {
    fn from_listing(
        provider: Provider,
        listing: &omnis_adapters::NativeListing,
        complete: bool,
        names: Option<Arc<CatalogNames>>,
        attempt: Option<SessionDiscoveryAttempt>,
        publication: Option<Sender<Option<bool>>>,
    ) -> Self {
        let database_alias_counts = crate::ide_database_alias_counts(
            provider,
            &listing.sessions,
            listing.desktop_summaries_unavailable,
            &listing.missing_desktop_summary_ids,
        )
        .into_iter()
        .map(|(id, count)| (id.to_owned(), count))
        .collect();
        Self::Listed {
            provider,
            sessions: listing.sessions.iter().map(indexed_session).collect(),
            scope: crate::discovery_scope(
                complete,
                listing.desktop_summaries_unavailable,
                &listing.missing_desktop_summary_ids,
                &listing.unverified_desktop_ids,
            ),
            names,
            attempt,
            publication,
            archived_ids: listing.archived_desktop_ids.clone(),
            database_alias_counts,
        }
    }

    pub(super) fn persist(self, store: &Store) -> omnis_store::Result<()> {
        match self {
            Self::Listed {
                provider,
                sessions,
                scope,
                archived_ids,
                database_alias_counts,
                names,
                attempt,
                publication,
            } => {
                if provider == Provider::Codex {
                    if let Some(names) = names {
                        store.refresh_codex_catalog_with_name_history(
                            &sessions,
                            scope == DiscoveryScope::CompleteProvider,
                            names.complete,
                            &names.inventory,
                            &names.prior_assignments,
                        )?;
                    }
                } else if let Some(attempt) = attempt {
                    let file_at = |position: usize| {
                        sessions[position]
                            .source_revision
                            .and_then(|revision| revision.file)
                    };
                    let count = |id: &str| database_alias_counts.get(id).copied();
                    let result = store.publish_session_discovery(
                        &attempt,
                        &sessions,
                        scope
                            .with_source_files(&file_at)
                            .with_database_alias_counts(&count)
                            .with_archived_desktop_ids(&archived_ids),
                        Utc::now(),
                    );
                    if let Some(publication) = publication {
                        let _ = publication.send(result.as_ref().ok().copied());
                    }
                    result?;
                } else {
                    store.cache_discovered_sessions(provider, &sessions, scope, None)?;
                }
            }
            Self::Failed(attempt) => {
                store.finish_session_discovery(&attempt, Utc::now())?;
            }
        }
        Ok(())
    }
}

pub(super) enum PickerUpdate {
    Cached(Result<CachedPickerEntries, String>),
    StaleCachedPreview(Box<PreviewKey>),
    NativeHomes(HashMap<Provider, PathBuf>),
    NativeTitles(Vec<(SessionRef, Option<Uuid>, Option<String>)>),
    Lineage(Result<Vec<HandoffRecord>, String>),
    Preview {
        key: Box<PreviewKey>,
        value: PreviewValue,
        metadata: Box<PreviewMetadata>,
    },
    TrajectorySearch {
        generation: u64,
        result: Result<SessionTrajectorySearchPage, String>,
    },
    Discovered(DiscoveryUpdate),
    RefreshStarted(Provider),
    AvailableUpdate(Option<String>),
    Warning(String),
    Indexing(IndexProgress),
    /// Unreadable sessions among the latest indexing candidates.
    IndexFailures(usize),
}

pub(super) struct PickerIndexRequest {
    pub(super) candidates: Vec<IndexCandidate>,
    pub(super) homes: HashMap<Provider, PathBuf>,
    pub(super) pending_titles: HashMap<SessionRef, Uuid>,
}

#[derive(Default)]
pub(super) struct PreviewMetadata {
    read_revision: Option<SourceRevision>,
    workspace: Option<omnis_store::CodexWorkspaceUpdate>,
    indexed_title: Option<IndexedTrajectoryTitle>,
    provider_name: Option<ProviderNameChange>,
}

pub(super) struct PickerWorkers {
    pub(super) receiver: Receiver<PickerUpdate>,
    pub(super) preview_sender: Sender<Vec<PreviewRequest>>,
    pub(super) trajectory_search_sender: SyncSender<TrajectorySearchRequest>,
    pub(super) search_index_sender: Sender<PickerIndexRequest>,
    search_index_stop: Arc<AtomicBool>,
}

impl Drop for PickerWorkers {
    // Background indexing never outlives the picker.
    fn drop(&mut self) {
        self.search_index_stop.store(true, Ordering::Relaxed);
    }
}

pub(super) fn spawn_updates(current_project: &Path) -> PickerWorkers {
    let (sender, receiver) = mpsc::channel();
    let (preview_sender, preview_receiver) = mpsc::channel::<Vec<PreviewRequest>>();
    let (trajectory_search_sender, trajectory_search_receiver) =
        mpsc::sync_channel::<TrajectorySearchRequest>(1);
    let (index_sender, index_receiver) = mpsc::channel::<SessionCacheUpdate>();
    let (refresh_codex, changed_codex) = mpsc::sync_channel(1);
    let (search_index_sender, search_index_receiver) = mpsc::channel::<PickerIndexRequest>();
    let search_index_stop = Arc::new(AtomicBool::new(false));

    spawn_index_writer(sender.clone(), index_receiver);
    spawn_cache_updates(
        sender.clone(),
        index_sender,
        current_project.to_path_buf(),
        changed_codex,
    );
    spawn_preview_updates(sender.clone(), preview_receiver, refresh_codex);
    spawn_trajectory_search_updates(sender.clone(), trajectory_search_receiver);
    spawn_search_index(
        sender.clone(),
        search_index_receiver,
        Arc::clone(&search_index_stop),
    );
    spawn_update_check(sender);

    PickerWorkers {
        receiver,
        preview_sender,
        trajectory_search_sender,
        search_index_sender,
        search_index_stop,
    }
}

// Newer candidate lists replace the running pass; stopping the picker ends it.
pub(super) fn spawn_search_index(
    sender: Sender<PickerUpdate>,
    receiver: Receiver<PickerIndexRequest>,
    stop: Arc<AtomicBool>,
) {
    thread::spawn(move || {
        let registry = AdapterRegistry::with_local_adapters();
        let Ok(store) = Store::open_default() else {
            return;
        };
        let pending = std::cell::RefCell::new(None::<PickerIndexRequest>);
        // Failed reads are recorded and later passes skip them, so warn about the whole unreadable
        // set only after this picker run found new failures.
        let mut found_failures = false;
        loop {
            let next = pending.borrow_mut().take();
            let candidates = match next {
                Some(candidates) => candidates,
                None => match receiver.recv() {
                    Ok(candidates) => candidates,
                    Err(_) => return,
                },
            };
            let PickerIndexRequest {
                candidates,
                homes,
                pending_titles,
            } = receiver.try_iter().last().unwrap_or(candidates);
            let selected: HashMap<_, _> = candidates
                .iter()
                .filter(|candidate| has_scoped_home(candidate.session.provider))
                .map(|candidate| (candidate.session.clone(), candidate.source_revision.file))
                .collect();
            send_native_titles(
                &store,
                &sender,
                &homes,
                pending_titles
                    .iter()
                    .filter(|(session, file)| selected.get(*session) == Some(&Some(**file)))
                    .map(|(session, file)| (session, Some(*file))),
            );
            let should_stop = || {
                if stop.load(Ordering::Relaxed) {
                    return true;
                }
                if let Some(newer) = receiver.try_iter().last() {
                    *pending.borrow_mut() = Some(newer);
                    return true;
                }
                false
            };
            match search_index::index_candidates(
                &registry,
                &store,
                candidates,
                false,
                &should_stop,
                &mut |mut progress| {
                    send_native_titles(
                        &store,
                        &sender,
                        &homes,
                        progress.titles.iter().filter_map(|receipt| {
                            Some((&receipt.session, *selected.get(&receipt.session)?))
                        }),
                    );
                    progress
                        .titles
                        .retain(|receipt| !has_scoped_home(receipt.session.provider));
                    let _ = sender.send(PickerUpdate::Indexing(progress));
                },
            ) {
                Ok(summary) => {
                    found_failures |= summary.failed > 0;
                    if found_failures && !summary.stopped {
                        let _ = sender.send(PickerUpdate::IndexFailures(
                            summary.failed + summary.failed_skipped,
                        ));
                    }
                }
                Err(error) => {
                    let _ = sender.send(PickerUpdate::Warning(format!("search index: {error}")));
                }
            }
            if stop.load(Ordering::Relaxed) {
                return;
            }
        }
    });
}

fn send_native_titles<'a>(
    store: &Store,
    sender: &Sender<PickerUpdate>,
    homes: &HashMap<Provider, PathBuf>,
    selected: impl Iterator<Item = (&'a SessionRef, Option<Uuid>)> + Clone,
) {
    if selected.clone().next().is_none() {
        return;
    }
    let titles = store.read_trajectory_search(|snapshot| -> omnis_store::Result<_> {
        let conflicts =
            crate::conflicting_search_documents(snapshot, Some(homes), selected.clone())?;
        let scoped: Vec<_> = selected
            .clone()
            .filter(|(session, _)| conflicts.contains(*session))
            .map(|(session, file)| (session.clone(), homes.get(&session.provider).cloned(), file))
            .collect();
        let scoped_titles: HashMap<_, _> = snapshot
            .scoped_native_titles(&scoped)?
            .into_iter()
            .filter_map(|row| Some((row.session, row.title?)))
            .collect();
        selected
            .filter(|(session, _)| homes.contains_key(&session.provider))
            .map(|(session, file)| {
                let title = if conflicts.contains(session) {
                    scoped_titles.get(session).cloned()
                } else {
                    snapshot.trajectory_title(session)?
                };
                Ok((session.clone(), file, title))
            })
            .collect()
    });
    match titles {
        Ok(titles) => {
            let _ = sender.send(PickerUpdate::NativeTitles(titles));
        }
        Err(error) => {
            let _ = sender.send(PickerUpdate::Warning(format!("session titles: {error}")));
        }
    }
}

pub(super) fn spawn_update_check(sender: Sender<PickerUpdate>) {
    if env::var_os("OMNI_NO_UPDATE_CHECK").is_some_and(|value| value == "1")
        || !crate::self_update::supported()
    {
        return;
    }
    thread::spawn(move || {
        let available = latest_release_version().filter(|latest| {
            version_parts(latest)
                .zip(version_parts(env!("CARGO_PKG_VERSION")))
                .is_some_and(|(latest, current)| latest > current)
        });
        let _ = sender.send(PickerUpdate::AvailableUpdate(available));
    });
}

pub(super) fn latest_release_version() -> Option<String> {
    let output = Command::new("curl")
        .args([
            "--fail",
            "--location",
            "--silent",
            "--show-error",
            "--proto",
            "=https",
            "--tlsv1.2",
            "--connect-timeout",
            "2",
            "--max-time",
            "4",
            "--output",
            "/dev/null",
            "--write-out",
            "%{url_effective}",
            LATEST_RELEASE_URL,
        ])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() || output.stdout.len() > 512 {
        return None;
    }
    let url = std::str::from_utf8(&output.stdout).ok()?.trim();
    release_version_from_url(url).map(ToOwned::to_owned)
}

pub(super) fn release_version_from_url(url: &str) -> Option<&str> {
    let version = url.strip_prefix("https://github.com/bvolpato/omnisession/releases/tag/v")?;
    version_parts(version)?;
    Some(version)
}

pub(super) fn version_parts(version: &str) -> Option<(u64, u64, u64)> {
    let mut parts = version.split('.');
    let parsed = (
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    );
    parts.next().is_none().then_some(parsed)
}

pub(super) fn spawn_cache_updates(
    sender: Sender<PickerUpdate>,
    index_sender: Sender<SessionCacheUpdate>,
    current_project: PathBuf,
    changed_codex: Receiver<()>,
) {
    thread::spawn(move || {
        let registry = AdapterRegistry::with_local_adapters();
        let Ok(store) = Store::open_default() else {
            discover_without_cache(&registry, &sender, &index_sender, &current_project);
            return;
        };
        // Observe the reader before loading the catalog so a concurrent publication cannot be missed.
        let mut selections: std::collections::HashMap<_, _> = super::NATIVE_HOME_PROVIDERS
            .map(|provider| {
                (
                    provider,
                    select_native_discovery(&store, &registry, provider, &sender),
                )
            })
            .into_iter()
            .collect();
        let homes: HashMap<_, _> = selections
            .iter()
            .filter_map(|(&provider, selection)| Some((provider, selection.home.clone()?)))
            .collect();
        if sender
            .send(PickerUpdate::NativeHomes(homes.clone()))
            .is_err()
        {
            return;
        }
        let home = |provider| homes.get(&provider).map(PathBuf::as_path);
        let Some(cache_loaded) = send_cached_picker_entries(
            &store,
            &sender,
            &current_project,
            home(Provider::Antigravity),
            home(Provider::AntigravityDesktop),
            home(Provider::AntigravityIde),
        ) else {
            return;
        };
        let lineage = store.handoff_lineage().map_err(|error| error.to_string());
        if sender.send(PickerUpdate::Lineage(lineage)).is_err() {
            return;
        }
        let mut refreshed_codex = false;
        for provider in PROVIDERS {
            let decision = match selections.remove(&provider) {
                Some(HomeDiscovery {
                    home: Some(home),
                    decision: PickerDiscovery::Fresh,
                }) if !cache_loaded => store
                    .mark_session_discovery_attempted(
                        provider,
                        omnis_core::search_document_version(provider),
                        &home,
                    )
                    .map(|attempt| PickerDiscovery::Due(Some(attempt))),
                Some(selection) => Ok(selection.decision),
                None => begin_picker_discovery(&store, provider, cache_loaded),
            };
            match decision.unwrap_or_else(|error| {
                let _ = sender.send(PickerUpdate::Warning(format!(
                    "{provider} session index: {error}"
                )));
                PickerDiscovery::Due(None)
            }) {
                PickerDiscovery::Fresh => {}
                PickerDiscovery::Wait(attempt) => {
                    if sender.send(PickerUpdate::RefreshStarted(provider)).is_ok() {
                        super::discovery_watch::spawn(
                            sender.clone(),
                            index_sender.clone(),
                            current_project.clone(),
                            provider,
                            attempt,
                        );
                    }
                }
                PickerDiscovery::Due(attempt) => {
                    refreshed_codex |= provider == Provider::Codex;
                    start_provider_update(
                        &sender,
                        &index_sender,
                        &current_project,
                        provider,
                        attempt,
                        homes.get(&provider),
                    );
                }
            }
        }
        drop(store);
        // A cached row can change before the discovery cache expires. Refresh once if a
        // preview observes the change. Discovery supplies the new workspace and source stamp.
        if !refreshed_codex && changed_codex.recv().is_ok() {
            start_provider_update(
                &sender,
                &index_sender,
                &current_project,
                Provider::Codex,
                None,
                None,
            );
        }
    });
}

fn discover_without_cache(
    registry: &AdapterRegistry,
    sender: &Sender<PickerUpdate>,
    index_sender: &Sender<SessionCacheUpdate>,
    project: &Path,
) {
    let homes: HashMap<_, _> = super::NATIVE_HOME_PROVIDERS
        .into_iter()
        .filter_map(|provider| Some((provider, registry.discovery_cache_home(provider).ok()??)))
        .collect();
    let _ = sender.send(PickerUpdate::NativeHomes(homes.clone()));
    let _ = sender.send(PickerUpdate::Cached(Err(
        "session index is unavailable".to_owned()
    )));
    for provider in PROVIDERS {
        start_provider_update(
            sender,
            index_sender,
            project,
            provider,
            None,
            homes.get(&provider),
        );
    }
}

fn send_cached_picker_entries(
    store: &Store,
    sender: &Sender<PickerUpdate>,
    project: &Path,
    antigravity: Option<&Path>,
    desktop: Option<&Path>,
    ide: Option<&Path>,
) -> Option<bool> {
    let entries = cached_picker_entries(store, project, antigravity, desktop, ide)
        .map_err(|error| error.to_string());
    let loaded = entries.is_ok();
    sender.send(PickerUpdate::Cached(entries)).ok()?;
    Some(loaded)
}

enum PickerDiscovery {
    Fresh,
    Wait(SessionDiscoveryAttempt),
    Due(Option<SessionDiscoveryAttempt>),
}

struct HomeDiscovery {
    home: Option<PathBuf>,
    decision: PickerDiscovery,
}

fn select_native_discovery(
    store: &Store,
    registry: &AdapterRegistry,
    provider: Provider,
    sender: &Sender<PickerUpdate>,
) -> HomeDiscovery {
    let home = match registry.discovery_cache_home(provider) {
        Ok(home) => home,
        Err(error) => {
            let _ = sender.send(PickerUpdate::Warning(format!(
                "{provider} session home: {error}"
            )));
            return HomeDiscovery {
                home: None,
                decision: PickerDiscovery::Fresh,
            };
        }
    };
    let decision = match home.as_deref() {
        Some(home) => store
            .try_begin_session_discovery(
                provider,
                omnis_core::search_document_version(provider),
                home,
                Utc::now() - SESSION_CACHE_TTL,
            )
            .map(|claim| match claim {
                SessionDiscoveryClaim::Fresh => PickerDiscovery::Fresh,
                SessionDiscoveryClaim::InProgress(attempt) => PickerDiscovery::Wait(attempt),
                SessionDiscoveryClaim::Claimed(attempt) => PickerDiscovery::Due(Some(attempt)),
            }),
        None => begin_picker_discovery(store, provider, true),
    }
    .unwrap_or_else(|error| {
        let _ = sender.send(PickerUpdate::Warning(format!(
            "{provider} session index: {error}"
        )));
        PickerDiscovery::Due(None)
    });
    HomeDiscovery { home, decision }
}

fn begin_picker_discovery(
    store: &Store,
    provider: Provider,
    cache_loaded: bool,
) -> omnis_store::Result<PickerDiscovery> {
    if cache_loaded
        && store
            .session_index_checked_at(provider)?
            .is_some_and(session_cache_is_fresh)
    {
        return Ok(PickerDiscovery::Fresh);
    }
    store.mark_session_index_checked(provider)?;
    Ok(PickerDiscovery::Due(None))
}

fn start_provider_update(
    sender: &Sender<PickerUpdate>,
    index_sender: &Sender<SessionCacheUpdate>,
    current_project: &Path,
    provider: Provider,
    attempt: Option<SessionDiscoveryAttempt>,
    home: Option<&PathBuf>,
) {
    if sender.send(PickerUpdate::RefreshStarted(provider)).is_ok() {
        spawn_provider_update_in_home(
            sender.clone(),
            index_sender.clone(),
            current_project.to_path_buf(),
            provider,
            attempt,
            home.cloned(),
        );
    }
}

pub(super) fn session_cache_is_fresh(checked_at: DateTime<Utc>) -> bool {
    Utc::now()
        .signed_duration_since(checked_at)
        .to_std()
        .map_or(true, |age| age <= SESSION_CACHE_TTL)
}

pub(super) fn spawn_index_writer(
    sender: Sender<PickerUpdate>,
    receiver: Receiver<SessionCacheUpdate>,
) {
    thread::spawn(move || {
        let Ok(store) = Store::open_default() else {
            let _ = sender.send(PickerUpdate::Warning(
                "session index writer is unavailable".to_owned(),
            ));
            return;
        };
        while let Ok(update) = receiver.recv() {
            if let Err(error) = update.persist(&store) {
                let _ = sender.send(PickerUpdate::Warning(format!("session index: {error}")));
            }
        }
    });
}

pub(super) fn spawn_provider_update(
    sender: Sender<PickerUpdate>,
    index_sender: Sender<SessionCacheUpdate>,
    current_project: PathBuf,
    provider: Provider,
    attempt: Option<SessionDiscoveryAttempt>,
) {
    let home = attempt.as_ref().map(|attempt| attempt.home().to_path_buf());
    spawn_provider_update_in_home(
        sender,
        index_sender,
        current_project,
        provider,
        attempt,
        home,
    );
}

fn list_picker_sessions(
    registry: &AdapterRegistry,
    provider: Provider,
    home: Option<&Path>,
    sender: &Sender<PickerUpdate>,
) -> anyhow::Result<(omnis_adapters::NativeListing, Option<Arc<CatalogNames>>)> {
    let observed = if provider == Provider::Codex {
        match Store::open_default().and_then(|store| store.observe_codex_catalog_inventory()) {
            Ok(sessions) => Some(sessions),
            Err(error) => {
                let _ = sender.send(PickerUpdate::Warning(format!("session index: {error}")));
                None
            }
        }
    } else {
        None
    };
    let listing = registry.list_sessions_in_home(provider, None, home)?;
    let names = observed.and_then(|inventory| {
        let adapter = registry.adapter(provider).ok()?;
        let complete = adapter.session_titles_are_complete();
        let prior_assignments =
            crate::prior_codex_assignments(adapter, &listing.sessions, &inventory, complete);
        Some(Arc::new(CatalogNames {
            inventory,
            prior_assignments,
            complete,
        }))
    });
    Ok((listing, names))
}

fn spawn_provider_update_in_home(
    sender: Sender<PickerUpdate>,
    index_sender: Sender<SessionCacheUpdate>,
    current_project: PathBuf,
    provider: Provider,
    attempt: Option<SessionDiscoveryAttempt>,
    home: Option<PathBuf>,
) {
    thread::spawn(move || {
        let registry = AdapterRegistry::with_local_adapters();
        let result = list_picker_sessions(&registry, provider, home.as_deref(), &sender);
        match result {
            Ok((mut listing, names)) => {
                let complete = (provider == Provider::Codex || listing.notes.is_empty())
                    && registry
                        .adapter(provider)
                        .is_ok_and(ProviderAdapter::discovery_is_complete);
                for note in listing.notes.drain(..) {
                    if sender.send(PickerUpdate::Warning(note)).is_err() {
                        return;
                    }
                }
                populate_approximate_updated_at(&mut listing.sessions);
                if attempt.is_some() || home.is_none() {
                    let (reply, waiting) = attempt.as_ref().map(|_| mpsc::channel()).unzip();
                    let update = SessionCacheUpdate::from_listing(
                        provider,
                        &listing,
                        complete,
                        names.clone(),
                        attempt.clone(),
                        reply,
                    );
                    if index_sender.send(update).is_err() {
                        let _ = sender.send(PickerUpdate::Warning(
                            "session index writer stopped".to_owned(),
                        ));
                    } else if let (Some(wait), Some(attempt)) = (waiting, attempt)
                        && matches!(
                            wait.recv_timeout(SESSION_CACHE_TTL),
                            Ok(Some(false)) | Err(mpsc::RecvTimeoutError::Timeout)
                        )
                    {
                        super::discovery_watch::spawn(
                            sender,
                            index_sender,
                            current_project,
                            provider,
                            attempt,
                        );
                        return;
                    }
                }
                let cached = cached_desktop_observations(
                    provider,
                    home.as_deref(),
                    listing.desktop_summaries_unavailable,
                    &mut listing.sessions,
                    &listing.missing_desktop_summary_ids,
                    &listing.unverified_desktop_ids,
                )
                .unwrap_or_else(|error| {
                    let _ = sender.send(PickerUpdate::Warning(format!(
                        "{provider} cached catalog: {error}"
                    )));
                    Vec::new()
                });
                let mut entries =
                    normalized_picker_entries(listing.sessions, &current_project, false);
                entries.extend(normalized_picker_entries(cached, &current_project, true));
                let _ = sender.send(PickerUpdate::Discovered(DiscoveryUpdate {
                    provider,
                    result: Ok((entries, complete)),
                    archived_ids: listing.archived_desktop_ids,
                    names,
                }));
            }
            Err(error) => {
                if let Some(attempt) = attempt
                    && registry
                        .discovery_cache_home(provider)
                        .is_ok_and(|home| home.as_deref() == Some(attempt.home()))
                {
                    let _ = index_sender.send(SessionCacheUpdate::Failed(attempt));
                }
                let _ = sender.send(PickerUpdate::Discovered(DiscoveryUpdate {
                    provider,
                    result: Err(error),
                    archived_ids: Vec::new(),
                    names: None,
                }));
            }
        }
    });
}

pub(super) fn read_codex_picker_preview(
    registry: &AdapterRegistry,
    key: &PreviewKey,
    source_path: Option<&Path>,
    writer: Option<(&Store, omnis_store::TrajectorySourceGuard)>,
    source_changed: impl FnOnce(),
) -> anyhow::Result<omnis_adapters::IndexRead> {
    let session = &key.session;
    let budget = if key.continuation_after.is_some() {
        u64::MAX
    } else {
        preview_read_budget(key, writer.map(|(_, guard)| guard))
    };
    let read = registry.index_session(session, source_path, budget)?;
    let read = if let Some((store, guard)) = writer {
        search_index::verify_legacy_search_read(registry, store, guard, read)?
    } else {
        read
    };
    let revision = preview_revision(read.source_revision);
    let changed = match &key.source {
        Some(PreviewSource::CodexFile(expected)) => *expected != revision,
        Some(PreviewSource::CachedCodex(expected)) => {
            let header = read
                .native_session
                .as_ref()
                .context("cached Codex preview has no verified catalog metadata")?;
            let effective = &read.snapshot.workspace.current_dir;
            let workspace = match &read.workspace_projection {
                omnis_adapters::CodexWorkspaceProjection::Observed(project) => project.as_deref(),
                omnis_adapters::CodexWorkspaceProjection::Unchecked
                    if !effective.as_os_str().is_empty() =>
                {
                    Some(effective.as_path())
                }
                omnis_adapters::CodexWorkspaceProjection::Unchecked if read.source_complete => None,
                omnis_adapters::CodexWorkspaceProjection::Unchecked => {
                    header.project_path.as_deref()
                }
            };
            workspace.map(Path::as_os_str) != expected.as_deref().map(Path::as_os_str)
        }
        _ => false,
    };
    if changed {
        source_changed();
        anyhow::bail!("Codex rollout or workspace changed before its preview was read");
    }
    Ok(read)
}

fn preview_read_budget(key: &PreviewKey, guard: Option<omnis_store::TrajectorySourceGuard>) -> u64 {
    let revision = key.source.as_ref().and_then(PreviewSource::revision);
    guard.map_or(search_index::FULL_READ_SOURCE_BYTES, |guard| {
        search_index::projection_read_budget(guard, key.updated_at, revision.unwrap_or_default())
    })
}

fn cached_desktop_observations(
    provider: Provider,
    home: Option<&Path>,
    summaries_unavailable: bool,
    sessions: &mut [NativeSession],
    missing_summary_ids: &[String],
    unverified_ids: &[String],
) -> omnis_store::Result<Vec<NativeSession>> {
    let Some(home) = home else {
        return Ok(Vec::new());
    };
    if !summaries_unavailable && missing_summary_ids.is_empty() && unverified_ids.is_empty() {
        return Ok(Vec::new());
    }
    let store = Store::open_default()?;
    if summaries_unavailable || !missing_summary_ids.is_empty() {
        crate::fill_cached_desktop_summaries(
            &store,
            home,
            sessions,
            summaries_unavailable,
            missing_summary_ids,
        )?;
    }
    if provider != Provider::AntigravityDesktop || unverified_ids.is_empty() {
        return Ok(Vec::new());
    }
    let unresolved: HashSet<_> = unverified_ids.iter().collect();
    let version = omnis_core::search_document_version(Provider::AntigravityDesktop);
    Ok(store
        .published_desktop_sessions(version, home)?
        .into_iter()
        .flatten()
        .filter(|row| unresolved.contains(&row.session.id))
        .map(native_session)
        .collect())
}

fn read_picker_preview(
    registry: &AdapterRegistry,
    store: Option<&Store>,
    key: &PreviewKey,
    source_path: Option<&Path>,
    writer: Option<(&Store, TrajectorySourceGuard)>,
    source_changed: impl FnOnce(),
) -> anyhow::Result<(IndexRead, Option<SearchSource>)> {
    let session = &key.session;
    if session.provider == Provider::Imported {
        let read = search_index::read_imported(store, session)?;
        return Ok((read, None));
    }
    let full_read = key.continuation_after.is_some();
    if session.provider == Provider::Codex {
        return read_codex_picker_preview(registry, key, source_path, writer, source_changed)
            .map(|read| (read, None));
    }
    let (snapshot, source) = if has_scoped_home(session.provider) {
        let home = key
            .native_home
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("native preview home is unavailable"))?;
        anyhow::ensure!(
            registry.discovery_cache_home(session.provider)?.as_deref() == Some(home),
            "native preview home changed"
        );
        let result = search_index::observe_native_read(
            registry,
            store,
            session,
            None,
            key.source_revision,
            |adapter, path| adapter.read_for_search(session, path, full_read),
        )?;
        let observation = result
            .1
            .as_ref()
            .and_then(|source| source.observation.as_ref());
        anyhow::ensure!(
            observation.is_some_and(|source| source.home == home),
            "native preview source changed"
        );
        result
    } else {
        read_native_search_snapshot(registry, store, session, full_read)?
    };
    let source_complete = (full_read
        || source
            .as_ref()
            .is_some_and(|source| source.origin == SessionTrajectoryOrigin::NativeSummary))
        && (!omnis_adapters::AntigravityAdapter::has_missing_database_steps(&snapshot)
            && !omnis_adapters::AntigravityIdeAdapter::has_unreadable_log_records(&snapshot));
    Ok((
        IndexRead {
            native_session: None,
            source_revision: source
                .as_ref()
                .map(SearchSource::revision)
                .unwrap_or_default(),
            workspace_projection: omnis_adapters::CodexWorkspaceProjection::Unchecked,
            rollback_title: omnis_adapters::RollbackTitle::Unknown,
            provider_limited: omnis_adapters::AntigravityAdapter::has_omitted_stored_steps(
                &snapshot,
            ),
            snapshot,
            source_complete,
        },
        source,
    ))
}

pub(super) fn spawn_preview_updates(
    sender: Sender<PickerUpdate>,
    receiver: Receiver<Vec<PreviewRequest>>,
    refresh_codex: SyncSender<()>,
) {
    thread::spawn(move || {
        let registry = AdapterRegistry::with_local_adapters();
        let mut store_warning_sent = false;
        let mut open_store = || match Store::open_default() {
            Ok(store) => Some(store),
            Err(error) => {
                if !store_warning_sent {
                    let _ = sender.send(PickerUpdate::Warning(format!(
                        "trajectory index unavailable: {error}"
                    )));
                    store_warning_sent = true;
                }
                None
            }
        };
        let mut store = open_store();
        while let Ok(keys) = receiver.recv() {
            if store.is_none() {
                store = open_store();
            }
            let mut queue = VecDeque::from(keys);
            while let Some(request) = queue.pop_front() {
                for newer in receiver.try_iter() {
                    queue = VecDeque::from(newer);
                }
                let update =
                    preview_update(&registry, store.as_ref(), request, &sender, &refresh_codex);
                if sender.send(update).is_err() {
                    return;
                }
            }
        }
    });
}

struct PreviewWriter<'a> {
    store: &'a Store,
    guard: TrajectorySourceGuard,
    selected: Option<SelectedSessionObservation>,
}

impl<'a> PreviewWriter<'a> {
    fn observe(store: &'a Store, key: &PreviewKey) -> omnis_store::Result<Self> {
        let selected = if key.session.provider == Provider::Codex {
            Some(store.observe_selected_session(&key.session)?)
        } else {
            None
        };
        let guard = match &selected {
            Some(prior) => TrajectorySourceGuard::Observed(prior.state),
            None => search_index::observe_source(store, &key.session)?,
        };
        Ok(Self {
            store,
            guard,
            selected,
        })
    }

    fn matching_selection(
        &self,
        read: &omnis_adapters::IndexRead,
    ) -> Option<(&IndexedSession, &SelectedSessionObservation)> {
        let observation = self.selected.as_ref()?;
        let row = observation.metadata.as_ref()?;
        (read.source_revision.contents.is_some()
            && row
                .source_revision?
                .matches_file_contents(read.source_revision)
            && row.updated_at?.timestamp_millis() == read.snapshot.captured_at.timestamp_millis())
        .then_some((row, observation))
    }
}

fn record_bundle_preview(
    session: &SessionRef,
    writer: Option<&PreviewWriter<'_>>,
    snapshot: &omnis_ir::CanonicalSnapshot,
) -> anyhow::Result<PreviewMetadata> {
    let mut metadata = PreviewMetadata::default();
    if let Some(writer) = writer {
        metadata.indexed_title = crate::store_search_document(
            writer.store,
            session,
            crate::SearchDocumentInput::Bundle(snapshot),
            None,
            writer.guard,
        )?;
    }
    Ok(metadata)
}

fn record_preview(
    registry: &AdapterRegistry,
    session: &SessionRef,
    writer: Option<&PreviewWriter<'_>>,
    previous_name: Option<String>,
    read: &mut omnis_adapters::IndexRead,
) -> anyhow::Result<PreviewMetadata> {
    if session.provider == Provider::Imported {
        return record_bundle_preview(session, writer, &read.snapshot);
    }
    let revision = read.source_revision;
    let native = read.native_session.as_ref();
    let name_observation = search_index::native_codex_name(registry, session)?;
    let name_observed = search_index::verified_name_coverage(name_observation.as_ref(), native)?;
    let known_name = native
        .filter(|native| {
            session.provider == Provider::Codex
                && revision.contents.is_some()
                && native
                    .source_revision
                    .is_some_and(|source| source.matches_file_contents(revision))
                && name_observed
        })
        .map(|native| &native.title);
    let mut metadata = PreviewMetadata {
        read_revision: revision.file.map(|_| preview_revision(revision)),
        workspace: native.and_then(|_| {
            crate::codex_workspace_update(
                &read.snapshot,
                revision,
                read.source_complete,
                &read.workspace_projection,
            )
        }),
        ..PreviewMetadata::default()
    };
    let observed = writer.and_then(|writer| writer.selected.as_ref());
    let prior = writer.and_then(|writer| writer.matching_selection(read));
    let mut selection = None;
    if let Some(name) = known_name {
        let name = name.as_deref().map(omnis_core::redact_secrets);
        if let Some((row, observation)) = prior
            && (row.title.is_some() || name.is_some() || name != previous_name)
        {
            anyhow::ensure!(
                row.title == name || row.title == previous_name,
                "Codex session name changed before its preview"
            );
            let mut selected = row.clone();
            selected.title.clone_from(&name);
            selection = Some((selected, observation, None));
        }
        metadata.provider_name = Some(ProviderNameChange {
            previous: previous_name,
            current: name,
        });
    } else if session.provider == Provider::Codex {
        if let Some((row, observation)) = prior {
            if row.title.is_some() {
                selection = Some((row.clone(), observation, None));
            }
            read.snapshot.title.clone_from(&row.title);
        } else if observed.is_none() {
            read.snapshot.title = previous_name;
        }
    }
    if prior.is_none()
        && let Some((native, observation)) = native.zip(observed)
    {
        let (selected, history) = crate::selected_read_metadata(
            registry,
            native,
            revision,
            &mut read.snapshot,
            name_observation.as_ref(),
            Some(observation),
        );
        if (!name_observed && selected.title.is_some())
            || observation.legacy_codex_name_candidate(&selected)
        {
            selection = Some((selected, observation, history));
        }
    }
    if let Some(writer) = writer {
        let target = match selection.as_ref() {
            Some((selected, observation, history)) => {
                crate::SearchDocumentTarget::Selected(selected, observation, history.as_deref())
            }
            None => native.map_or(
                crate::SearchDocumentTarget::Session(session),
                crate::SearchDocumentTarget::VisibleNative,
            ),
        };
        metadata.indexed_title = crate::store_search_document(
            writer.store,
            target,
            crate::SearchDocumentInput::Read(read, name_observation.as_ref()),
            Some(registry),
            writer.guard,
        )?;
    }
    Ok(metadata)
}

fn record_observed_preview(
    key: &PreviewKey,
    writer: &PreviewWriter<'_>,
    read: &IndexRead,
    source: &SearchSource,
) -> anyhow::Result<PreviewMetadata> {
    crate::store_observed_search_document(
        writer.store,
        &key.session,
        &read.snapshot,
        read.source_complete,
        source,
        writer.guard,
        None,
    )?;
    let file = key.source_revision.and_then(|revision| revision.file);
    let title = match key.native_home.as_deref() {
        Some(home) => writer
            .store
            .trajectory_title_for_native_source(&key.session, home, file)?,
        None => writer.store.trajectory_title(&key.session)?,
    };
    Ok(PreviewMetadata {
        indexed_title: Some(IndexedTrajectoryTitle {
            session: key.session.clone(),
            title,
            source_revision: read.source_revision,
        }),
        ..PreviewMetadata::default()
    })
}

fn preview_update(
    registry: &AdapterRegistry,
    store: Option<&Store>,
    request: PreviewRequest,
    sender: &Sender<PickerUpdate>,
    refresh_codex: &SyncSender<()>,
) -> PickerUpdate {
    let PreviewRequest {
        key,
        source_path,
        cached,
        provider_name,
    } = request;
    let full_read = key.continuation_after.is_some();
    let writer = store.and_then(|store| match PreviewWriter::observe(store, &key) {
        Ok(writer) => Some(writer),
        Err(error) => {
            let _ = sender.send(PickerUpdate::Warning(format!(
                "trajectory index status: {error}"
            )));
            None
        }
    });
    let mut metadata = PreviewMetadata::default();
    let value = if key.session.provider == Provider::OpenCode && !full_read {
        PreviewValue::Unavailable
    } else {
        let mut changed = false;
        let read = read_picker_preview(
            registry,
            store,
            &key,
            source_path.as_deref(),
            writer.as_ref().map(|writer| (writer.store, writer.guard)),
            || {
                changed = true;
            },
        );
        if cached && key.session.provider == Provider::Codex && read.is_err() {
            refresh_failed_cached_preview(registry, &key, changed, sender, refresh_codex);
        }
        read.map_or(PreviewValue::Unavailable, |(mut read, source)| {
            let receipts = if matches!(key.session.provider, Provider::Codex | Provider::Imported) {
                record_preview(
                    registry,
                    &key.session,
                    writer.as_ref(),
                    provider_name,
                    &mut read,
                )
            } else if let (Some(writer), Some(source)) = (&writer, source) {
                record_observed_preview(&key, writer, &read, &source)
            } else {
                Ok(PreviewMetadata::default())
            };
            match receipts {
                Ok(receipts) => metadata = receipts,
                Err(error) => {
                    let _ = sender.send(PickerUpdate::Warning(format!(
                        "trajectory index write: {error}"
                    )));
                }
            }
            let snapshot = read.snapshot;
            let complete = read.source_complete;
            let coverage = omnis_core::session_message_coverage(&snapshot, complete);
            PreviewValue::Ready {
                continuation: key
                    .continuation_after
                    .and_then(|handoff_at| first_user_message_after(&snapshot, handoff_at)),
                preview: Box::new(session_preview(&snapshot)),
                complete,
                source_provider: snapshot.session.provider,
                can_use_first_title: snapshot.session.provider != Provider::Codex
                    || coverage != omnis_core::MessageCoverage::Unknown,
                message_count_is_complete: coverage == omnis_core::MessageCoverage::Complete,
            }
        })
    };
    PickerUpdate::Preview {
        key: Box::new(key),
        value,
        metadata: Box::new(metadata),
    }
}

fn refresh_failed_cached_preview(
    registry: &AdapterRegistry,
    key: &PreviewKey,
    source_changed: bool,
    sender: &Sender<PickerUpdate>,
    refresh: &SyncSender<()>,
) {
    let changed = if source_changed {
        true
    } else {
        match registry.find_native_session(Provider::Codex, &key.session.id) {
            // A complete discovery can distinguish deletion from a missing or unreadable home.
            Ok(None) => false,
            Ok(Some(selected))
                if key
                    .source
                    .as_ref()
                    .and_then(PreviewSource::revision)
                    .zip(selected.source_revision.map(preview_revision))
                    .is_some_and(|(expected, actual)| expected != actual) =>
            {
                true
            }
            _ => return,
        }
    };
    if changed {
        let _ = sender.send(PickerUpdate::StaleCachedPreview(Box::new(key.clone())));
    }
    let _ = refresh.try_send(());
}

pub(super) fn spawn_trajectory_search_updates(
    sender: Sender<PickerUpdate>,
    receiver: Receiver<TrajectorySearchRequest>,
) {
    thread::spawn(move || {
        let mut store = None;
        while let Ok(mut request) = receiver.recv() {
            for newer in receiver.try_iter() {
                request = newer;
            }
            if store.is_none() {
                store = Store::open_default().ok();
            }
            let result = store.as_ref().map_or_else(
                || Err("local trajectory index is unavailable".to_owned()),
                |store| {
                    store
                        .read_trajectory_search(|snapshot| {
                            let selections = crate::ScopedSearchSelections::new(
                                snapshot,
                                Some(&request.homes),
                                request.eligible_sessions,
                            )?;
                            selections.search(snapshot, &request.query, TRAJECTORY_SEARCH_LIMIT)
                        })
                        .map_err(|error| error.to_string())
                },
            );
            if sender
                .send(PickerUpdate::TrajectorySearch {
                    generation: request.generation,
                    result,
                })
                .is_err()
            {
                break;
            }
        }
    });
}

fn remember_native_titles(
    state: &mut PickerState,
    titles: Vec<(SessionRef, Option<Uuid>, Option<String>)>,
) {
    let titles = titles
        .into_iter()
        .filter_map(|(session, file, title)| {
            let index = *state.entry_positions.get(&session.to_string())?;
            let selected_file = state.entries[index].source_file();
            if selected_file == file {
                state.pending_native_titles.remove(&session);
                Some(IndexedTrajectoryTitle {
                    session,
                    title,
                    source_revision: SourceRevision {
                        file,
                        ..SourceRevision::default()
                    },
                })
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    state.remember_source_titles(titles);
}

pub(super) fn receive_updates(
    receiver: &Receiver<PickerUpdate>,
    state: &mut PickerState,
    pending: &mut HashSet<Provider>,
    warnings: &mut Vec<String>,
    _current_project: &Path,
) -> bool {
    let mut changed = false;
    while let Ok(update) = receiver.try_recv() {
        changed = true;
        match update {
            PickerUpdate::NativeHomes(homes) => state.native_homes = homes,
            PickerUpdate::NativeTitles(titles) => remember_native_titles(state, titles),
            PickerUpdate::Cached(Ok(cached)) => {
                state.cache_loaded = true;
                state.replace_all_entries(cached.entries);
                state.remember_source_titles(cached.titles);
                for warning in cached.warnings {
                    record_picker_warning(warnings, warning);
                }
            }
            PickerUpdate::Cached(Err(error)) => {
                state.cache_loaded = true;
                record_picker_warning(warnings, error);
            }
            PickerUpdate::StaleCachedPreview(key) => state.discard_stale_cached_preview(&key),
            PickerUpdate::Warning(error) => record_picker_warning(warnings, error),
            PickerUpdate::Lineage(Ok(records)) => state.replace_lineage(records),
            PickerUpdate::Lineage(Err(error)) => {
                record_picker_warning(warnings, format!("session lineage: {error}"));
            }
            PickerUpdate::Preview {
                key,
                value,
                metadata,
            } => {
                let mut key = *key;
                if metadata
                    .provider_name
                    .as_ref()
                    .is_none_or(|name| state.accepts_provider_name(&key, name))
                    && state.remember_preview_title(
                        &mut key,
                        &value,
                        metadata.read_revision,
                        metadata.indexed_title,
                    )
                {
                    if let Some(name) = metadata.provider_name {
                        state.remember_provider_name(&key, name);
                    }
                    if let Some(workspace) = metadata.workspace {
                        state.remember_codex_workspaces([(key.session.clone(), workspace)]);
                    }
                    state.previews.insert(key, value, &state.preview_window);
                    state.trajectory_index_changed();
                }
            }
            PickerUpdate::TrajectorySearch { generation, result } => {
                if generation == state.trajectory_search_generation {
                    state.trajectory_search_pending = false;
                    match result {
                        Ok(matches) => state.replace_trajectory_matches(matches),
                        Err(error) => {
                            record_picker_warning(warnings, format!("trajectory search: {error}"));
                        }
                    }
                }
            }
            PickerUpdate::Discovered(update) => {
                pending.remove(&update.provider);
                update.apply(state, warnings);
            }
            PickerUpdate::RefreshStarted(provider) => {
                pending.insert(provider);
            }
            PickerUpdate::AvailableUpdate(version) => {
                state.available_update = version;
            }
            PickerUpdate::Indexing(progress) => {
                state.index_progress = Some((progress.processed(), progress.total));
                if !progress.workspaces.is_empty() {
                    state.remember_codex_workspaces(progress.workspaces);
                }
                if !progress.titles.is_empty() {
                    state.remember_source_titles(progress.titles);
                }
                state.trajectory_index_changed();
            }
            PickerUpdate::IndexFailures(count) => record_index_failures(warnings, count),
        }
    }
    changed
}

pub(super) fn record_picker_warning(warnings: &mut Vec<String>, warning: String) {
    if warnings.len() < PICKER_WARNING_LIMIT && !warnings.contains(&warning) {
        warnings.push(warning);
    }
}

const INDEX_FAILURE_PREFIX: &str = "search index: ";
const INDEX_FAILURE_SUFFIX: &str = " could not be read";

/// Keeps one warning line with the latest count of sessions the search index could not read.
pub(super) fn record_index_failures(warnings: &mut Vec<String>, count: usize) {
    let existing = warnings.iter().position(|warning| {
        warning.starts_with(INDEX_FAILURE_PREFIX) && warning.ends_with(INDEX_FAILURE_SUFFIX)
    });
    let warning = (count > 0).then(|| {
        let noun = if count == 1 { "session" } else { "sessions" };
        format!("{INDEX_FAILURE_PREFIX}{count} {noun}{INDEX_FAILURE_SUFFIX}")
    });
    match (existing, warning) {
        (Some(index), Some(warning)) => warnings[index] = warning,
        (Some(index), None) => {
            warnings.remove(index);
        }
        (None, Some(warning)) => record_picker_warning(warnings, warning),
        (None, None) => {}
    }
}

pub(super) use crate::indexed_session_metadata as indexed_session;

pub(super) fn native_session(session: IndexedSession) -> NativeSession {
    NativeSession {
        session: session.session,
        title: session.title,
        project_path: session.project_path,
        git_branch: session.git_branch,
        created_at: session.created_at,
        updated_at: session.updated_at,
        updated_at_approximate: session.updated_at_approximate,
        event_count: session.event_count,
        source_path: None,
        source_revision: session.source_revision,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imported_preview_indexes_the_bundle_without_replacing_its_native_session()
    -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let folder = temporary.path().join("sessions");
        std::fs::create_dir(&folder)?;
        let id = uuid::Uuid::new_v4();
        let path = folder.join(format!("rollout-{id}.jsonl"));
        std::fs::write(
            &path,
            format!(
                "{}\n{}\n",
                serde_json::json!({"type":"session_meta","payload":{"id":id}}),
                serde_json::json!({"type":"event_msg","payload":{"type":"user_message","message":"bundle question"}}),
            ),
        )?;
        let mut registry = AdapterRegistry::new();
        registry.register(omnis_adapters::CodexAdapter::with_root(temporary.path()));
        let source = SessionRef::new(Provider::Codex, id.to_string());
        let mut read = registry.index_session(&source, Some(&path), u64::MAX)?;
        read.native_session = None;
        read.source_revision = SourceRevision::default();
        let store = Store::open(temporary.path().join("index.sqlite3"))?;
        store.upsert_session_trajectory(
            &source,
            "local response",
            read.snapshot.captured_at - chrono::Duration::days(1),
            true,
        )?;
        let original = store.trajectory_index_state(&source)?;
        let imported = crate::imported_session_ref(uuid::Uuid::new_v4());
        let writer = PreviewWriter {
            store: &store,
            guard: TrajectorySourceGuard::NewerSource,
            selected: None,
        };
        record_preview(&registry, &imported, Some(&writer), None, &mut read)?;

        assert_eq!(store.trajectory_index_state(&source)?, original);
        assert_eq!(
            store.search_session_trajectories("local response", 2)?,
            [source]
        );
        assert_eq!(
            store.search_session_trajectories("bundle question", 2)?,
            [imported]
        );
        Ok(())
    }
}
