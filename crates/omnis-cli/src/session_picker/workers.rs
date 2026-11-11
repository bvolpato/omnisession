use super::{
    AdapterRegistry, Command, DateTime, HandoffRecord, HashSet, IndexedSession, LATEST_RELEASE_URL,
    NativeSession, PICKER_WARNING_LIMIT, PROVIDERS, Path, PathBuf, PickerEntry, PickerState,
    PreviewKey, PreviewSource, PreviewValue, Provider, Receiver, Result, SESSION_CACHE_TTL, Sender,
    SessionRef, SessionTrajectorySearchPage, Stdio, Store, SyncSender, TRAJECTORY_SEARCH_LIMIT,
    TrajectorySearchRequest, Utc, VecDeque, cached_picker_entries, env, first_user_message_after,
    mpsc, normalized_picker_entries, populate_approximate_updated_at, session_preview, thread,
};

use std::collections::HashMap;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use omnis_adapters::ProviderAdapter;
use omnis_store::{CatalogObservation, CatalogRetry, StoreError, TrajectorySourceGuard};

use crate::{
    DiscoveredCatalog, read_search_snapshot,
    search_index::{self, IndexCandidate, IndexProgress},
};

pub(super) struct DiscoveryUpdate {
    pub(super) provider: Provider,
    pub(super) result: anyhow::Result<Vec<PickerEntry>>,
}

pub(super) enum CatalogReply {
    Published,
    Partial {
        accepted: Vec<usize>,
        visible: Vec<IndexedSession>,
        retry_hidden: Vec<usize>,
    },
    Retry(CatalogRetry),
}

pub(super) struct SessionCacheUpdate {
    pub(super) catalog: DiscoveredCatalog,
    pub(super) retry: Option<CatalogRetry>,
    pub(super) reply: Option<SyncSender<omnis_store::Result<CatalogReply>>>,
}

impl SessionCacheUpdate {
    pub(super) fn persist(self, store: &Store) -> omnis_store::Result<()> {
        let Some(reply) = self.reply else {
            return self.catalog.persist(store);
        };
        let response = if let Some(retry) = self.retry {
            self.catalog
                .persist_retry(store, &retry)
                .and_then(|accepted| match accepted {
                    Some(accepted) => {
                        let visible = store.indexed_sessions_for_provider(self.catalog.provider)?;
                        let ids: HashSet<_> = visible.iter().map(|row| &row.session.id).collect();
                        let mut retry_hidden = Vec::new();
                        for &index in &accepted {
                            let row = &self.catalog.sessions[index];
                            if !ids.contains(&row.session.id)
                                && let Some(revision) = row.source_revision
                                && store.can_retry_hidden_session(
                                    &row.session,
                                    revision,
                                    omnis_core::search_document_version(row.session.provider),
                                )?
                            {
                                retry_hidden.push(index);
                            }
                        }
                        Ok(CatalogReply::Partial {
                            accepted,
                            visible,
                            retry_hidden,
                        })
                    }
                    None => Ok(CatalogReply::Published),
                })
        } else {
            match self.catalog.persist(store) {
                Err(StoreError::TrajectorySourceChanged) => {
                    self.catalog.observe_retry(store).map(CatalogReply::Retry)
                }
                other => other.map(|()| CatalogReply::Published),
            }
        };
        let _ = reply.send(response);
        Ok(())
    }
}

