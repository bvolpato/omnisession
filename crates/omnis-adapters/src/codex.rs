use std::{
    cmp::Reverse,
    collections::{BinaryHeap, HashMap, HashSet},
    ffi::OsString,
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
};

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, TimeDelta, Utc};
use omnis_ir::{EventKind, Provider, ReplayPolicy, SessionRef, SourceRevision};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    CodexWorkspaceProjection, EnvChange, IndexRead, LaunchPlan, LaunchTarget, NativeResumeSupport,
    NativeSession, ProviderAdapter, ProviderInstallation, RollbackTitle,
    support::{
        EventBuilder, JsonLineSkips, MAX_PREVIEW_TAIL_SIZE, MAX_STREAMED_TRANSCRIPT_LINE_SIZE,
        json_lines_preview_with_omissions, omitted_images_text, parse_timestamp, paths_match,
        provider_executable, provider_file, sort_sessions, string_at, validate_provider, value_at,
        visit_index_json_lines,
    },
};

mod current_launch;
mod current_rollout;
mod gui_names;
mod rollout_cache;
mod rollout_file;
mod workspace_check;
mod workspace_damage;
mod workspace_search;
mod workspace_settings;

use current_rollout::{
    CurrentFile, CurrentRolloutStamp, CurrentRolloutUnavailable, CurrentRollouts,
};
use rollout_cache::{ObservedRollout, RolloutCache, RolloutHeader, RolloutStamp};

const SCAN_LIMIT: usize = 10_000;
const UNAVAILABLE_ANCESTRY: &str = "omnisession.codex_history_base";
const MAX_CANONICAL_TOOL_EVENTS: usize = 256;
// Rollouts stream without a record budget, so retained events and turn checkpoints keep the newest.
const MAX_CANONICAL_EVENTS: usize = 100_000;
const MAX_TOOL_STRING_CHARACTERS: usize = 32 * 1024;
const MAX_TOOL_ARRAY_ITEMS: usize = 256;
const TOOL_COMPACTION_RECORD_INTERVAL: usize = 1_024;

#[derive(Clone, Debug, Default)]
struct CodexFileScan {
    directories: Vec<(PathBuf, Option<RolloutStamp>)>,
    files: Vec<PathBuf>,
    candidates: HashMap<String, Vec<usize>>,
    notes: Vec<String>,
    active_entries_complete: bool,
    // Native filename verification checks these link targets before authorizing an in-place resume.
    active_links: Vec<PathBuf>,
}

impl CodexFileScan {
    fn current(&self) -> bool {
        self.directories.len() <= SCAN_LIMIT
            && self
                .directories
                .iter()
                .all(|(dir, stamp)| match fs::symlink_metadata(dir) {
                    Ok(metadata) => stamp.is_some() && RolloutStamp::new(&metadata) == *stamp,
                    Err(error) => stamp.is_none() && error.kind() == ErrorKind::NotFound,
                })
    }

    fn legacy_paths(
        &self,
        home: &Path,
        mut cache: Option<&mut RolloutCache>,
    ) -> HashMap<String, Vec<&Path>> {
        let mut legacy = HashMap::<String, Vec<&Path>>::new();
        let Ok(root) = fs::canonicalize(home) else {
            return legacy;
        };
        for path in &self.files {
            if !current_rollout::canonical_name(path)
                && live_rollout_path(home, &root, path)
                && let Ok(Some(session)) = listed_session(path, cache.as_deref_mut())
            {
                // A header supplies an ID to look up. Only the SQLite row can select this file by that ID.
                legacy.entry(session.id).or_default().push(path);
            }
        }
        legacy
    }

    fn listed_rollout(
        &self,
        home: &Path,
        id: &str,
        row: Option<&current_rollout::CurrentRollout>,
        mut cache: Option<&mut RolloutCache>,
        invalid: &mut InvalidHeaders,
    ) -> Result<Option<SelectedRollout>> {
        if let Some(row) = row
            && !matches!(self.candidates.get(id).map(Vec::as_slice), Some([i]) if row.ordinary_path_is(&self.files[*i]))
            && let Some(file) = row.resolve(home, id, self.paths(id), self.notes.is_empty())?
        {
            return Ok(Some(file.into()));
        }
        anyhow::ensure!(!ambiguous_generations(self, id), CurrentRolloutUnavailable);
        Ok(select_rollout(
            self.paths(id),
            |path| listed_session(path, cache.as_deref_mut()),
            invalid,
        ))
    }

    fn unique_path(&self, id: &str) -> Option<&Path> {
        let [index] = self.candidates.get(id)?.as_slice() else {
            return None;
        };
        self.notes.is_empty().then(|| self.files[*index].as_path())
    }

    fn paths(&self, id: &str) -> impl Iterator<Item = &Path> {
        self.candidates
            .get(id)
            .into_iter()
            .flatten()
            .map(|&i| self.files[i].as_path())
    }

    fn remember(&mut self, id: &str, path: &Path, active: &Path) {
        let indices = self.candidates.entry(id.to_owned()).or_default();
        let rank = rollout_priority(path, active);
        let position =
            indices.partition_point(|&i| rollout_priority(&self.files[i], active) > rank);
        indices.insert(position, self.files.len());
        self.files.push(path.to_owned());
    }
}

#[derive(Default)]
struct InvalidHeaders {
    unreadable: usize,
    without_metadata: usize,
    first_error: Option<anyhow::Error>,
}

struct SelectedRollout {
    session: CodexSession,
    shadowed: bool,
    from_registry: bool,
}

impl From<CurrentFile> for SelectedRollout {
    fn from(file: CurrentFile) -> Self {
        debug_assert_eq!(file.session.observed_revision, Some(file.revision));
        Self {
            session: file.session,
            shadowed: !file.native,
            from_registry: true,
        }
    }
}

fn select_rollout<'a>(
    candidates: impl IntoIterator<Item = &'a Path>,
    mut read: impl FnMut(&Path) -> Result<Option<CodexSession>>,
    invalid: &mut InvalidHeaders,
) -> Option<SelectedRollout> {
    let mut shadowed = false;
    for path in candidates {
        match read(path) {
            Ok(Some(session)) => {
                let Some((thread, file)) = current_rollout::ids(path) else {
                    continue;
                };
                if session.id != thread {
                    // A suffix on a manual filename can name an unrelated thread.
                    continue;
                }
                let dual = thread != file;
                return Some(SelectedRollout {
                    session,
                    shadowed: shadowed || dual,
                    from_registry: false,
                });
            }
            Ok(None) => invalid.without_metadata += 1,
            Err(error) => {
                invalid.unreadable += 1;
                invalid.first_error.get_or_insert(error);
            }
        }
        // A native lookup by ID can still open the malformed file before its valid sibling.
        shadowed |= path.extension().is_some_and(|ext| ext != "gz");
    }
    None
}

#[derive(Debug, Default)]
struct CodexListing {
    scan: Arc<CodexFileScan>,
    sessions: Vec<CodexSession>,
    notes: Vec<String>,
    selected_files: HashMap<String, (PathBuf, SourceRevision)>,
    requires_scan: bool,
}

#[derive(Debug)]
struct CachedListing {
    database: Option<CurrentRolloutStamp>,
    listing: Arc<CodexListing>,
}

#[derive(Debug, Default)]
struct TitleIndex {
    titles: HashMap<String, String>,
    notes: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct CodexAdapter {
    codex_home: Option<PathBuf>,
    relative_home: bool,
    current: Arc<CurrentRollouts>,
    use_workspace_index: bool,
    /// Rollout header cache file. Explicit roots leave it unset.
    rollout_cache: Option<PathBuf>,
    session_scan: Arc<Mutex<Option<Arc<CodexFileScan>>>>,
    listing: Arc<Mutex<Option<CachedListing>>>,
    titles: Arc<OnceLock<TitleIndex>>,
}

impl CodexAdapter {
    /// Returns the verified treatment of a leading tilde in `CODEX_SQLITE_HOME` for this Codex release.
    #[must_use]
    pub fn expands_sqlite_env_tilde(version: &str) -> Option<bool> {
        match version {
            "0.137.0" => Some(false),
            "0.167.1" => Some(true),
            _ => None,
        }
    }

    /// Resolves an explicit `CODEX_HOME` before a child changes its working directory.
    ///
    /// # Errors
    ///
    /// Returns an error if the caller's directory or an existing relative path cannot be resolved.
    pub fn absolute_home(home: &Path) -> Result<PathBuf> {
        if home.is_absolute() {
            return Ok(home.to_owned());
        }
        match omnis_core::canonicalize_path(home) {
            Err(error) if error.kind() == ErrorKind::NotFound => std::path::absolute(home),
            result => result,
        }
        .context("resolving the selected Codex home")
    }

    fn selected_home(home: PathBuf) -> (Option<PathBuf>, bool) {
        let relative = home.is_relative();
        (Some(Self::absolute_home(&home).unwrap_or(home)), relative)
    }

    fn native_launch(&self, target: &LaunchTarget, args: Vec<String>) -> Result<LaunchPlan> {
        let mut env = Vec::new();
        if let Some(home) = self.codex_home.as_deref().filter(|_| self.relative_home) {
            anyhow::ensure!(
                home.is_absolute(),
                "could not resolve the selected Codex home"
            );
            let value = home
                .to_str()
                .context("Codex requires a Unicode path in CODEX_HOME")?;
            env.push(EnvChange::Set {
                name: "CODEX_HOME".to_owned(),
                value: value.to_owned(),
            });
        }
        Ok(LaunchPlan {
            program: "codex".to_owned(),
            args,
            cwd: target.cwd.clone(),
            env,
        })
    }

    /// Returns the directory Codex uses when `CODEX_HOME` is unset. It can be absent before the first launch.
    #[must_use]
    pub fn default_home() -> Option<PathBuf> {
        #[cfg(windows)]
        let profile = known_folders::get_known_folder_path(known_folders::KnownFolder::Profile)?;
        #[cfg(not(windows))]
        let profile = directories::BaseDirs::new()?.home_dir().to_owned();
        Some(profile.join(".codex"))
    }

    /// Returns whether Codex selected a rollout that could not be verified.
    #[must_use]
    pub fn is_current_rollout_unavailable(error: &anyhow::Error) -> bool {
        error.is::<CurrentRolloutUnavailable>()
            || error.is::<current_rollout::CurrentRolloutUnconfirmed>()
    }

    /// Identifies a rollout file across appends and distinguishes same-path replacements.
    #[must_use]
    pub fn index_file_id(path: &Path) -> Option<Uuid> {
        Self::index_file_revision(path)?.file
    }

    /// Observes which file and contents a reader can use without reading the transcript.
    #[must_use]
    pub fn index_file_revision(path: &Path) -> Option<SourceRevision> {
        Some(ObservedRollout::read(path)?.revision)
    }

    /// Checks a legacy search document against the currently selected rollout with bounded memory.
    ///
    /// # Errors
    ///
    /// Returns an error if the source changes during the check or cannot be read.
    pub fn verify_legacy_search_projection(
        session: &SessionRef,
        path: &Path,
        read: &IndexRead,
        expected_hash: &[u8],
    ) -> Result<Option<IndexRead>> {
        validate_provider(session, Provider::Codex)?;
        if read.source_complete || read.source_revision.contents.is_none() {
            return Ok(None);
        }
        let observed = ObservedRollout::read(path)
            .filter(|observed| {
                observed
                    .revision
                    .matches_file_contents(read.source_revision)
            })
            .context("selected Codex rollout changed before verifying its previous search text")?;
        let native = CodexSession::parse_with_observation(path, observed)?
            .filter(|native| native.id == session.id && read.snapshot.session == *session)
            .context("selected Codex rollout has a different session header")?;
        native.read_unchanged(|native, _| {
            let (events, project, workspace_reconstructed) =
                native.reconstructed_preview(rollout_file::LEGACY_PROJECTION_LIMITS)?;
            let workspace_projection = match &read.workspace_projection {
                CodexWorkspaceProjection::Unchecked => {
                    CodexWorkspaceProjection::from_reconstruction(
                        workspace_reconstructed,
                        project.as_deref(),
                    )
                }
                observed @ CodexWorkspaceProjection::Observed(_) => observed.clone(),
            };
            let rollback_title = events.rollback_title();
            let branch = native.branch_at(project.as_deref());
            let snapshot = events.snapshot(
                session.clone(),
                read.snapshot.title.clone(),
                project,
                branch,
                read.snapshot.captured_at,
            );
            let document = omnis_core::trajectory_search_document(&snapshot);
            if Sha256::digest(document.text.as_bytes())[..] != *expected_hash {
                return Ok(None);
            }
            Ok(Some(IndexRead {
                rollback_title,
                workspace_projection,
                snapshot,
                source_complete: document.source_complete,
                provider_limited: read.provider_limited,
                source_revision: read.source_revision,
                native_session: read.native_session.clone(),
            }))
        })
    }

    /// Checks whether an applied rollback removed the local opener that supplied a saved title.
    ///
    /// # Errors
    ///
    /// Returns an error if the selected rollout changes or cannot be read.
    pub fn rollout_withdrew_search_title(
        path: &Path,
        session: &SessionRef,
        revision: SourceRevision,
        prior_title: &str,
    ) -> Result<bool> {
        Self::verify_selected_title(path, session, revision, |native| {
            native.withdrew_search_title(prior_title)
        })
    }

