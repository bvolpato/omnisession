use super::{
    AdapterRegistry, Command, DateTime, HandoffRecord, HashSet, IndexedSession, LATEST_RELEASE_URL,
    NativeSession, PICKER_WARNING_LIMIT, PROVIDERS, Path, PathBuf, PickerEntry, PickerState,
    PreviewKey, PreviewValue, Provider, Receiver, Result, SESSION_CACHE_TTL, Sender, SessionRef,
    SessionTrajectoryOrigin, SessionTrajectorySearchPage, Stdio, Store, SyncSender,
    TRAJECTORY_SEARCH_LIMIT, TrajectorySearchRequest, Utc, VecDeque, cached_picker_entries, env,
    first_user_message_after, mpsc, normalized_picker_entries, populate_approximate_updated_at,
    preview_continuation_title, session_preview, thread, trajectory_search_document,
};

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use omnis_adapters::ProviderAdapter;

use crate::{
    read_session,
    search_index::{self, IndexCandidate, IndexProgress},
};

pub(super) struct DiscoveryUpdate {
    pub(super) provider: Provider,
    pub(super) result: anyhow::Result<(Vec<PickerEntry>, bool)>,
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

pub(super) enum PreviewTitle {
    FromPreview,
    Indexed(Option<String>),
}

pub(super) enum PickerUpdate {
    Cached(Result<Vec<PickerEntry>, String>),
    Lineage(Result<Vec<HandoffRecord>, String>),
    Preview {
        key: PreviewKey,
        value: PreviewValue,
        indexed_title: PreviewTitle,
    },
    TrajectorySearch {
        generation: u64,
        result: Result<SessionTrajectorySearchPage, String>,
    },
    Discovered(DiscoveryUpdate),
    RefreshStarted(Provider),
    AvailableUpdate(Option<String>),
    Warning(String),
    Titles(Vec<(SessionRef, Option<String>)>),
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

