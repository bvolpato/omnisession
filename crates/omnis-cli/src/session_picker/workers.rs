use super::{
    AdapterRegistry, Command, DateTime, HandoffRecord, HashSet, IndexedSession, LATEST_RELEASE_URL,
    NativeSession, PICKER_WARNING_LIMIT, PROVIDERS, Path, PathBuf, PickerEntry, PickerState,
    PreviewIndexTitle, PreviewKey, PreviewValue, Provider, Receiver, Result, SESSION_CACHE_TTL,
    Sender, SessionRef, SessionTrajectorySearchPage, Stdio, Store, SyncSender,
    TRAJECTORY_SEARCH_LIMIT, TrajectorySearchRequest, Utc, VecDeque, cached_picker_entries, env,
    first_user_message_after, has_scoped_home, mpsc, normalized_picker_entries,
    populate_approximate_updated_at, session_preview, thread,
};

use std::collections::HashMap;
use uuid::Uuid;

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use omnis_adapters::{IndexRead, ProviderAdapter};
use omnis_store::{
    DiscoveryScope, SessionDiscoveryAttempt, SessionDiscoveryClaim, SessionTrajectoryOrigin,
};

use crate::{
    read_native_search_snapshot,
    search_index::{self, IndexCandidate, IndexProgress, SearchSource},
};

pub(super) struct DiscoveryUpdate {
    pub(super) provider: Provider,
    pub(super) result: anyhow::Result<(Vec<PickerEntry>, bool)>,
}

pub(super) enum SessionCacheUpdate {
    Listed {
        provider: Provider,
        sessions: Vec<IndexedSession>,
        scope: DiscoveryScope,
        attempt: Option<SessionDiscoveryAttempt>,
        publication: Option<Sender<Option<bool>>>,
    },
    Failed(SessionDiscoveryAttempt),
}

impl SessionCacheUpdate {
    pub(super) fn persist(&self, store: &Store) -> omnis_store::Result<()> {
        match self {
            Self::Listed {
                provider,
                sessions,
                scope,
                attempt,
                publication,
            } => {
                if let Some(attempt) = attempt {
                    let result =
                        store.publish_session_discovery(attempt, sessions, *scope, Utc::now());
                    if let Some(publication) = publication {
                        let _ = publication.send(result.as_ref().ok().copied());
                    }
                    result?;
                } else {
                    store.cache_discovered_sessions(*provider, sessions, *scope, None)?;
                }
            }
            Self::Failed(attempt) => {
                store.finish_session_discovery(attempt, Utc::now())?;
            }
        }
        Ok(())
    }
}

pub(super) enum PickerUpdate {
    Cached(Result<Vec<PickerEntry>, String>),
    NativeHomes(HashMap<Provider, PathBuf>),
    NativeTitles(Vec<(SessionRef, Option<Uuid>, Option<String>)>),
    Lineage(Result<Vec<HandoffRecord>, String>),
    Preview {
        key: Box<PreviewKey>,
        value: PreviewValue,
        indexed_title: PreviewIndexTitle,
    },
    TrajectorySearch {
        generation: u64,
        result: Result<SessionTrajectorySearchPage, String>,
    },
    Discovered(DiscoveryUpdate),
    RefreshStarted(Provider),
    AvailableUpdate(Option<String>),
    Warning(String),
    Titles(Vec<(SessionRef, String)>),
    Indexing(IndexProgress),
    /// Unreadable sessions among the latest indexing candidates.
    IndexFailures(usize),
}

pub(super) struct PickerIndexRequest {
    pub(super) candidates: Vec<IndexCandidate>,
    pub(super) homes: HashMap<Provider, PathBuf>,
    pub(super) pending_titles: HashMap<SessionRef, Uuid>,
}

pub(super) struct PickerWorkers {
    pub(super) receiver: Receiver<PickerUpdate>,
    pub(super) preview_sender: Sender<Vec<PreviewKey>>,
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
    let (preview_sender, preview_receiver) = mpsc::channel::<Vec<PreviewKey>>();
    let (trajectory_search_sender, trajectory_search_receiver) =
        mpsc::sync_channel::<TrajectorySearchRequest>(1);
    let (index_sender, index_receiver) = mpsc::channel::<SessionCacheUpdate>();
    let (search_index_sender, search_index_receiver) = mpsc::channel::<PickerIndexRequest>();
    let search_index_stop = Arc::new(AtomicBool::new(false));