pub(super) enum PickerUpdate {
    Cached(Result<Vec<PickerEntry>, String>),
    Lineage(Result<Vec<HandoffRecord>, String>),
    Preview {
        key: PreviewKey,
        value: PreviewValue,
        read_revision: Option<omnis_ir::SourceRevision>,
    },
    TrajectorySearch {
        generation: u64,
        result: Result<SessionTrajectorySearchPage, String>,
    },
    Discovered(DiscoveryUpdate),
    RefreshStarted(Provider),
    AvailableUpdate(Option<String>),
    Warning(String),
    Titles(Vec<(SessionRef, omnis_store::IndexedTitle)>),
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
        let cached = match cached_picker_entries(&store, &current_project) {
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
        if let Ok(titles) = store.trajectory_title_records(None)
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
        let freshness = PROVIDERS.map(|provider| {
            (
                provider,
                picker_discovery_due(&store, provider, cache_loaded),
            )
        });
        for (provider, due) in freshness {
            let refresh = match due {
                Ok(due) => due,
                Err(error) => {
                    if sender
                        .send(PickerUpdate::Warning(format!(
                            "{provider} session index: {error}"
                        )))
                        .is_err()
                    {
                        return;
                    }
                    true
                }
            };
            if !refresh {
                continue;
            }
            if !TrajectorySourceGuard::uses_file_time(provider)
                && let Err(error) = store.mark_session_index_checked(provider)
                && sender
                    .send(PickerUpdate::Warning(format!(
                        "{provider} session index: {error}"
                    )))
                    .is_err()
            {
                return;
            }
            start_provider_update(
                &sender,
                &index_sender,
                &current_project,
                provider,
                Some(&store),
            );
        }
    });
}