    /// Checks one saved title when an ordinary rollout withdrew more openers than the receipt retains.
    ///
    /// # Errors
    ///
    /// Returns an error if the selected rollout changes or cannot be read.
    pub fn verify_earlier_opener_withdrawal(
        path: &Path,
        session: &SessionRef,
        revision: SourceRevision,
        prior_title: &str,
    ) -> Result<bool> {
        Self::verify_selected_title(path, session, revision, |native| {
            native.check_saved_opener(prior_title, SavedOpenerCheck::Withdrawal)
        })
    }

    /// Rechecks an old opening title against the source records that its prior reader misclassified.
    ///
    /// # Errors
    ///
    /// Returns an error if the selected rollout changes or cannot be read.
    pub fn verify_legacy_local_opener(
        path: &Path,
        session: &SessionRef,
        revision: SourceRevision,
        prior_title: &str,
        requires_tool_catalog: bool,
    ) -> Result<bool> {
        Self::verify_selected_title(path, session, revision, |native| {
            let check = if requires_tool_catalog {
                SavedOpenerCheck::ToolCatalog
            } else {
                SavedOpenerCheck::LegacyOrToolCatalog
            };
            native.check_saved_opener(prior_title, check)
        })
    }

    fn verify_selected_title(
        path: &Path,
        session: &SessionRef,
        revision: SourceRevision,
        read: impl FnOnce(CodexSession) -> Result<bool>,
    ) -> Result<bool> {
        validate_provider(session, Provider::Codex)?;
        if revision.contents.is_none() {
            return Ok(false);
        }
        let observed = ObservedRollout::read(path)
            .filter(|observed| observed.revision.matches_file_contents(revision))
            .context("selected Codex rollout changed before checking its search title")?;
        let native = CodexSession::parse_with_observation(path, observed)?
            .filter(|native| native.id == session.id)
            .context("selected Codex rollout has a different session header")?;
        native.read_unchanged(|native, _| read(native))
    }

    /// The decoded size of a compressed rollout can differ from its filesystem metadata.
    #[must_use]
    pub fn is_compressed_rollout(path: &Path) -> bool {
        rollout_file::jsonl_file_name(path).is_some() && rollout_file::compressed(path)
    }

    #[must_use]
    pub fn with_root(codex_home: impl Into<PathBuf>) -> Self {
        let (codex_home, relative_home) = Self::selected_home(codex_home.into());
        Self {
            codex_home,
            relative_home,
            current: Arc::default(),
            use_workspace_index: false,
            rollout_cache: None,
            session_scan: Arc::default(),
            listing: Arc::default(),
            titles: Arc::default(),
        }
    }

    /// Selects existing threads from the SQLite directory used for the native import.
    #[must_use]
    pub fn for_import(codex_home: &Path, sqlite_home: &Path) -> Self {
        let mut adapter = Self::with_root(codex_home);
        adapter.current = Arc::new(CurrentRollouts::pinned(sqlite_home));
        adapter
    }

    fn discover_session_scan(&self) -> CodexFileScan {
        let Some(home) = self.codex_home.as_deref() else {
            return CodexFileScan::default();
        };
        scan_session_files(home, SCAN_LIMIT)
    }

    /// Returns the scan and whether this caller reused a cached snapshot.
    /// Readers that rejected the same snapshot share one refresh.
    fn session_scan(&self, rejected: Option<&Arc<CodexFileScan>>) -> (Arc<CodexFileScan>, bool) {
        let mut entry = self
            .session_scan
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(cached) = entry.as_ref()
            && !rejected.is_some_and(|old| Arc::ptr_eq(old, cached))
        {
            return (Arc::clone(cached), true);
        }
        let scan = Arc::new(self.discover_session_scan());
        *entry = Some(Arc::clone(&scan));
        (scan, false)
    }

    fn current_session_scan(&self) -> (Arc<CodexFileScan>, bool) {
        let (scan, reused) = self.session_scan(None);
        if reused && !scan.current() {
            self.session_scan(Some(&scan))
        } else {
            (scan, reused)
        }
    }

    fn scan_for_lookup(
        &self,
        id: &str,
        source_path: Option<&Path>,
        home: &Path,
        canonical_home: &Path,
    ) -> (Arc<CodexFileScan>, bool) {
        let (scan, reused) = self.current_session_scan();
        let Some(path) = source_path else {
            return (scan, reused);
        };
        if scan.paths(id).any(|known| known == path)
            || path_uuid(path).as_deref() != Some(id)
            || !live_rollout_path(home, canonical_home, path)
        {
            return (scan, reused);
        }

        // Another picker can discover a new active file while this worker still has a readable archive.
        drop(scan);
        let mut entry = self
            .session_scan
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let scan = entry.as_mut().expect("the session scan was initialized");
        // Limit the number of paths that a long-running picker can add between scans.
        if scan.files.len() >= SCAN_LIMIT * 2 {
            *scan = Arc::new(self.discover_session_scan());
        }
        if !scan.paths(id).any(|known| known == path) {
            Arc::make_mut(scan).remember(id, path, &home.join("sessions"));
        }
        (Arc::clone(scan), reused)
    }

    fn title_index(&self) -> &TitleIndex {
        self.titles.get_or_init(|| {
            let Some(home) = self.codex_home.as_deref() else {
                return TitleIndex::default();
            };
            let Some(path) = provider_file(home, &home.join("session_index.jsonl")) else {
                return TitleIndex::default();
            };
            let mut rows: HashMap<String, (Option<DateTime<Utc>>, usize, String)> = HashMap::new();
            let mut position = 0_usize;
            let scan = visit_index_json_lines(&path, |row| {
                position += 1;
                let Some(id) = string_at(&row, &[&["id"]]).filter(|id| Uuid::parse_str(id).is_ok())
                else {
                    return;
                };
                let Some(title) = string_at(&row, &[&["thread_name"]]) else {
                    return;
                };
                let updated_at = parse_timestamp(row.get("updated_at"));
                let replace = rows
                    .get(id)
                    .is_none_or(|(current_time, current_position, _)| {
                        updated_at > *current_time
                            || (updated_at == *current_time && position > *current_position)
                    });
                if replace {
                    rows.insert(id.to_owned(), (updated_at, position, title.to_owned()));
                }
            });
            let notes = match scan {
                Ok(scan) => scan.notes("Codex session_index.jsonl"),
                Err(error) => vec![format!(
                    "Codex session_index.jsonl could not be read: {}.",
                    compact_discovery_error(&error)
                )],
            };
            TitleIndex {
                titles: rows
                    .into_iter()
                    .map(|(id, (_, _, title))| (id, title))
                    .collect(),
                notes,
            }
        })
    }

    fn selected_title(
        &self,
        selected: &SelectedRollout,
        row: Option<&current_rollout::CurrentRollout>,
        scan: impl FnOnce() -> Arc<CodexFileScan>,
    ) -> Option<String> {
        let session = &selected.session;
        let indexed = || {
            let title = self.title_index().titles.get(&session.id)?;
            let scan = scan();
            (scan.unique_path(&session.id) == Some(session.path.as_path())).then(|| title.clone())
        };
        match row {
            Some(row) if selected.from_registry || row.ordinary_path_is(&session.path) => {
                row.title(indexed)
            }
            Some(_) => None,
            None => indexed(),
        }
    }

    /// Refreshes cached names and metadata from the currently selected rollouts.
    pub fn refresh_session_titles(&self, sessions: &mut [NativeSession]) {
        if !sessions
            .iter()
            .any(|s| s.session.provider == Provider::Codex)
        {
            return;
        }
        let Some(home) = self.codex_home.as_deref() else {
            return;
        };
        let (scan, _) = self.current_session_scan();
        let ids = sessions
            .iter()
            .filter(|s| s.session.provider == Provider::Codex)
            .map(|s| s.session.id.as_str());
        let rows = self.current.rows(home, ids);
        let mut cache = self
            .rollout_cache
            .as_deref()
            .and_then(|path| RolloutCache::load(path, home));
        for cached in sessions
            .iter_mut()
            .filter(|s| s.session.provider == Provider::Codex)
        {
            let id = cached.session.id.as_str();
            let Ok(rows) = &rows else {
                cached.title = None;
                continue;
            };
            let row = rows.get(id);
            if let Some(path) = scan.unique_path(id)
                && row.is_none_or(|row| row.ordinary_path_is(path))
                && cached
                    .source_revision
                    .zip(Self::index_file_revision(path))
                    .is_some_and(|(old, new)| old.matches_file_contents(new))
            {
                let indexed = || self.title_index().titles.get(id).cloned();
                cached.title = match row {
                    Some(row) => row.title(indexed),
                    None => indexed(),
                };
                cached.source_path = Some(path.to_owned());
                continue;
            }
            let Ok(Some(selected)) = scan.listed_rollout(
                home,
                id,
                row,
                cache.as_mut(),
                &mut InvalidHeaders::default(),
            ) else {
                cached.title = None;
                continue;
            };
            let title = self.selected_title(&selected, row, || Arc::clone(&scan));
            let current = selected.session;
            if cached
                .source_revision
                .zip(current.observed_revision)
                .is_some_and(|(old, new)| old.matches_file_contents(new))
            {
                cached.source_path = Some(current.path);
            } else {
                *cached = current.native_session();
            }
            cached.title = title;
        }
    }

    fn listing(&self, refresh: bool) -> (Arc<CodexListing>, bool) {
        let mut cache = self
            .listing
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let stamp = || {
            self.codex_home
                .as_deref()
                .and_then(|home| self.current.stamp(home))
        };
        let database = stamp();
        if let Some(cached) = cache.as_ref() {
            if !refresh
                || (database.is_some()
                    && cached.database == database
                    && !cached.listing.requires_scan
                    && cached.listing.scan.current()
                    && cached.listing.sessions.iter().all(|session| {
                        cached.listing.selected_files.contains_key(&session.id)
                            || Self::index_file_revision(&session.path) == session.observed_revision
                    })
                    && cached
                        .listing
                        .selected_files
                        .values()
                        .all(|(path, revision)| Self::index_file_revision(path) == Some(*revision)))
            {
                return (Arc::clone(&cached.listing), true);
            }
            let (scan, _) = self.session_scan(None);
            self.session_scan(Some(&scan));
        }
        let listing = Arc::new(self.build_listing());
        let database = database.filter(|before| stamp().as_ref() == Some(before));
        *cache = Some(CachedListing {
            database,
            listing: Arc::clone(&listing),
        });
        (listing, false)
    }

    fn build_listing(&self) -> CodexListing {
        let (scan, _) = self.session_scan(None);
        let Some(home) = self.codex_home.as_deref() else {
            return CodexListing::default();
        };
        let mut cache = self
            .rollout_cache
            .as_deref()
            .zip(self.codex_home.as_deref())
            .and_then(|(path, home)| RolloutCache::load(path, home));
        let legacy = scan.legacy_paths(home, cache.as_mut());
        let ids = || {
            scan.candidates.keys().chain(
                legacy
                    .keys()
                    .filter(|id| !scan.candidates.contains_key(*id)),
            )
        };
        let rows = match self.current.rows(home, ids().map(String::as_str)) {
            Ok(rows) => rows,
            Err(error) => {
                return CodexListing {
                    notes: vec![format!(
                        "Codex could not verify the current rollouts: {}.",
                        compact_discovery_error(&anyhow!("{error:#}"))
                    )],
                    requires_scan: true,
                    ..CodexListing::default()
                };
            }
        };
        let title_index = self.title_index();
        let mut sessions = Vec::new();
        let mut selected_files = HashMap::new();
        let mut requires_scan = false;
        let mut subagents = 0_usize;
        let mut invalid = InvalidHeaders {
            without_metadata: scan.files.len()
                - scan.candidates.values().map(Vec::len).sum::<usize>(),
            ..Default::default()
        };
        for id in ids() {
            let row = rows.get(id);
            let selected = match scan.listed_rollout(home, id, row, cache.as_mut(), &mut invalid) {
                Ok(Some(selected)) => selected,
                Ok(None) => continue,
                Err(error) => {
                    requires_scan = true;
                    invalid.unreadable += 1;
                    invalid.first_error.get_or_insert(error);
                    continue;
                }
            };
            let title = self.selected_title(&selected, row, || Arc::clone(&scan));
            let mut session = selected.session;
            if current_rollout::ids(&session.path).is_none()
                && legacy
                    .get(id)
                    .is_some_and(|files| files.contains(&session.path.as_path()))
            {
                invalid.without_metadata -= 1;
            }
            if row.is_some_and(|row| row.requires_selection(home)) {
                requires_scan |= selected.shadowed;
                if !selected.shadowed
                    && let Some(revision) = session.observed_revision
                {
                    selected_files.insert(id.clone(), (session.path.clone(), revision));
                }
            }
            if session.is_subagent {
                subagents += 1;
                continue;
            }
            session.title = title;
            sessions.push(session);
        }
        if let Some(cache) = cache {
            cache.save();
        }
        let mut notes = listing_notes(
            &scan,
            sessions.len(),
            subagents,
            invalid.unreadable,
            invalid.without_metadata,
            invalid
                .first_error
                .as_ref()
                .map(compact_discovery_error)
                .as_deref(),
        );
        notes.extend(title_index.notes.iter().cloned());
        CodexListing {
            scan,
            sessions,
            notes,
            selected_files,
            requires_scan,
        }
    }

