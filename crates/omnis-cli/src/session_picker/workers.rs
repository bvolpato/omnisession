use super::{
    AdapterRegistry, CachedPickerEntries, Command, DateTime, HandoffRecord, HashSet,
    IndexedSession, LATEST_RELEASE_URL, NativeSession, PICKER_WARNING_LIMIT, PROVIDERS, Path,
    PathBuf, PickerEntry, PickerState, PreviewKey, PreviewRequest, PreviewSource, PreviewValue,
    Provider, Receiver, Result, SESSION_CACHE_TTL, Sender, SessionTrajectorySearchPage, Stdio,
    Store, SyncSender, TRAJECTORY_SEARCH_LIMIT, TrajectorySearchRequest, Utc, VecDeque,
    cached_picker_entries, env, first_user_message_after, mpsc, normalized_picker_entries,
    populate_approximate_updated_at, preview_revision, session_preview, thread,
};

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use anyhow::Context;
use omnis_adapters::ProviderAdapter;
use omnis_ir::SourceRevision;

use crate::{
    full_index_read,
    search_index::{self, IndexCandidate, IndexProgress},
};

pub(super) struct DiscoveryUpdate {
    pub(super) provider: Provider,
    pub(super) result: anyhow::Result<Vec<PickerEntry>>,
    pub(super) complete: bool,
}

pub(super) struct SessionCacheUpdate {
    pub(super) provider: Provider,
    pub(super) sessions: Vec<IndexedSession>,
    pub(super) complete: bool,
}

impl SessionCacheUpdate {
    pub(super) fn persist(&self, store: &Store) -> omnis_store::Result<()> {
        if self.complete {
            store.replace_indexed_sessions(self.provider, &self.sessions)
        } else {
            store.merge_indexed_sessions(self.provider, &self.sessions)
        }
    }
}

pub(super) enum PickerUpdate {
    Cached(Result<CachedPickerEntries, String>),
    StaleCachedPreview(PreviewKey),
    Lineage(Result<Vec<HandoffRecord>, String>),
    Preview {
        key: PreviewKey,
        value: PreviewValue,
        read_revision: Option<SourceRevision>,
        indexed_title: Option<Box<omnis_store::IndexedTrajectoryTitle>>,
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

pub(super) struct PickerWorkers {
    pub(super) receiver: Receiver<PickerUpdate>,
    pub(super) preview_sender: Sender<Vec<PreviewRequest>>,
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
    let (preview_sender, preview_receiver) = mpsc::channel::<Vec<PreviewRequest>>();
    let (trajectory_search_sender, trajectory_search_receiver) =
        mpsc::sync_channel::<TrajectorySearchRequest>(1);
    let (index_sender, index_receiver) = mpsc::channel::<SessionCacheUpdate>();
    let (refresh_codex, changed_codex) = mpsc::sync_channel(1);
    let (search_index_sender, search_index_receiver) = mpsc::channel::<Vec<IndexCandidate>>();
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
    changed_codex: Receiver<()>,
) {
    thread::spawn(move || {
        let Ok(store) = Store::open_default() else {
            let _ = sender.send(PickerUpdate::Cached(Err(
                "session index is unavailable".to_owned()
            )));
            spawn_all_provider_updates(&sender, &index_sender, &current_project);
            return;
        };
        let cached =
            cached_picker_entries(&store, &current_project).map_err(|error| error.to_string());
        let cache_loaded = cached.is_ok();
        if sender.send(PickerUpdate::Cached(cached)).is_err() {
            return;
        }
        let lineage = store.handoff_lineage().map_err(|error| error.to_string());
        if sender.send(PickerUpdate::Lineage(lineage)).is_err() {
            return;
        }
        let freshness = PROVIDERS.map(|provider| {
            (
                provider,
                store
                    .session_index_checked_at(provider)
                    .map_err(|error| error.to_string()),
            )
        });
        let mut refreshed_codex = false;
        for (provider, checked_at) in freshness {
            let refresh = match checked_at {
                Ok(Some(checked_at)) => !cache_loaded || !session_cache_is_fresh(checked_at),
                Ok(None) => true,
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
            if let Err(error) = store.mark_session_index_checked(provider)
                && sender
                    .send(PickerUpdate::Warning(format!(
                        "{provider} session index: {error}"
                    )))
                    .is_err()
            {
                return;
            }
            start_provider_update(&sender, &index_sender, &current_project, provider);
            refreshed_codex |= provider == Provider::Codex;
        }
        drop(store);
        // A cached row can change before the discovery cache expires. Refresh once if a
        // preview observes the change. Discovery supplies the new workspace and source stamp.
        if !refreshed_codex && changed_codex.recv().is_ok() {
            start_provider_update(&sender, &index_sender, &current_project, Provider::Codex);
        }
    });
}

pub(super) fn spawn_all_provider_updates(
    sender: &Sender<PickerUpdate>,
    index_sender: &Sender<SessionCacheUpdate>,
    current_project: &Path,
) {
    for provider in PROVIDERS {
        start_provider_update(sender, index_sender, current_project, provider);
    }
}

pub(super) fn start_provider_update(
    sender: &Sender<PickerUpdate>,
    index_sender: &Sender<SessionCacheUpdate>,
    current_project: &Path,
    provider: Provider,
) {
    if sender.send(PickerUpdate::RefreshStarted(provider)).is_ok() {
        spawn_provider_update(
            sender.clone(),
            index_sender.clone(),
            current_project.to_path_buf(),
            provider,
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
) {
    thread::spawn(move || {
        let registry = AdapterRegistry::with_local_adapters();
        let result = registry.list_sessions_with_notes(provider, None);
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
                        result: Ok(entries),
                        complete,
                    }))
                    .is_ok()
                    && index_sender
                        .send(SessionCacheUpdate {
                            provider,
                            sessions: indexed,
                            complete,
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
                    complete: false,
                }));
            }
        }
    });
}