    spawn_index_writer(sender.clone(), index_receiver);
    spawn_cache_updates(sender.clone(), index_sender, current_project.to_path_buf());
    spawn_preview_updates(sender.clone(), preview_receiver);
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
                        progress
                            .titles
                            .iter()
                            .filter_map(|(session, _)| Some((session, *selected.get(session)?))),
                    );
                    progress
                        .titles
                        .retain(|(session, _)| !has_scoped_home(session.provider));
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
        let conflicts = crate::conflicting_search_documents(snapshot, homes, selected.clone())?;
        selected
            .filter(|(session, _)| {
                homes.contains_key(&session.provider) && !conflicts.contains(*session)
            })
            .map(|(session, file)| Ok((session.clone(), file, snapshot.trajectory_title(session)?)))
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
) {
    thread::spawn(move || {
        let registry = AdapterRegistry::with_local_adapters();
        let Ok(store) = Store::open_default() else {
            discover_without_cache(&registry, &sender, &index_sender, &current_project);
            return;
        };
        // Observe the reader before loading the catalog so a concurrent publication cannot be missed.
        let mut selections: std::collections::HashMap<_, _> =
            [Provider::Antigravity, Provider::AntigravityDesktop]
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
        ) else {
            return;
        };
        if !send_cached_titles(&store, &sender, &homes) {
            return;
        }
        let lineage = store.handoff_lineage().map_err(|error| error.to_string());
        if sender.send(PickerUpdate::Lineage(lineage)).is_err() {
            return;
        }
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
            match decision {
                Ok(PickerDiscovery::Fresh) => {}
                Ok(PickerDiscovery::Wait(attempt)) => {
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
                Ok(PickerDiscovery::Due(attempt)) => {
                    start_provider_update(
                        &sender,
                        &index_sender,
                        &current_project,
                        provider,
                        attempt,
                        homes.get(&provider),
                    );
                }
                Err(error) => {
                    let _ = sender.send(PickerUpdate::Warning(format!(
                        "{provider} session index: {error}"
                    )));
                    start_provider_update(
                        &sender,
                        &index_sender,
                        &current_project,
                        provider,
                        None,
                        homes.get(&provider),
                    );
                }
            }
        }
    });
}

fn discover_without_cache(
    registry: &AdapterRegistry,
    sender: &Sender<PickerUpdate>,
    index_sender: &Sender<SessionCacheUpdate>,
    project: &Path,
) {
    let homes: HashMap<_, _> = [Provider::Antigravity, Provider::AntigravityDesktop]
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

fn send_cached_titles(
    store: &Store,
    sender: &Sender<PickerUpdate>,
    homes: &HashMap<Provider, PathBuf>,
) -> bool {
    let titles = store.read_trajectory_search(|snapshot| -> omnis_store::Result<_> {
        let mut titles = snapshot.trajectory_titles()?;
        let conflicts = crate::conflicting_search_documents(
            snapshot,
            homes,
            titles.keys().map(|session| (session, None)),
        )?;
        titles.retain(|session, _| {
            (!has_scoped_home(session.provider) || homes.contains_key(&session.provider))
                && !conflicts.contains(session)
        });
        Ok(titles.into_iter().collect())
    });
    match titles {
        Ok(titles) => sender.send(PickerUpdate::Titles(titles)).is_ok(),
        Err(error) => sender
            .send(PickerUpdate::Warning(format!("session titles: {error}")))
            .is_ok(),
    }
}

fn send_cached_picker_entries(
    store: &Store,
    sender: &Sender<PickerUpdate>,
    project: &Path,
    antigravity: Option<&Path>,
    desktop: Option<&Path>,
) -> Option<bool> {
    let entries = match cached_picker_entries(store, project, antigravity, desktop) {
        Ok((entries, warnings)) => {
            for warning in warnings {
                sender.send(PickerUpdate::Warning(warning)).ok()?;
            }
            Ok(entries)
        }
        Err(error) => Err(error.to_string()),
    };
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
        let result = registry.list_sessions_in_home(provider, None, home.as_deref());
        match result {
            Ok(omnis_adapters::NativeListing {
                mut sessions,
                notes,
                desktop_summaries_unavailable: unavailable,
            }) => {
                let complete = notes.is_empty()
                    && registry
                        .adapter(provider)
                        .is_ok_and(ProviderAdapter::discovery_is_complete);
                for note in notes {
                    if sender.send(PickerUpdate::Warning(note)).is_err() {
                        return;
                    }
                }
                populate_approximate_updated_at(&mut sessions);
                let indexed = sessions.iter().map(indexed_session).collect::<Vec<_>>();
                if attempt.is_some() || home.is_none() {
                    let (reply, waiting) = attempt.as_ref().map(|_| mpsc::channel()).unzip();
                    let update = SessionCacheUpdate::Listed {
                        provider,
                        sessions: indexed,
                        scope: if unavailable {
                            DiscoveryScope::DesktopDatabasesWithoutSummaries
                        } else if complete {
                            DiscoveryScope::CompleteProvider
                        } else {
                            DiscoveryScope::PartialProvider
                        },
                        attempt: attempt.clone(),
                        publication: reply,
                    };
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
                if unavailable
                    && let Some(home) = home.as_deref()
                    && let Err(error) = Store::open_default().and_then(|store| {
                        crate::fill_cached_desktop_summaries(&store, home, &mut sessions)
                    })
                {
                    let _ = sender.send(PickerUpdate::Warning(format!(
                        "{provider} cached summaries: {error}"
                    )));
                }
                let entries = normalized_picker_entries(sessions, &current_project, false);
                let _ = sender.send(PickerUpdate::Discovered(DiscoveryUpdate {
                    provider,
                    result: Ok((entries, complete)),
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
                }));
            }
        }
    });
}