    fn find_session(&self, id: &str, source_path: Option<&Path>) -> Result<CodexSession> {
        Uuid::parse_str(id).context("Codex session ID must be a UUID")?;
        self.resolve_rollout(id, source_path, true)?
            .map(|s| s.session)
            .ok_or_else(|| anyhow!("Codex session {id} was not found"))
    }

    fn require_rollout(&self, id: &str, source_path: Option<&Path>) -> Result<SelectedRollout> {
        Uuid::parse_str(id).context("Codex session ID must be a UUID")?;
        self.selected_rollout(id, source_path)?
            .ok_or_else(|| anyhow!("Codex session `{id}` was not found"))
    }

    fn read_selected<T>(
        &self,
        native: CodexSession,
        read: impl FnOnce(CodexSession, SourceRevision) -> Result<T>,
    ) -> Result<T> {
        let id = native.id.clone();
        let path = native.path.clone();
        let revision = native.observed_revision;
        let value = native.read_unchanged(read);
        let selected = self
            .selected_rollout(&id, Some(&path))
            .context("selected Codex rollout changed during the read")?;
        anyhow::ensure!(
            selected.is_some_and(|selected| {
                selected.session.path == path && selected.session.observed_revision == revision
            }),
            "selected Codex rollout changed during the read"
        );
        value
    }

    /// Reads the model provider that a session records.
    ///
    /// Codex resumes a session only with the provider that created it.
    ///
    /// # Errors
    ///
    /// Returns an error when the rollout cannot be read. A missing session or a rollout that
    /// records no provider is `Ok(None)`.
    pub fn recorded_model_provider(&self, id: &str) -> Result<Option<String>> {
        if Uuid::parse_str(id).is_err() {
            return Ok(None);
        }
        let Some(selected) = self.selected_rollout(id, None)? else {
            return Ok(None);
        };
        let records = rollout_file::header(&selected.session.path)?;
        Ok(records
            .first()
            .filter(|record| record.get("type").and_then(Value::as_str) == Some("session_meta"))
            .and_then(|record| record.get("payload"))
            .filter(|payload| string_at(payload, &[&["id"], &["session_id"]]) == Some(id))
            .and_then(|payload| string_at(payload, &[&["model_provider"]]))
            .map(str::to_owned))
    }

    fn selected_rollout(
        &self,
        id: &str,
        source_path: Option<&Path>,
    ) -> Result<Option<SelectedRollout>> {
        self.resolve_rollout(id, source_path, false)
    }

    fn resolve_rollout(
        &self,
        id: &str,
        source_path: Option<&Path>,
        with_title: bool,
    ) -> Result<Option<SelectedRollout>> {
        let Some(home) = self.codex_home.as_deref() else {
            return Ok(None);
        };
        let Ok(canonical_home) = fs::canonicalize(home) else {
            return Ok(None);
        };
        let rows = self.current.rows(home, [id])?;
        let row = rows.get(id);
        let finish = |mut selected: SelectedRollout| {
            if with_title {
                selected.session.title =
                    self.selected_title(&selected, row, || self.current_session_scan().0);
            }
            Ok(Some(selected))
        };
        if let Some(row) = row {
            match row.resolve(home, id, std::iter::empty(), false) {
                Ok(Some(file)) => return finish(file.into()),
                Err(error) if row.has_legacy_name() => return Err(error),
                _ => {}
            }
        }
        let mut invalid = InvalidHeaders::default();
        let (mut scan, cached) = self.scan_for_lookup(id, source_path, home, &canonical_home);
        if let Some(row) = row {
            if cached && row.requires_selection(home) {
                // A restored copy can appear outside the recorded directory between lookups.
                scan = self.session_scan(Some(&scan)).0;
            }
            if let Some(file) = row.resolve(home, id, scan.paths(id), scan.notes.is_empty())? {
                return finish(file.into());
            }
        }
        anyhow::ensure!(!ambiguous_generations(&scan, id), CurrentRolloutUnavailable);
        if let Some(selected) =
            select_live_rollout(home, &canonical_home, scan.paths(id), &mut invalid)
        {
            return finish(selected);
        }
        if cached {
            // A remaining malformed sibling does not prove that the last valid file is still here.
            let (fresh, _) = self.session_scan(Some(&scan));
            if !fresh.paths(id).eq(scan.paths(id)) {
                anyhow::ensure!(
                    !ambiguous_generations(&fresh, id),
                    CurrentRolloutUnavailable
                );
                invalid = InvalidHeaders::default();
                if let Some(selected) =
                    select_live_rollout(home, &canonical_home, fresh.paths(id), &mut invalid)
                {
                    return finish(selected);
                }
            }
        }
        invalid.first_error.map_or(Ok(None), Err)
    }

    /// Finds user sessions created after a native fork command started.
    ///
    /// A candidate must record the source as its fork parent in the launch workspace.
    ///
    /// # Errors
    ///
    /// Returns an error for a non-Codex source reference or unreadable metadata.
    pub fn fork_candidates_created_since(
        &self,
        source: &SessionRef,
        project: &Path,
        started_at: DateTime<Utc>,
    ) -> Result<Vec<SessionRef>> {
        validate_provider(source, Provider::Codex)?;
        let parent =
            Uuid::parse_str(&source.id).context("Codex parent session ID must be a UUID")?;
        let Some(home) = self.codex_home.as_deref() else {
            return Ok(Vec::new());
        };

        // Codex files rollouts by local date, which can differ from the UTC date by one day.
        let mut directories = HashSet::new();
        for timestamp in [started_at, Utc::now()] {
            for days in [-1, 0, 1] {
                directories.insert(
                    home.join("sessions").join(
                        (timestamp + TimeDelta::days(days))
                            .format("%Y/%m/%d")
                            .to_string(),
                    ),
                );
            }
        }

        let mut candidates = Vec::new();
        let matches = |session: &CodexSession| {
            session.id != source.id
                && session.forked_from_id == Some(parent)
                && !session.is_subagent
                && session
                    .created_at
                    .is_some_and(|created| created >= started_at)
                && session
                    .project_path
                    .as_deref()
                    .is_some_and(|path| paths_match(path, project))
        };
        for directory in directories {
            let mut paths = Vec::new();
            let mut unreadable_dirs = 0;
            let mut depth_limited_dirs = 0;
            collect_jsonl(
                &directory,
                0,
                &mut paths,
                SCAN_LIMIT,
                (&mut unreadable_dirs, &mut depth_limited_dirs),
                &mut Vec::new(),
                None,
            );
            for indices in ranked_rollouts(&paths, &home.join("sessions")).into_values() {
                let mut invalid = InvalidHeaders::default();
                let Some(selected) = select_rollout(
                    indices.iter().map(|&i| paths[i].as_path()),
                    CodexSession::parse_observed_metadata,
                    &mut invalid,
                ) else {
                    if let Some(error) = invalid.first_error {
                        return Err(error);
                    }
                    continue;
                };
                if matches(&selected.session) {
                    let current =
                        self.find_session(&selected.session.id, Some(&selected.session.path))?;
                    if matches(&current) {
                        candidates.push(current);
                    }
                }
            }
        }
        candidates.sort_by_key(|session| session.created_at);
        candidates.dedup_by(|left, right| left.id == right.id);
        Ok(candidates
            .into_iter()
            .map(|session| SessionRef::new(Provider::Codex, session.id))
            .collect())
    }
}

impl NativeSession {
    /// Applies the surviving Codex workspace when it belongs to this exact file revision.
    /// Returns whether the project or branch changed.
    pub fn apply_codex_workspace(&mut self, workspace: &omnis_store::CodexWorkspace) -> bool {
        if self.session.provider == Provider::Codex
            && self.source_revision.is_some_and(|revision| {
                revision.contents.is_some()
                    && revision.matches_file_contents(workspace.source_revision)
            })
            && (self.project_path != workspace.project_path
                || self.git_branch != workspace.git_branch)
        {
            self.project_path.clone_from(&workspace.project_path);
            self.git_branch.clone_from(&workspace.git_branch);
            return true;
        }
        false
    }

    /// Applies the workspace recorded for this exact Codex source. A pending recheck hides the old directory.
    /// Returns true only for a verified observation, including one that established an unknown workspace.
    pub fn apply_codex_workspace_index(
        &mut self,
        index: &omnis_store::CodexWorkspaceIndex,
        cached_header: bool,
    ) -> bool {
        let Some(revision) = self
            .source_revision
            .filter(|_| self.session.provider == Provider::Codex)
        else {
            return false;
        };
        let Some(file) = revision.file else {
            return false;
        };
        let verified = index
            .verified
            .get(&file)
            .filter(|entry| revision.matches_file_contents(entry.source_revision));
        let pending = index.needs_recheck(revision);
        if verified.is_none() && !pending {
            return false;
        }
        if cached_header
            && !self
                .source_path
                .as_deref()
                .and_then(CodexAdapter::index_file_revision)
                .is_some_and(|current| current.matches_file_contents(revision))
        {
            return false;
        }
        if let Some(workspace) = verified {
            self.apply_codex_workspace(workspace);
            return true;
        }
        self.project_path = None;
        self.git_branch = None;
        false
    }
}

impl Default for CodexAdapter {
    fn default() -> Self {
        let (codex_home, relative_home) = std::env::var_os("CODEX_HOME")
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
            .map_or_else(|| (Self::default_home(), false), Self::selected_home);
        Self {
            rollout_cache: codex_home.as_deref().and_then(rollout_cache::default_path),
            current: Arc::new(CurrentRollouts::from_env()),
            codex_home,
            relative_home,
            use_workspace_index: true,
            session_scan: Arc::default(),
            listing: Arc::default(),
            titles: Arc::default(),
        }
    }
}

/// Reads a rollout's listing metadata, reusing the cached header while the rollout is unchanged.
fn listed_session(path: &Path, cache: Option<&mut RolloutCache>) -> Result<Option<CodexSession>> {
    let observed =
        ObservedRollout::read(path).context("selected Codex rollout changed during discovery")?;
    let (Some(cache), Some(stamp)) = (cache, observed.stamp) else {
        return CodexSession::parse_with_observation(path, observed);
    };
    if let Some(cached) = cache.get(path, stamp) {
        return Ok(cached
            .header
            .clone()
            .map(|header| CodexSession::from_header(path, header, observed)));
    }
    let session = CodexSession::parse_with_observation(path, observed)?;
    cache.insert(path, stamp, session.as_ref().map(CodexSession::header));
    Ok(session)
}

fn scan_session_files(home: &Path, limit: usize) -> CodexFileScan {
    let mut files = Vec::new();
    let mut active_links = Vec::new();
    let mut directories = Vec::new();
    let mut unreadable_dirs = 0_usize;
    let mut depth_limited_dirs = 0_usize;
    let collect_limit = limit.saturating_add(1);
    collect_jsonl(
        &home.join("sessions"),
        5,
        &mut files,
        collect_limit,
        (&mut unreadable_dirs, &mut depth_limited_dirs),
        &mut directories,
        Some(&mut active_links),
    );
    let active_entries_complete = unreadable_dirs == 0
        && depth_limited_dirs == 0
        && files.len() <= limit
        && active_links.len() <= limit;
    if files.len() <= limit {
        collect_jsonl(
            &home.join("archived_sessions"),
            5,
            &mut files,
            collect_limit,
            (&mut unreadable_dirs, &mut depth_limited_dirs),
            &mut directories,
            None,
        );
    }
    let truncated = files.len() > limit;
    files.truncate(limit);
    let mut notes = Vec::new();
    if unreadable_dirs > 0 {
        notes.push(format!(
            "Codex encountered {unreadable_dirs} errors while reading session directories."
        ));
    }
    if depth_limited_dirs > 0 {
        notes.push(format!(
            "Codex skipped {depth_limited_dirs} session directories beyond the scan depth limit."
        ));
    }
    if truncated {
        notes.push(format!(
            "Codex scan stopped after {limit} newest jsonl files; older sessions are omitted."
        ));
    }
    let candidates = ranked_rollouts(&files, &home.join("sessions"));
    CodexFileScan {
        directories,
        files,
        candidates,
        notes,
        active_entries_complete,
        active_links,
    }
}

fn ranked_rollouts(files: &[PathBuf], active: &Path) -> HashMap<String, Vec<usize>> {
    let mut candidates = HashMap::<String, Vec<usize>>::new();
    for (i, path) in files.iter().enumerate() {
        let Some(id) = path_uuid(path) else {
            continue;
        };
        candidates.entry(id).or_default().push(i);
    }
    for indices in candidates.values_mut() {
        indices.sort_unstable_by(|&a, &b| {
            rollout_priority(&files[b], active).cmp(&rollout_priority(&files[a], active))
        });
    }
    candidates
}

fn ambiguous_generations(scan: &CodexFileScan, id: &str) -> bool {
    let mut paths = scan.paths(id);
    let Some(first) = paths.next().and_then(current_rollout::ids) else {
        return false;
    };
    if paths.all(|path| current_rollout::ids(path) == Some(first)) {
        return false;
    }
    let mut live = scan
        .paths(id)
        .filter(|path| fs::symlink_metadata(path).is_ok_and(|meta| meta.is_file()))
        .filter_map(current_rollout::ids);
    live.next()
        .is_some_and(|first| live.any(|other| other != first))
}

fn select_live_rollout<'a>(
    home: &Path,
    canonical_home: &Path,
    candidates: impl Iterator<Item = &'a Path>,
    invalid: &mut InvalidHeaders,
) -> Option<SelectedRollout> {
    let active = home.join("sessions");
    let archive = home.join("archived_sessions");
    let mut paths = Vec::new();
    for path in candidates {
        let Some(name) = rollout_file::jsonl_file_name(path) else {
            continue;
        };
        paths.extend(representations(&path.with_file_name(name)));
        if path.starts_with(&active) {
            paths.extend(representations(&archive.join(name)));
        } else if path.starts_with(&archive)
            && let Some(plain) = restored_rollout(home, path)
        {
            paths.extend(representations(&plain));
        }
    }
    paths.sort_unstable_by(|a, b| rollout_priority(b, &active).cmp(&rollout_priority(a, &active)));
    paths.dedup();
    let live = paths
        .iter()
        .map(PathBuf::as_path)
        .filter(|path| live_rollout_path(home, canonical_home, path));
    select_rollout(live, CodexSession::parse_observed_metadata, invalid)
}