    let registry = Arc::new(AdapterRegistry::with_local_adapters());
    spawn_index_writer(sender.clone(), index_receiver);
    spawn_cache_updates(
        Arc::clone(&registry),
        sender.clone(),
        index_sender,
        current_project.to_path_buf(),
    );
    spawn_preview_updates(Arc::clone(&registry), sender.clone(), preview_receiver);
    spawn_trajectory_search_updates(sender.clone(), trajectory_search_receiver);
    spawn_search_index(
        registry,
        sender.clone(),
        search_index_receiver,
        Arc::clone(&search_index_stop),
        current_project.to_path_buf(),
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
    registry: Arc<AdapterRegistry>,
    sender: Sender<PickerUpdate>,
    receiver: Receiver<Vec<IndexCandidate>>,
    stop: Arc<AtomicBool>,
    current_project: PathBuf,
) {
    thread::spawn(move || {
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
            let has_codex = candidates
                .iter()
                .any(|candidate| candidate.session.provider == Provider::Codex);
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
                    if has_codex
                        && summary.indexed > 0
                        && !stop.load(Ordering::Relaxed)
                        && let Ok(mut sessions) = registry.list_sessions(Provider::Codex, None)
                    {
                        populate_approximate_updated_at(&mut sessions);
                        let entries = normalized_picker_entries(sessions, &current_project, false);
                        let _ = sender.send(PickerUpdate::Discovered(DiscoveryUpdate {
                            provider: Provider::Codex,
                            // Indexing refreshes observed workspaces. Only discovery can remove rows.
                            result: Ok((entries, false)),
                        }));
                    }
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

fn alias_cached_pi_titles(store: &Store, entries: &[PickerEntry]) {
    let rows = entries
        .iter()
        .filter(|entry| {
            matches!(
                entry.session.session.provider,
                Provider::Pi | Provider::OhMyPi
            )
        })
        .filter_map(|entry| {
            Some((
                entry.session.session.clone(),
                entry.session.project_path.clone()?,
            ))
        })
        .collect::<Vec<_>>();
    if let Ok(keys) = store.pi_copy_keys(&rows) {
        let aliases = rows
            .into_iter()
            .zip(keys)
            .filter_map(|((session, path), key)| {
                let alias = crate::pi_copy_selector(&session.id, &path);
                Some((session, key?, alias))
            })
            .collect::<Vec<_>>();
        let _ = store.alias_pi_documents(&aliases);
    }
}

pub(super) fn spawn_cache_updates(
    registry: Arc<AdapterRegistry>,
    sender: Sender<PickerUpdate>,
    index_sender: Sender<SessionCacheUpdate>,
    current_project: PathBuf,
) {
    thread::spawn(move || {
        let Ok(store) = Store::open_default() else {
            let _ = sender.send(PickerUpdate::Cached(Err(
                "session index is unavailable".to_owned()
            )));
            spawn_all_provider_updates(&registry, &sender, &index_sender, &current_project);
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
        if let Ok(entries) = &cached {
            alias_cached_pi_titles(&store, entries);
        }
        let cache_loaded = cached.is_ok();
        if sender.send(PickerUpdate::Cached(cached)).is_err() {
            return;
        }
        if let Ok(titles) = store.trajectory_titles()
            && sender
                .send(PickerUpdate::Titles(
                    titles
                        .into_iter()
                        .map(|(key, title)| (key, Some(title)))
                        .collect(),
                ))
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
                store
                    .session_index_checked_at(provider)
                    .map_err(|error| error.to_string()),
            )
        });
        for (provider, checked_at) in freshness {
            let refresh = match checked_at {
                Ok(Some(checked_at)) => {
                    !cache_loaded
                        || !session_cache_is_fresh(checked_at)
                        || matches!(provider, Provider::Pi | Provider::OhMyPi)
                }
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
            start_provider_update(
                &registry,
                &sender,
                &index_sender,
                &current_project,
                provider,
            );
        }
    });
}

pub(super) fn spawn_all_provider_updates(
    registry: &Arc<AdapterRegistry>,
    sender: &Sender<PickerUpdate>,
    index_sender: &Sender<SessionCacheUpdate>,
    current_project: &Path,
) {
    for provider in PROVIDERS {
        start_provider_update(registry, sender, index_sender, current_project, provider);
    }
}

pub(super) fn start_provider_update(
    registry: &Arc<AdapterRegistry>,
    sender: &Sender<PickerUpdate>,
    index_sender: &Sender<SessionCacheUpdate>,
    current_project: &Path,
    provider: Provider,
) {
    if sender.send(PickerUpdate::RefreshStarted(provider)).is_ok() {
        spawn_provider_update(
            Arc::clone(registry),
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
    registry: Arc<AdapterRegistry>,
    sender: Sender<PickerUpdate>,
    index_sender: Sender<SessionCacheUpdate>,
    current_project: PathBuf,
    provider: Provider,
) {
    thread::spawn(move || {
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
                if matches!(provider, Provider::Pi | Provider::OhMyPi)
                    && let Ok(adapter) = registry.adapter(provider)
                {
                    sessions.extend(adapter.additional_search_sessions());
                }
                populate_approximate_updated_at(&mut sessions);
                let indexed = sessions
                    .iter()
                    .map(crate::indexed_session_metadata)
                    .collect::<Vec<_>>();
                let entries = normalized_picker_entries(sessions, &current_project, false);
                if sender
                    .send(PickerUpdate::Discovered(DiscoveryUpdate {
                        provider,
                        result: Ok((
                            entries,
                            complete && !matches!(provider, Provider::Pi | Provider::OhMyPi),
                        )),
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
                }));
            }
        }
    });
}

struct PickerPreviewRead {
    read: omnis_adapters::IndexRead,
    revision: Option<omnis_adapters::TranscriptRevision>,
    guard: omnis_store::TrajectoryWriteGuard,
}

impl PickerPreviewRead {
    fn store(
        &self,
        registry: &AdapterRegistry,
        store: &Store,
        key: &PreviewKey,
    ) -> Result<PreviewTitle> {
        let snapshot = &self.read.snapshot;
        let origin = if key.session.provider == Provider::Imported {
            SessionTrajectoryOrigin::ImportedBundle
        } else {
            SessionTrajectoryOrigin::Native
        };
        let observed = self
            .revision
            .map(|r| r.modified)
            .or(key.updated_at)
            .unwrap_or(snapshot.captured_at);
        let guard = if matches!(key.session.provider, Provider::Pi | Provider::OhMyPi) {
            crate::select_pi_snapshot(store, snapshot, self.revision)?;
            store.trajectory_write_guard(&key.session)?
        } else {
            self.guard
        };
        let source = omnis_store::TrajectorySource {
            expected: guard,
            file_bytes: self.revision.map(|r| r.bytes),
            revision: self.revision.map(|r| r.key),
            provider_limited: self.read.provider_limited,
        };
        crate::store_search_document(
            (store, Some(registry)),
            &key.session,
            snapshot,
            &trajectory_search_document(snapshot),
            self.read.source_complete,
            origin,
            (observed, source),
        )?;
        if key.session.provider == Provider::Claude {
            Ok(PreviewTitle::Indexed(store.trajectory_title(&key.session)?))
        } else {
            Ok(PreviewTitle::FromPreview)
        }
    }
}

fn read_picker_preview(
    registry: &AdapterRegistry,
    key: &PreviewKey,
    store: Option<&Store>,
) -> Result<PickerPreviewRead> {
    let guard = store
        .and_then(|store| store.trajectory_write_guard(&key.session).ok())
        .unwrap_or_default();
    let hint = key.copy_path.as_deref();
    if key.copy_directory.is_some() && hint.is_none() {
        anyhow::bail!("waiting for native copy discovery");
    }
    let revision = registry.search_revision(&key.session, hint)?;
    let full = |snapshot| omnis_adapters::IndexRead {
        snapshot,
        source_complete: true,
        provider_limited: false,
    };
    let read = if key.session.provider == Provider::Imported {
        full(read_session(registry, &key.session)?)
    } else if key.continuation_after.is_some() {
        full(registry.read_session_at(&key.session, hint)?)
    } else if store.is_some_and(|store| {
        store
            .needs_full_projection_upgrade(
                &key.session,
                revision.map(|r| r.key),
                revision.map(|r| r.bytes),
                key.updated_at,
                omnis_core::SEARCH_DOCUMENT_VERSION,
                crate::search_index::FULL_READ_SOURCE_BYTES,
            )
            .unwrap_or(false)
    }) {
        registry.index_session(&key.session, hint, u64::MAX)?
    } else {
        omnis_adapters::IndexRead {
            snapshot: registry.preview_session_at(&key.session, hint)?,
            source_complete: false,
            provider_limited: false,
        }
    };
    anyhow::ensure!(
        revision == registry.search_revision(&key.session, hint)?,
        "selected transcript changed during indexing"
    );
    Ok(PickerPreviewRead {
        read,
        revision,
        guard,
    })
}

pub(super) fn spawn_preview_updates(
    registry: Arc<AdapterRegistry>,
    sender: Sender<PickerUpdate>,
    receiver: Receiver<Vec<PreviewKey>>,
) {
    thread::spawn(move || {
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
                let mut indexed_title = PreviewTitle::FromPreview;
                let value = if key.session.provider == Provider::OpenCode
                    && key.continuation_after.is_none()
                {
                    PreviewValue::Unavailable
                } else {
                    let read = read_picker_preview(&registry, &key, store.as_ref());
                    read.map_or(PreviewValue::Unavailable, |preview| {
                        if let Some(store) = &store {
                            match preview.store(&registry, store, &key) {
                                Ok(title) => indexed_title = title,
                                Err(error) => {
                                    let _ = sender.send(PickerUpdate::Warning(format!(
                                        "trajectory index write: {error}"
                                    )));
                                }
                            }
                        }
                        let snapshot = preview.read.snapshot;
                        let full_read = preview.read.source_complete;
                        PreviewValue::Ready {
                            continuation: key.continuation_after.and_then(|handoff_at| {
                                first_user_message_after(&snapshot, handoff_at)
                            }),
                            preview: Box::new(session_preview(&snapshot)),
                            complete: full_read,
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
                    let keys = store
                        .pi_copy_keys(&request.pi_rows)
                        .map_err(|error| error.to_string())?;
                    let aliases = request
                        .pi_rows
                        .iter()
                        .zip(keys)
                        .filter_map(|((session, path), key)| {
                            Some((
                                session.clone(),
                                key?,
                                crate::pi_copy_selector(&session.id, path),
                            ))
                        })
                        .collect::<Vec<_>>();
                    store
                        .alias_pi_documents(&aliases)
                        .map_err(|error| error.to_string())?;
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
                if key.revision == state.previews.revision(&key.session) {
                    state.remember_preview_workspace(&key, &value);
                    if let PreviewTitle::Indexed(title) = indexed_title {
                        let title = preview_continuation_title(&value)
                            .or(title)
                            .or_else(|| super::preview_title(&value));
                        state.remember_derived_titles(vec![(key.session.clone(), title)]);
                    } else {
                        state.remember_preview_title(&key, &value);
                    }
                    state.previews.insert(key, value, &state.preview_window);
                }
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
    }
}
