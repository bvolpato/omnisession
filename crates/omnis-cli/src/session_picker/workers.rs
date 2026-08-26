use super::{
    AdapterRegistry, Command, DateTime, HandoffRecord, HashSet, IndexedSession, InvertedIndex,
    LATEST_RELEASE_URL, NativeSession, PICKER_WARNING_LIMIT, PROVIDERS, Path, PathBuf, PickerEntry,
    PickerState, PreviewKey, PreviewValue, Provider, Receiver, Result, SESSION_CACHE_TTL,
    SearchIndexRequest, Sender, SessionTrajectoryOrigin, SessionTrajectorySearchPage, Stdio, Store,
    SyncSender, TRAJECTORY_SEARCH_LIMIT, TrajectorySearchRequest, Utc, VecDeque, env,
    first_user_message_after, mpsc, normalized_picker_entries, picker_entries,
    populate_approximate_updated_at, session_preview, thread, trajectory_search_document,
};

pub(super) struct DiscoveryUpdate {
    pub(super) provider: Provider,
    pub(super) result: anyhow::Result<Vec<PickerEntry>>,
}

pub(super) enum PickerUpdate {
    Cached(Result<Vec<PickerEntry>, String>),
    Lineage(Result<Vec<HandoffRecord>, String>),
    Preview {
        key: PreviewKey,
        value: PreviewValue,
    },
    SearchIndex {
        generation: u64,
        index: InvertedIndex,
    },
    TrajectorySearch {
        generation: u64,
        result: Result<SessionTrajectorySearchPage, String>,
    },
    Discovered(DiscoveryUpdate),
    RefreshStarted(Provider),
    AvailableUpdate(Option<String>),
    Warning(String),
}

pub(super) struct PickerWorkers {
    pub(super) receiver: Receiver<PickerUpdate>,
    pub(super) preview_sender: Sender<Vec<PreviewKey>>,
    pub(super) search_sender: SyncSender<SearchIndexRequest>,
    pub(super) trajectory_search_sender: SyncSender<TrajectorySearchRequest>,
}

pub(super) fn spawn_updates(current_project: &Path) -> PickerWorkers {
    let (sender, receiver) = mpsc::channel();
    let (preview_sender, preview_receiver) = mpsc::channel::<Vec<PreviewKey>>();
    let (search_sender, search_receiver) = mpsc::sync_channel::<SearchIndexRequest>(1);
    let (trajectory_search_sender, trajectory_search_receiver) =
        mpsc::sync_channel::<TrajectorySearchRequest>(1);
    let (index_sender, index_receiver) = mpsc::channel::<(Provider, Vec<IndexedSession>)>();

    spawn_index_writer(sender.clone(), index_receiver);
    spawn_cache_updates(sender.clone(), index_sender, current_project.to_path_buf());
    spawn_preview_updates(sender.clone(), preview_receiver);
    spawn_search_updates(sender.clone(), search_receiver);
    spawn_trajectory_search_updates(sender.clone(), trajectory_search_receiver);
    spawn_update_check(sender);

    PickerWorkers {
        receiver,
        preview_sender,
        search_sender,
        trajectory_search_sender,
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
    index_sender: Sender<(Provider, Vec<IndexedSession>)>,
    current_project: PathBuf,
) {
    thread::spawn(move || {
        let Ok(store) = Store::open_default() else {
            let _ = sender.send(PickerUpdate::Warning(
                "session index is unavailable".to_owned(),
            ));
            spawn_all_provider_updates(&sender, &index_sender, &current_project);
            return;
        };
        let cached = store
            .indexed_sessions()
            .map(|sessions| {
                picker_entries(
                    sessions.into_iter().map(native_session).collect(),
                    &current_project,
                    true,
                )
            })
            .map_err(|error| error.to_string());
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
            if let Err(error) = store.mark_session_index_checked(provider) {
                if sender
                    .send(PickerUpdate::Warning(format!(
                        "{provider} session index: {error}"
                    )))
                    .is_err()
                {
                    return;
                }
            }
            start_provider_update(&sender, &index_sender, &current_project, provider);
        }
    });
}

pub(super) fn spawn_all_provider_updates(
    sender: &Sender<PickerUpdate>,
    index_sender: &Sender<(Provider, Vec<IndexedSession>)>,
    current_project: &Path,
) {
    for provider in PROVIDERS {
        start_provider_update(sender, index_sender, current_project, provider);
    }
}