fn live_rollout_path(home: &Path, canonical_home: &Path, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(home) else {
        return false;
    };
    let Ok(tail) = relative
        .strip_prefix("sessions")
        .or_else(|_| relative.strip_prefix("archived_sessions"))
    else {
        return false;
    };
    // Discovery accepts a file below at most five directories in either rollout root.
    tail.components().count() <= 6
        && tail
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)))
        && path.is_file()
        && fs::canonicalize(path).is_ok_and(|actual| actual == canonical_home.join(relative))
}

// Codex restores archived files into the active directory named by their original date.
fn restored_rollout(home: &Path, archived: &Path) -> Option<PathBuf> {
    let name = rollout_file::jsonl_file_name(archived)?;
    let stamp = name.to_str()?.strip_prefix("rollout-")?.get(..10)?;
    let day = stamp.parse::<chrono::NaiveDate>().ok()?;
    Some(
        home.join("sessions")
            .join(day.format("%Y/%m/%d").to_string())
            .join(name),
    )
}

fn representations(plain: &Path) -> impl Iterator<Item = PathBuf> + '_ {
    ["jsonl", "jsonl.zst", "jsonl.gz"]
        .into_iter()
        .map(|ext| plain.with_extension(ext))
}

// The provider opens active, native rollouts. A backup's mtime does not change that choice.
fn rollout_priority<'a>(
    path: &'a Path,
    active: &Path,
) -> (bool, bool, Option<&'a Path>, bool, &'a Path) {
    (
        path.starts_with(active),
        path.extension().is_some_and(|ext| ext != "gz"),
        rollout_file::jsonl_file_name(path),
        !rollout_file::compressed(path),
        path,
    )
}

fn listing_notes(
    scan: &CodexFileScan,
    listed: usize,
    subagents: usize,
    unreadable_files: usize,
    skipped_metadata: usize,
    first_unreadable: Option<&str>,
) -> Vec<String> {
    let mut notes = scan.notes.clone();
    if unreadable_files > 0 {
        notes.push(match first_unreadable {
            Some(error) if !error.is_empty() => {
                format!("Codex skipped {unreadable_files} unreadable session file(s): {error}.")
            }
            _ => format!("Codex skipped {unreadable_files} unreadable session file(s)."),
        });
    }
    if skipped_metadata > 0 {
        notes.push(format!(
            "Codex skipped {skipped_metadata} session file(s) without readable session metadata."
        ));
    }
    if !scan.files.is_empty() && listed == 0 {
        notes.push(format!(
            "Found {} Codex jsonl file(s) but listed 0 user sessions ({subagents} subagent(s) skipped, {unreadable_files} unreadable, {skipped_metadata} without session metadata).",
            scan.files.len()
        ));
    }
    notes
}

fn compact_discovery_error(error: &anyhow::Error) -> String {
    error
        .to_string()
        .lines()
        .next()
        .unwrap_or("unreadable")
        .chars()
        .take(160)
        .collect()
}

fn collect_jsonl(
    root: &Path,
    depth: usize,
    output: &mut Vec<PathBuf>,
    limit: usize,
    errors: (&mut usize, &mut usize),
    stamps: &mut Vec<(PathBuf, Option<RolloutStamp>)>,
    mut skipped_links: Option<&mut Vec<PathBuf>>,
) {
    if output.len() >= limit {
        return;
    }
    let (unreadable_dirs, depth_limited_dirs) = errors;
    let metadata = fs::symlink_metadata(root);
    if stamps.len() <= SCAN_LIMIT {
        stamps.push((
            root.to_owned(),
            metadata.as_ref().ok().and_then(RolloutStamp::new),
        ));
    }
    if metadata.is_ok_and(|meta| meta.file_type().is_symlink()) {
        *unreadable_dirs = unreadable_dirs.saturating_add(1);
        return;
    }
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return,
        Err(_) => {
            *unreadable_dirs = unreadable_dirs.saturating_add(1);
            return;
        }
    };
    // Retain every directory because even a lexically early directory may be empty, while
    // keeping only the newest file names this branch could contribute to the bounded result.
    // This preserves the old reverse-lexical DFS selection without storing and sorting every
    // path in a wide session directory.
    let file_limit = limit.saturating_sub(output.len());
    let mut directories = Vec::new();
    let mut files = BinaryHeap::<Reverse<OsString>>::new();
    for entry in entries {
        let Ok(entry) = entry else {
            *unreadable_dirs = unreadable_dirs.saturating_add(1);
            continue;
        };
        let Ok(file_type) = entry.file_type() else {
            *unreadable_dirs = unreadable_dirs.saturating_add(1);
            continue;
        };
        if file_type.is_symlink() {
            if let Some(links) = skipped_links.as_deref_mut()
                && links.len() < limit
            {
                links.push(entry.path());
            }
            continue;
        }

        let name = entry.file_name();
        if file_type.is_dir() {
            directories.push(name);
        } else if file_type.is_file() && rollout_file::jsonl_file_name(Path::new(&name)).is_some() {
            if files.len() < file_limit {
                files.push(Reverse(name));
            } else if files
                .peek()
                .is_some_and(|Reverse(smallest)| name.as_os_str() > smallest.as_os_str())
            {
                files.pop();
                files.push(Reverse(name));
            }
        }
    }

    directories.sort_unstable_by(|left, right| right.cmp(left));
    let mut files = files
        .into_iter()
        .map(|Reverse(name)| name)
        .collect::<Vec<_>>();
    files.sort_unstable_by(|left, right| right.cmp(left));
    let mut directories = directories.into_iter().peekable();
    let mut files = files.into_iter().peekable();
    while output.len() < limit {
        let next_is_directory = match (directories.peek(), files.peek()) {
            (Some(directory), Some(file)) => directory > file,
            (Some(_), None) => true,
            _ => false,
        };
        if next_is_directory {
            let directory = root.join(directories.next().expect("peeked directory"));
            if depth > 0 {
                collect_jsonl(
                    &directory,
                    depth - 1,
                    output,
                    limit,
                    (unreadable_dirs, depth_limited_dirs),
                    stamps,
                    skipped_links.as_deref_mut(),
                );
            } else {
                *depth_limited_dirs = depth_limited_dirs.saturating_add(1);
            }
        } else if let Some(file) = files.next() {
            output.push(root.join(file));
        } else {
            break;
        }
    }
}

fn path_uuid(path: &Path) -> Option<String> {
    current_rollout::ids(path).map(|(thread, _)| thread.to_owned())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum CodexHistoryMode {
    Legacy,
    Paginated,
    Unknown,
}

impl CodexHistoryMode {
    fn declared(payload: &Value) -> Self {
        payload.get("history_mode").map_or(Self::Legacy, |value| {
            Self::deserialize(value).unwrap_or(Self::Unknown)
        })
    }

    fn from_payload(payload: &Value) -> Self {
        let mode = Self::declared(payload);
        let prefix = &payload["history_base"];
        match mode {
            // A native legacy resume reads only this file and drops any inherited prefix.
            Self::Legacy if prefix.is_null() => mode,
            Self::Paginated if Self::valid_prefix(prefix) => mode,
            _ => Self::Unknown,
        }
    }

    fn valid_prefix(prefix: &Value) -> bool {
        #[derive(Deserialize)]
        struct HistoryPosition<'a> {
            thread_id: &'a str,
            end_ordinal_exclusive: u64,
            end_byte_offset: u64,
        }

        prefix.is_null()
            || HistoryPosition::deserialize(prefix).is_ok_and(|position| {
                Uuid::parse_str(position.thread_id).is_ok()
                    && position.end_ordinal_exclusive > 0
                    && position.end_byte_offset > 0
            })
    }
}

#[derive(Clone, Copy)]
enum SavedOpenerCheck {
    Withdrawal,
    LegacyOrToolCatalog,
    ToolCatalog,
}

#[derive(Debug)]
struct CodexSession {
    id: String,
    forked_from_id: Option<Uuid>,
    title: Option<String>,
    path: PathBuf,
    observed_revision: Option<SourceRevision>,
    project_path: Option<PathBuf>,
    git_branch: Option<String>,
    cli_version: Option<String>,
    history_mode: CodexHistoryMode,
    is_subagent: bool,
    created_at: Option<DateTime<Utc>>,
    updated_at: Option<DateTime<Utc>>,
}

impl CodexSession {
    fn parse_observed_metadata(path: &Path) -> Result<Option<Self>> {
        let observed = ObservedRollout::read(path)
            .context("selected Codex rollout changed during header lookup")?;
        Self::parse_with_observation(path, observed)
    }

    fn read_unchanged<T>(self, read: impl FnOnce(Self, SourceRevision) -> Result<T>) -> Result<T> {
        let path = self.path.clone();
        let revision = self
            .observed_revision
            .context("selected Codex rollout changed during lookup")?;
        let result = read(self, revision);
        anyhow::ensure!(
            CodexAdapter::index_file_revision(&path) == Some(revision),
            "selected Codex rollout changed during the read"
        );
        result
    }

    fn parse_with_observation(path: &Path, observed: ObservedRollout) -> Result<Option<Self>> {
        let records = rollout_file::header(path)?;
        anyhow::ensure!(
            CodexAdapter::index_file_revision(path) == Some(observed.revision),
            "selected Codex rollout changed during header lookup"
        );
        Ok(records
            .first()
            .and_then(|record| Self::parse_metadata_record(path.to_path_buf(), record, observed)))
    }

    fn parse_metadata_record(
        path: PathBuf,
        record: &Value,
        observed: ObservedRollout,
    ) -> Option<Self> {
        if record.get("type").and_then(Value::as_str) != Some("session_meta") {
            return None;
        }
        let payload = record.get("payload")?;
        let id = string_at(payload, &[&["id"], &["session_id"]])?;
        Uuid::parse_str(id).ok()?;
        if current_rollout::ids(&path).is_some_and(|(thread, _)| thread != id)
            && current_rollout::canonical_name(&path)
        {
            return None;
        }
        Some(Self {
            id: id.to_owned(),
            forked_from_id: string_at(payload, &[&["forked_from_id"]])
                .and_then(|parent| Uuid::parse_str(parent).ok()),
            title: None,
            path,
            observed_revision: Some(observed.revision),
            project_path: string_at(payload, &[&["cwd"]]).map(PathBuf::from),
            git_branch: string_at(payload, &[&["git", "branch"], &["git", "branch_name"]])
                .map(str::to_owned),
            cli_version: string_at(payload, &[&["cli_version"]]).map(str::to_owned),
            history_mode: CodexHistoryMode::from_payload(payload),
            is_subagent: string_at(
                payload,
                &[
                    &["agent_role"],
                    &["agent_path"],
                    &["thread_source", "agent_role"],
                ],
            )
            .is_some()
                || value_at(payload, &[&["source", "subagent"]]).is_some(),
            created_at: parse_timestamp(record.get("timestamp")),
            updated_at: observed.modified,
        })
    }

    fn from_header(path: &Path, header: RolloutHeader, observed: ObservedRollout) -> Self {
        Self {
            id: header.id,
            forked_from_id: header.forked_from_id,
            title: None,
            path: path.to_path_buf(),
            observed_revision: Some(observed.revision),
            project_path: header.project_path,
            git_branch: header.git_branch,
            cli_version: header.cli_version,
            history_mode: header.history_mode,
            is_subagent: header.is_subagent,
            created_at: header.created_at,
            updated_at: observed.modified,
        }
    }

