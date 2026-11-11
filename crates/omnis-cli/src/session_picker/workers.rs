use super::{
    AdapterRegistry, Command, DateTime, HandoffRecord, HashSet, IndexedSession, LATEST_RELEASE_URL,
    NativeSession, PICKER_WARNING_LIMIT, PROVIDERS, Path, PathBuf, PickerEntry, PickerState,
    PreviewIndexTitle, PreviewKey, PreviewValue, Provider, Receiver, Result, SESSION_CACHE_TTL,
    Sender, SessionRef, SessionTrajectorySearchPage, Stdio, Store, SyncSender,
    TRAJECTORY_SEARCH_LIMIT, TrajectorySearchRequest, Utc, VecDeque, cached_picker_entries, env,
    first_user_message_after, mpsc, normalized_picker_entries, populate_approximate_updated_at,
    session_preview, thread,
};

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use omnis_adapters::{IndexRead, ProviderAdapter};
use omnis_store::{DiscoveryScope, SessionDiscoveryAttempt, SessionTrajectoryOrigin};

use crate::{
    read_native_search_snapshot,
    search_index::{self, IndexCandidate, IndexProgress, SearchSource},
};

pub(super) struct DiscoveryUpdate {
    pub(super) provider: Provider,
    pub(super) result: anyhow::Result<(Vec<PickerEntry>, bool)>,
}

pub(super) struct SessionCacheUpdate {
    pub(super) provider: Provider,
    pub(super) sessions: Vec<IndexedSession>,
    pub(super) complete: bool,
    pub(super) home: Option<PathBuf>,
}

impl SessionCacheUpdate {
    pub(super) fn persist(&self, store: &Store) -> omnis_store::Result<()> {
        let scope = if self.complete {
            DiscoveryScope::CompleteProvider
        } else {
            DiscoveryScope::PartialProvider
        };
        store.cache_discovered_sessions(self.provider, &self.sessions, scope, self.home.as_deref())
    }
}