fn picker_discovery_due(
    store: &Store,
    provider: Provider,
    cache_loaded: bool,
) -> omnis_store::Result<bool> {
    if !cache_loaded
        || !store
            .session_index_checked_at(provider)?
            .is_some_and(session_cache_is_fresh)
    {
        return Ok(true);
    }
    // Cached metadata has no source paths. Rediscover these sessions for the background reader.
    match provider {
        Provider::Antigravity | Provider::AntigravityDesktop => {
            store.has_outdated_search_index(provider, omnis_core::search_document_version(provider))
        }
        _ => Ok(false),
    }
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
    store: Option<&Store>,
) {
    if sender.send(PickerUpdate::RefreshStarted(provider)).is_ok() {
        let observation = store.and_then(|store| match store.observe_session_catalog(provider) {
            Ok(observation) => observation,
            Err(error) => {
                let _ = sender.send(PickerUpdate::Warning(format!(
                    "{provider} session cache: {error}"
                )));
                None
            }
        });
        spawn_provider_update(
            sender.clone(),
            index_sender.clone(),
            current_project.to_path_buf(),
            provider,
            observation,
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
    observation: Option<CatalogObservation>,
) {
    thread::spawn(move || {
        let registry = AdapterRegistry::with_local_adapters();
        let result = discover_for_picker(
            &registry,
            provider,
            &current_project,
            observation,
            &index_sender,
            &sender,
        );
        let _ = sender.send(PickerUpdate::Discovered(DiscoveryUpdate {
            provider,
            result,
        }));
    });
}

pub(super) fn discover_for_picker(
    registry: &AdapterRegistry,
    provider: Provider,
    current_project: &Path,
    mut observation: Option<CatalogObservation>,
    index_sender: &Sender<SessionCacheUpdate>,
    sender: &Sender<PickerUpdate>,
) -> Result<Vec<PickerEntry>> {
    let mut retry = None;
    for _ in 0..2 {
        let (mut sessions, notes) = crate::discover_search_sessions(registry, provider, None)?;
        let complete = notes.is_empty()
            && registry
                .adapter(provider)
                .is_ok_and(ProviderAdapter::discovery_is_complete);
        for note in notes {
            let _ = sender.send(PickerUpdate::Warning(note));
        }
        populate_approximate_updated_at(&mut sessions);
        if observation.is_some() || !TrajectorySourceGuard::uses_file_time(provider) {
            // A checked listing reaches the picker only after the writer accepts its files.
            let (reply, response) = observation.as_ref().map(|_| mpsc::sync_channel(1)).unzip();
            let catalog = DiscoveredCatalog::new(
                provider,
                observation.take(),
                &sessions,
                complete,
                indexed_session,
            );
            if index_sender
                .send(SessionCacheUpdate {
                    catalog,
                    retry: retry.take(),
                    reply,
                })
                .is_err()
            {
                let _ = sender.send(PickerUpdate::Warning(
                    "session index writer stopped".to_owned(),
                ));
            } else if let Some(response) = response {
                match response.recv()?? {
                    CatalogReply::Retry(current) => {
                        observation = Some(current.observation());
                        retry = Some(current);
                        continue;
                    }
                    CatalogReply::Partial {
                        accepted,
                        mut visible,
                        retry_hidden,
                    } => {
                        let note = crate::discovery_conflict_note(
                            provider,
                            sessions.len(),
                            accepted.len(),
                        );
                        let _ = sender.send(PickerUpdate::Warning(note));
                        if !retry_hidden.is_empty() {
                            let hidden: HashMap<_, _> = retry_hidden
                                .into_iter()
                                .map(|index| {
                                    let selected = &sessions[index];
                                    (&selected.session.id, selected)
                                })
                                .collect();
                            for current in registry.adapter(provider)?.list_search_sessions(None)? {
                                if let Some(prior) = hidden.get(&current.session.id)
                                    && prior.source_path == current.source_path
                                    && prior.source_revision == current.source_revision
                                {
                                    visible.push(indexed_session(&current));
                                }
                            }
                        }
                        DiscoveredCatalog::retain_accepted(&mut sessions, &accepted);
                        return Ok(reconcile_partial(sessions, visible, current_project));
                    }
                    CatalogReply::Published => {}
                }
            }
        }
        return Ok(normalized_picker_entries(sessions, current_project, false));
    }
    Err(StoreError::TrajectorySourceChanged.into())
}

fn reconcile_partial(
    accepted: Vec<NativeSession>,
    visible: Vec<IndexedSession>,
    project: &Path,
) -> Vec<PickerEntry> {
    let mut accepted: HashMap<_, _> = accepted
        .into_iter()
        .map(|session| (session.session.id.clone(), session))
        .collect();
    let mut matcher = super::SessionWorkspaceMatcher::new(project);
    visible
        .into_iter()
        .map(|row| {
            let mut session = native_session(row);
            if let Some(live) = accepted.remove(&session.session.id) {
                match session.source_revision {
                    // A legacy catalog keeps its old workspace until the complete document migrates.
                    None => session = live,
                    Some(revision)
                        if live
                            .source_revision
                            .is_some_and(|source| revision.matches_file_contents(source)) =>
                    {
                        session.source_path = live.source_path;
                    }
                    Some(_) => {}
                }
            }
            let cached = session.source_path.is_none();
            super::picker_entry(session, &mut matcher, cached)
        })
        .collect()
}

pub(super) fn spawn_preview_updates(
    sender: Sender<PickerUpdate>,
    receiver: Receiver<Vec<PreviewKey>>,
) {
    thread::spawn(move || {
        let registry = AdapterRegistry::with_local_adapters();
        let mut store_warning_sent = false;
        let mut store = match Store::open_default() {
            Ok(store) => Some(store),
            Err(error) => {
                let _ = sender.send(PickerUpdate::Warning(format!(
                    "trajectory index unavailable: {error}"
                )));
                store_warning_sent = true;
                None
            }
        };
        while let Ok(keys) = receiver.recv() {
            if store.is_none() {
                match Store::open_default() {
                    Ok(opened) => store = Some(opened),
                    Err(error) => {
                        if !store_warning_sent {
                            let _ = sender.send(PickerUpdate::Warning(format!(
                                "trajectory index unavailable: {error}"
                            )));
                            store_warning_sent = true;
                        }
                    }
                }
            }
            let mut queue = VecDeque::from(keys);
            while let Some(key) = queue.pop_front() {
                for newer in receiver.try_iter() {
                    queue = VecDeque::from(newer);
                }
                let (value, read_revision) =
                    read_picker_preview(&registry, store.as_ref(), &key, &sender)
                        .unwrap_or((PreviewValue::Unavailable, None));
                if sender
                    .send(PickerUpdate::Preview {
                        key,
                        value,
                        read_revision,
                    })
                    .is_err()
                {
                    return;
                }
            }
        }
    });
}

fn read_picker_preview(
    registry: &AdapterRegistry,
    store: Option<&Store>,
    key: &PreviewKey,
    sender: &Sender<PickerUpdate>,
) -> Option<(PreviewValue, Option<omnis_ir::SourceRevision>)> {
    let full_read = key.continuation_after.is_some();
    if key.session.provider == Provider::OpenCode && !full_read {
        return None;
    }
    let imported = key.session.provider == Provider::Imported;
    let file_backed = omnis_store::TrajectorySourceGuard::uses_file_time(key.session.provider);
    let mut retry = true;
    loop {
        let writer = store.and_then(|store| {
            let observed = if file_backed || key.session.provider == Provider::Antigravity {
                store.observe_verified_session(&key.session)
            } else {
                search_index::observe_source(store, &key.session)
            };
            match observed {
                Ok(guard) => Some((store, guard)),
                Err(error) => {
                    let _ = sender.send(PickerUpdate::Warning(format!(
                        "trajectory index status: {error}"
                    )));
                    None
                }
            }
        });
        let (read, source_time) = read_search_snapshot(registry, &key.session, full_read).ok()?;
        if file_backed && source_time.is_none() {
            if retry {
                retry = false;
                continue;
            }
            return None;
        }
        let revision = read.source_revision;
        match &key.source {
            Some(PreviewSource::File(expected)) if !expected.matches_file_contents(revision) => {
                return None;
            }
            Some(PreviewSource::Workspace(expected)) => {
                let native = read.native_session.as_ref()?;
                if native.project_path.as_deref() != expected.as_deref() || revision.file.is_none()
                {
                    return None;
                }
            }
            _ => {}
        }
        let snapshot = read.snapshot;
        if let (Some((store, guard)), Some(source_time)) = (writer, source_time)
            && let Err(error) = crate::store_search_document(
                store,
                &key.session,
                &snapshot,
                full_read || imported,
                (source_time, revision),
                guard,
                read.native_session.as_ref(),
            )
        {
            if matches!(error, omnis_store::StoreError::TrajectorySourceChanged) {
                if retry {
                    retry = false;
                    continue;
                }
                return None;
            }
            let _ = sender.send(PickerUpdate::Warning(format!(
                "trajectory index write: {error}"
            )));
        }
        if key.session.provider == Provider::OhMyPi && source_time != key.updated_at {
            return None;
        }
        return Some((
            PreviewValue::Ready {
                continuation: key
                    .continuation_after
                    .and_then(|handoff_at| first_user_message_after(&snapshot, handoff_at)),
                preview: Box::new(session_preview(&snapshot)),
                complete: full_read || imported,
            },
            revision.file.map(|_| revision),
        ));
    }
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
                        .search_selected_trajectory_page(
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
                read_revision,
            } => {
                state.accept_preview(key, value, read_revision);
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
                    Ok(entries) => state.replace_provider_entries(update.provider, entries),
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
            PickerUpdate::Titles(titles) => state.remember_derived_titles(titles),
            PickerUpdate::Indexing(progress) => {
                state.index_progress = Some((progress.indexed, progress.total));
                if !progress.titles.is_empty() {
                    state.remember_derived_titles(progress.titles);
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
        source_revision: session.source_revision,
        event_count: session.event_count,
    }
}

pub(super) fn native_session(session: IndexedSession) -> NativeSession {
    NativeSession {
        session: session.session,
        title: session.title,
        project_path: session.project_path,
        git_branch: session.git_branch,
        created_at: session.created_at,
        updated_at: session.updated_at,
        updated_at_approximate: session.updated_at_approximate,
        source_revision: session.source_revision,
        event_count: session.event_count,
        source_path: None,
    }
}
