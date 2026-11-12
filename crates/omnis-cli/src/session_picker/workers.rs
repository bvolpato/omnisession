use super::{
    AdapterRegistry, CachedPickerEntries, Command, DateTime, HandoffRecord, HashSet,
    IndexedSession, LATEST_RELEASE_URL, NativeSession, PICKER_WARNING_LIMIT, PROVIDERS, Path,
    PathBuf, PickerEntry, PickerState, PreviewKey, PreviewRequest, PreviewSource, PreviewValue,
    Provider, Receiver, Result, SESSION_CACHE_TTL, Sender, SessionRef, SessionTrajectorySearchPage,
    Stdio, Store, SyncSender, TRAJECTORY_SEARCH_LIMIT, TrajectorySearchRequest, Utc, VecDeque,
    cached_picker_entries, env, first_user_message_after, mpsc, normalized_picker_entries,
    populate_approximate_updated_at, preview_revision, session_preview, thread,
};

use std::collections::HashMap;
use std::sync::{
    Arc, Mutex, PoisonError,
    atomic::{AtomicBool, Ordering},
};

use anyhow::Context;
use omnis_adapters::{GrokAdapter, ProviderAdapter};
use omnis_ir::SourceRevision;
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
    Cached(Result<CachedPickerEntries, String>),
    Lineage(Result<Vec<HandoffRecord>, String>),
    Preview {
        key: PreviewKey,
        value: PreviewValue,
        read_revision: Option<SourceRevision>,
        workspace: Option<Box<omnis_store::CodexWorkspaceUpdate>>,
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
    PreviewFailures(HashMap<SessionRef, omnis_store::TrajectoryIndexFailure>),
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

type PreviewFailures = Mutex<HashMap<SessionRef, omnis_store::TrajectoryIndexFailure>>;

pub(super) fn spawn_updates(current_project: &Path) -> PickerWorkers {
    let (sender, receiver) = mpsc::channel();
    let (preview_sender, preview_receiver) = mpsc::channel::<Vec<PreviewRequest>>();
    let (trajectory_search_sender, trajectory_search_receiver) =
        mpsc::sync_channel::<TrajectorySearchRequest>(1);
    let (index_sender, index_receiver) = mpsc::channel::<SessionCacheUpdate>();
    let (search_index_sender, search_index_receiver) = mpsc::channel::<Vec<IndexCandidate>>();
    let search_index_stop = Arc::new(AtomicBool::new(false));
    let preview_failures = Arc::new(PreviewFailures::default());

    // Grok index and preview reads use the catalog snapshot that supplied their candidates.
    let grok = GrokAdapter::default();
    let worker_registry = || {
        let mut registry = AdapterRegistry::with_local_adapters();
        registry.register(grok.clone());
        registry
    };
    spawn_index_writer(sender.clone(), index_receiver);
    spawn_cache_updates(
        sender.clone(),
        index_sender.clone(),
        current_project.to_path_buf(),
        Arc::new(worker_registry()),
    );
    spawn_preview_updates(
        sender.clone(),
        preview_receiver,
        Arc::clone(&preview_failures),
        Arc::new(worker_registry()),
        index_sender,
        current_project.to_path_buf(),
    );
    spawn_trajectory_search_updates(sender.clone(), trajectory_search_receiver);
    spawn_search_index(
        sender.clone(),
        search_index_receiver,
        Arc::clone(&search_index_stop),
        preview_failures,
        worker_registry(),
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
    preview_failures: Arc<PreviewFailures>,
    registry: AdapterRegistry,
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
            let mut reported_failures = 0;
            match search_index::index_candidates(
                &registry,
                &store,
                candidates,
                false,
                &should_stop,
                &mut |progress| {
                    if progress.failed > reported_failures {
                        reported_failures = progress.failed;
                        refresh_preview_failures(&store, &sender, &preview_failures, |_| true);
                    }
                    let _ = sender.send(PickerUpdate::Indexing(progress));
                },
            ) {
                Ok(summary) => {
                    refresh_preview_failures(&store, &sender, &preview_failures, |known| {
                        summary.failed > 0 || summary.failed_skipped > 0 || !known.is_empty()
                    });
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

fn refresh_preview_failures(
    store: &Store,
    sender: &Sender<PickerUpdate>,
    previous: &PreviewFailures,
    refresh: impl FnOnce(&HashMap<SessionRef, omnis_store::TrajectoryIndexFailure>) -> bool,
) {
    // Keep failure updates from the preview and index workers in query order.
    let mut previous = previous.lock().unwrap_or_else(PoisonError::into_inner);
    if !refresh(&previous) {
        return;
    }
    let Ok(mut failures) = store.trajectory_index_failures() else {
        return;
    };
    failures.retain(|session, failure| {
        (session.provider == Provider::Grok
            || TrajectorySourceGuard::uses_file_time(session.provider))
            && failure.source_file.is_some()
            && failure.source_contents.is_some()
    });
    if failures == *previous {
        return;
    }
    let restored_titles: Vec<_> = previous
        .keys()
        .filter(|session| !failures.contains_key(*session))
        .filter_map(|session| {
            store
                .trajectory_title_records(Some(session))
                .ok()?
                .remove_entry(session)
        })
        .collect();
    previous.clone_from(&failures);
    let _ = sender.send(PickerUpdate::PreviewFailures(failures));
    if !restored_titles.is_empty() {
        let _ = sender.send(PickerUpdate::Titles(restored_titles));
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
    registry: Arc<AdapterRegistry>,
) {
    thread::spawn(move || {
        let Ok(store) = Store::open_default() else {
            let _ = sender.send(PickerUpdate::Cached(Err(
                "session index is unavailable".to_owned()
            )));
            spawn_all_provider_updates(&sender, &index_sender, &current_project, &registry);
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
                &registry,
            );
        }
    });
}

pub(super) fn spawn_all_provider_updates(
    sender: &Sender<PickerUpdate>,
    index_sender: &Sender<SessionCacheUpdate>,
    current_project: &Path,
    registry: &Arc<AdapterRegistry>,
) {
    for provider in PROVIDERS {
        start_provider_update(
            sender,
            index_sender,
            current_project,
            provider,
            None,
            registry,
        );
    }
}

pub(super) fn start_provider_update(
    sender: &Sender<PickerUpdate>,
    index_sender: &Sender<SessionCacheUpdate>,
    current_project: &Path,
    provider: Provider,
    store: Option<&Store>,
    registry: &Arc<AdapterRegistry>,
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
            Arc::clone(registry),
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
    registry: Arc<AdapterRegistry>,
) {
    thread::spawn(move || {
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
        let unstable = !notes.is_empty();
        let complete = !unstable
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
                unstable,
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

pub(super) fn read_selected_preview(
    registry: &AdapterRegistry,
    key: &PreviewKey,
    source_path: Option<&Path>,
    projection_budget: u64,
) -> anyhow::Result<omnis_adapters::IndexRead> {
    let session = &key.session;
    let budget = if key.continuation_after.is_some() {
        u64::MAX
    } else {
        projection_budget
    };
    let read = registry.index_session(session, source_path, budget)?;
    let revision = preview_revision(read.source_revision);
    match &key.source {
        Some(PreviewSource::File(expected)) if *expected != revision => {
            anyhow::bail!(
                "selected {} source changed before its preview was read",
                session.provider
            );
        }
        Some(PreviewSource::Workspace(expected)) => {
            let header = read
                .native_session
                .as_ref()
                .context("cached preview has no verified catalog metadata")?;
            let effective = &read.snapshot.workspace.current_dir;
            let workspace = match &read.workspace_projection {
                omnis_adapters::CodexWorkspaceProjection::Observed(project) => project.as_deref(),
                omnis_adapters::CodexWorkspaceProjection::Unchecked
                    if !effective.as_os_str().is_empty() =>
                {
                    Some(effective.as_path())
                }
                omnis_adapters::CodexWorkspaceProjection::Unchecked
                    if session.provider == Provider::Codex && read.source_complete =>
                {
                    None
                }
                omnis_adapters::CodexWorkspaceProjection::Unchecked => {
                    header.project_path.as_deref()
                }
            };
            anyhow::ensure!(
                workspace.map(Path::as_os_str) == expected.as_deref().map(Path::as_os_str),
                "cached {} workspace changed before its preview was read",
                session.provider
            );
        }
        _ => {}
    }
    Ok(read)
}

fn preview_read_budget(key: &PreviewKey, guard: Option<omnis_store::TrajectorySourceGuard>) -> u64 {
    let revision = key.source.as_ref().and_then(PreviewSource::revision);
    let default_budget = if key.session.provider == Provider::Grok {
        0
    } else {
        search_index::FULL_READ_SOURCE_BYTES
    };
    guard.map_or(default_budget, |guard| {
        search_index::projection_read_budget(
            key.session.provider,
            guard,
            key.updated_at,
            revision.unwrap_or_default(),
            default_budget,
        )
    })
}

fn codex_preview_update(
    registry: &AdapterRegistry,
    store: Option<&Store>,
    sender: &Sender<PickerUpdate>,
    request: PreviewRequest,
) -> PickerUpdate {
    let PreviewRequest {
        key, source_path, ..
    } = request;
    let writer =
        store.and_then(
            |store| match crate::search_index::observe_source(store, &key.session) {
                Ok(guard) => Some((store, guard)),
                Err(error) => {
                    let _ = sender.send(PickerUpdate::Warning(format!(
                        "trajectory index status: {error}"
                    )));
                    None
                }
            },
        );
    let mut read_revision = None;
    let mut workspace = None;
    let read = read_selected_preview(
        registry,
        &key,
        source_path.as_deref(),
        preview_read_budget(&key, writer.map(|(_, guard)| guard)),
    );
    let read = read.and_then(|read| {
        let Some((store, guard)) = writer else {
            return Ok(read);
        };
        let read = search_index::verify_legacy_search_read(registry, store, guard, read)?;
        if let Some(PreviewSource::File(expected)) = key.source.as_ref() {
            anyhow::ensure!(
                *expected == preview_revision(read.source_revision),
                "Codex rollout changed during its preview"
            );
        }
        Ok(read)
    });
    let value = read.map_or(PreviewValue::Unavailable, |read| {
        if read.source_revision.file.is_some() {
            read_revision = Some(preview_revision(read.source_revision));
        }
        let snapshot = &read.snapshot;
        let complete = read.source_complete;
        if read.native_session.is_some() {
            workspace = crate::codex_workspace_update(
                snapshot,
                read.source_revision,
                complete,
                &read.workspace_projection,
            )
            .map(Box::new);
        }
        if let Some((store, guard)) = writer
            && let Err(error) = crate::store_search_document(
                store,
                read.native_session.as_ref().map_or(
                    crate::SearchDocumentTarget::Session(&key.session),
                    crate::SearchDocumentTarget::VisibleNative,
                ),
                crate::SearchDocumentInput::Read(&read),
                Some(registry),
                guard,
            )
        {
            workspace = None;
            let _ = sender.send(PickerUpdate::Warning(format!(
                "trajectory index write: {error}"
            )));
        }
        PreviewValue::from_snapshot(snapshot, key.continuation_after, complete)
    });
    PickerUpdate::Preview {
        key,
        value,
        read_revision,
        workspace,
    }
}

fn reports_incomplete_preview(event: &omnis_ir::OmniEvent) -> bool {
    if event.source.provider == Provider::Grok {
        return matches!(
            event.source.raw_record_type.as_deref(),
            Some(
                "omnisession.grok_missing_updates"
                    | "omnisession.grok_malformed_updates"
                    | "omnisession.record_size_limit"
                    | "omnisession.malformed_records"
                    | "omnisession.collection_limit"
            )
        ) && omnis_core::reports_omitted_events(event);
    }
    omnis_core::reports_omitted_events(event)
}

fn grok_preview_update(
    registry: &AdapterRegistry,
    store: Option<&Store>,
    sender: &Sender<PickerUpdate>,
    request: PreviewRequest,
    preview_failures: &PreviewFailures,
) -> PickerUpdate {
    let PreviewRequest {
        key, source_path, ..
    } = request;
    let writer = store.and_then(|store| match store.observe_selected_session(&key.session) {
        Ok(observed) => Some((store, observed)),
        Err(error) => {
            let _ = sender.send(PickerUpdate::Warning(format!(
                "trajectory index status: {error}"
            )));
            None
        }
    });
    let guard = writer
        .as_ref()
        .map(|(_, observed)| TrajectorySourceGuard::ObservedRevision(observed.state));
    let read = read_selected_preview(
        registry,
        &key,
        source_path.as_deref(),
        preview_read_budget(&key, guard),
    )
    .and_then(|read| {
        if let Some((store, observed)) = &writer {
            search_index::check_grok_preview(store, &read, observed.failure)?;
        }
        Ok(read)
    })
    .inspect_err(|error| {
        if let Some((store, observed)) = &writer {
            let _ =
                search_index::record_grok_rejection(registry, store, &key.session, observed, error);
        }
    });
    let mut read_revision = None;
    let value = read.map_or(PreviewValue::Unavailable, |read| {
        read_revision = Some(preview_revision(read.source_revision));
        if let Some((store, mut observed)) = writer
            && let Some(native) = &read.native_session
        {
            observed.ownership_revision = read.ownership_revision;
            let target = crate::indexed_session_metadata(native);
            if let Err(error) = crate::store_search_document(
                store,
                crate::SearchDocumentTarget::Selected(&target, &observed),
                crate::SearchDocumentInput::Read(&read),
                Some(registry),
                TrajectorySourceGuard::ObservedRevision(observed.state),
            ) {
                let stale = matches!(
                    error.downcast_ref::<StoreError>(),
                    Some(
                        StoreError::TrajectorySourceChanged | StoreError::TrajectoryPreviewRejected
                    )
                );
                let _ = sender.send(PickerUpdate::Warning(format!(
                    "trajectory index write: {error}"
                )));
                if stale {
                    return PreviewValue::Unavailable;
                }
            }
        }
        let complete =
            read.source_complete && !read.snapshot.events.iter().any(reports_incomplete_preview);
        PreviewValue::from_snapshot(&read.snapshot, key.continuation_after, complete)
    });
    if let Some(store) = store {
        refresh_preview_failures(store, sender, preview_failures, |known| {
            matches!(value, PreviewValue::Unavailable) || known.contains_key(&key.session)
        });
    }
    PickerUpdate::Preview {
        key,
        value,
        read_revision,
        workspace: None,
    }
}

pub(super) fn spawn_preview_updates(
    sender: Sender<PickerUpdate>,
    receiver: Receiver<Vec<PreviewRequest>>,
    preview_failures: Arc<PreviewFailures>,
    registry: Arc<AdapterRegistry>,
    index_sender: Sender<SessionCacheUpdate>,
    current_project: PathBuf,
) {
    thread::spawn(move || {
        let mut refreshed_grok_catalog = false;
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
                let cached_grok = request.cached && request.key.session.provider == Provider::Grok;
                let update = if request.key.session.provider == Provider::Grok {
                    grok_preview_update(
                        &registry,
                        store.as_ref(),
                        &sender,
                        request,
                        &preview_failures,
                    )
                } else if request.key.session.provider == Provider::Codex {
                    codex_preview_update(&registry, store.as_ref(), &sender, request)
                } else {
                    let key = request.key;
                    let (value, read_revision) = read_provider_preview(
                        &registry,
                        store.as_ref(),
                        &key,
                        &sender,
                        &preview_failures,
                    )
                    .unwrap_or((PreviewValue::Unavailable, None));
                    PickerUpdate::Preview {
                        key,
                        value,
                        read_revision,
                        workspace: None,
                    }
                };
                if cached_grok
                    && !refreshed_grok_catalog
                    && matches!(
                        update,
                        PickerUpdate::Preview {
                            value: PreviewValue::Unavailable,
                            ..
                        }
                    )
                {
                    refreshed_grok_catalog = true;
                    start_provider_update(
                        &sender,
                        &index_sender,
                        &current_project,
                        Provider::Grok,
                        store.as_ref(),
                        &registry,
                    );
                }
                if sender.send(update).is_err() {
                    return;
                }
            }
        }
    });
}

fn preview_write_guard(
    store: &Store,
    session: &SessionRef,
    sender: &Sender<PickerUpdate>,
) -> Option<TrajectorySourceGuard> {
    let observed = if TrajectorySourceGuard::uses_file_time(session.provider) {
        store.observe_verified_session(session)
    } else {
        search_index::observe_source(store, session)
    };
    observed
        .inspect_err(|error| {
            let _ = sender.send(PickerUpdate::Warning(format!(
                "trajectory index status: {error}"
            )));
        })
        .ok()
}

fn report_rejected_preview(
    registry: &AdapterRegistry,
    writer: Option<(&Store, TrajectorySourceGuard)>,
    error: &anyhow::Error,
    key: &PreviewKey,
    sender: &Sender<PickerUpdate>,
    failures: &PreviewFailures,
) -> omnis_store::Result<Option<omnis_ir::SourceRevision>> {
    if TrajectorySourceGuard::uses_file_time(key.session.provider)
        && search_index::retryable_read_failure(error)
    {
        return Err(StoreError::TrajectorySourceChanged);
    }
    let Some((store, guard)) = writer else {
        return Ok(None);
    };
    let Some(native) = search_index::record_strict_read_failure(
        &mut search_index::FileSelections::new(registry),
        store,
        guard,
        error,
    )
    .inspect_err(|error| {
        if !matches!(error, StoreError::TrajectorySourceChanged) {
            let _ = sender.send(PickerUpdate::Warning(format!(
                "trajectory index write: {error}"
            )));
        }
    })?
    else {
        return Ok(None);
    };
    refresh_preview_failures(store, sender, failures, |_| true);
    Ok(native
        .source_revision
        .filter(|&revision| preview_source_matches(key, Some(native), revision)))
}

fn read_provider_preview(
    registry: &AdapterRegistry,
    store: Option<&Store>,
    key: &PreviewKey,
    sender: &Sender<PickerUpdate>,
    preview_failures: &PreviewFailures,
) -> Option<(PreviewValue, Option<omnis_ir::SourceRevision>)> {
    let full_read = key.continuation_after.is_some();
    if key.session.provider == Provider::OpenCode && !full_read {
        return None;
    }
    let imported = key.session.provider == Provider::Imported;
    let file_backed = omnis_store::TrajectorySourceGuard::uses_file_time(key.session.provider);
    let mut retry = true;
    loop {
        let writer = store
            .and_then(|store| Some((store, preview_write_guard(store, &key.session, sender)?)));
        // If the store is unavailable, only a full read can validate this native conversation.
        let strict = full_read || (file_backed && writer.is_none());
        let (read, source_time) = match read_search_snapshot(registry, &key.session, strict) {
            Ok(read) => read,
            Err(error) => {
                match report_rejected_preview(
                    registry,
                    writer,
                    &error,
                    key,
                    sender,
                    preview_failures,
                ) {
                    Err(StoreError::TrajectorySourceChanged) if retry => {
                        retry = false;
                        continue;
                    }
                    result => {
                        return result
                            .ok()
                            .flatten()
                            .map(|revision| (PreviewValue::Unavailable, Some(revision)));
                    }
                }
            }
        };
        if file_backed && source_time.is_none() {
            if retry {
                retry = false;
                continue;
            }
            return None;
        }
        let revision = read.source_revision;
        if !preview_source_matches(key, read.native_session.as_ref(), revision) {
            return None;
        }
        let snapshot = read.snapshot;
        if let (Some((store, guard)), Some(source_time)) = (writer, source_time)
            && let Err(error) = crate::store_verified_search_document(
                store,
                &key.session,
                &snapshot,
                strict || imported,
                (source_time, revision),
                guard,
                read.native_session
                    .as_ref()
                    .map(|native| (registry, native)),
            )
        {
            if matches!(error, omnis_store::StoreError::TrajectoryPreviewRejected) {
                refresh_preview_failures(store, sender, preview_failures, |known| {
                    !known
                        .get(&key.session)
                        .is_some_and(|failed| failed.matches_file_revision(revision))
                });
                return Some((PreviewValue::Unavailable, Some(revision)));
            }
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
            if file_backed && !strict {
                return None;
            }
        }
        if let Some((store, _)) = writer
            && file_backed
        {
            refresh_preview_failures(store, sender, preview_failures, |known| {
                known.contains_key(&key.session)
            });
        }
        if key.session.provider == Provider::OhMyPi && source_time != key.updated_at {
            return None;
        }
        return Some((
            PreviewValue::from_snapshot(&snapshot, key.continuation_after, strict || imported),
            revision.file.map(|_| revision),
        ));
    }
}

impl PreviewValue {
    fn from_snapshot(
        snapshot: &omnis_ir::CanonicalSnapshot,
        continuation_after: Option<DateTime<Utc>>,
        complete: bool,
    ) -> Self {
        Self::Ready {
            continuation: continuation_after.and_then(|at| first_user_message_after(snapshot, at)),
            preview: Box::new(session_preview(snapshot)),
            complete,
        }
    }
}

fn preview_source_matches(
    key: &PreviewKey,
    native: Option<&NativeSession>,
    revision: omnis_ir::SourceRevision,
) -> bool {
    match &key.source {
        Some(PreviewSource::File(expected)) => expected.matches_file_contents(revision),
        Some(PreviewSource::Workspace(expected)) => {
            revision.file.is_some() && native.is_some_and(|native| native.project_path == *expected)
        }
        None | Some(PreviewSource::Path(_)) => true,
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
            PickerUpdate::Warning(error) => record_picker_warning(warnings, error),
            PickerUpdate::Lineage(Ok(records)) => state.replace_lineage(records),
            PickerUpdate::Lineage(Err(error)) => {
                record_picker_warning(warnings, format!("session lineage: {error}"));
            }
            PickerUpdate::Preview {
                key,
                value,
                read_revision,
                workspace,
            } => {
                let session = key.session.clone();
                if state.accept_preview(key, value, read_revision)
                    && let Some(workspace) = workspace
                {
                    state.remember_codex_workspaces([(session, *workspace)]);
                }
            }
            PickerUpdate::Titles(titles) => state.remember_derived_titles(titles),
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
            PickerUpdate::Indexing(progress) => {
                state.index_progress = Some((progress.processed(), progress.total));
                if !progress.workspaces.is_empty() {
                    state.remember_codex_workspaces(progress.workspaces);
                }
                state.remember_derived_titles(progress.source_titles);
                if !progress.titles.is_empty() {
                    state.remember_source_titles(
                        progress
                            .titles
                            .into_iter()
                            .filter(|row| row.session.provider == Provider::Codex),
                    );
                }
                state.trajectory_index_changed();
            }
            PickerUpdate::IndexFailures(count) => record_index_failures(warnings, count),
            PickerUpdate::PreviewFailures(failures) => state.replace_preview_failures(failures),
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
        source_revision: session.source_revision,
        event_count: session.event_count,
        source_path: None,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_handoff_preview_withdraws_a_sample_when_the_full_read_finds_a_missing_parent() -> Result<()>
    {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("session.jsonl");
        let header = json!({"type":"session","version":3,"id":"target","cwd":temp.path()});
        let user = json!({"type":"message","id":"user","parentId":null,
            "message":{"role":"user","content":"Check the synthetic session."}});
        let orphan = json!({"type":"message","id":"reply","parentId":"absent",
            "message":{"role":"assistant","content":[{"type":"text","text":"orphanedreplyneedle"}]}});
        std::fs::write(path, format!("{header}\n{user}\n{orphan}\n"))?;
        let mut registry = AdapterRegistry::new();
        registry.register(omnis_adapters::PiAdapter::with_root(temp.path()));
        let native = registry
            .find_native_session(Provider::Pi, "target")?
            .unwrap();
        let store = Store::open(temp.path().join("state.db"))?;
        let failures = PreviewFailures::default();
        let (sender, updates) = mpsc::channel();
        let mut key = PreviewKey {
            session: native.session.clone(),
            source: Some(PreviewSource::File(native.source_revision.unwrap())),
            updated_at: native.updated_at,
            continuation_after: None,
        };
        assert!(matches!(
            read_provider_preview(&registry, Some(&store), &key, &sender, &failures),
            Some((PreviewValue::Ready { .. }, _))
        ));
        assert_eq!(
            store
                .search_session_trajectories("orphanedreplyneedle", 1)?
                .as_slice(),
            std::slice::from_ref(&native.session)
        );
        key.continuation_after = Some(Utc::now());
        assert!(matches!(
            read_provider_preview(&registry, Some(&store), &key, &sender, &failures),
            Some((PreviewValue::Unavailable, revision)) if revision == native.source_revision
        ));
        assert!(
            store
                .search_session_trajectories("orphanedreplyneedle", 1)?
                .is_empty()
        );
        assert!(updates.try_iter().any(|update| matches!(update,
            PickerUpdate::PreviewFailures(current) if current[&native.session].matches_file_revision(native.source_revision.unwrap()))));
        Ok(())
    }
}

#[cfg(test)]
mod grok_preview_tests {
    use super::*;
    use omnis_ir::SessionRef;
    use omnis_store::{TrajectoryIndexState, TrajectorySourceGuard};

    #[test]
    fn picker_receives_grok_rejections_and_their_removal() -> Result<()> {
        let root = tempfile::tempdir()?;
        let store = Store::open(root.path().join("store.sqlite3"))?;
        let session = SessionRef::new(Provider::Grok, "selected");
        let failure = omnis_store::TrajectoryIndexFailure {
            source_file: Some(uuid::Uuid::from_u128(1)),
            source_contents: Some(uuid::Uuid::from_u128(2)),
            source_updated_at: None,
            source_time_precise: true,
            document_version: omnis_core::search_document_version(Provider::Grok),
        };
        let observation = store.observe_selected_session(&session)?;
        store.record_selected_trajectory_index_failure(&session, &failure, &observation)?;
        let stored = store.trajectory_index_failure(&session)?.unwrap();
        let known = PreviewFailures::default();
        let (sender, updates) = mpsc::channel();
        refresh_preview_failures(&store, &sender, &known, |_| true);
        assert!(
            matches!(updates.try_recv()?, PickerUpdate::PreviewFailures(current) if current.get(&session) == Some(&stored))
        );
        store.clear_observed_trajectory_index_failure(&session, &stored)?;
        refresh_preview_failures(&store, &sender, &known, |_| true);
        assert!(
            matches!(updates.try_recv()?, PickerUpdate::PreviewFailures(current) if current.is_empty())
        );
        Ok(())
    }

    #[test]
    fn picker_refreshes_a_complete_grok_document_after_its_plan_changes() -> Result<()> {
        let root = tempfile::tempdir()?;
        let id = "11111111-2222-4333-8444-555555555555";
        let directory = root.path().join("sessions").join(id);
        std::fs::create_dir_all(&directory)?;
        std::fs::write(
            directory.join("summary.json"),
            serde_json::json!({"id":id}).to_string(),
        )?;
        std::fs::write(directory.join("updates.jsonl"), serde_json::json!({"params":{"sessionId":id,"update":{"sessionUpdate":"session_info_update"}}}).to_string())?;
        let adapter = GrokAdapter::with_root(root.path());
        let session = SessionRef::new(Provider::Grok, id);
        let prior = adapter.index_session(&session, None, u64::MAX)?;
        std::fs::write(directory.join("plan.json"), "[]")?;
        let selected = adapter.find_native_session(id)?.unwrap();
        let mut registry = AdapterRegistry::new();
        registry.register(adapter);
        let key = PreviewKey {
            session,
            source: selected.source_revision.map(PreviewSource::File),
            updated_at: None,
            continuation_after: None,
        };
        let guard = TrajectorySourceGuard::ObservedRevision(Some(TrajectoryIndexState {
            source_complete: true,
            source_revision: prior.source_revision,
            legacy_tail_receipt: None,
            source_updated_at: prior.snapshot.captured_at,
            document_version: omnis_core::search_document_version(Provider::Grok),
            source_time_precise: true,
            title_authoritative: None,
            provider_limited: false,
        }));
        let read = read_selected_preview(
            &registry,
            &key,
            selected.source_path.as_deref(),
            preview_read_budget(&key, Some(guard)),
        )?;
        assert!(read.source_complete);
        Ok(())
    }
}