fn read_picker_preview(
    registry: &AdapterRegistry,
    store: Option<&Store>,
    key: &PreviewKey,
) -> anyhow::Result<(IndexRead, Option<SearchSource>)> {
    let session = &key.session;
    if session.provider == Provider::Imported {
        let read = search_index::read_imported(store, session)?;
        let source = SearchSource::imported(read.snapshot.captured_at);
        return Ok((read, Some(source)));
    }
    let full_read = key.continuation_after.is_some();
    if !full_read && session.provider == Provider::Codex {
        let path = registry
            .find_native_session(session.provider, &session.id)?
            .and_then(|native| native.source_path)
            .ok_or_else(|| anyhow::anyhow!("Codex rollout was not found"))?;
        let read =
            registry.index_session(session, Some(&path), search_index::FULL_READ_SOURCE_BYTES)?;
        let source = SearchSource {
            updated_at: read.snapshot.captured_at,
            observation: None,
            origin: SessionTrajectoryOrigin::Native,
        };
        return Ok((read, Some(source)));
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
        read_native_search_snapshot(registry, session, full_read)?
    };
    Ok((
        IndexRead {
            snapshot,
            source_complete: full_read
                || source
                    .as_ref()
                    .is_some_and(|source| source.origin == SessionTrajectoryOrigin::NativeSummary),
            provider_limited: false,
        },
        source,
    ))
}