pub(super) fn read_picker_preview(
    registry: &AdapterRegistry,
    key: &PreviewKey,
    source_path: Option<&Path>,
    projection_budget: u64,
    source_changed: impl FnOnce(),
) -> anyhow::Result<omnis_adapters::IndexRead> {
    let session = &key.session;
    if session.provider == Provider::Codex {
        let budget = if key.continuation_after.is_some() {
            u64::MAX
        } else {
            projection_budget
        };
        let read = registry.index_session(session, source_path, budget)?;
        let revision = preview_revision(read.source_revision);
        let changed = match &key.source {
            Some(PreviewSource::CodexFile(expected)) => *expected != revision,
            Some(PreviewSource::CachedCodex(expected)) => {
                let header = read
                    .native_session
                    .as_ref()
                    .context("cached Codex preview has no verified catalog metadata")?;
                let effective = &read.snapshot.workspace.current_dir;
                let workspace = if effective.as_os_str().is_empty() {
                    header.project_path.as_deref()
                } else {
                    Some(effective.as_path())
                };
                workspace.map(Path::as_os_str) != expected.as_deref().map(Path::as_os_str)
            }
            _ => false,
        };
        if changed {
            source_changed();
            anyhow::bail!("Codex rollout or workspace changed before its preview was read");
        }
        return Ok(read);
    }
    if session.provider == Provider::Imported || key.continuation_after.is_some() {
        return full_index_read(registry, session);
    }
    registry
        .preview_session(session)
        .map(|snapshot| omnis_adapters::IndexRead {
            native_session: None,
            snapshot,
            source_complete: false,
            source_revision: SourceRevision::default(),
        })
}

fn observe_preview_source(
    store: &Store,
    session: &omnis_ir::SessionRef,
) -> omnis_store::Result<(
    omnis_store::TrajectorySourceGuard,
    Option<omnis_store::SelectedSessionObservation>,
)> {
    if session.provider == Provider::Codex {
        let observed = store.observe_selected_session(session)?;
        Ok((
            search_index::source_guard(session.provider, observed.state.as_ref()),
            Some(observed),
        ))
    } else {
        Ok((search_index::observe_source(store, session)?, None))
    }
}