pub(super) fn start_provider_update(
    sender: &Sender<PickerUpdate>,
    index_sender: &Sender<(Provider, Vec<IndexedSession>)>,
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
    receiver: Receiver<(Provider, Vec<IndexedSession>)>,
) {
    thread::spawn(move || {
        let Ok(store) = Store::open_default() else {
            let _ = sender.send(PickerUpdate::Warning(
                "session index writer is unavailable".to_owned(),
            ));
            return;
        };
        while let Ok((provider, indexed)) = receiver.recv() {
            if let Err(error) = store.replace_indexed_sessions(provider, &indexed) {
                let _ = sender.send(PickerUpdate::Warning(format!("session index: {error}")));
            }
        }
    });
}

pub(super) fn spawn_provider_update(
    sender: Sender<PickerUpdate>,
    index_sender: Sender<(Provider, Vec<IndexedSession>)>,
    current_project: PathBuf,
    provider: Provider,
) {
    thread::spawn(move || {
        let registry = AdapterRegistry::with_local_adapters();
        let result = registry.list_sessions(provider, None);
        match result {
            Ok(mut sessions) => {
                populate_approximate_updated_at(&mut sessions);
                let indexed = sessions.iter().map(indexed_session).collect::<Vec<_>>();
                let entries = normalized_picker_entries(sessions, &current_project, false);
                if sender
                    .send(PickerUpdate::Discovered(DiscoveryUpdate {
                        provider,
                        result: Ok(entries),
                    }))
                    .is_ok()
                    && index_sender.send((provider, indexed)).is_err()
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
                let full_read = key.continuation_after.is_some();
                let value = if key.session.provider == Provider::OpenCode && !full_read {
                    PreviewValue::Unavailable
                } else {
                    let snapshot = if full_read {
                        registry.read_session(&key.session)
                    } else {
                        registry.preview_session(&key.session)
                    };
                    snapshot.map_or(PreviewValue::Unavailable, |snapshot| {
                        if let Some(store) = &store {
                            let document = trajectory_search_document(&snapshot);
                            if let Err(error) = store.upsert_session_trajectory_document(
                                &snapshot.session,
                                &document.text,
                                snapshot.captured_at,
                                document.source_byte_count,
                                document.indexed_byte_count,
                                document.truncation_strategy.as_str(),
                                full_read && document.source_complete,
                                SessionTrajectoryOrigin::Native,
                            ) {
                                let _ = sender.send(PickerUpdate::Warning(format!(
                                    "trajectory index write: {error}"
                                )));
                            }
                        }
                        PreviewValue::Ready {
                            continuation: key.continuation_after.and_then(|handoff_at| {
                                first_user_message_after(&snapshot, handoff_at)
                            }),
                            preview: Box::new(session_preview(&snapshot)),
                            complete: full_read,
                        }
                    })
                };
                if sender.send(PickerUpdate::Preview { key, value }).is_err() {
                    return;
                }
            }
        }
    });
}

pub(super) fn spawn_search_updates(
    sender: Sender<PickerUpdate>,
    receiver: Receiver<SearchIndexRequest>,
) {
    thread::spawn(move || {
        while let Ok(mut request) = receiver.recv() {
            for newer in receiver.try_iter() {
                request = newer;
            }
            let index = InvertedIndex::build(&request.values);
            if sender
                .send(PickerUpdate::SearchIndex {
                    generation: request.generation,
                    index,
                })
                .is_err()
            {
                break;
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
            PickerUpdate::Cached(Ok(entries)) => state.replace_all_entries(entries),
            PickerUpdate::Cached(Err(error)) | PickerUpdate::Warning(error) => {
                record_picker_warning(warnings, error);
            }
            PickerUpdate::Lineage(Ok(records)) => state.replace_lineage(records),
            PickerUpdate::Lineage(Err(error)) => {
                record_picker_warning(warnings, format!("session lineage: {error}"));
            }
            PickerUpdate::Preview { key, value } => {
                state.previews.insert(key, value, &state.preview_window);
                state.trajectory_index_changed();
            }
            PickerUpdate::SearchIndex { generation, index } => {
                if generation == state.entries_generation {
                    state.search_index = Some(index);
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
        }
    }
    changed
}

pub(super) fn record_picker_warning(warnings: &mut Vec<String>, warning: String) {
    if warnings.len() < PICKER_WARNING_LIMIT && !warnings.contains(&warning) {
        warnings.push(warning);
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