pub(super) fn spawn_preview_updates(
    sender: Sender<PickerUpdate>,
    receiver: Receiver<Vec<PreviewKey>>,
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
            while let Some(key) = queue.pop_front() {
                for newer in receiver.try_iter() {
                    queue = VecDeque::from(newer);
                }
                let full_read = key.continuation_after.is_some();
                let writer = store.as_ref().and_then(|store| {
                    match crate::search_index::observe_source(store, &key.session) {
                        Ok(guard) => Some((store, guard)),
                        Err(error) => {
                            let _ = sender.send(PickerUpdate::Warning(format!(
                                "trajectory index status: {error}"
                            )));
                            None
                        }
                    }
                });
                let mut indexed_title = PreviewIndexTitle::NotWritten;
                let value = if key.session.provider == Provider::OpenCode && !full_read {
                    PreviewValue::Unavailable
                } else {
                    let read = read_picker_preview(&registry, store.as_ref(), &key);
                    read.map_or(PreviewValue::Unavailable, |(read, source)| {
                        let IndexRead {
                            snapshot,
                            source_complete: complete,
                            ..
                        } = read;
                        if let (Some((store, guard)), Some(source)) = (writer, source) {
                            match crate::store_search_document(
                                store,
                                &key.session,
                                &snapshot,
                                complete,
                                &source,
                                guard,
                                None,
                            )
                            .and_then(|_| {
                                match key.native_home.as_deref() {
                                    Some(home) => store.trajectory_title_for_native_source(
                                        &key.session,
                                        home,
                                        key.source_revision.and_then(|revision| revision.file),
                                    ),
                                    None => store.trajectory_title(&key.session),
                                }
                            }) {
                                Ok(title) => indexed_title = PreviewIndexTitle::Written(title),
                                Err(error) => {
                                    let _ = sender.send(PickerUpdate::Warning(format!(
                                        "trajectory index write: {error}"
                                    )));
                                }
                            }
                        }
                        PreviewValue::Ready {
                            continuation: key.continuation_after.and_then(|handoff_at| {
                                first_user_message_after(&snapshot, handoff_at)
                            }),
                            preview: Box::new(session_preview(&snapshot)),
                            complete,
                        }
                    })
                };
                if sender
                    .send(PickerUpdate::Preview {
                        key: Box::new(key),
                        value,
                        indexed_title,
                    })
                    .is_err()
                {
                    return;
                }
            }
        }
    });
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
                            let conflicts = crate::conflicting_search_documents(
                                snapshot,
                                &request.homes,
                                request
                                    .eligible_sessions
                                    .iter()
                                    .map(|(session, file)| (session, *file)),
                            )?;
                            let eligible = request
                                .eligible_sessions
                                .into_iter()
                                .filter_map(|(session, _)| {
                                    ((!has_scoped_home(session.provider)
                                        || request.homes.contains_key(&session.provider))
                                        && !conflicts.contains(&session))
                                    .then_some(session)
                                })
                                .collect::<Vec<_>>();
                            snapshot.search_session_trajectory_page_for_sessions(
                                &request.query,
                                TRAJECTORY_SEARCH_LIMIT,
                                &eligible,
                            )
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
            PickerUpdate::NativeTitles(titles) => {
                let titles = titles
                    .into_iter()
                    .filter_map(|(session, file, title)| {
                        let index = *state.entry_positions.get(&session.to_string())?;
                        let selected_file = state.entries[index].source_file();
                        if selected_file == file {
                            state.pending_native_titles.remove(&session);
                            Some((session, title))
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>();
                state.remember_indexed_titles(titles);
            }
            PickerUpdate::Cached(Ok(entries)) => {
                state.cache_loaded = true;
                state.replace_all_entries(entries);
            }
            PickerUpdate::Cached(Err(error)) => {
                state.cache_loaded = true;
                record_picker_warning(warnings, error);
            }
            PickerUpdate::Warning(error) => record_picker_warning(warnings, error),
            PickerUpdate::Lineage(Ok(records)) => state.replace_lineage(records),
            PickerUpdate::Lineage(Err(error)) => {
                record_picker_warning(warnings, format!("session lineage: {error}"));
            }
            PickerUpdate::Preview {
                key,
                value,
                indexed_title,
            } => {
                let key = *key;
                if has_scoped_home(key.session.provider)
                    && let Some(i) = state.entry_positions.get(&key.session.to_string())
                    && state.preview_key(&state.entries[*i]) != key
                {
                    continue;
                }
                state.remember_preview_title(&key, &value, indexed_title);
                state.previews.insert(key, value, &state.preview_window);
                state.trajectory_index_changed();
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
                match update.result {
                    Ok((entries, complete)) => {
                        state.update_provider_entries(update.provider, entries, complete);
                    }
                    Err(error) => {
                        record_picker_warning(warnings, format!("{}: {error}", update.provider));
                    }
                }
            }
            PickerUpdate::RefreshStarted(provider) => {
                pending.insert(provider);
            }
            PickerUpdate::AvailableUpdate(version) => {
                state.available_update = version;
            }
            PickerUpdate::Titles(titles) => state.remember_indexed_titles(
                titles
                    .into_iter()
                    .map(|(session, title)| (session, Some(title))),
            ),
            PickerUpdate::Indexing(progress) => {
                state.index_progress = Some((progress.indexed, progress.total));
                if !progress.titles.is_empty() {
                    state.remember_indexed_titles(progress.titles);
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

pub(super) fn indexed_session(session: &NativeSession) -> IndexedSession {
    IndexedSession {
        session: session.session.clone(),
        title: session.title.clone(),
        project_path: session.project_path.clone(),
        git_branch: session.git_branch.clone(),
        created_at: session.created_at,
        updated_at: session.updated_at,
        updated_at_approximate: session.updated_at_approximate,
        event_count: session.event_count,
    }
}

pub(super) fn native_session(session: IndexedSession) -> NativeSession {
    NativeSession {
        source_revision: None,
        session: session.session,
        title: session.title,
        project_path: session.project_path,
        git_branch: session.git_branch,
        created_at: session.created_at,
        updated_at: session.updated_at,
        updated_at_approximate: session.updated_at_approximate,
        event_count: session.event_count,
        source_path: None,
    }
}