pub(super) enum PickerUpdate {
    Cached(Result<Vec<PickerEntry>, String>),
    Lineage(Result<Vec<HandoffRecord>, String>),
    Preview {
        key: PreviewKey,
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

pub(super) struct PickerWorkers {
    pub(super) receiver: Receiver<PickerUpdate>,
    pub(super) preview_sender: Sender<Vec<PreviewKey>>,
    pub(super) trajectory_search_sender: SyncSender<TrajectorySearchRequest>,
    pub(super) search_index_sender: Sender<Vec<IndexCandidate>>,
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
    let (search_index_sender, search_index_receiver) = mpsc::channel::<Vec<IndexCandidate>>();
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
    receiver: Receiver<Vec<IndexCandidate>>,
    stop: Arc<AtomicBool>,
) {
    thread::spawn(move || {
        let registry = AdapterRegistry::with_local_adapters();
        let Ok(store) = Store::open_default() else {
            return;
        };
        let pending = std::cell::RefCell::new(None::<Vec<IndexCandidate>>);
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
            let candidates = receiver.try_iter().last().unwrap_or(candidates);
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
                &mut |progress| {
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
        let Ok(store) = Store::open_default() else {
            let _ = sender.send(PickerUpdate::Cached(Err(
                "session index is unavailable".to_owned()
            )));
            spawn_all_provider_updates(&sender, &index_sender, &current_project);
            return;
        };
        let registry = AdapterRegistry::with_local_adapters();
        let home = |provider| registry.discovery_cache_home(provider);
        let cached = match cached_picker_entries(
            &store,
            &current_project,
            home(Provider::Antigravity).as_deref(),
            home(Provider::AntigravityDesktop).as_deref(),
        ) {
            Ok((entries, warnings)) => {
                for warning in warnings {
                    if sender.send(PickerUpdate::Warning(warning)).is_err() {
                        return;
                    }
                }
                Ok(entries)
            }
            Err(error) => Err(error.to_string()),
        };
        let cache_loaded = cached.is_ok();
        if sender.send(PickerUpdate::Cached(cached)).is_err() {
            return;
        }
        if let Ok(titles) = store.trajectory_titles()
            && sender
                .send(PickerUpdate::Titles(titles.into_iter().collect()))
                .is_err()
        {
            return;
        }
        let lineage = store.handoff_lineage().map_err(|error| error.to_string());
        if sender.send(PickerUpdate::Lineage(lineage)).is_err() {
            return;
        }
        let (finished, completions) = mpsc::channel();
        for provider in PROVIDERS {
            let attempt = match begin_picker_discovery(&store, &registry, provider, cache_loaded) {
                Ok(PickerDiscovery::Fresh) => continue,
                Ok(PickerDiscovery::Due(attempt)) => attempt,
                Err(error) => {
                    if sender
                        .send(PickerUpdate::Warning(format!(
                            "{provider} session index: {error}"
                        )))
                        .is_err()
                    {
                        return;
                    }
                    None
                }
            };
            start_provider_update(
                &sender,
                &index_sender,
                &current_project,
                provider,
                attempt.map(|attempt| (finished.clone(), attempt)),
            );
        }
        drop(finished);
        for (attempt, finished_at) in completions {
            if let Err(error) = store.finish_session_discovery(&attempt, finished_at) {
                let _ = sender.send(PickerUpdate::Warning(format!(
                    "session discovery check: {error}"
                )));
            }
        }
    });
}

type DiscoveryCompletion = (
    Sender<(SessionDiscoveryAttempt, DateTime<Utc>)>,
    SessionDiscoveryAttempt,
);

enum PickerDiscovery {
    Fresh,
    Due(Option<SessionDiscoveryAttempt>),
}

fn begin_picker_discovery(
    store: &Store,
    registry: &AdapterRegistry,
    provider: Provider,
    cache_loaded: bool,
) -> omnis_store::Result<PickerDiscovery> {
    if matches!(
        provider,
        Provider::Antigravity | Provider::AntigravityDesktop
    ) && let Some(home) = registry.discovery_cache_home(provider)
    {
        let version = omnis_core::search_document_version(provider);
        let attempt = if cache_loaded {
            store.try_begin_session_discovery(
                provider,
                version,
                &home,
                Utc::now() - SESSION_CACHE_TTL,
            )?
        } else {
            Some(store.mark_session_discovery_attempted(provider, version, &home)?)
        };
        return Ok(match attempt {
            Some(attempt) => PickerDiscovery::Due(Some(attempt)),
            None => PickerDiscovery::Fresh,
        });
    }
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

pub(super) fn spawn_all_provider_updates(
    sender: &Sender<PickerUpdate>,
    index_sender: &Sender<SessionCacheUpdate>,
    current_project: &Path,
) {
    for provider in PROVIDERS {
        start_provider_update(sender, index_sender, current_project, provider, None);
    }
}

pub(super) fn start_provider_update(
    sender: &Sender<PickerUpdate>,
    index_sender: &Sender<SessionCacheUpdate>,
    current_project: &Path,
    provider: Provider,
    completion: Option<DiscoveryCompletion>,
) {
    if sender.send(PickerUpdate::RefreshStarted(provider)).is_ok() {
        spawn_provider_update(
            sender.clone(),
            index_sender.clone(),
            current_project.to_path_buf(),
            provider,
            completion,
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
    completion: Option<DiscoveryCompletion>,
) {
    thread::spawn(move || {
        let registry = AdapterRegistry::with_local_adapters();
        let home = completion
            .as_ref()
            .map(|(_, attempt)| attempt.home().to_owned())
            .or_else(|| registry.discovery_cache_home(provider));
        let result = registry.list_sessions_in_home(provider, None, home.as_deref());
        if let Some((finished, attempt)) = completion
            && registry.discovery_cache_home(provider).as_deref() == Some(attempt.home())
        {
            let _ = finished.send((attempt, Utc::now()));
        }
        match result {
            Ok((mut sessions, notes)) => {
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
                let entries = normalized_picker_entries(sessions, &current_project, false);
                if sender
                    .send(PickerUpdate::Discovered(DiscoveryUpdate {
                        provider,
                        result: Ok((entries, complete)),
                    }))
                    .is_ok()
                    && index_sender
                        .send(SessionCacheUpdate {
                            provider,
                            sessions: indexed,
                            complete,
                            home,
                        })
                        .is_err()
                {
                    let _ = sender.send(PickerUpdate::Warning(
                        "session index writer stopped".to_owned(),
                    ));
                }
            }
            Err(error) => {
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
    let (snapshot, source) = read_native_search_snapshot(registry, session, full_read)?;
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
                            .and_then(|_| store.trajectory_title(&key.session))
                            {
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
                        key,
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
                        .search_session_trajectory_page_for_sessions(
                            &request.query,
                            TRAJECTORY_SEARCH_LIMIT,
                            &request.eligible_sessions,
                        )
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
                if key.session.provider == Provider::Antigravity
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