    fn header(&self) -> RolloutHeader {
        RolloutHeader {
            id: self.id.clone(),
            forked_from_id: self.forked_from_id,
            project_path: self.project_path.clone(),
            git_branch: self.git_branch.clone(),
            cli_version: self.cli_version.clone(),
            history_mode: self.history_mode,
            is_subagent: self.is_subagent,
            created_at: self.created_at,
        }
    }

    fn native_session(&self) -> NativeSession {
        NativeSession {
            session: SessionRef::new(Provider::Codex, self.id.clone()),
            title: self.title.clone(),
            project_path: self.project_path.clone(),
            git_branch: self.git_branch.clone(),
            created_at: self.created_at,
            updated_at: self.updated_at,
            updated_at_approximate: false,
            event_count: 0,
            source_path: Some(self.path.clone()),
            source_revision: self.observed_revision,
        }
    }

    fn check_saved_opener(&self, prior_title: &str, check: SavedOpenerCheck) -> Result<bool> {
        let mut builder = self.event_builder();
        builder.plan_preview(None);
        builder.track_saved_opener(prior_title);
        let mut matched_source = false;
        let (events, _) =
            self.canonical_events_into(builder, CodexHistory::preview(), |visit| {
                rollout_file::visit(&self.path, |record, skipped| {
                    matched_source |= match check {
                        SavedOpenerCheck::LegacyOrToolCatalog => {
                            (record["type"] == "session_meta"
                                && record["payload"]["history_mode"] == "legacy")
                                || is_dynamic_tool_catalog(&record)
                        }
                        SavedOpenerCheck::ToolCatalog => is_dynamic_tool_catalog(&record),
                        SavedOpenerCheck::Withdrawal => false,
                    };
                    visit(record, skipped)
                })
            })?;
        Ok(match check {
            SavedOpenerCheck::Withdrawal => events.rollback_title().withdraws(prior_title),
            _ => matched_source && events.confirms_saved_local_opener(prior_title),
        })
    }

    fn withdrew_search_title(&self, prior_title: &str) -> Result<bool> {
        let mut builder = self.event_builder();
        let mut history = CodexHistory::preview();
        let mut opener: Option<(u64, bool)> = None;
        let mut withdrew = false;
        let complete = rollout_file::visit_all_records(&self.path, |record| {
            if history.records_seen == 0 {
                anyhow::ensure!(
                    record["type"] == "session_meta"
                        && string_at(&record, &[&["payload", "id"], &["payload", "session_id"]])
                            == Some(self.id.as_str()),
                    "selected Codex rollout has a different session header"
                );
            }
            // An unknown record or an unreconstructed compaction can change the visible opener.
            if !matches!(
                record["type"].as_str(),
                Some("session_meta" | "turn_context" | "response_item" | "event_msg")
            ) && !is_dynamic_tool_catalog(&record)
            {
                return Ok(false);
            }
            let rollback = history.push(&mut builder, &record)?;
            if let Some(boundary) = rollback
                && let Some((_, matched)) = opener.take_if(|(sequence, _)| *sequence >= boundary)
            {
                withdrew |= matched;
            }
            let events = builder.drain_events();
            if opener.is_none() {
                opener = omnis_core::search_title_opener_from_messages(events.as_slice())
                    .map(|(sequence, title)| (sequence, title.as_deref() == Some(prior_title)));
            }
            drop(events);
            if history
                .records_seen
                .is_multiple_of(TOOL_COMPACTION_RECORD_INTERVAL)
            {
                history.compact(&mut builder);
            }
            Ok(true)
        })?;
        Ok(complete && withdrew && !opener.is_some_and(|(_, matches)| matches))
    }

    fn canonical_events(&self) -> Result<(EventBuilder, Option<PathBuf>)> {
        self.canonical_events_with(|visit| rollout_file::visit(&self.path, visit))
    }

    fn canonical_events_with(
        &self,
        read: impl FnOnce(&mut dyn FnMut(Value, JsonLineSkips) -> Result<()>) -> Result<JsonLineSkips>,
    ) -> Result<(EventBuilder, Option<PathBuf>)> {
        let mut builder = self.event_builder();
        builder.track_rollback_opener(true);
        let (mut events, history) = self.canonical_events_into(
            builder,
            CodexHistory::new(self.project_path.clone()),
            read,
        )?;
        let unknown = history.workspace_unknown() || history.settings.has_owned();
        let project = history.settings.directory(history.project_path);
        if unknown && project.is_none() {
            events.mark_codex_workspace_unknown(self.updated_at);
        }
        Ok((events, project))
    }

    fn canonical_events_into(
        &self,
        mut builder: EventBuilder,
        mut history: CodexHistory,
        read: impl FnOnce(&mut dyn FnMut(Value, JsonLineSkips) -> Result<()>) -> Result<JsonLineSkips>,
    ) -> Result<(EventBuilder, CodexHistory)> {
        let mut unread_ancestor = false;
        let mut first_gap = None;
        let unreadable = read(&mut |record, skipped_before| {
            if builder.tracks_rollback_opener()
                && !ordinary_rollback_record(&record, history.records_seen == 0)
            {
                builder.discard_rollback_opener();
            }
            if skipped_before.total() > 0 && first_gap.is_none() {
                first_gap = Some(builder.checkpoint());
                builder.note_rollback_history_gap();
            }
            history.observe_workspace_gaps(skipped_before.workspace_gaps());
            // The latest settings for this thread survive conversation rollback.
            history.settings.observe_gaps(skipped_before);
            history.settings.observe(&self.id, &record);
            unread_ancestor |= records_history_base(&record);
            history.push(&mut builder, &record)?;
            if history
                .records_seen
                .checked_rem(TOOL_COMPACTION_RECORD_INTERVAL)
                == Some(0)
            {
                history.compact(&mut builder);
            }
            Ok(())
        })?;
        history.observe_workspace_gaps(unreadable.workspace_gaps());
        history.settings.observe_gaps(unreadable);
        let prefix_end = first_gap.unwrap_or_else(|| builder.checkpoint());
        history.compact(&mut builder);
        if unreadable.total() != 0 {
            builder.note_rollback_history_gap();
        }
        if unread_ancestor || self.is_subagent {
            builder.discard_rollback_opener();
        }
        if history.omitted_events > 0 {
            builder.push(
                EventKind::ProviderEvent,
                json!({
                    "omitted_events": history.omitted_events,
                    "retained_latest_events": MAX_CANONICAL_EVENTS,
                }),
                self.updated_at,
                ReplayPolicy::HistoricalOnly,
                Some("omnisession.codex_event_limit".to_owned()),
                None,
            );
        }
        let omitted_tool_events = history.omitted_tool_events;
        if omitted_tool_events > 0 {
            builder.push(
                EventKind::ProviderEvent,
                json!({
                    "omitted_events": omitted_tool_events,
                    "omitted_tool_events": omitted_tool_events,
                    "event_kind": "tool",
                    "retained_latest_tool_events": MAX_CANONICAL_TOOL_EVENTS,
                }),
                self.updated_at,
                ReplayPolicy::HistoricalOnly,
                Some("omnisession.codex_tool_limit".to_owned()),
                None,
            );
        }
        builder.push_json_line_omissions(
            unreadable,
            MAX_STREAMED_TRANSCRIPT_LINE_SIZE,
            Some(prefix_end),
            self.updated_at,
        );
        if unread_ancestor {
            history.workspace_history_incomplete |=
                history.workspace_record.is_none_or(|record| record <= 1);
            report_unread_ancestor(&mut builder, self.updated_at);
        }
        Ok((builder, history))
    }

    fn branch_at(&self, project: Option<&Path>) -> Option<String> {
        (project.is_some() && project == self.project_path.as_deref())
            .then(|| self.git_branch.clone())
            .flatten()
    }

    fn read(self, session: &SessionRef) -> Result<(omnis_ir::CanonicalSnapshot, RollbackTitle)> {
        let captured_at = self.updated_at.unwrap_or_else(Utc::now);
        let (events, project_path) = self.canonical_events()?;
        let branch = self.branch_at(project_path.as_deref());
        let title = events.rollback_title();
        Ok((
            events.snapshot(
                session.clone(),
                self.title,
                project_path,
                branch,
                captured_at,
            ),
            title,
        ))
    }

    fn preview(
        self,
        session: &SessionRef,
    ) -> Result<(
        omnis_ir::CanonicalSnapshot,
        RollbackTitle,
        CodexWorkspaceProjection,
    )> {
        let captured_at = self.updated_at.unwrap_or_else(Utc::now);
        if rollout_file::compressed(&self.path) {
            let read = self.compressed_sample(session, MAX_PREVIEW_TAIL_SIZE)?;
            return Ok((
                read.snapshot,
                read.rollback_title,
                read.workspace_projection,
            ));
        }
        let scan = self.scan_workspace()?;
        let (events, project, reconstructed) = if scan.reconstruct_history {
            self.reconstructed_preview(rollout_file::LIMITS)?
        } else {
            self.preview_events(scan.directory)?
        };
        let branch = self.branch_at(project.as_deref());
        let title = events.rollback_title();
        let workspace =
            CodexWorkspaceProjection::from_reconstruction(reconstructed, project.as_deref());
        Ok((
            events.snapshot(session.clone(), self.title, project, branch, captured_at),
            title,
            workspace,
        ))
    }

    fn index(
        self,
        session: &SessionRef,
        full_read_bytes: u64,
        source_revision: SourceRevision,
    ) -> Result<IndexRead> {
        let native_session = (!self.is_subagent).then(|| self.native_session());
        if full_read_bytes != u64::MAX && rollout_file::compressed(&self.path) {
            let mut read = self.compressed_sample(session, full_read_bytes)?;
            read.source_revision = source_revision;
            read.native_session = native_session;
            return Ok(read);
        }
        let source_complete = fs::metadata(&self.path)?.len() <= full_read_bytes;
        let (snapshot, rollback_title, workspace_projection) = if source_complete {
            let (snapshot, title) = self.read(session)?;
            (snapshot, title, CodexWorkspaceProjection::Unchecked)
        } else {
            self.preview(session)?
        };
        Ok(IndexRead {
            native_session,
            snapshot,
            rollback_title,
            workspace_projection,
            source_complete,
            provider_limited: false,
            source_revision,
        })
    }

    fn compressed_sample(self, session: &SessionRef, budget: u64) -> Result<IndexRead> {
        let captured_at = self.updated_at.unwrap_or_else(Utc::now);
        let (events, project, source_complete, workspace_reconstructed) =
            match rollout_file::sample(&self.path, budget, self.project_path.as_deref(), &self.id)
                .context("sampling compressed Codex rollout")?
            {
                rollout_file::Sample::Complete(bytes) => {
                    let (events, project) = self.canonical_events_with(|visit| {
                        rollout_file::read_records(bytes.as_slice(), visit)
                    })?;
                    (events, project, true, false)
                }
                rollout_file::Sample::Reconstruct => {
                    let (events, project, workspace_reconstructed) =
                        self.reconstructed_preview(rollout_file::LIMITS)?;
                    (events, project, false, workspace_reconstructed)
                }
                rollout_file::Sample::Edges {
                    records,
                    omitted,
                    unreadable,
                    directory,
                } => {
                    let mut events = self.events_from_records(records);
                    events.push_json_line_omissions(
                        unreadable,
                        MAX_STREAMED_TRANSCRIPT_LINE_SIZE,
                        None,
                        self.updated_at,
                    );
                    if omitted > 0 {
                        events.push(
                            EventKind::ProviderEvent,
                            json!({"omitted_events": omitted}),
                            self.updated_at,
                            ReplayPolicy::HistoricalOnly,
                            Some("omnisession.preview_limit".to_owned()),
                            None,
                        );
                    }
                    (events, directory, false, true)
                }
            };
        let branch = self.branch_at(project.as_deref());
        let rollback_title = events.rollback_title();
        let workspace_projection = CodexWorkspaceProjection::from_reconstruction(
            workspace_reconstructed,
            project.as_deref(),
        );
        let snapshot = events.snapshot(session.clone(), self.title, project, branch, captured_at);
        Ok(IndexRead {
            native_session: None,
            workspace_projection,
            rollback_title,
            snapshot,
            source_complete,
            provider_limited: false,
            source_revision: SourceRevision::default(),
        })
    }