fn store_picker_preview(
    registry: &AdapterRegistry,
    store: &Store,
    session: &omnis_ir::SessionRef,
    read: &omnis_adapters::IndexRead,
    observed: Option<&omnis_store::SelectedSessionObservation>,
    guard: omnis_store::TrajectorySourceGuard,
) -> Result<Option<omnis_store::IndexedTrajectoryTitle>> {
    let selected = read.native_session.as_ref().map(|native| {
        crate::selected_read_metadata(native, &read.snapshot, read.source_revision, observed)
    });
    let target = selected.as_ref().zip(observed).map_or(
        crate::SearchDocumentTarget::Session(session),
        |(selected, observed)| crate::SearchDocumentTarget::Selected(selected, observed),
    );
    crate::store_search_document(
        store,
        target,
        &read.snapshot,
        read.source_complete,
        read.source_revision,
        (session.provider != Provider::Imported).then_some(registry),
        guard,
    )
}

fn preview_read_budget(key: &PreviewKey, guard: Option<omnis_store::TrajectorySourceGuard>) -> u64 {
    let revision = key.source.as_ref().and_then(PreviewSource::revision);
    guard.map_or(search_index::FULL_READ_SOURCE_BYTES, |guard| {
        search_index::projection_read_budget(guard, key.updated_at, revision.unwrap_or_default())
    })
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
                let update = picker_preview_update(
                    &registry,
                    store.as_ref(),
                    request,
                    &refresh_codex,
                    &sender,
                );
                if sender.send(update).is_err() {
                    return;
                }
            }
        }
    });
}

fn picker_preview_update(
    registry: &AdapterRegistry,
    store: Option<&Store>,
    request: PreviewRequest,
    refresh_codex: &SyncSender<()>,
    sender: &Sender<PickerUpdate>,
) -> PickerUpdate {
    let PreviewRequest {
        key,
        source_path,
        cached,
    } = request;
    let full_read = key.continuation_after.is_some();
    let writer = store.and_then(|store| match observe_preview_source(store, &key.session) {
        Ok((guard, observed)) => Some((store, guard, observed)),
        Err(error) => {
            let _ = sender.send(PickerUpdate::Warning(format!(
                "trajectory index status: {error}"
            )));
            None
        }
    });
    let mut read_revision = None;
    let mut indexed_title = None;
    let value = if key.session.provider == Provider::OpenCode && !full_read {
        PreviewValue::Unavailable
    } else {
        let budget = preview_read_budget(&key, writer.as_ref().map(|(_, guard, _)| *guard));
        let mut changed = false;
        let read = read_picker_preview(registry, &key, source_path.as_deref(), budget, || {
            changed = true;
        });
        if cached && key.session.provider == Provider::Codex && read.is_err() {
            refresh_failed_cached_preview(registry, &key, changed, sender, refresh_codex);
        }
        read.map_or(PreviewValue::Unavailable, |read| {
            if read.source_revision.file.is_some() {
                read_revision = Some(preview_revision(read.source_revision));
            }
            if let Some((store, guard, observed)) = writer {
                match store_picker_preview(
                    registry,
                    store,
                    &key.session,
                    &read,
                    observed.as_ref(),
                    guard,
                ) {
                    Ok(receipt) => indexed_title = receipt.map(Box::new),
                    Err(error) => {
                        let _ = sender.send(PickerUpdate::Warning(format!(
                            "trajectory index write: {error}"
                        )));
                    }
                }
            }
            let snapshot = read.snapshot;
            let complete = read.source_complete;
            PreviewValue::Ready {
                continuation: key
                    .continuation_after
                    .and_then(|handoff_at| first_user_message_after(&snapshot, handoff_at)),
                preview: Box::new(session_preview(&snapshot)),
                complete,
                can_use_first_title: key.session.provider != Provider::Codex
                    || omnis_core::session_message_prefix_is_known(&snapshot, complete),
            }
        })
    };
    PickerUpdate::Preview {
        key,
        value,
        read_revision,
        indexed_title,
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
        let _ = sender.send(PickerUpdate::StaleCachedPreview(key.clone()));
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
                mut key,
                value,
                read_revision,
                indexed_title,
            } => {
                if state.remember_preview_title(
                    &mut key,
                    &value,
                    read_revision,
                    indexed_title.map(|title| *title),
                ) {
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
                match update.result {
                    Ok(entries) => {
                        state.replace_provider_entries(update.provider, entries, update.complete);
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
            PickerUpdate::Indexing(progress) => {
                state.index_progress = Some((progress.indexed, progress.total));
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
