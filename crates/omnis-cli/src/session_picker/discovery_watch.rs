use std::{
    path::{Path, PathBuf},
    sync::mpsc::Sender,
    thread,
    time::{Duration, Instant},
};

use anyhow::{Result, bail, ensure};
use chrono::Utc;
use omnis_adapters::AdapterRegistry;
use omnis_ir::Provider;
use omnis_store::{SessionDiscoveryAttempt, SessionDiscoveryClaim, SessionDiscoveryState, Store};

use super::{
    SESSION_CACHE_TTL, normalized_picker_entries,
    workers::{
        DiscoveryUpdate, PickerUpdate, SessionCacheUpdate, native_session, spawn_provider_update,
    },
};

pub(super) fn spawn(
    sender: Sender<PickerUpdate>,
    index_sender: Sender<SessionCacheUpdate>,
    project: PathBuf,
    provider: Provider,
    attempt: SessionDiscoveryAttempt,
) {
    thread::spawn(
        move || match wait_for_catalog(&sender, &project, provider, attempt) {
            Ok(Some(next)) => {
                spawn_provider_update(sender, index_sender, project, provider, Some(next));
            }
            Ok(None) => {}
            Err(error) => {
                let _ = sender.send(PickerUpdate::Discovered(DiscoveryUpdate {
                    archived_ids: Vec::new(),
                    provider,
                    names: None,
                    result: Err(error),
                }));
            }
        },
    );
}

fn wait_for_catalog(
    sender: &Sender<PickerUpdate>,
    project: &Path,
    provider: Provider,
    mut attempt: SessionDiscoveryAttempt,
) -> Result<Option<SessionDiscoveryAttempt>> {
    let deadline = Instant::now() + 2 * SESSION_CACHE_TTL;
    let mut delay = Duration::from_millis(50);
    let store = Store::open_default()?;
    let registry = AdapterRegistry::with_local_adapters();
    let version = omnis_core::search_document_version(provider);
    let verify_home = |expected: &Path| {
        let result = registry
            .discovery_cache_home(provider)
            .and_then(|selected| {
                ensure!(
                    selected.as_deref().map(Path::as_os_str) == Some(expected.as_os_str()),
                    "provider home changed during session discovery"
                );
                Ok(())
            });
        if result.is_err() {
            let _ = sender.send(PickerUpdate::Discovered(DiscoveryUpdate {
                archived_ids: Vec::new(),
                provider,
                names: None,
                result: Ok((Vec::new(), true)),
            }));
        }
        result
    };
    loop {
        verify_home(attempt.home())?;
        let fresh_since = Utc::now() - SESSION_CACHE_TTL;
        match store.observe_session_discovery(&attempt, fresh_since)? {
            SessionDiscoveryState::Published => break,
            SessionDiscoveryState::Failed => {
                bail!("session discovery failed before publishing its catalog")
            }
            SessionDiscoveryState::InProgress => {}
            SessionDiscoveryState::Expired | SessionDiscoveryState::Superseded => {
                if sender.send(PickerUpdate::RefreshStarted(provider)).is_err() {
                    return Ok(None);
                }
                match store.try_begin_session_discovery(
                    provider,
                    version,
                    attempt.home(),
                    fresh_since,
                )? {
                    SessionDiscoveryClaim::Fresh => break,
                    SessionDiscoveryClaim::Claimed(next) => return Ok(Some(next)),
                    SessionDiscoveryClaim::InProgress(next) => attempt = next,
                }
            }
        }
        ensure!(
            Instant::now() < deadline,
            "timed out waiting for the session catalog"
        );
        let remaining = deadline.saturating_duration_since(Instant::now());
        thread::sleep(delay.min(remaining));
        delay = (delay * 2).min(Duration::from_millis(500));
    }
    let catalog = if provider == Provider::AntigravityDesktop {
        let Some(published) = store.published_desktop_sessions(version, attempt.home())? else {
            verify_home(attempt.home())?;
            bail!("no published Antigravity Desktop listing is available");
        };
        published
    } else {
        store.indexed_sessions_for_provider_in_home(provider, attempt.home())?
    };
    let sessions = catalog.into_iter().map(native_session).collect();
    let entries = normalized_picker_entries(sessions, project, true);
    verify_home(attempt.home())?;
    let _ = sender.send(PickerUpdate::Discovered(DiscoveryUpdate {
        archived_ids: Vec::new(),
        provider,
        names: None,
        result: Ok((entries, true)),
    }));
    Ok(None)
}