    fn reconstructed_preview(
        &self,
        limits: crate::support::JsonLinesLimits,
    ) -> Result<(EventBuilder, Option<PathBuf>, bool)> {
        let stamp = || fs::metadata(&self.path).map(|metadata| RolloutStamp::new(&metadata));
        let before = stamp()?;
        let per_edge = usize::try_from(MAX_PREVIEW_TAIL_SIZE)?;
        let mut builder = self.event_builder();
        builder.plan_preview(None);
        builder.track_rollback_opener(false);
        let ((selected, omitted), workspace_record, first_settings, first_title) = {
            let (plan, history) =
                self.canonical_events_into(builder, CodexHistory::preview(), |visit| {
                    rollout_file::visit_with_limits(&self.path, limits, visit)
                })?;
            (
                plan.select_preview(rollout_file::SAMPLE_RECORDS, per_edge),
                history.workspace_record,
                history.settings,
                plan.rollback_title(),
            )
        };
        let expected = selected.len();
        let mut builder = self.event_builder();
        builder.plan_preview(Some(selected.clone()));
        builder.track_rollback_opener(false);
        let mut project = None;
        let mut position = 0_usize;
        let (mut events, history) =
            self.canonical_events_into(builder, CodexHistory::preview(), |visit| {
                rollout_file::visit_with_limits(&self.path, limits, |record, skipped_before| {
                    position += 1;
                    if workspace_record == Some(position) {
                        project = string_at(&record, &[&["payload", "cwd"]]).map(PathBuf::from);
                    }
                    visit(record, skipped_before)
                })
            })?;
        if stamp()? != before
            || history.workspace_record != workspace_record
            || history.settings != first_settings
            || events.rollback_title() != first_title
            || events.select_preview(rollout_file::SAMPLE_RECORDS, per_edge) != (selected, omitted)
        {
            return Err(anyhow!(
                "Codex rollout changed while reconstructing its preview"
            ));
        }
        events.finish_preview(expected)?;
        let project = history.settings.directory(project);
        let owned_settings = history.settings.has_owned();
        if project.is_none() && (history.workspace_unknown() || owned_settings) {
            events.mark_codex_workspace_unknown(self.updated_at);
        }
        if omitted > 0 {
            events.push(
                EventKind::ProviderEvent,
                json!({"omitted_events": omitted}),
                self.updated_at,
                ReplayPolicy::HistoricalOnly,
                Some("omnisession.preview_limit".to_owned()),
                None,
            );
        }
        Ok((
            events,
            project,
            owned_settings || !history.workspace_history_incomplete,
        ))
    }

    fn preview_events(
        &self,
        directory: Option<PathBuf>,
    ) -> Result<(EventBuilder, Option<PathBuf>, bool)> {
        const SAMPLE_RECORDS: usize = 1_024;
        let preview = json_lines_preview_with_omissions(
            &self.path,
            SAMPLE_RECORDS,
            MAX_PREVIEW_TAIL_SIZE,
            MAX_STREAMED_TRANSCRIPT_LINE_SIZE,
            rollout_file::workspace_record_candidate,
        )?;
        if preview.omissions.workspace_gaps() > 0 {
            return self.reconstructed_preview(rollout_file::LIMITS);
        }
        let mut events = self.events_from_records(preview.records);
        events.push_json_line_omissions(
            preview.omissions,
            MAX_STREAMED_TRANSCRIPT_LINE_SIZE,
            None,
            self.updated_at,
        );
        if preview.sampled {
            events.push(
                EventKind::ProviderEvent,
                json!({"native_history_unverified": true}),
                self.updated_at,
                ReplayPolicy::HistoricalOnly,
                Some("omnisession.preview_limit".to_owned()),
                None,
            );
        }
        Ok((events, directory, true))
    }

    fn events_from_records(&self, records: Vec<Value>) -> EventBuilder {
        let mut builder = self.event_builder();
        let mut last_visible: Option<(EventKind, String, &'static str)> = None;
        let unread_ancestor = records.iter().any(records_history_base);
        for record in records {
            push_record(&mut builder, &record, &mut last_visible);
        }
        if unread_ancestor {
            report_unread_ancestor(&mut builder, self.updated_at);
        }
        builder
    }

    fn event_builder(&self) -> EventBuilder {
        let mut builder = EventBuilder::new(Provider::Codex, &self.id);
        builder.set_provider_version(self.cli_version.clone());
        builder
    }
}

struct TurnCheckpoint {
    sequence: u64,
    project_path: Option<PathBuf>,
    workspace_record: Option<usize>,
    tool_events: usize,
}

struct CodexHistory {
    project_path: Option<PathBuf>,
    settings: workspace_check::WorkspaceSettings,
    workspace_record: Option<usize>,
    retain_workspace_path: bool,
    workspace_gaps: usize,
    records_before_workspace_gap: usize,
    records_seen: usize,
    last_visible: Option<(EventKind, String, &'static str)>,
    turns: Vec<TurnCheckpoint>,
    pruned_turn_checkpoints: bool,
    workspace_history_incomplete: bool,
    pending_turn: Option<TurnCheckpoint>,
    tool_events: usize,
    omitted_tool_events: usize,
    omitted_events: usize,
}

impl CodexHistory {
    fn new(project_path: Option<PathBuf>) -> Self {
        Self {
            project_path,
            settings: workspace_check::WorkspaceSettings::from_header(None),
            workspace_record: Some(1),
            retain_workspace_path: true,
            workspace_gaps: 0,
            records_before_workspace_gap: 0,
            records_seen: 0,
            last_visible: None,
            turns: Vec::new(),
            pruned_turn_checkpoints: false,
            workspace_history_incomplete: false,
            pending_turn: None,
            tool_events: 0,
            omitted_tool_events: 0,
            omitted_events: 0,
        }
    }

    /// During preview, a checkpoint holds the position of the cwd record. The second pass reads the surviving path.
    fn preview() -> Self {
        Self {
            retain_workspace_path: false,
            ..Self::new(None)
        }
    }

    fn workspace_unknown(&self) -> bool {
        self.workspace_gaps > 0 && self.workspace_record.is_none()
    }

    fn observe_workspace_gaps(&mut self, skipped: usize) {
        if skipped != self.workspace_gaps {
            self.workspace_gaps = skipped;
            // Only a cwd recorded after this gap can establish the workspace again.
            self.records_before_workspace_gap = self.records_seen;
            self.workspace_record = None;
            self.project_path = None;
        }
    }

    /// Applies the retained tool event, event, and turn checkpoint caps.
    fn compact(&mut self, builder: &mut EventBuilder) {
        self.omitted_tool_events += builder.retain_latest_tool_events(MAX_CANONICAL_TOOL_EVENTS);
        self.omitted_events += builder.retain_latest_events(MAX_CANONICAL_EVENTS);
        let excess = self.turns.len().saturating_sub(MAX_CANONICAL_EVENTS);
        if excess > 0 || self.omitted_events > 0 || self.omitted_tool_events > 0 {
            builder.discard_rollback_opener();
        }
        self.pruned_turn_checkpoints |= excess > 0;
        self.turns.drain(..excess);
    }

    fn checkpoint(&self, builder: &EventBuilder) -> TurnCheckpoint {
        TurnCheckpoint {
            sequence: builder.checkpoint(),
            project_path: self.project_path.clone(),
            workspace_record: self.workspace_record,
            tool_events: self.tool_events,
        }
    }

    fn push(&mut self, builder: &mut EventBuilder, record: &Value) -> Result<Option<u64>> {
        self.records_seen += 1;
        let record_type = record.get("type").and_then(Value::as_str);
        let event_type = (record_type == Some("event_msg"))
            .then(|| string_at(record, &[&["payload", "type"]]))
            .flatten();
        if event_type == Some("thread_rolled_back") {
            let count = record["payload"]["num_turns"]
                .as_u64()
                .context("Codex rollback marker has no valid num_turns")?;
            let mut truncated = None;
            if count > 0 {
                let count = usize::try_from(count).unwrap_or(usize::MAX);
                if self.pruned_turn_checkpoints && count > self.turns.len() {
                    self.workspace_history_incomplete = true;
                }
                let retained = self.turns.len().saturating_sub(count);
                if retained < self.turns.len() {
                    let checkpoint = self.turns.remove(retained);
                    self.turns.truncate(retained);
                    builder.truncate_from(checkpoint.sequence);
                    truncated = Some(checkpoint.sequence);
                    self.workspace_record = checkpoint
                        .workspace_record
                        .filter(|position| *position > self.records_before_workspace_gap);
                    self.project_path = self.workspace_record.and(checkpoint.project_path);
                    self.tool_events = checkpoint.tool_events;
                    self.omitted_tool_events = self.omitted_tool_events.min(self.tool_events);
                } else if self.workspace_gaps > 0 {
                    self.project_path = None;
                    self.workspace_record = None;
                }
                self.last_visible = None;
                self.pending_turn = None;
            }
            return Ok(truncated);
        }
        if matches!(event_type, Some("task_started" | "turn_started")) {
            self.pending_turn = Some(self.checkpoint(builder));
            self.last_visible = None;
        } else if matches!(
            event_type,
            Some("task_complete" | "turn_complete" | "turn_aborted")
        ) {
            self.pending_turn = None;
            self.last_visible = None;
        }
        if record_type == Some("turn_context") {
            if self.pending_turn.is_none()
                && (self.workspace_unknown()
                    || self
                        .last_visible
                        .as_ref()
                        .is_none_or(|(kind, _, _)| *kind != EventKind::MessageUser))
            {
                self.pending_turn = Some(self.checkpoint(builder));
            }
            if let Some(cwd) = string_at(record, &[&["payload", "cwd"]]) {
                self.workspace_record = Some(self.records_seen);
                if self.retain_workspace_path {
                    self.project_path = Some(PathBuf::from(cwd));
                }
            }
        }
        let checkpoint = self.checkpoint(builder);
        if push_record(builder, record, &mut self.last_visible) {
            self.turns
                .push(self.pending_turn.take().unwrap_or(checkpoint));
        } else if record_type == Some("response_item")
            && record["payload"]["type"] != "message"
            && builder.checkpoint() > checkpoint.sequence
        {
            self.tool_events += 1;
        }
        Ok(None)
    }
}

fn ordinary_rollback_record(record: &Value, first: bool) -> bool {
    let payload = &record["payload"];
    match record["type"].as_str() {
        Some("session_meta") => {
            first
                && payload.is_object()
                && payload["history_base"].is_null()
                && payload["forked_from_id"].is_null()
                && payload
                    .get("history_mode")
                    .is_none_or(|mode| mode == "legacy")
        }
        Some("event_msg" | "response_item") => !first && payload["type"].is_string(),
        Some("turn_context") => !first && payload.is_object(),
        Some("dynamic_tools_updated") => !first && is_dynamic_tool_catalog(record),
        _ => false,
    }
}

fn is_dynamic_tool_catalog(record: &Value) -> bool {
    let payload = &record["payload"];
    record["type"] == "dynamic_tools_updated"
        && payload["thread_id"]
            .as_str()
            .is_some_and(|id| Uuid::parse_str(id).is_ok())
        && payload["tools"].as_array().is_some_and(|tools| {
            tools
                .iter()
                .all(|tool| tool["name"].is_string() && tool["description"].is_string())
        })
}

fn records_history_base(record: &Value) -> bool {
    record["type"] == "session_meta" && !record["payload"]["history_base"].is_null()
}

fn report_unread_ancestor(builder: &mut EventBuilder, timestamp: Option<DateTime<Utc>>) {
    builder.push(
        EventKind::ProviderEvent,
        json!({"native_history_unverified":true,"reason":"The rollout references earlier history that was not read."}),
        timestamp,
        ReplayPolicy::HistoricalOnly,
        Some(UNAVAILABLE_ANCESTRY.to_owned()),
        None,
    );
}

fn push_record(
    builder: &mut EventBuilder,
    record: &Value,
    last_visible: &mut Option<(EventKind, String, &'static str)>,
) -> bool {
    let timestamp = parse_timestamp(record.get("timestamp"));
    let kind = record.get("type").and_then(Value::as_str);
    match kind {
        Some("compacted") => builder.push(
            EventKind::ProviderEvent,
            json!({"native_history_unverified":true,"reason":"The active Codex compaction checkpoint was not reconstructed."}),
            timestamp,
            ReplayPolicy::HistoricalOnly,
            Some("omnisession.codex_compacted".to_owned()),
            None,
        ),
        Some("session_meta" | "turn_context") => {
            if let Some(payload) = record.get("payload") {
                push_codex_session_metadata(builder, payload, timestamp, false);
            }
        }
        Some("response_item") => {
            if let Some(payload) = record.get("payload") {
                return push_response_item(builder, payload, timestamp, last_visible);
            }
        }
        Some("event_msg") => {
            if let Some(payload) = record.get("payload") {
                push_codex_session_metadata(builder, payload, timestamp, true);
                return push_event_message(builder, payload, timestamp, last_visible);
            }
        }
        _ => {}
    }
    false
}

fn push_codex_session_metadata(
    builder: &mut EventBuilder,
    payload: &Value,
    timestamp: Option<DateTime<Utc>>,
    cumulative_usage: bool,
) {
    let model = string_at(
        payload,
        &[
            &["model"],
            &["model_id"],
            &["model_slug"],
            &["info", "model"],
        ],
    );
    let reasoning_mode = string_at(
        payload,
        &[
            &["effort"],
            &["reasoning_effort"],
            &["reasoning", "effort"],
            &["info", "reasoning_effort"],
        ],
    );
    let total_tokens = value_at(
        payload,
        &[
            &["total_tokens"],
            &["total_token_usage", "total_tokens"],
            &["info", "total_token_usage", "total_tokens"],
            &["usage", "total_tokens"],
        ],
    )
    .and_then(Value::as_u64);
    if model.is_none() && reasoning_mode.is_none() && total_tokens.is_none() {
        return;
    }
    builder.push(
        EventKind::ProviderEvent,
        json!({
            "model": model,
            "reasoning_mode": reasoning_mode,
            "total_tokens": total_tokens,
            "token_usage": if cumulative_usage { "cumulative" } else { "incremental" },
        }),
        timestamp,
        ReplayPolicy::HistoricalOnly,
        Some("omnisession.session_metadata".to_owned()),
        None,
    );
}

fn push_message_text(
    builder: &mut EventBuilder,
    kind: EventKind,
    text: &str,
    timestamp: Option<DateTime<Utc>>,
    raw_type: &'static str,
    last_visible: &mut Option<(EventKind, String, &'static str)>,
) -> bool {
    let mirrored = last_visible
        .as_ref()
        .is_some_and(|(last_kind, last_text, last_raw_type)| {
            last_kind == &kind && last_text == text && last_raw_type != &raw_type
        });
    if mirrored {
        return false;
    }
    *last_visible = Some((kind.clone(), text.to_owned(), raw_type));
    let user_turn = kind == EventKind::MessageUser && !is_contextual_user_text(text);
    if text.is_empty() {
        return user_turn;
    }
    builder.push(
        kind,
        json!({ "text": text }),
        timestamp,
        ReplayPolicy::Contextual,
        Some(raw_type.to_owned()),
        None,
    );
    user_turn
}

fn is_contextual_user_text(text: &str) -> bool {
    let text = text.trim_start();
    [
        "# AGENTS.md instructions for ",
        "<environment_context>",
        "<turn_aborted>",
        "<user_shell_command>",
        "<subagent_notification>",
    ]
    .iter()
    .any(|prefix| text.starts_with(prefix))
}

/// Visible text parts of a message payload and how many images it carries.
///
/// Codex wraps local images in tag texts that the user never typed, so those tags are dropped.
fn message_parts(payload: &Value) -> (Vec<&str>, usize) {
    let content = payload
        .get("content")
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice);
    let part_type = |index: Option<usize>| {
        index
            .and_then(|index| content.get(index))
            .and_then(|part| part.get("type"))
            .and_then(Value::as_str)
    };
    let images = (0..content.len())
        .filter(|index| part_type(Some(*index)) == Some("input_image"))
        .count();
    let parts = content
        .iter()
        .enumerate()
        .filter(|(index, _)| {
            matches!(
                part_type(Some(*index)),
                Some("input_text" | "output_text" | "text")
            )
        })
        .filter_map(|(index, part)| Some((index, part.get("text").and_then(Value::as_str)?)))
        .filter(|(index, text)| {
            !is_image_wrapper(
                text,
                part_type(index.checked_sub(1)) == Some("input_image"),
                part_type(Some(index + 1)) == Some("input_image"),
            )
        })
        .map(|(_, text)| text)
        .collect();
    (parts, images)
}

fn push_response_item(
    builder: &mut EventBuilder,
    payload: &Value,
    timestamp: Option<DateTime<Utc>>,
    last_visible: &mut Option<(EventKind, String, &'static str)>,
) -> bool {
    let item_type = payload.get("type").and_then(Value::as_str).unwrap_or("");
    if item_type == "message" {
        let kind = match payload.get("role").and_then(Value::as_str) {
            Some("user") => Some(EventKind::MessageUser),
            Some("assistant") => Some(EventKind::MessageAssistant),
            _ => None,
        };
        let Some(kind) = kind else {
            return false;
        };
        if let Some(text) = payload.get("content").and_then(Value::as_str) {
            return push_message_text(
                builder,
                kind,
                text,
                timestamp,
                "response_item.message",
                last_visible,
            );
        }
        let (parts, images) = message_parts(payload);
        let contextual = parts.iter().any(|text| is_contextual_user_text(text));
        // An image-only turn keeps a placeholder so the reply still follows a request.
        let text = if parts.is_empty() && images > 0 && kind == EventKind::MessageUser {
            omitted_images_text(images)
        } else {
            parts.join("\n")
        };
        return push_message_text(
            builder,
            kind,
            &text,
            timestamp,
            "response_item.message",
            last_visible,
        ) && !contextual;
    }

    let kind = match item_type {
        "function_call" | "custom_tool_call" | "web_search_call" | "computer_call" => {
            Some(EventKind::ToolCalled)
        }
        "local_shell_call" => Some(EventKind::CommandExecuted),
        "function_call_output"
        | "custom_tool_call_output"
        | "local_shell_call_output"
        | "computer_call_output" => {
            let failed = payload
                .get("status")
                .and_then(Value::as_str)
                .is_some_and(|status| matches!(status, "failed" | "error"));
            Some(if failed {
                EventKind::ToolFailed
            } else {
                EventKind::ToolCompleted
            })
        }
        _ => None,
    };
    let Some(kind) = kind else {
        return false;
    };
    builder.push(
        kind,
        compact_tool_value(payload, None),
        timestamp,
        ReplayPolicy::HistoricalOnly,
        Some(format!("response_item.{item_type}")),
        None,
    );
    *last_visible = None;
    false
}

fn compact_tool_value(value: &Value, field: Option<&str>) -> Value {
    if field.is_some_and(|field| {
        matches!(
            field,
            "image_url"
                | "data"
                | "blob"
                | "bytes"
                | "encrypted_content"
                | "internal_chat_message_metadata_passthrough"
        )
    }) {
        let mut omitted = Map::from_iter([("content_omitted".to_owned(), Value::Bool(true))]);
        if let Some(value) = value.as_str() {
            omitted.insert("original_bytes".to_owned(), Value::from(value.len()));
        } else if let Some(value) = value.as_array() {
            omitted.insert("original_items".to_owned(), Value::from(value.len()));
        }
        return Value::Object(omitted);
    }
    match value {
        Value::String(value) => Value::String(compact_tool_string(value)),
        Value::Array(values) if values.len() > MAX_TOOL_ARRAY_ITEMS => {
            let edge = MAX_TOOL_ARRAY_ITEMS / 2;
            let mut compact = values
                .iter()
                .take(edge)
                .map(|value| compact_tool_value(value, None))
                .collect::<Vec<_>>();
            compact.push(json!({
                "content_omitted": true,
                "omitted_items": values.len() - MAX_TOOL_ARRAY_ITEMS,
            }));
            compact.extend(
                values
                    .iter()
                    .skip(values.len() - edge)
                    .map(|value| compact_tool_value(value, None)),
            );
            Value::Array(compact)
        }
        Value::Array(values) => Value::Array(
            values
                .iter()
                .map(|value| compact_tool_value(value, None))
                .collect(),
        ),
        Value::Object(values) => Value::Object(
            values
                .iter()
                .map(|(field, value)| (field.clone(), compact_tool_value(value, Some(field))))
                .collect::<Map<_, _>>(),
        ),
        _ => value.clone(),
    }
}

fn compact_tool_string(value: &str) -> String {
    let character_count = value.chars().count();
    if character_count <= MAX_TOOL_STRING_CHARACTERS {
        return value.to_owned();
    }
    let edge = MAX_TOOL_STRING_CHARACTERS / 2;
    let prefix = value.chars().take(edge).collect::<String>();
    let suffix = value
        .chars()
        .rev()
        .take(edge)
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>();
    format!(
        "{prefix}\n[{} characters omitted]\n{suffix}",
        character_count - MAX_TOOL_STRING_CHARACTERS
    )
}

fn push_event_message(
    builder: &mut EventBuilder,
    payload: &Value,
    timestamp: Option<DateTime<Utc>>,
    last_visible: &mut Option<(EventKind, String, &'static str)>,
) -> bool {
    let event_type = payload.get("type").and_then(Value::as_str).unwrap_or("");
    let kind = match event_type {
        "user_message" => Some(EventKind::MessageUser),
        "agent_message" => Some(EventKind::MessageAssistant),
        _ => None,
    };
    let Some(kind) = kind else {
        return false;
    };
    let text = string_at(payload, &[&["message"], &["text"]]).unwrap_or("");
    // The import notification has no response item. A literal reply with the same
    // text has one, so let the response item supply its role and content.
    if kind == EventKind::MessageAssistant && text == omnis_core::CODEX_IMPORT_MARKER {
        return false;
    }
    let images = ["images", "local_images"]
        .into_iter()
        .filter_map(|field| payload.get(field).and_then(Value::as_array))
        .map(Vec::len)
        .sum::<usize>();
    if text.is_empty() && images > 0 && kind == EventKind::MessageUser {
        let placeholder = omitted_images_text(images);
        return push_message_text(
            builder,
            kind,
            &placeholder,
            timestamp,
            "event_msg",
            last_visible,
        );
    }
    push_message_text(builder, kind, text, timestamp, "event_msg", last_visible)
}

/// Whether `text` is Codex's tag around an adjacent image part (`codex-rs/protocol/src/models.rs`).
///
/// A tag the user typed without an image beside it stays visible.
fn is_image_wrapper(text: &str, image_before: bool, image_after: bool) -> bool {
    let opens = text == "<image>"
        || text
            .strip_prefix("<image name=")
            .is_some_and(|rest| rest.ends_with('>'));
    (opens && image_after) || (text == "</image>" && image_before)
}

impl ProviderAdapter for CodexAdapter {
    fn provider(&self) -> Provider {
        Provider::Codex
    }

    fn fork_candidates_created_since(
        &self,
        source: &SessionRef,
        project: &Path,
        started_at: DateTime<Utc>,
    ) -> Result<Vec<SessionRef>> {
        Self::fork_candidates_created_since(self, source, project, started_at)
    }

    fn probe(&self) -> ProviderInstallation {
        let executable = provider_executable(Provider::Codex);
        ProviderInstallation {
            provider: Provider::Codex,
            installed: executable.is_some() || self.codex_home.as_deref().is_some_and(Path::is_dir),
            executable,
            data_root: self.codex_home.clone(),
        }
    }

    fn discovery_notes(&self) -> Vec<String> {
        self.listing(false).0.notes.clone()
    }

    fn discovery_is_complete(&self) -> bool {
        // A missing Codex home is not evidence that previously cached sessions were deleted.
        self.codex_home.as_deref().is_some_and(Path::is_dir)
            && self.listing(false).0.notes.is_empty()
    }

    fn unavailable_codex_rollouts(
        &self,
        prior: &[(String, Option<SourceRevision>)],
    ) -> Result<HashSet<String>> {
        let Some(home) = self.codex_home.as_deref().filter(|home| home.is_dir()) else {
            return Ok(HashSet::new());
        };
        let cached = self
            .listing
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let valid = cached.as_ref().filter(|cached| {
            cached.database.is_some() && cached.database == self.current.stamp(home)
        });
        let mut unavailable = HashSet::new();
        let prior = prior
            .iter()
            .filter(|(id, prior)| {
                let known = valid.and_then(|cached| cached.listing.selected_files.get(id.as_str()));
                if let Some((path, selected)) = known
                    && Self::index_file_revision(path) == Some(*selected)
                {
                    if prior
                        .and_then(|revision| revision.file)
                        .is_some_and(|file| Some(file) != selected.file)
                    {
                        unavailable.insert(id.clone());
                    }
                    return false;
                }
                true
            })
            .collect::<Vec<_>>();
        drop(cached);
        if prior.is_empty() {
            return Ok(unavailable);
        }
        let rows = self
            .current
            .rows(home, prior.iter().map(|(id, _)| id.as_str()))?;
        let (scan, _) = self.session_scan(None);
        for (id, revision) in prior {
            match rows.get(id) {
                Some(row) if row.requires_selection(home) => {
                    match row.resolve(home, id, scan.paths(id), scan.notes.is_empty()) {
                        Err(error) if error.is::<CurrentRolloutUnavailable>() => {
                            unavailable.insert(id.clone());
                        }
                        Err(error) if error.is::<current_rollout::CurrentRolloutUnconfirmed>() => {
                            if row.has_legacy_name() || row.outside_home(home) {
                                unavailable.insert(id.clone());
                            } else if let Some(selected) = row.generation(id)
                                && let Some(file) = revision.and_then(|revision| revision.file)
                                && scan.paths(id).any(|path| {
                                    current_rollout::ids(path)
                                        .is_some_and(|(_, generation)| generation != selected)
                                        && Self::index_file_id(path) == Some(file)
                                })
                            {
                                unavailable.insert(id.clone());
                            }
                        }
                        Err(error) => return Err(error),
                        Ok(Some(selected))
                            if revision
                                .and_then(|revision| revision.file)
                                .is_some_and(|file| Some(file) != selected.revision.file) =>
                        {
                            unavailable.insert(id.clone());
                        }
                        Ok(_) => {}
                    }
                }
                None if ambiguous_generations(&scan, id) => {
                    unavailable.insert(id.clone());
                }
                _ => {}
            }
        }
        Ok(unavailable)
    }

    fn list_sessions(&self, project: Option<&Path>) -> Result<Vec<NativeSession>> {
        Ok(self.list_project_candidates(project, false).0)
    }

    fn project_search_candidates(
        &self,
        project: &Path,
    ) -> Result<(Vec<NativeSession>, Vec<String>)> {
        let (sessions, unchecked) = self.list_project_candidates(Some(project), true);
        let mut notes = self.discovery_notes();
        if unchecked > 0 {
            notes.push(format!("Codex could not check {unchecked} other-workspace rollout(s) for a directory change within this search. Run `omni index --provider codex` to refresh their workspaces."));
        }
        Ok((sessions, notes))
    }

    /// Reads the one matching rollout, so subagent threads that listings hide stay reachable.
    fn find_native_session(&self, id: &str) -> Result<Option<NativeSession>> {
        if Uuid::parse_str(id).is_err() {
            return Ok(None);
        }
        let Some(selected) = self.resolve_rollout(id, None, true)? else {
            return Ok(None);
        };
        let mut native = selected.session.native_session();
        if self.use_workspace_index
            && let Some(file) = native.source_revision.and_then(|revision| revision.file)
            && let Ok(workspaces) = omnis_store::CodexWorkspace::discovery_index(Some(file))
        {
            native.apply_codex_workspace_index(&workspaces, false);
        }
        Ok(Some(native))
    }

    fn native_resume_support(&self, session: &SessionRef) -> Result<NativeResumeSupport> {
        validate_provider(session, Provider::Codex)?;
        let selected = self.require_rollout(&session.id, None)?;
        let path = selected.session.path;
        let archived = self
            .codex_home
            .as_ref()
            .is_some_and(|home| path.starts_with(home.join("archived_sessions")));
        let compressed = rollout_file::compressed(&path);
        Ok(
            if selected.shadowed
                || archived
                || path.extension().is_some_and(|ext| ext == "gz")
                || (compressed && fs::symlink_metadata(path.with_extension("")).is_ok())
                || (!selected.from_registry
                    && !current_launch::source_filename_selects(self, &session.id, &path)?)
            {
                NativeResumeSupport::ImportOnly
            } else {
                match selected.session.history_mode {
                    CodexHistoryMode::Paginated if current_rollout::canonical_name(&path) => {
                        NativeResumeSupport::Since("0.167.0")
                    }
                    CodexHistoryMode::Legacy if compressed => NativeResumeSupport::Since("0.137.0"),
                    CodexHistoryMode::Legacy => NativeResumeSupport::Direct,
                    CodexHistoryMode::Paginated | CodexHistoryMode::Unknown => {
                        NativeResumeSupport::ImportOnly
                    }
                }
            },
        )
    }

    fn read_session(&self, session: &SessionRef) -> Result<omnis_ir::CanonicalSnapshot> {
        validate_provider(session, Provider::Codex)?;
        self.read_selected(self.find_session(&session.id, None)?, |native, _| {
            native.read(session)
        })
        .map(|(snapshot, _)| snapshot)
    }

    fn preview_session(&self, session: &SessionRef) -> Result<omnis_ir::CanonicalSnapshot> {
        validate_provider(session, Provider::Codex)?;
        self.read_selected(self.find_session(&session.id, None)?, |native, _| {
            native.preview(session)
        })
        .map(|(snapshot, _, _)| snapshot)
    }

    fn read_full_index_at(
        &self,
        session: &SessionRef,
        source_path: Option<&Path>,
    ) -> Result<IndexRead> {
        self.index_session(session, source_path, u64::MAX)
    }

    fn index_session(
        &self,
        session: &SessionRef,
        source_path: Option<&Path>,
        full_read_bytes: u64,
    ) -> Result<IndexRead> {
        validate_provider(session, Provider::Codex)?;
        let native = self.find_session(&session.id, source_path)?;
        let changed = source_path.is_some_and(|path| path != native.path);
        let read = self.read_selected(native, |native, revision| {
            let selected_path = native.path.clone();
            let mut read = native.index(session, full_read_bytes, revision)?;
            read.observe_inherited_workspace(&selected_path)?;
            Ok(read)
        });
        if changed {
            read.context("selected Codex rollout changed during indexing")
        } else {
            read
        }
    }

    fn new_session_plan(&self, target: &LaunchTarget) -> Result<LaunchPlan> {
        self.native_launch(target, target.prompt.iter().cloned().collect())
    }

    fn launch_plan(&self, session: &SessionRef, target: &LaunchTarget) -> Result<LaunchPlan> {
        validate_provider(session, Provider::Codex)?;
        Uuid::parse_str(&session.id).context("Codex session ID must be a UUID")?;
        let mut args = vec![
            if target.fork { "fork" } else { "resume" }.to_owned(),
            session.id.clone(),
        ];
        if let Some(prompt) = &target.prompt {
            args.push(prompt.clone());
        }
        self.native_launch(target, args)
    }

    fn codex_continuation_sqlite_home(
        &self,
        session: Option<&SessionRef>,
        cwd: &Path,
        overrides: &crate::CodexLaunchOverrides,
        expand_env_tilde: &dyn Fn() -> Result<bool>,
    ) -> Result<Option<PathBuf>> {
        if let Some(session) = session {
            return current_launch::verified_sqlite_home(
                self,
                session,
                cwd,
                overrides,
                expand_env_tilde,
            );
        }
        let home = self
            .codex_home
            .as_deref()
            .context("Codex home was not found")?;
        self.current
            .root_for_launch(home, cwd, overrides, expand_env_tilde)
            .map(Some)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;
    use proptest::prelude::*;

    #[test]
    fn scan_session_files_reports_truncation_and_unreadable_roots() {
        let temporary = tempfile::tempdir().expect("temporary Codex home");
        let day = temporary.path().join("sessions/2026/09/09");
        fs::create_dir_all(&day).expect("session day");
        for index in 0..3 {
            fs::write(day.join(format!("rollout-{index}.jsonl")), "{}\n").expect("jsonl fixture");
        }
        let scan = scan_session_files(temporary.path(), 2);
        assert_eq!(scan.files.len(), 2);
        assert!(
            scan.notes
                .iter()
                .any(|note| note.contains("stopped after 2 newest jsonl files"))
        );

        let missing = scan_session_files(&temporary.path().join("missing-home"), 10);
        assert!(missing.files.is_empty());
        assert!(missing.notes.is_empty());

        let deep = day.join("one/two/three");
        fs::create_dir_all(&deep).expect("depth-limited directory");
        fs::write(deep.join("rollout-deep.jsonl"), "{}\n").expect("deep fixture");
        let depth_limited = scan_session_files(temporary.path(), 10);
        assert!(
            depth_limited
                .notes
                .iter()
                .any(|note| note.contains("scan depth limit"))
        );

        let blocked = tempfile::tempdir().expect("blocked Codex home");
        fs::write(blocked.path().join("sessions"), b"x")
            .expect("file posing as sessions directory");
        let unreadable = scan_session_files(blocked.path(), 10);
        assert!(unreadable.files.is_empty());
        assert!(
            unreadable
                .notes
                .iter()
                .any(|note| note.contains("errors while reading session directories")),
            "{:?}",
            unreadable.notes
        );
    }

    #[test]
    fn scan_session_files_keeps_reverse_lexical_top_files_across_empty_directories() {
        let temporary = tempfile::tempdir().expect("temporary Codex home");
        let day = temporary.path().join("sessions/2026/10/06");
        let nested = day.join("zz-nested");
        fs::create_dir_all(&nested).expect("nested directory");
        fs::create_dir(day.join("zz-empty")).expect("empty directory");
        fs::write(nested.join("rollout-inside.jsonl"), "{}\n").expect("nested fixture");
        let older_day = temporary.path().join("sessions/2026/10/05");
        fs::create_dir_all(&older_day).expect("older session day");
        fs::write(older_day.join("rollout-older.jsonl"), "{}\n").expect("older fixture");
        for name in ["a", "b", "c", "d", "e", "f"] {
            fs::write(day.join(format!("rollout-{name}.jsonl")), "{}\n").expect("rollout fixture");
        }
        fs::write(day.join("zz-unrelated.tmp"), "").expect("unrelated file");

        let mut expected = vec![
            nested.join("rollout-inside.jsonl"),
            older_day.join("rollout-older.jsonl"),
        ];
        expected.extend(
            (b'a'..=b'f').map(|name| day.join(format!("rollout-{}.jsonl", char::from(name)))),
        );
        expected.sort_unstable_by(|left, right| right.cmp(left));
        for limit in 1..=expected.len() {
            let scan = scan_session_files(temporary.path(), limit);
            assert_eq!(
                scan.files,
                expected.iter().take(limit).cloned().collect::<Vec<_>>(),
                "selection differs at limit {limit}"
            );
        }
    }

    #[test]
    fn listing_reuses_cached_rollout_headers_until_the_rollout_changes() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let home = temporary.path().join("codex");
        let day = home.join("sessions/2026/09/15");
        fs::create_dir_all(&day).expect("session day");
        let id = "019a3b1c-0000-7000-8000-000000000001";
        let rollout = day.join(format!("rollout-2026-09-15T10-00-00-{id}.jsonl"));
        let write_rollout = |cwd: &str| {
            fs::write(
                &rollout,
                format!(
                    "{{\"timestamp\":\"2026-09-15T10:00:00Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"{id}\",\"cwd\":\"{cwd}\"}}}}\n"
                ),
            )
            .expect("rollout");
        };
        let cache = temporary.path().join("state/cache/codex-rollouts.json");
        let listed_projects = || {
            let mut adapter = CodexAdapter::with_root(&home);
            adapter.rollout_cache = Some(cache.clone());
            adapter
                .listing(true)
                .0
                .sessions
                .iter()
                .map(|session| session.project_path.clone())
                .collect::<Vec<_>>()
        };

        write_rollout("/workspace/original");
        assert_eq!(
            listed_projects(),
            [Some(PathBuf::from("/workspace/original"))]
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mode = |path: &Path| {
                fs::metadata(path)
                    .expect("cache metadata")
                    .permissions()
                    .mode()
                    & 0o777
            };
            assert_eq!(mode(&cache), 0o600);
            assert_eq!(mode(cache.parent().expect("cache directory")), 0o700);
        }

        // Unchanged rollouts are not read again, so an edited cache entry shows through.
        let cached = fs::read_to_string(&cache).expect("cache file");
        fs::write(
            &cache,
            cached.replace("/workspace/original", "/workspace/cached"),
        )
        .expect("edited cache");
        assert_eq!(
            listed_projects(),
            [Some(PathBuf::from("/workspace/cached"))]
        );

        // A cache written by another parser build is ignored.
        let mut stale: Value =
            serde_json::from_slice(&fs::read(&cache).expect("cache file")).expect("cache JSON");
        stale["format"] = json!("0-stale");
        fs::write(
            &cache,
            serde_json::to_vec(&stale).expect("stale cache JSON"),
        )
        .expect("stale cache");
        assert_eq!(
            listed_projects(),
            [Some(PathBuf::from("/workspace/original"))]
        );

        write_rollout("/workspace/renamed-longer");
        assert_eq!(
            listed_projects(),
            [Some(PathBuf::from("/workspace/renamed-longer"))]
        );
    }

    #[test]
    fn path_uuid_ignores_stems_that_end_mid_character() {
        let mid_character = format!("{}a.jsonl", "\u{e9}".repeat(18));
        assert_eq!(path_uuid(Path::new(&mid_character)), None);
        assert_eq!(
            path_uuid(Path::new(
                "rollout-2026-01-01T00-00-00-11111111-1111-4111-8111-111111111111.jsonl"
            )),
            Some("11111111-1111-4111-8111-111111111111".to_owned())
        );
    }

    #[test]
    fn listing_notes_describe_empty_and_partial_scans() {
        let scan = CodexFileScan {
            files: vec![PathBuf::from("a.jsonl"), PathBuf::from("b.jsonl")],
            notes: vec![
                "Codex scan stopped after 2 newest jsonl files; older sessions are omitted."
                    .to_owned(),
            ],
            ..CodexFileScan::default()
        };
        let notes = listing_notes(&scan, 0, 1, 1, 0, Some("permission denied"));
        assert!(notes.iter().any(|note| note.contains("stopped after 2")));
        assert!(
            notes
                .iter()
                .any(|note| note.contains("unreadable session file(s): permission denied"))
        );
        assert!(
            notes
                .iter()
                .any(|note| note.contains("listed 0 user sessions") && note.contains("1 subagent"))
        );

        let readable = listing_notes(&scan, 4, 0, 0, 0, None);
        assert_eq!(readable.len(), 1);
        assert!(readable[0].contains("stopped after 2"));
    }

    #[test]
    fn compact_discovery_error_keeps_the_first_line() {
        assert_eq!(
            compact_discovery_error(&anyhow!("first line\nsecond line")),
            "first line"
        );
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(128))]

        #[test]
        fn path_uuid_never_panics_on_arbitrary_stems(
            stem in any::<String>(),
            extension in "[a-z]{0,6}",
        ) {
            let _ = path_uuid(Path::new(&format!("{stem}.{extension}")));
            let _ = path_uuid(Path::new(&stem));
        }

        #[test]
        fn path_uuid_extracts_a_trailing_uuid(prefix in "[^/\\\\.]{0,24}", id in any::<u128>()) {
            let id = Uuid::from_u128(id).to_string();
            prop_assert_eq!(
                path_uuid(Path::new(&format!("rollout-{prefix}{id}.jsonl"))),
                Some(id)
            );
        }

    }
}
