use std::{
    cmp::Reverse,
    collections::{BinaryHeap, HashMap, HashSet},
    ffi::OsString,
    fs,
    io::{ErrorKind, Read},
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

mod attachments;
mod checkpoint_wire;
mod completed_item;
mod completion_positions;
mod deferred_receipts;
mod display_text;
mod message_mirrors;
mod model_cut;
use message_mirrors::{MessageIdentity, MessageMatch, MessageMirrors};
mod current_launch;
mod current_rollout;
mod gui_names;
mod paginated_answers;
mod paginated_checkpoint;
mod paginated_rollback;
mod paginated_workspace;
mod rollout_cache;
mod rollout_file;
mod rollout_lines;
mod tool_history;
mod tool_occurrences;
mod tool_revisions;
mod turn_aliases;
mod wire_enums;
mod workspace_check;
mod workspace_damage;
mod workspace_search;
mod workspace_settings;

use current_rollout::{
    CurrentFile, CurrentRolloutStamp, CurrentRolloutUnavailable, CurrentRollouts,
};
use paginated_answers::{PaginatedAnswers, ToolId};
use paginated_rollback::{InputPosition, LegacyPrivateInputs, PaginatedRollback, Withdrawal};
use rollout_cache::{ObservedRollout, RolloutCache, RolloutHeader, RolloutStamp};
use rollout_lines::{Admission, OrdinalGap, RolloutLines};
use tool_history::{NativeCalls, PendingOutcomes};
use tool_revisions::{DisplayRevisions, PreviousDisplay};
use turn_aliases::{MessageIds, TurnAliases};

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

#[derive(Default)]
struct LegacyPaths<'a> {
    native_ids: HashSet<String>,
    by_id: HashMap<Uuid, Vec<&'a Path>>,
    revisions: Vec<(PathBuf, Option<SourceRevision>)>,
    unidentified: HashMap<&'a Path, Vec<&'a Path>>,
}

impl<'a> LegacyPaths<'a> {
    fn add_filename(&mut self, path: &'a Path) {
        if let Some((thread, _)) = current_rollout::ids(path)
            && let Ok(owner) = Uuid::parse_str(thread)
        {
            self.by_id.entry(owner).or_default().push(path);
        }
    }

    fn title_paths(&self, id: &str) -> &[&'a Path] {
        Uuid::parse_str(id)
            .ok()
            .and_then(|owner| self.by_id.get(&owner))
            .map_or(&[], Vec::as_slice)
    }
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

    fn legacy_paths(&self, home: &Path, mut cache: Option<&mut RolloutCache>) -> LegacyPaths<'_> {
        let mut legacy = LegacyPaths::default();
        let root = fs::canonicalize(home).ok();
        for path in &self.files {
            if current_rollout::canonical_name(path) {
                legacy.add_filename(path);
                continue;
            }
            if root
                .as_ref()
                .is_some_and(|root| live_rollout_path(home, root, path))
                && let Ok(Some(session)) = listed_session(path, cache.as_deref_mut())
                && let Ok(owner) = Uuid::parse_str(&session.id)
            {
                // A header supplies an ID to look up. Only the SQLite row can select this file by that ID.
                legacy.native_ids.insert(session.id);
                legacy.by_id.entry(owner).or_default().push(path);
                legacy
                    .revisions
                    .push((path.clone(), session.observed_revision));
            } else if let Some(name) = rollout_file::jsonl_file_name(path) {
                legacy.add_filename(path);
                legacy.unidentified.entry(name).or_default().push(path);
                legacy.revisions.push((path.clone(), None));
            }
        }
        legacy
    }

    fn thread_ids<'a>(&'a self, legacy: &'a LegacyPaths<'_>) -> impl Iterator<Item = &'a String> {
        self.candidates.keys().chain(
            legacy
                .native_ids
                .iter()
                .filter(|id| !self.candidates.contains_key(*id)),
        )
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

    fn index_paths(
        &self,
        selected: &SelectedRollout,
        legacy: &LegacyPaths<'_>,
    ) -> Option<Vec<PathBuf>> {
        if !self.notes.is_empty() {
            return None;
        }
        let session = &selected.session;
        let mut paths: Vec<_> = legacy
            .title_paths(&session.id)
            .iter()
            .copied()
            .map(Path::to_owned)
            .collect();
        if selected.from_registry {
            // SQLite can select a physical representation whose filename has no thread ID.
            paths.push(session.path.clone());
        }
        if let Some(name) = rollout_file::jsonl_file_name(&session.path)
            && let Some(unknown) = legacy.unidentified.get(name)
        {
            paths.extend(unknown.iter().map(|path| path.to_path_buf()));
        }
        paths.sort_unstable();
        paths.dedup();
        Some(paths)
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
    legacy_revisions: Vec<(PathBuf, Option<SourceRevision>)>,
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

    fn named_rows<'a>(
        &self,
        home: &Path,
        ids: impl IntoIterator<Item = &'a str>,
    ) -> Result<HashMap<String, current_rollout::CurrentRollout>> {
        self.current
            .rows_with_titles(home, ids, &self.title_index().titles)
    }

    fn header_cache(&self, home: &Path) -> Option<RolloutCache> {
        self.rollout_cache
            .as_deref()
            .and_then(|path| RolloutCache::load(path, home))
    }

    fn selected_title(
        &self,
        selected: &SelectedRollout,
        row: Option<&current_rollout::CurrentRollout>,
        candidates: impl FnOnce() -> Option<Vec<PathBuf>>,
    ) -> Option<String> {
        let session = &selected.session;
        let indexed = || {
            let title = self.title_index().titles.get(&session.id)?;
            let paths = candidates()?;
            if paths.as_slice() == [session.path.as_path()] {
                return Some(title.clone());
            }
            if let Some(row) = row
                && row.corroborates_index()
                && let Some(home) = self.codex_home.as_deref()
                && row
                    .index_copies_match(
                        home,
                        &session.id,
                        &session.path,
                        paths.iter().map(PathBuf::as_path),
                    )
                    .unwrap_or(false)
            {
                return Some(title.clone());
            }
            None
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
        let rows = self.named_rows(home, ids);
        let mut cache = self.header_cache(home);
        let legacy = scan.legacy_paths(home, cache.as_mut());
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
                && current_rollout::canonical_name(path)
                && legacy.title_paths(id).iter().all(|&file| file == path)
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
            let title =
                self.selected_title(&selected, row, || scan.index_paths(&selected, &legacy));
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
                    && cached
                        .listing
                        .legacy_revisions
                        .iter()
                        .all(|(path, previous)| {
                            previous.is_some() && Self::index_file_revision(path) == *previous
                        })
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
        let mut cache = self.header_cache(home);
        let legacy = scan.legacy_paths(home, cache.as_mut());
        let rows = match self.named_rows(home, scan.thread_ids(&legacy).map(String::as_str)) {
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
        let mut sessions = Vec::new();
        let mut selected_files = HashMap::new();
        let mut requires_scan = false;
        let mut subagents = 0_usize;
        let mut invalid = InvalidHeaders {
            without_metadata: scan.files.len()
                - scan.candidates.values().map(Vec::len).sum::<usize>(),
            ..Default::default()
        };
        for id in scan.thread_ids(&legacy) {
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
            if row.is_some_and(current_rollout::CurrentRollout::corroborates_index)
                && (scan.unique_path(id).is_none()
                    || !current_rollout::canonical_name(&selected.session.path)
                    || legacy.title_paths(id).len() > 1)
            {
                requires_scan = true;
            }
            let title =
                self.selected_title(&selected, row, || scan.index_paths(&selected, &legacy));
            let mut session = selected.session;
            if current_rollout::ids(&session.path).is_none()
                && legacy.title_paths(id).contains(&session.path.as_path())
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
        notes.extend(self.title_index().notes.iter().cloned());
        CodexListing {
            legacy_revisions: legacy.revisions,
            scan,
            sessions,
            notes,
            selected_files,
            requires_scan,
        }
    }

    fn index_rollout(
        &self,
        session: &SessionRef,
        native: CodexSession,
        source_path: Option<&Path>,
        full_read_bytes: u64,
    ) -> Result<IndexRead> {
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
        let rows = if with_title {
            self.named_rows(home, [id])?
        } else {
            self.current.rows(home, [id])?
        };
        let row = rows.get(id);
        let finish = |mut selected: SelectedRollout| {
            if with_title {
                selected.session.title = self.selected_title(&selected, row, || {
                    let (scan, _) = self.current_session_scan();
                    let legacy = scan.legacy_paths(home, None);
                    scan.index_paths(&selected, &legacy)
                });
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
                rollout_file::visit(&self.path, |record, skipped, terminated| {
                    matched_source |= match check {
                        SavedOpenerCheck::LegacyOrToolCatalog => {
                            (record["type"] == "session_meta"
                                && record["payload"]["history_mode"] == "legacy")
                                || is_dynamic_tool_catalog(&record)
                        }
                        SavedOpenerCheck::ToolCatalog => is_dynamic_tool_catalog(&record),
                        SavedOpenerCheck::Withdrawal => false,
                    };
                    visit(record, skipped, terminated)
                })
            })?;
        Ok(match check {
            SavedOpenerCheck::Withdrawal => events.rollback_title().withdraws(prior_title),
            _ => matched_source && events.confirms_saved_local_opener(prior_title),
        })
    }

    fn withdrew_search_title(&self, prior_title: &str) -> Result<bool> {
        if self.history_mode == CodexHistoryMode::Paginated {
            return match self.canonical_events_with(|visit| {
                rollout_file::visit_with_limits(
                    &self.path,
                    rollout_file::LEGACY_PROJECTION_LIMITS,
                    visit,
                )
            }) {
                Ok((events, _)) => Ok(events.withdrew_saved_opener(prior_title)),
                Err(error) if error.is::<crate::TranscriptReadLimit>() => Ok(false),
                Err(error) => Err(error),
            };
        }
        let mut builder = self.event_builder();
        let mut history = CodexHistory::preview();
        let mut opener: Option<(u64, bool)> = None;
        let mut withdrew = false;
        let complete = rollout_file::visit_all_records(&self.path, |mut record, terminated| {
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
            let rollback = history.push(&mut builder, &mut record, terminated)?;
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
        read: impl FnOnce(
            &mut dyn FnMut(Value, JsonLineSkips, bool) -> Result<()>,
        ) -> Result<JsonLineSkips>,
    ) -> Result<(EventBuilder, Option<PathBuf>)> {
        let mut builder = self.event_builder();
        builder.track_rollback_opener(true);
        let (mut events, history) = self.canonical_events_into(
            builder,
            CodexHistory::new(self.project_path.clone()),
            read,
        )?;
        let unknown = history.workspace.unknown() || history.settings.has_owned();
        let project = history.settings.directory(history.workspace.path);
        if unknown && project.is_none() {
            events.mark_codex_workspace_unknown(self.updated_at);
        }
        Ok((events, project))
    }

    fn canonical_events_into(
        &self,
        mut builder: EventBuilder,
        mut history: CodexHistory,
        read: impl FnOnce(
            &mut dyn FnMut(Value, JsonLineSkips, bool) -> Result<()>,
        ) -> Result<JsonLineSkips>,
    ) -> Result<(EventBuilder, CodexHistory)> {
        let mut unread_ancestor = false;
        let mut first_gap = None;
        let mut malformed = 0;
        let unreadable = read(&mut |mut record, skipped_before, terminated| {
            history.observe_malformed(&mut malformed, skipped_before);
            if builder.tracks_rollback_opener()
                && !ordinary_rollback_record(&record, history.records_seen == 0)
            {
                builder.discard_rollback_opener();
            }
            if skipped_before.total() > 0 && first_gap.is_none() {
                first_gap = Some(builder.checkpoint());
                builder.note_rollback_history_gap();
            }
            history
                .workspace
                .observe_gaps(skipped_before.workspace_gaps(), history.records_seen);
            // The latest settings for this thread survive conversation rollback.
            history.settings.observe_gaps(skipped_before);
            history.settings.observe(&self.id, &record);
            unread_ancestor |= records_history_base(&record);
            history.push(&mut builder, &mut record, terminated)?;
            if history
                .records_seen
                .checked_rem(TOOL_COMPACTION_RECORD_INTERVAL)
                == Some(0)
            {
                history.compact(&mut builder);
            }
            Ok(())
        })?;
        history.observe_malformed(&mut malformed, unreadable);
        history
            .workspace
            .observe_gaps(unreadable.workspace_gaps(), history.records_seen);
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
            history.workspace.incomplete |=
                history.workspace.record.is_none_or(|record| record <= 1);
            report_unread_ancestor(&mut builder, self.updated_at);
        }
        history.finish(&mut builder);
        if !self.is_subagent && history.paginated && !history.withdrawn.is_empty() {
            builder.record_paginated_opener_withdrawal(unread_ancestor);
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
        let (events, project, reconstructed) = if self.history_mode == CodexHistoryMode::Legacy {
            let scan = self.scan_workspace(true)?;
            if scan.reconstruct_history || contains_rollback_marker(&self.path)? {
                self.reconstructed_preview(rollout_file::LIMITS)?
            } else {
                self.preview_events(scan.directory)?
            }
        } else {
            self.reconstructed_preview(rollout_file::LIMITS)?
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
            match rollout_file::sample(
                &self.path,
                budget,
                self.project_path.as_deref(),
                &self.id,
                self.history_mode,
            )
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
                    let mut events = self.events_from_records(records)?;
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

    fn materialized_preview(
        &self,
        limits: crate::support::JsonLinesLimits,
    ) -> Result<(EventBuilder, Option<PathBuf>, bool)> {
        let mut builder = self.event_builder();
        builder.track_rollback_opener(true);
        let (mut events, history) = self.canonical_events_into(
            builder,
            CodexHistory::new(self.project_path.clone()),
            |visit| rollout_file::visit_with_limits(&self.path, limits, visit),
        )?;
        let verified = history.settings.has_owned() || !history.workspace.incomplete;
        let unknown = history.workspace.unknown() || history.settings.has_owned();
        let project = history.settings.directory(history.workspace.path);
        if project.is_none() && unknown {
            events.mark_codex_workspace_unknown(self.updated_at);
        }
        events.retain_bounded_preview(
            rollout_file::SAMPLE_RECORDS,
            usize::try_from(MAX_PREVIEW_TAIL_SIZE)?,
            self.updated_at.unwrap_or_else(Utc::now),
        );
        Ok((events, project, verified))
    }

    fn reconstructed_preview(
        &self,
        limits: crate::support::JsonLinesLimits,
    ) -> Result<(EventBuilder, Option<PathBuf>, bool)> {
        if self.history_mode != CodexHistoryMode::Legacy {
            return self.materialized_preview(limits);
        }
        let stamp = || fs::metadata(&self.path).map(|metadata| RolloutStamp::new(&metadata));
        let before = stamp()?;
        let per_edge = usize::try_from(MAX_PREVIEW_TAIL_SIZE)?;
        let mut builder = self.event_builder();
        builder.plan_preview(None);
        builder.track_rollback_opener(false);
        let mut materialize = false;
        let mut replay = DisplayReplay::default();
        let ((selected, omitted), workspace_record, first_settings, first_title) = {
            let (plan, history) =
                self.canonical_events_into(builder, CodexHistory::preview(), |visit| {
                    rollout_file::visit_with_limits(
                        &self.path,
                        limits,
                        |record, skipped, terminated| {
                            materialize |= replay.observe(&record);
                            visit(record, skipped, terminated)
                        },
                    )
                })?;
            (
                plan.select_preview(rollout_file::SAMPLE_RECORDS, per_edge),
                history.workspace.record,
                history.settings,
                plan.rollback_title(),
            )
        };
        if materialize {
            let preview = self.materialized_preview(limits)?;
            anyhow::ensure!(
                stamp()? == before,
                "Codex rollout changed while reconstructing its preview"
            );
            return Ok(preview);
        }
        let expected = selected.len();
        let mut builder = self.event_builder();
        builder.plan_preview(Some(selected.clone()));
        builder.track_rollback_opener(false);
        let mut project = None;
        let mut position = 0_usize;
        let (mut events, history) =
            self.canonical_events_into(builder, CodexHistory::preview(), |visit| {
                rollout_file::visit_with_limits(
                    &self.path,
                    limits,
                    |record, skipped_before, terminated| {
                        position += 1;
                        if workspace_record == Some(position) {
                            project = string_at(&record, &[&["payload", "cwd"]]).map(PathBuf::from);
                        }
                        visit(record, skipped_before, terminated)
                    },
                )
            })?;
        if stamp()? != before
            || history.workspace.record != workspace_record
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
        if project.is_none() && (history.workspace.unknown() || owned_settings) {
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
            owned_settings || !history.workspace.incomplete,
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
        let mut events = self.events_from_records(preview.records)?;
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

    fn events_from_records(&self, records: Vec<Value>) -> Result<EventBuilder> {
        let mut builder = self.event_builder();
        let mut history = CodexHistory::new(self.project_path.clone());
        let has_history_base = records.first().is_some_and(records_history_base);
        for mut record in records {
            history.push(&mut builder, &mut record, true)?;
        }
        if has_history_base {
            self.report_unread_history_base(&mut builder);
        }
        history.finish(&mut builder);
        Ok(builder)
    }

    fn report_unread_history_base(&self, builder: &mut EventBuilder) {
        builder.push(
            EventKind::ProviderEvent,
            json!({
                "omitted_events": 1,
                "native_history_unverified": true,
                "reason": "The child rollout references an inherited prefix that is unavailable in this snapshot."
            }),
            self.updated_at,
            ReplayPolicy::HistoricalOnly,
            Some("omnisession.codex_history_base".to_owned()),
            None,
        );
    }

    fn event_builder(&self) -> EventBuilder {
        let mut builder = EventBuilder::new(Provider::Codex, &self.id);
        builder.set_provider_version(self.cli_version.clone());
        builder
    }
}

struct TurnCheckpoint {
    id: Option<String>,
    sequence: u64,
    request_sequence: u64,
    admission: Option<InputPosition>,
    project_path: Option<PathBuf>,
    workspace_record: Option<usize>,
    tool_events: usize,
}

impl TurnCheckpoint {
    fn admitted_sequence(&self) -> Option<u64> {
        // A completed display alone does not establish a native instruction boundary.
        self.admission.map(|_| self.sequence)
    }
}

fn confirm_admission(turns: &mut [TurnCheckpoint], sequence: Option<u64>, position: InputPosition) {
    if let Some(sequence) = sequence
        && let Ok(index) = turns.binary_search_by_key(&sequence, |turn| turn.request_sequence)
    {
        turns[index].admission = Some(position);
    }
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
enum CompletedKind {
    User,
    Assistant,
    AsyncAssistant,
    Documentary,
    Plan,
    Tool,
}

impl CompletedKind {
    fn for_item(payload: &Value) -> Self {
        match payload["item"]["type"].as_str() {
            Some("UserMessage") => Self::User,
            Some("AgentMessage")
                if completed_item::assistant::is_async(&payload["item"]["delivery"]) =>
            {
                Self::AsyncAssistant
            }
            Some("AgentMessage") => Self::Assistant,
            Some("Plan") => Self::Plan,
            Some(_) if !completed_item::model_receipt(&payload["item"]) => Self::Documentary,
            _ => Self::Tool,
        }
    }
}

#[derive(Clone, Copy)]
struct CompletedPosition {
    display_identity: Option<tool_history::DisplayCallIdentity>,
    pending_output_call: Option<u64>,
    deferred_output: Option<[u8; 32]>,
    receipt: Option<tool_occurrences::Receipt>,
    ambiguous: bool,
    model: Option<u64>,
    kind: CompletedKind,
    sequence: u64,
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
enum CompletionScope {
    Published,
    LegacyOutput,
    Historical(CompletedKind),
}

type CompletedTurn = HashMap<(CompletionScope, String), CompletedPosition>;

struct WorkspaceHistory {
    path: Option<PathBuf>,
    record: Option<usize>,
    retain_path: bool,
    gaps: usize,
    records_before_gap: usize,
    checkpoints_pruned: bool,
    incomplete: bool,
}

impl WorkspaceHistory {
    fn new(path: Option<PathBuf>) -> Self {
        Self {
            path,
            record: Some(1),
            retain_path: true,
            gaps: 0,
            records_before_gap: 0,
            checkpoints_pruned: false,
            incomplete: false,
        }
    }

    fn unknown(&self) -> bool {
        self.gaps > 0 && self.record.is_none()
    }

    fn observe_gaps(&mut self, skipped: usize, records_seen: usize) {
        if skipped != self.gaps {
            self.gaps = skipped;
            // Only a cwd recorded after this gap can establish the workspace again.
            self.records_before_gap = records_seen;
            self.record = None;
            self.path = None;
        }
    }
}

struct CodexHistory {
    workspace: WorkspaceHistory,
    items: CodexItems,
    settings: workspace_check::WorkspaceSettings,
    records_seen: usize,
    last_rollback_boundary: Option<u64>,
    turns: Vec<TurnCheckpoint>,
    pending_turn: Option<TurnCheckpoint>,
    active_turn_id: Option<String>,
    source_ordinal: u64,
    paginated: bool,
    paginated_inputs: PaginatedRollback,
    paginated_checkpoint: paginated_checkpoint::CheckpointHistory,
    checkpoint_turns: paginated_checkpoint::CheckpointTurns,
    paginated_workspace: paginated_workspace::PaginatedWorkspace,
    paginated_answers: PaginatedAnswers,
    withdrawn: HashMap<u64, Withdrawal>,
    explicit_display_turn: bool,
    legacy_display: MessageIdentity,
    legacy_request: Option<u64>,
    uncertain_legacy_rollback: bool,
    legacy_private: LegacyPrivateInputs,
    rollout_lines: RolloutLines,
    // Codex updates a completed item in place using its turn and item IDs.
    completed: HashMap<String, CompletedTurn>,
    named_turns: HashMap<String, u64>,
    plan_turns: HashSet<String>,
    plan_receipts: display_text::PlanReceipts,
    message_ids: MessageIds,
    aliases: TurnAliases,
    calls: NativeCalls,
    occurrences: tool_occurrences::ToolOccurrences,
    display_revisions: DisplayRevisions,
    late_owners: HashMap<u64, u64>,
    display_positions: HashMap<u64, (u64, u64)>,
    ambiguous_receipts: usize,
    deferred_receipts: deferred_receipts::DeferredReceipts,
    omitted_tool_events: usize,
    omitted_events: usize,
}

impl CodexHistory {
    fn new(project_path: Option<PathBuf>) -> Self {
        Self {
            paginated_workspace: paginated_workspace::PaginatedWorkspace::new(project_path.clone()),
            workspace: WorkspaceHistory::new(project_path),
            items: CodexItems::default(),
            settings: workspace_check::WorkspaceSettings::from_header(None),
            records_seen: 0,
            last_rollback_boundary: None,
            turns: Vec::new(),
            pending_turn: None,
            active_turn_id: None,
            source_ordinal: 0,
            paginated: false,
            paginated_inputs: PaginatedRollback::default(),
            paginated_checkpoint: paginated_checkpoint::CheckpointHistory::default(),
            checkpoint_turns: paginated_checkpoint::CheckpointTurns::default(),
            paginated_answers: PaginatedAnswers::default(),
            withdrawn: HashMap::new(),
            explicit_display_turn: false,
            legacy_display: MessageIdentity::default(),
            legacy_request: None,
            uncertain_legacy_rollback: false,
            legacy_private: LegacyPrivateInputs::default(),
            rollout_lines: RolloutLines::default(),
            completed: HashMap::new(),
            named_turns: HashMap::new(),
            plan_turns: HashSet::new(),
            plan_receipts: display_text::PlanReceipts::default(),
            message_ids: MessageIds::default(),
            aliases: TurnAliases::default(),
            calls: NativeCalls::default(),
            occurrences: tool_occurrences::ToolOccurrences::default(),
            display_revisions: DisplayRevisions::default(),
            late_owners: HashMap::new(),
            display_positions: HashMap::new(),
            ambiguous_receipts: 0,
            deferred_receipts: deferred_receipts::DeferredReceipts::default(),
            omitted_tool_events: 0,
            omitted_events: 0,
        }
    }

    /// During preview, a checkpoint holds the position of the cwd record. The second pass reads the surviving path.
    fn preview() -> Self {
        let mut history = Self::new(None);
        history.workspace.retain_path = false;
        history
    }

    fn observe_malformed(&mut self, prior: &mut usize, skipped: JsonLineSkips) {
        for _ in *prior..skipped.malformed {
            self.rollout_lines.malformed();
        }
        *prior = skipped.malformed;
    }

    /// Applies the retained tool event, event, and turn checkpoint caps.
    fn compact(&mut self, builder: &mut EventBuilder) {
        let tools_removed = builder.retain_latest_tool_events(MAX_CANONICAL_TOOL_EVENTS);
        let events_removed = builder.retain_latest_events(MAX_CANONICAL_EVENTS);
        self.omitted_tool_events += tools_removed;
        self.omitted_events += events_removed;
        if tools_removed > 0 || events_removed > 0 {
            self.prune_completed(builder);
            let first = builder.first_sequence();
            self.items.mirrors.discard_before(first);
            self.paginated_inputs.discard_before(first);
            self.paginated_answers.discard_before(first);
            self.withdrawn.retain(|seq, _| *seq >= first);
        }
        let excess = self.turns.len().saturating_sub(MAX_CANONICAL_EVENTS);
        if excess > 0 || self.omitted_events > 0 || self.omitted_tool_events > 0 {
            builder.discard_rollback_opener();
        }
        self.workspace.checkpoints_pruned |= excess > 0;
        if self.plan_turns.len() > MAX_CANONICAL_EVENTS {
            self.plan_turns.retain(|id| {
                self.named_turns.contains_key(id) || self.active_turn_id.as_deref() == Some(id)
            });
        }
        if excess > 0 {
            self.turns.drain(..excess);
            let earliest = self.turns[0].sequence;
            self.named_turns.retain(|_, seq| *seq >= earliest);
            self.aliases.retain(&self.named_turns);
            self.plan_turns
                .retain(|id| self.named_turns.contains_key(id));
        }
        if tools_removed > 0 || events_removed > 0 || excess > 0 {
            let oldest = self.turns.first().map_or(0, |turn| turn.sequence);
            self.display_revisions.prune(builder, oldest);
        }
    }

    fn checkpoint(&self, builder: &EventBuilder) -> TurnCheckpoint {
        TurnCheckpoint {
            id: self.active_turn_id.clone(),
            sequence: builder.checkpoint(),
            request_sequence: builder.checkpoint(),
            admission: None,
            project_path: self.workspace.path.clone(),
            tool_events: self.items.tool_events,
            workspace_record: self.workspace.record,
        }
    }

    fn remember_turn(&mut self, mut checkpoint: TurnCheckpoint, request: u64) {
        checkpoint.request_sequence = request;
        if let Some(id) = &checkpoint.id {
            let canonical = self.aliases.resolve(id).to_owned();
            self.named_turns
                .insert(canonical.clone(), checkpoint.sequence);
            checkpoint.id = Some(canonical);
        }
        self.turns.push(checkpoint);
    }

    fn prune_completed(&mut self, builder: &mut EventBuilder) {
        self.deferred_receipts
            .retain(|seq, _| builder.event_at_mut(seq).is_some());
        self.items
            .outcomes
            .retain(|seq| builder.event_at_mut(seq).is_some());
        self.calls.retain(|seq| builder.event_at_mut(seq).is_some());
        self.occurrences
            .retain(|seq| builder.event_at_mut(seq).is_some());
        self.message_ids
            .retain(|seq| builder.event_at_mut(seq).is_some());
        self.completed.retain(|_, items| {
            items.retain(|_, position| builder.event_at_mut(position.sequence).is_some());
            !items.is_empty()
        });
        self.late_owners
            .retain(|seq, _| builder.event_at_mut(*seq).is_some());
        self.display_positions
            .retain(|seq, _| builder.event_at_mut(*seq).is_some());
    }

    fn rollback(&mut self, builder: &mut EventBuilder, count: u64) {
        if count == 0 {
            return;
        }
        if self.workspace.checkpoints_pruned && count > self.turns.len() as u64 {
            self.workspace.incomplete = true;
        }
        let retained = self
            .turns
            .len()
            .saturating_sub(usize::try_from(count).unwrap_or(usize::MAX));
        if retained < self.turns.len() {
            self.uncertain_legacy_rollback |= self.turns[retained..]
                .iter()
                .any(|turn| turn.admission.is_none());
            if let Some(cut) = self.turns[retained].admission {
                let excess = count.saturating_sub(self.turns.len() as u64);
                self.uncertain_legacy_rollback |= self.legacy_private.crosses(cut, excess);
            }
            let checkpoint = self.turns.remove(retained);
            self.turns.truncate(retained);
            self.plan_receipts.rollback(checkpoint.sequence);
            // A migrated turn can have both its original and generated ID.
            self.named_turns.retain(|_, seq| *seq < checkpoint.sequence);
            for turn in &self.turns {
                self.uncertain_legacy_rollback |= turn.admission.is_none();
                if let Some(id) = &turn.id {
                    self.named_turns.insert(id.clone(), turn.sequence);
                }
            }
            self.aliases.retain(&self.named_turns);
            self.items.tool_events = checkpoint.tool_events;
            if self.late_owners.is_empty() {
                builder.truncate_from(checkpoint.sequence);
            } else {
                builder.retain_events(|event| {
                    let owner = self
                        .late_owners
                        .get(&event.sequence)
                        .copied()
                        .unwrap_or(event.sequence);
                    let keep = owner < checkpoint.sequence;
                    if keep != (event.sequence < checkpoint.sequence)
                        && matches!(
                            event.kind,
                            EventKind::CommandExecuted
                                | EventKind::ToolCalled
                                | EventKind::ToolCompleted
                                | EventKind::ToolFailed
                        )
                    {
                        if keep {
                            self.items.tool_events += 1;
                        } else {
                            self.items.tool_events = self.items.tool_events.saturating_sub(1);
                        }
                    }
                    keep
                });
            }
            self.omitted_tool_events = self.omitted_tool_events.min(self.items.tool_events);
            self.display_revisions
                .rollback(builder, checkpoint.sequence);
            self.deferred_receipts
                .retain(|_, owner| owner < checkpoint.sequence);
            self.last_rollback_boundary = Some(checkpoint.sequence);
            self.workspace.record = checkpoint
                .workspace_record
                .filter(|position| *position > self.workspace.records_before_gap);
            self.workspace.path = self.workspace.record.and(checkpoint.project_path);
            self.prune_completed(builder);
        } else if self.workspace.gaps > 0 {
            self.workspace.record = None;
            self.workspace.path = None;
        }
        self.plan_turns
            .retain(|id| self.named_turns.contains_key(id));
        self.items.clear_mirrors();
        self.items.in_turn = false;
        self.explicit_display_turn = false;
        self.legacy_display = MessageIdentity::default();
        self.legacy_request = None;
        self.items.plan_mode = false;
        self.pending_turn = None;
        self.active_turn_id = None;
    }

    fn model_plan_mode(&self, item: &Value) -> bool {
        let turn = item["internal_chat_message_metadata_passthrough"]["turn_id"]
            .as_str()
            .filter(|turn| !turn.is_empty())
            .map(|turn| self.aliases.resolve(turn));
        turn.map_or(self.items.plan_mode, |turn| {
            self.plan_turns.contains(turn)
                || (self.items.plan_mode
                    && self.current_turn_id().is_none_or(|active| active == turn))
        })
    }

    fn set_plan_mode(&mut self, turn: Option<&str>, plan: bool) {
        self.items.plan_mode = plan;
        if let Some(turn) = turn {
            let turn = self.aliases.resolve(turn);
            if plan {
                self.plan_turns.insert(turn.to_owned());
            } else {
                self.plan_turns.remove(turn);
            }
        }
    }

    fn current_turn_id(&self) -> Option<&str> {
        self.active_turn_id
            .as_deref()
            .or_else(|| self.turns.last()?.id.as_deref())
            .map(|turn| self.aliases.resolve(turn))
    }

    fn observe_context_turn(&mut self, turn: &str, sequence: u64) {
        if self
            .active_turn_id
            .as_deref()
            .is_some_and(|active| self.aliases.resolve(active) == self.aliases.resolve(turn))
        {
            return;
        }
        // Some older rollouts name the turn in its context after the first request.
        if self.active_turn_id.is_none()
            && self
                .items
                .message
                .as_ref()
                .is_some_and(|message| message.kind == EventKind::MessageUser)
            && let Some(checkpoint) = self.turns.last_mut()
            && checkpoint.id.as_deref().is_none_or(|id| id == turn)
        {
            checkpoint.id = Some(turn.to_owned());
            self.named_turns
                .insert(turn.to_owned(), checkpoint.sequence);
        } else {
            if is_implicit_turn(turn) {
                self.items.turn_boundary(Some(turn));
            } else {
                self.items.message = None;
                self.items.mirrors.start_context(sequence);
                self.items.outcomes.retain_models();
            }
            self.pending_turn = None;
        }
        self.active_turn_id = Some(turn.to_owned());
        self.items.in_turn = true;
    }

    fn push(
        &mut self,
        builder: &mut EventBuilder,
        record: &mut Value,
        terminated: bool,
    ) -> Result<Option<u64>> {
        self.records_seen += 1;
        self.last_rollback_boundary = None;
        if self.source_ordinal == 0 {
            self.rollout_lines.start_segment(record);
            self.paginated = self.rollout_lines.paginated();
            self.items.tool_projection.paginated = self.paginated;
        }
        let disposition =
            self.rollout_lines
                .prepare(record, terminated, self.paginated.then_some(true));
        self.push_admitted(builder, record, disposition)?;
        Ok(self.last_rollback_boundary)
    }

    fn push_admitted(
        &mut self,
        builder: &mut EventBuilder,
        record: &Value,
        disposition: Admission,
    ) -> Result<()> {
        let index = self.source_ordinal;
        self.source_ordinal += 1;
        match disposition {
            Admission::Complete if record["type"] == "realtime_item" => {
                self.rollout_lines.project_realtime(record, index, builder);
                Ok(())
            }
            Admission::Complete => self.push_verified(builder, record, index),
            Admission::ModelOnly => {
                let event_type = (record["type"] == "event_msg")
                    .then(|| record["payload"]["type"].as_str())
                    .flatten();
                if !self.consume_model_record(record, event_type, builder)? {
                    self.push_model_tool(builder, record);
                }
                Ok(())
            }
            Admission::Skip => Ok(()),
        }
    }

    fn push_verified(
        &mut self,
        builder: &mut EventBuilder,
        record: &Value,
        index: u64,
    ) -> Result<()> {
        let record_type = record.get("type").and_then(Value::as_str);
        let payload = &record["payload"];
        let event_type = (record_type == Some("event_msg"))
            .then(|| payload["type"].as_str())
            .flatten();
        if self.paginated && index > 0 && record_type == Some("session_meta") {
            self.start_copied_segment(builder.checkpoint());
        }
        if self.consume_model_record(record, event_type, builder)? {
            return Ok(());
        }
        let turn_id = payload["turn_id"].as_str();
        if event_type == Some("thread_rolled_back") {
            let count = rollout_lines::rollback_turns(payload)
                .context("Codex rollback marker has no valid num_turns")?;
            self.rollback(builder, u64::from(count));
            return Ok(());
        }
        if matches!(event_type, Some("task_started" | "turn_started")) {
            if self.paginated {
                self.paginated_inputs
                    .turn_started(turn_id, self.source_ordinal);
            }
            self.explicit_display_turn = true;
            self.legacy_request = None;
            // The migrator writes a default mode when it synthesizes a turn after its context.
            let generated = turn_id.is_some_and(is_implicit_turn);
            let plan =
                payload["collaboration_mode_kind"] == "plan" || (generated && self.items.plan_mode);
            self.set_plan_mode(turn_id, plan);
            self.active_turn_id = turn_id.map(str::to_owned);
            self.pending_turn = Some(self.checkpoint(builder));
            self.items.turn_boundary(turn_id);
            self.items.in_turn = true;
        } else if matches!(
            event_type,
            Some("task_complete" | "turn_complete" | "turn_aborted")
        ) && turn_id
            .zip(self.current_turn_id())
            .is_none_or(|(target, current)| self.aliases.resolve(target) == current)
        {
            self.explicit_display_turn = false;
            self.legacy_display = MessageIdentity::default();
            self.legacy_request = None;
            self.pending_turn = None;
            self.active_turn_id = None;
            self.items.turn_boundary(turn_id);
            self.items.in_turn = false;
            self.items.plan_mode = false;
        }
        if record_type == Some("turn_context") {
            if let Some(mode) = payload["collaboration_mode"]["mode"].as_str() {
                self.set_plan_mode(turn_id, mode == "plan");
            } else if turn_id.is_some()
                && turn_id.map(|id| self.aliases.resolve(id)) != self.current_turn_id()
            {
                self.items.plan_mode = turn_id
                    .is_some_and(|turn| self.plan_turns.contains(self.aliases.resolve(turn)));
            }
            if let Some(turn) = turn_id {
                self.observe_context_turn(turn, builder.checkpoint());
            }
            if self.pending_turn.is_none()
                && (self.workspace.unknown()
                    || self
                        .items
                        .message
                        .as_ref()
                        .is_none_or(|message| message.kind != EventKind::MessageUser))
            {
                self.pending_turn = Some(self.checkpoint(builder));
            }
            if let Some(cwd) = string_at(record, &[&["payload", "cwd"]]) {
                if self.paginated {
                    self.paginated_workspace
                        .observe(turn_id.or(self.active_turn_id.as_deref()), Path::new(&cwd));
                }
                self.workspace.record = Some(self.records_seen);
                if self.workspace.retain_path {
                    self.workspace.path = Some(PathBuf::from(cwd));
                }
            }
        }
        // A legacy request can start a generated display turn at its source ordinal.
        if event_type == Some("user_message") && self.items.in_turn && !self.explicit_display_turn {
            self.legacy_display =
                MessageIdentity::new(Some(&format!("rollout-{index}")), None, true);
        }
        if event_type == Some("item_completed")
            && let (Some(turn), Some(id)) = (turn_id, payload["item"]["id"].as_str())
        {
            self.push_identified_item(
                builder,
                payload,
                turn,
                id,
                parse_timestamp(record.get("timestamp")),
            );
        } else if !self.push_model_tool(builder, record) {
            self.push_message_or_metadata(builder, record, event_type);
        }
        Ok(())
    }

    fn start_copied_segment(&mut self, sequence: u64) {
        self.paginated_inputs.segment();
        self.paginated_workspace.new_segment(sequence);
        self.paginated_answers.segment();
    }

    fn consume_model_record(
        &mut self,
        record: &Value,
        event_type: Option<&str>,
        builder: &mut EventBuilder,
    ) -> Result<bool> {
        if !self.paginated {
            return Ok(self.legacy_private.observe(record, self.source_ordinal));
        }
        let payload = &record["payload"];
        self.checkpoint_turns.observe(record, self.source_ordinal);
        if let Some(turn) = payload["turn_id"].as_str() {
            let finished = matches!(event_type, Some("task_complete" | "turn_complete"));
            if finished && self.plan_turns.contains(self.aliases.resolve(turn)) {
                self.plan_receipts.finish_turn(turn, self.source_ordinal);
            }
            if finished
                || (record["type"] == "token_usage_record" && payload["response_id"].is_string())
            {
                self.paginated_answers.end_response(turn);
            }
        }
        if record["type"] == "compacted" {
            self.record_paginated_checkpoint(record, builder);
            return Ok(true);
        }
        if record["type"] == "inter_agent_communication"
            || (record["type"] == "response_item"
                && matches!(payload["type"].as_str(), Some("message" | "agent_message")))
        {
            if !self
                .paginated_inputs
                .model_user(record, self.source_ordinal, builder.checkpoint())
                && payload["type"] == "message"
                && payload["role"] == "assistant"
            {
                let plan_mode = self.model_plan_mode(payload);
                let active = self
                    .active_turn_id
                    .as_deref()
                    .filter(|t| !is_implicit_turn(t));
                self.paginated_answers.model(
                    record,
                    self.source_ordinal,
                    builder.checkpoint(),
                    plan_mode,
                    active,
                );
            }
            return Ok(true);
        }
        if event_type == Some("thread_rolled_back") {
            let count = rollout_lines::rollback_turns(payload)
                .context("Codex rollback marker has no valid num_turns")?;
            self.withdraw_displayed_inputs(builder, usize::try_from(count).unwrap_or(usize::MAX));
            return Ok(true);
        }
        Ok(matches!(event_type, Some("user_message" | "agent_message")))
    }

    fn record_paginated_checkpoint(&mut self, record: &Value, builder: &mut EventBuilder) {
        let payload = &record["payload"];
        let selection = self.checkpoint_turns.selected();
        if let Some(window) =
            paginated_checkpoint::PaginatedCheckpoint::read(self, builder, payload)
        {
            self.paginated_checkpoint.record(Some(window), selection);
        } else if let Some(message) = payload["message"].as_str() {
            self.paginated_checkpoint.record_legacy(
                self.source_ordinal,
                builder.checkpoint(),
                message,
                &self.paginated_inputs,
                selection,
            );
            let summary = if message.is_empty() {
                "(no summary available)"
            } else {
                message
            };
            builder.push(
                EventKind::CompactionCreated,
                json!({"summary":summary,"ordinal":self.source_ordinal}),
                parse_timestamp(record.get("timestamp")),
                ReplayPolicy::Contextual,
                Some(omnis_core::CODEX_POSITIONED_LEGACY_SUMMARY.to_owned()),
                None,
            );
        }
    }

    fn withdraw_displayed_inputs(&mut self, builder: &EventBuilder, count: usize) {
        let effective = self.paginated_checkpoint.rollback(
            count,
            &self.paginated_inputs,
            self.checkpoint_turns.selected(),
        );
        let aliases = self.paginated_inputs.resolved_turns();
        let checkpoint_floor = self
            .paginated_checkpoint
            .as_ref()
            .map_or(0, paginated_checkpoint::PaginatedCheckpoint::source_ordinal);
        let mut owners = self.paginated_inputs.withdraw(
            effective.input_count,
            &aliases,
            effective.boundary,
            checkpoint_floor,
        );
        if effective.changed {
            self.workspace.path = self.paginated_workspace.surviving(
                &self.aliases,
                &aliases,
                self.turns.iter().map(|turn| {
                    let seq = turn.request_sequence;
                    let baseline = owners
                        .get(&seq)
                        .or_else(|| self.withdrawn.get(&seq))
                        .copied();
                    let status = self.paginated_checkpoint.status(seq, Some(seq), baseline);
                    if turn.id.is_some()
                        && self
                            .paginated_checkpoint
                            .as_ref()
                            .is_some_and(|window| window.applies_to(seq))
                        && let Some(status @ (Withdrawal::Confirmed | Withdrawal::Unverified)) =
                            status
                    {
                        owners.insert(seq, status);
                    }
                    (seq, turn.id.as_deref(), status)
                }),
            );
        }
        self.paginated_answers.resolve_turns(aliases);
        let replies = effective
            .boundary
            .map_or_else(HashMap::new, |(ordinal, order)| {
                self.paginated_answers.rollback(ordinal, order)
            });
        if owners.is_empty() && replies.is_empty() {
            return;
        }
        builder.visit_events(|event| {
            if let Some(&withdrawal) = replies.get(&event.sequence) {
                if let Some(status) = withdrawal {
                    self.withdrawn.insert(event.sequence, status);
                }
                return;
            }
            let sequence = self
                .late_owners
                .get(&event.sequence)
                .copied()
                .unwrap_or(event.sequence);
            if let Some(input) = self
                .turns
                .partition_point(|turn| turn.sequence <= sequence)
                .checked_sub(1)
                && let Some(status) = owners.get(&self.turns[input].request_sequence)
            {
                self.withdrawn.insert(event.sequence, *status);
            }
        });
    }

    fn finish(&mut self, builder: &mut EventBuilder) {
        let missing = self
            .plan_receipts
            .missing_paginated(self.paginated_answers.surviving_plans(), || {
                self.paginated_inputs.resolved_turns()
            });
        self.plan_receipts.report(builder, missing);
        self.display_revisions.finish(builder);
        self.finish_deferred_receipts(builder);
        if !self.paginated {
            self.finish_legacy_outputs(builder);
        }
        if self.uncertain_legacy_rollback {
            builder.push(
                EventKind::ProviderEvent,
                json!({"native_history_unverified": true, "reason": "The legacy display and model records do not establish the same rollback boundary."}),
                None,
                ReplayPolicy::HistoricalOnly,
                Some("omnisession.codex_legacy_rollback".into()),
                None,
            );
        }
        for (count, record, reason) in [
            (
                self.ambiguous_receipts,
                "omnisession.codex_ambiguous_receipt",
                "The published command does not distinguish repeated model calls. Their outcomes remain separate.",
            ),
            (
                self.rollout_lines.invalid_checkpoints,
                "omnisession.codex_invalid_checkpoint",
                "Codex recorded an invalid compaction summary or replacement. The handoff uses the last verified history.",
            ),
            (
                self.rollout_lines.invalid_messages,
                "omnisession.codex_invalid_message",
                "A Codex history item could not be verified. The handoff uses its last verified version.",
            ),
        ] {
            if count > 0 {
                builder.push(
                    EventKind::ProviderEvent,
                    json!({"omitted_events": count, "native_history_unverified": true, "reason": reason}),
                    None,
                    ReplayPolicy::HistoricalOnly,
                    Some(record.into()),
                    None,
                );
            }
        }
        let gap = self.rollout_lines.gap;
        if self.rollout_lines.skipped > 0 || gap != OrdinalGap::None {
            let reason = if self.rollout_lines.contested {
                "Codex records compete for a display position that OmniSession could not verify. The handoff uses the last verified version of each item."
            } else if gap == OrdinalGap::Possible {
                "A documentary Codex record follows a gap in the recorded positions. OmniSession could not verify whether any published history is missing."
            } else if !self.paginated {
                "The Codex rollout contains unreadable records. The handoff uses the last verified history."
            } else {
                "The paginated Codex rollout contains missing or unreadable display records. The handoff uses the last verified version of each item."
            };
            builder.push(
                EventKind::ProviderEvent,
                json!({
                    "omitted_events": self.rollout_lines.skipped,
                    "ordinal_gap": gap == OrdinalGap::Confirmed,
                    "possible_ordinal_gap": gap == OrdinalGap::Possible,
                    "contested_ordinal": self.rollout_lines.contested,
                    "native_history_unverified": true,
                    "reason": reason
                }),
                None,
                ReplayPolicy::HistoricalOnly,
                Some("omnisession.codex_rollout_line".into()),
                None,
            );
        }
        if self.paginated {
            self.annotate_model_window(builder);
        } else {
            builder.retain_events(|event| {
                event.source.raw_record_type.as_deref()
                    != Some("omnisession.codex_tool_display_mirror")
            });
        }
        builder.visit_events_mut(|event| completed_item::clear_source_receipt(&mut event.payload));
        builder.order_delayed_messages(&self.display_positions);
    }

    fn annotate_model_window(&mut self, builder: &mut EventBuilder) {
        let aliases = self.paginated_inputs.resolved_turns();
        let answers = self.paginated_answers.finish_answers(&aliases);
        let checkpoint = self.paginated_checkpoint.as_ref();
        if self.paginated_checkpoint.saw_legacy() {
            builder.retain_events(|event| {
                event.source.raw_record_type.as_deref()
                    != Some(omnis_core::CODEX_POSITIONED_LEGACY_SUMMARY)
                    || event.payload["ordinal"].as_u64().is_some_and(|ordinal| {
                        self.paginated_checkpoint.legacy_summary_active(ordinal)
                    })
            });
        }
        let mut has_positioned_replies = false;
        builder.visit_events_mut(|event| {
            if event.source.raw_record_type.as_deref() == Some(rollout_lines::TRANSCRIPT_SEGMENT) {
                return;
            }
            if !matches!(
                event.kind,
                EventKind::MessageUser
                    | EventKind::MessageAssistant
                    | EventKind::ToolCalled
                    | EventKind::ToolCompleted
                    | EventKind::ToolFailed
                    | EventKind::CommandExecuted
            ) {
                return;
            }
            let seq = event.sequence;
            let baseline = answers
                .get(&seq)
                .copied()
                .unwrap_or_else(|| self.withdrawn.get(&seq).copied());
            let status = if self.paginated_checkpoint.is_empty() {
                baseline
            } else {
                let physical = self.late_owners.get(&seq).copied().unwrap_or(seq);
                let owner = self
                    .turns
                    .partition_point(|turn| turn.sequence <= physical)
                    .checked_sub(1)
                    .map(|index| self.turns[index].request_sequence);
                self.paginated_checkpoint.status(seq, owner, baseline)
            };
            let (status, prior) = if event.kind == EventKind::MessageAssistant {
                let projection =
                    self.paginated_answers
                        .project_answer(seq, &self.paginated_checkpoint, status);
                if !projection.versions.is_empty() {
                    has_positioned_replies = true;
                    let mut versions = Vec::with_capacity(projection.versions.len());
                    for version in projection.versions {
                        let position = match version.anchor {
                            paginated_answers::ReplyAnchor::Checkpoint { ordinal, index } => {
                                let Some(checkpoint) = checkpoint else {
                                    continue;
                                };
                                json!([checkpoint.display_end(), ordinal, index])
                            }
                            paginated_answers::ReplyAnchor::Source { ordinal, sequence } => {
                                json!([sequence, ordinal, 0])
                            }
                        };
                        versions.push(json!({"position":position,"text":version.spelling}));
                    }
                    event.payload["omnisession_codex_active_replies"] = versions.into();
                }
                (projection.status, projection.prior)
            } else {
                (status, None)
            };
            if let Some(status) = status {
                let field = if status == Withdrawal::OutsideCheckpoint {
                    "omnisession_codex_checkpoint"
                } else {
                    "omnisession_codex_rollback"
                };
                event.payload[field] = status.as_str().into();
                if let Some(prior) = prior {
                    event.payload["omnisession_codex_prior_display"] = prior.into();
                }
            }
        });
        if has_positioned_replies {
            self.position_model_window(builder, &aliases);
        }
    }

    fn position_model_window(
        &self,
        builder: &mut EventBuilder,
        aliases: &HashMap<[u8; 32], [u8; 32]>,
    ) {
        let legacy = self.paginated_checkpoint.legacy_ordinal();
        let checkpoint = self
            .paginated_checkpoint
            .as_ref()
            .filter(|_| legacy.is_none());
        let Some(after) = legacy
            .or_else(|| checkpoint.map(paginated_checkpoint::PaginatedCheckpoint::source_ordinal))
        else {
            return;
        };
        let users = self
            .paginated_inputs
            .confirmed_suffix_positions(after, aliases);
        builder.visit_events_mut(|event| {
            let seq = event.sequence;
            if let Some((ordinal, slot)) = users
                .get(&seq)
                .copied()
                .or_else(|| self.paginated_answers.surviving_position(seq, after))
            {
                event.payload["omnisession_codex_checkpoint_position"] = json!([slot, ordinal, 0]);
            } else if let Some(window) = checkpoint
                && let Some(index) = window.first_retained_index(seq)
            {
                event.payload["omnisession_codex_checkpoint_position"] =
                    json!([window.display_end(), window.source_ordinal(), index]);
            }
        });
    }

    fn push_message_or_metadata(
        &mut self,
        builder: &mut EventBuilder,
        record: &Value,
        event_type: Option<&str>,
    ) {
        let payload = &record["payload"];
        let checkpoint = self.checkpoint(builder);
        if self.push_record(builder, record) {
            let mut checkpoint = self.pending_turn.take().unwrap_or(checkpoint);
            checkpoint.admission = InputPosition::user(record, self.source_ordinal);
            // The migrator can emit the next raw request before closing a synthesized turn.
            if let Some(stamped) =
                payload["internal_chat_message_metadata_passthrough"]["turn_id"].as_str()
            {
                checkpoint.id = Some(stamped.to_owned());
            } else if checkpoint.id.as_deref().is_some_and(|id| {
                is_implicit_turn(id) && self.named_turns.contains_key(self.aliases.resolve(id))
            }) {
                checkpoint.id = None;
            }
            let request = self.items.projected.unwrap_or(checkpoint.sequence);
            self.remember_turn(checkpoint, request);
        }
        if event_type == Some("user_message") {
            self.legacy_request = self.items.projected;
        } else if event_type == Some("agent_message")
            && let Some(sequence) = self.items.projected
            && let Some(request) = self.legacy_request
            && let Ok(input) = self
                .turns
                .binary_search_by_key(&request, |turn| turn.request_sequence)
        {
            self.place_early_answer(builder, input, sequence);
        }
    }

    fn legacy_revision_input(&self, owner: Option<u64>) -> Option<&TurnCheckpoint> {
        if self.paginated {
            return None;
        }
        if let Some(owner) = owner {
            let index = self
                .turns
                .binary_search_by_key(&owner, |turn| turn.sequence)
                .ok()?;
            self.turns.get(index)
        } else {
            self.turns.last()
        }
    }

    fn push_unscoped_record(&mut self, builder: &mut EventBuilder, record: &Value) -> bool {
        let payload = &record["payload"];
        self.items.projected = None;
        self.items.identity = if record["type"] == "event_msg"
            && !self.explicit_display_turn
            && matches!(
                payload["type"].as_str(),
                Some("user_message" | "agent_message")
            ) {
            self.legacy_display
        } else {
            MessageIdentity::default()
        };
        self.items.cited_display =
            payload["type"] == "agent_message" && payload["memory_citation"].is_object();
        let input = self
            .legacy_revision_input(None)
            .and_then(TurnCheckpoint::admitted_sequence);
        let new_turn = push_record(builder, record, &mut self.items);
        self.display_revisions.observe_message(
            builder,
            self.items.previous_message.take(),
            input,
            &self.late_owners,
        );
        if record["type"] == "response_item" {
            self.observe_model_tool(self.items.projected, record, None, None);
        }
        new_turn
    }

    fn push_record(&mut self, builder: &mut EventBuilder, record: &Value) -> bool {
        let payload = &record["payload"];
        if record["type"] != "response_item" || payload["type"] != "message" {
            return self.push_unscoped_record(builder, record);
        }
        let role = payload["role"].as_str();
        if !matches!(role, Some("assistant" | "user")) {
            return false;
        }
        let stamped = payload["internal_chat_message_metadata_passthrough"]["turn_id"]
            .as_str()
            .filter(|turn| !turn.is_empty())
            .map(|turn| self.aliases.resolve(turn));
        // A generated turn can close after the next raw message has already arrived.
        let turn = stamped.or_else(|| {
            self.active_turn_id
                .as_deref()
                .filter(|id| !is_implicit_turn(id))
                .map(|id| self.aliases.resolve(id))
        });
        let mut identity = MessageIdentity::new(turn, payload["id"].as_str(), false);
        identity.ordered_user_input =
            role == Some("user") && model_cut::input_order(&record["metadata"]).is_some();
        let key = identity.scoped(role == Some("assistant"));
        let owner = stamped
            .filter(|turn| self.current_turn_id().is_some_and(|active| active != *turn))
            .and_then(|turn| self.named_turns.get(turn).copied());
        let input = self
            .legacy_revision_input(owner)
            .and_then(TurnCheckpoint::admitted_sequence);
        let plan_mode = self.model_plan_mode(payload);
        let mut late = CodexItems::default();
        let items = if owner.is_some() {
            &mut late
        } else {
            &mut self.items
        };
        let plan_scope = turn.or(self.active_turn_id.as_deref());
        let previous_plan_mode = std::mem::replace(&mut items.plan_mode, plan_mode);
        items.identity = identity;
        items.cited_display = false;
        items.update = key.and_then(|key| self.message_ids.get(&key).copied());
        items.projected = None;
        let new_turn = push_record(builder, record, items);
        if let Some(proposal) = items.raw_proposal.take() {
            let owner = owner.or_else(|| self.turns.last().map(|turn| turn.sequence));
            self.plan_receipts.proposal(plan_scope, proposal, owner);
        }
        if !new_turn && let Some(position) = InputPosition::user(record, self.source_ordinal) {
            confirm_admission(&mut self.turns, items.projected, position);
        }
        self.display_revisions.observe_message(
            builder,
            items.previous_message.take(),
            input,
            &self.late_owners,
        );
        if role == Some("assistant")
            && let Some(copy) = items
                .mirrors
                .confirm_ordered_copy(items.projected, builder.checkpoint())
        {
            // The raw admission remains a rollback checkpoint even when both displayed messages match.
            builder.truncate_from(copy.accepted);
            if let Some(key) = copy.scoped_id {
                self.message_ids.insert(key, copy.original);
            }
        }
        items.plan_mode = previous_plan_mode;
        if let Some(seq) = items.projected {
            if let Some(key) = key {
                self.message_ids.insert(key, seq);
            }
            if let Some(owner) = owner {
                self.late_owners.entry(seq).or_insert(owner);
            }
        }
        items.update = None;
        items.identity = MessageIdentity::default();
        new_turn && owner.is_none()
    }

    fn owns_implicit_call(&self, named: &str, id: &str) -> bool {
        // A generated turn can remain open after the next raw input has arrived.
        // Use it only while its confirmed request is the most recent checkpoint.
        let Some(owner) = self.named_turns.get(named) else {
            return false;
        };
        let completed = self.completed.get(named).is_some_and(|items| {
            items
                .get(&(CompletionScope::Published, id.to_owned()))
                .is_some_and(|p| p.kind == CompletedKind::Tool)
                || items.contains_key(&(
                    CompletionScope::Historical(CompletedKind::Tool),
                    id.to_owned(),
                ))
        });
        self.turns
            .last()
            .is_some_and(|turn| turn.sequence == *owner)
            && !completed
    }

    fn push_model_tool(&mut self, builder: &mut EventBuilder, record: &Value) -> bool {
        if record["type"] != "response_item" {
            return false;
        }
        let payload = &record["payload"];
        let stamp = parse_timestamp(record.get("timestamp"));
        let stamped = payload["internal_chat_message_metadata_passthrough"]["turn_id"]
            .as_str()
            .filter(|turn| !turn.is_empty())
            .map(|turn| self.aliases.resolve(turn));
        let current = self.active_turn_id.as_deref();
        let active = current
            .filter(|turn| !is_implicit_turn(turn))
            .map(|turn| self.aliases.resolve(turn));
        let scoped = stamped
            .or(active)
            .is_some_and(|turn| !is_implicit_turn(turn));
        if let Some(id) = scoped_model_call_id(payload, scoped) {
            let known = stamped.or(active);
            let implicit = current.filter(|_| known.is_none()).and_then(|current| {
                let named = self.aliases.resolve(current);
                self.owns_implicit_call(named, id).then(|| named.to_owned())
            });
            let turn = known.or(implicit.as_deref());
            let sequence = builder.checkpoint();
            let owner = turn.and_then(|turn| self.named_turns.get(turn).copied());
            if let Some(owner) = owner.filter(|_| turn != self.current_turn_id()) {
                self.late_owners.insert(sequence, owner);
                let mut late = CodexItems::default();
                push_response_item(builder, payload, stamp, &mut late);
                self.items.tool_events += late.tool_events;
            } else {
                push_response_item(builder, payload, stamp, &mut self.items);
            }
            self.calls
                .push(id, turn.map(str::to_owned), sequence, owner, payload);
            let key = self.paginated_tool_id(payload, turn, id);
            self.observe_model_tool(Some(sequence), record, None, key);
            return true;
        }
        let Some(id) = scoped_model_result_id(payload, scoped) else {
            return false;
        };
        let call = self.calls.take(id, stamped, active);
        if id.is_empty() && call.is_none() {
            return false;
        }
        let turn = stamped
            .or_else(|| call.as_ref()?.turn.as_deref())
            .or(active)
            .map(str::to_owned);
        let rejected = call.as_ref().is_some_and(|call| {
            self.reject_pending_display(builder, turn.as_deref(), id, call, &payload["output"])
        });
        let sequence = if let Some(turn) = &turn {
            self.push_identified_item(builder, payload, turn, id, stamp)
        } else {
            self.items.projected = None;
            self.items.tool_owner = self.legacy_revision_input(None).and_then(|input| {
                (input.admission.is_some()
                    && call
                        .as_ref()
                        .is_some_and(|call| call.sequence >= input.request_sequence))
                .then_some(input.sequence)
            });
            self.items.tool_projection.wait_for_call = rejected;
            push_response_item(builder, payload, stamp, &mut self.items);
            self.items.tool_projection.wait_for_call = false;
            self.items.tool_owner = None;
            self.finish_unscoped_result(id);
            self.items.projected
        };
        let key = self.paginated_tool_id(payload, turn.as_deref(), id);
        self.observe_model_tool(
            sequence,
            record,
            call.as_ref().map(|call| call.sequence),
            key,
        );
        if let Some(sequence) = sequence
            && let Some(call) = call
        {
            self.calls.result_calls.insert(sequence, call.sequence);
            if let Some(owner) = call.owner.or_else(|| {
                turn.as_ref()
                    .and_then(|turn| self.named_turns.get(turn).copied())
            }) {
                self.late_owners.insert(sequence, owner);
                self.late_owners.insert(call.sequence, owner);
            } else if call.turn.is_none() {
                self.calls.linked.insert(sequence, call.sequence);
            }
        }
        true
    }

    fn paginated_tool_id(&self, payload: &Value, turn: Option<&str>, id: &str) -> Option<ToolId> {
        if !self.paginated {
            return None;
        }
        ToolId::new(
            payload["internal_chat_message_metadata_passthrough"]["turn_id"]
                .as_str()
                .or(turn),
            id,
        )
    }

    fn observe_model_tool(
        &mut self,
        sequence: Option<u64>,
        record: &Value,
        call: Option<u64>,
        key: Option<ToolId>,
    ) {
        if self.paginated
            && let Some(sequence) = sequence
            && let Some(status) =
                self.paginated_answers
                    .model_tool(sequence, record, self.source_ordinal, call, key)
        {
            self.withdrawn.insert(sequence, status);
        }
    }

    fn push_identified_item(
        &mut self,
        builder: &mut EventBuilder,
        payload: &Value,
        turn: &str,
        id: &str,
        timestamp: Option<DateTime<Utc>>,
    ) -> Option<u64> {
        let display = payload["type"] == "item_completed";
        if display {
            push_codex_session_metadata(builder, payload, timestamp, true);
        }
        let native_turn = self.aliases.resolve(turn).to_owned();
        let kind = CompletedKind::for_item(payload);
        let projection = self.completion_position(builder, &native_turn, id, kind, payload);
        let original = projection.original;
        let owner = self
            .named_turns
            .get(&native_turn)
            .copied()
            .filter(|_| self.current_turn_id() != Some(native_turn.as_str()));
        let input = self.legacy_revision_input(owner).filter(|_| display);
        let message_input = input.and_then(TurnCheckpoint::admitted_sequence);
        let revision_input = input.map(|turn| turn.sequence);
        let previous_display = revision_input
            .filter(|_| owner.is_none())
            .and_then(|input| self.display_revisions.before(builder, original, input));
        // A late result cannot act as a message or tool mirror in the current turn.
        let mut late = self.items.for_late_turn();
        let checkpoint = self.checkpoint(builder);
        let items = if owner.is_some() {
            &mut late
        } else {
            &mut self.items
        };
        let message_id =
            items.prepare_identified(payload, turn, id, &native_turn, projection, self.paginated);
        items.tool_owner = self.named_turns.get(&native_turn).copied();
        items.update =
            original.or_else(|| message_id.and_then(|key| self.message_ids.get(&key).copied()));
        items.projected = None;
        let new_turn = items.project_identified(builder, payload, turn, id, timestamp);
        let previous_message = items.previous_message.take();
        let peer = items.tool_projection.peer.take();
        let projected = items.finish_identified();
        let was_displayed = self.observe_reply_revision(builder, previous_message, message_input);
        if owner.is_some() {
            self.items.tool_events += late.tool_events;
        }
        let sequence = projected?;
        self.detach_tool_display(builder, projection, peer);
        if display {
            self.deferred_receipts.observe(
                (&native_turn, id),
                sequence,
                revision_input.or(owner).unwrap_or(sequence),
                projection.reconsider,
                projection.ambiguous && projection.reconsider.is_some(),
                projection.display_identity,
            );
        }
        if let Some(input) = revision_input {
            self.display_revisions
                .observe(builder, sequence, input, previous_display);
        }
        if display {
            self.observe_display_sequence(
                builder,
                peer.unwrap_or(sequence),
                turn,
                id,
                &payload["item"],
            );
        }
        if let Some(key) = message_id {
            self.message_ids.insert(key, sequence);
        }
        let item_owner = (original.is_none()
            && payload["item"]["type"] != "UserMessage"
            && payload["item"]["type"] != "AgentMessage")
            .then(|| self.named_turns.get(&native_turn).copied())
            .flatten();
        if let Some(owner) = owner
            .filter(|_| sequence >= checkpoint.sequence)
            .or(item_owner)
        {
            self.late_owners.entry(sequence).or_insert(owner);
        } else if new_turn {
            let mut checkpoint = self.pending_turn.take().unwrap_or(checkpoint);
            checkpoint.id = Some(turn.to_owned());
            self.remember_turn(checkpoint, sequence);
        } else if payload["item"]["type"] == "UserMessage" && original.is_none() {
            self.name_request_turn(turn, sequence);
        }
        if display
            && original.is_none()
            && !was_displayed
            && payload["item"]["type"] == "AgentMessage"
            && let Some(boundary) = self.named_turns.get(&native_turn)
            && let Ok(input) = self
                .turns
                .binary_search_by_key(boundary, |turn| turn.sequence)
        {
            self.place_early_answer(builder, input, sequence);
        }
        self.remember_completion(builder, (turn, id), kind, projection, sequence, peer);
        Some(sequence)
    }

    fn observe_reply_revision(
        &mut self,
        builder: &mut EventBuilder,
        previous: Option<(u64, PreviousDisplay)>,
        input: Option<u64>,
    ) -> bool {
        if self.paginated {
            self.paginated_answers
                .archive_changed_reply(builder, previous.as_ref());
        }
        // A late completed copy cannot move an answer that Codex already displayed.
        let was_displayed = previous
            .as_ref()
            .is_some_and(|(_, prior)| prior.was_displayed_message());
        self.display_revisions
            .observe_message(builder, previous, input, &self.late_owners);
        was_displayed
    }

    fn observe_display_sequence(
        &mut self,
        builder: &mut EventBuilder,
        sequence: u64,
        turn: &str,
        id: &str,
        item: &Value,
    ) {
        if item["type"] == "Plan"
            && let Some(text) = item["text"].as_str()
        {
            let turn = self.aliases.resolve(turn);
            let owner = self
                .named_turns
                .get(turn)
                .copied()
                .or_else(|| self.turns.last().map(|turn| turn.sequence));
            self.plan_receipts.completed(turn, text, owner);
        }
        if let Some(call) = self.calls.linked.remove(&sequence)
            && let Some(owner) = self.named_turns.get(self.aliases.resolve(turn))
        {
            self.late_owners.entry(call).or_insert(*owner);
        }
        if !self.paginated {
            return;
        }
        if matches!(
            item["type"].as_str(),
            Some("Plan" | "AgentMessage" | "ExitedReviewMode")
        ) {
            if let Some(text) = builder
                .event_at_mut(sequence)
                .and_then(|event| event.payload["text"].as_str())
            {
                self.paginated_answers.display(
                    sequence,
                    turn,
                    id,
                    text,
                    self.source_ordinal,
                    item["type"] == "Plan",
                );
            }
            if !completed_item::assistant::is_async(&item["delivery"]) {
                return;
            }
        }
        if self.withdrawn.contains_key(&sequence) {
            return;
        }
        if item["type"] == "UserMessage" {
            self.paginated_inputs
                .display_user(sequence, turn, item, self.source_ordinal);
        } else {
            let withdrawal = match item["type"].as_str() {
                None => None,
                _ => self
                    .paginated_answers
                    .display_tool(sequence, turn, id, item),
            };
            if let Some(status) = withdrawal {
                self.withdrawn.insert(sequence, status);
            }
        }
    }

    fn place_early_answer(&mut self, builder: &EventBuilder, input: usize, sequence: u64) {
        let turn = &self.turns[input];
        if sequence >= turn.request_sequence || self.display_positions.contains_key(&sequence) {
            return;
        }
        let next = self
            .turns
            .get(input + 1)
            .map_or_else(|| builder.checkpoint(), |next| next.sequence);
        self.display_positions
            .insert(sequence, (next, self.source_ordinal));
        self.late_owners.insert(sequence, turn.sequence);
    }

    /// A migrated request can precede its generated display turn.
    fn name_request_turn(&mut self, turn: &str, sequence: u64) {
        let Some(index) = self
            .turns
            .partition_point(|checkpoint| checkpoint.sequence <= sequence)
            .checked_sub(1)
        else {
            return;
        };
        let checkpoint = &mut self.turns[index];
        if let Some(original) = &checkpoint.id
            && self.aliases.bind(turn, original)
        {
            self.message_ids.bind(turn, original);
            self.occurrences.bind(turn, original);
            if let Some(known) = self.completed.remove(turn) {
                self.completed
                    .entry(original.clone())
                    .or_default()
                    .extend(known);
            }
            if self.plan_turns.remove(turn) {
                self.plan_turns.insert(original.clone());
            }
        }
        let canonical = self.aliases.resolve(turn);
        checkpoint.id = Some(canonical.to_owned());
        self.named_turns
            .insert(canonical.to_owned(), checkpoint.sequence);
        if self
            .pending_turn
            .as_ref()
            .is_some_and(|pending| sequence < pending.sequence)
        {
            self.pending_turn = None;
        }
    }
}

fn encoded_rollback(record: &Value) -> bool {
    let payload = &record["payload"];
    record["type"] == "event_msg"
        && payload
            .get("type")
            .or_else(|| payload.get(0))
            .is_some_and(|tag| {
                tag == "thread_rolled_back"
                    || tag.as_u64() == Some(u64::from(rollout_lines::ROLLBACK_TAG))
            })
}

fn contains_rollback_marker(path: &Path) -> Result<bool> {
    if !contains_rollback_marker_with_chunk(path, 1024 * 1024)? {
        return Ok(false);
    }
    let mut rollback = false;
    rollout_file::visit(path, |record, _, _| {
        rollback |= encoded_rollback(&record);
        Ok(())
    })?;
    Ok(rollback)
}

fn contains_rollback_marker_with_chunk(path: &Path, chunk: usize) -> Result<bool> {
    const MARKER: &[u8] = b"thread_rolled_back";
    let marker = memchr::memmem::Finder::new(MARKER);
    let tag = rollout_lines::ROLLBACK_TAG.to_string();
    let numeric = memchr::memmem::Finder::new(tag.as_bytes());
    let escape = memchr::memmem::Finder::new(b"\\u00");
    let mut file = fs::File::open(path)?;
    // Windows overlap by `MARKER.len() - 1` bytes, so each read needs room for one new byte.
    let mut buffer = vec![0_u8; chunk.max(MARKER.len())];
    let mut overlap = 0;
    loop {
        let read = file.read(&mut buffer[overlap..])?;
        if read == 0 {
            return Ok(false);
        }
        let length = overlap + read;
        let window = &buffer[..length];
        // Writers never need to escape letters, so an escaped letter may be hiding the marker.
        if marker.find(window).is_some()
            // A possible numeric tag is enough to select the full reader, which checks its context.
            || numeric.find_iter(window).any(|start| {
                start.checked_sub(1).and_then(|i| window.get(i)).is_some_and(|b|
                    matches!(b, b':' | b'[') || b.is_ascii_whitespace()
                ) && window.get(start + tag.len()).is_some_and(|b|
                    matches!(b, b',' | b'}') || b.is_ascii_whitespace()
                )
            })
            || escape
                .find_iter(window)
                .any(|position| escapes_ascii_letter(window.get(position + 4..position + 6)))
        {
            return Ok(true);
        }
        overlap = (MARKER.len() - 1).min(length);
        buffer.copy_within(length - overlap..length, 0);
    }
}

fn escapes_ascii_letter(hex: Option<&[u8]>) -> bool {
    hex.and_then(|hex| std::str::from_utf8(hex).ok())
        .and_then(|hex| u8::from_str_radix(hex, 16).ok())
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
}

/// Published displays can update or reorder earlier events. An outline cannot apply those edits.
#[derive(Default)]
struct DisplayReplay {
    model_message: bool,
    display_message: bool,
}

impl DisplayReplay {
    fn observe(&mut self, record: &Value) -> bool {
        let payload = &record["payload"];
        match record["type"].as_str() {
            Some("session_meta") => payload["history_mode"] == "paginated",
            Some("event_msg")
                if matches!(
                    payload["type"].as_str(),
                    Some("agent_message" | "user_message")
                ) =>
            {
                self.display_message = true;
                self.model_message
                    || payload["turn_id"].is_string()
                    || payload["item_id"].is_string()
            }
            Some("event_msg") => !matches!(
                payload["type"].as_str(),
                Some(
                    "thread_settings_applied"
                        | "thread_rolled_back"
                        | "token_count"
                        | "task_started"
                        | "turn_started"
                        | "task_complete"
                        | "turn_complete"
                )
            ),
            Some("response_item") => {
                self.model_message |= payload["type"] == "message";
                self.display_message
                    || payload["type"] != "message"
                    || payload["id"].is_string()
                    || payload["internal_chat_message_metadata_passthrough"]["turn_id"].is_string()
                    || model_cut::input_order(&record["metadata"]).is_some()
                    || payload["content"].as_array().is_some_and(|parts| {
                        parts.iter().any(|part| {
                            !matches!(
                                part["type"].as_str(),
                                Some("input_text" | "output_text" | "text")
                            )
                        })
                    })
            }
            Some("compacted") => true,
            _ => false,
        }
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
        json!({"omitted_events":1,"native_history_unverified":true,"reason":"The rollout references earlier history that was not read."}),
        timestamp,
        ReplayPolicy::HistoricalOnly,
        Some(UNAVAILABLE_ANCESTRY.to_owned()),
        None,
    );
}

fn push_record(builder: &mut EventBuilder, record: &Value, items: &mut CodexItems) -> bool {
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
                return push_response_item(builder, payload, timestamp, items);
            }
        }
        Some("event_msg") => {
            if let Some(payload) = record.get("payload") {
                push_codex_session_metadata(builder, payload, timestamp, true);
                return push_event_message(builder, payload, timestamp, items);
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

#[derive(Default)]
struct ToolProjection {
    wait_for_call: bool,
    ambiguous: bool,
    paginated: bool,
    peer: Option<u64>,
    matched_turn: Option<String>,
}

#[derive(Default)]
struct CodexItems {
    shell_sources: completed_item::ShellSources,
    tool_projection: ToolProjection,
    message: Option<VisibleMessage>,
    previous_message: Option<(u64, PreviousDisplay)>,
    mirrors: MessageMirrors,
    identity: MessageIdentity,
    cited_display: bool,
    plan_mode: bool,
    raw_proposal: Option<[u8; 32]>,
    in_turn: bool,
    update: Option<u64>,
    projected: Option<u64>,
    // A completed display item and a response item can describe the same tool result.
    outcomes: PendingOutcomes,
    tool_turn: Option<String>,
    tool_owner: Option<u64>,
    tool_events: usize,
}

struct VisibleMessage {
    kind: EventKind,
    sequence: u64,
}

fn is_implicit_turn(id: &str) -> bool {
    id.strip_prefix("rollout-")
        .is_some_and(|index| !index.is_empty() && index.bytes().all(|byte| byte.is_ascii_digit()))
}

impl CodexItems {
    fn for_late_turn(&self) -> Self {
        Self {
            shell_sources: self.shell_sources.clone(),
            ..Self::default()
        }
    }

    fn can_publish_empty(&self, kind: &EventKind) -> bool {
        match kind {
            EventKind::MessageAssistant => {
                self.update.is_some() || self.identity.scoped(true).is_some()
            }
            EventKind::MessageUser => self.update.is_some(),
            _ => false,
        }
    }

    fn finish_identified(&mut self) -> Option<u64> {
        self.tool_projection.wait_for_call = false;
        self.tool_projection.ambiguous = false;
        self.tool_turn = None;
        self.tool_owner = None;
        self.update = None;
        self.identity = MessageIdentity::default();
        self.projected
    }

    fn clear_mirrors(&mut self) {
        self.message = None;
        self.mirrors.clear();
        self.outcomes = PendingOutcomes::default();
    }

    fn turn_boundary(&mut self, turn_id: Option<&str>) {
        if turn_id.is_some_and(is_implicit_turn) {
            // The migrator writes model records before synthesizing a display turn.
            self.mirrors.retain_unpaired_models();
            self.outcomes.retain_models();
        } else {
            self.clear_mirrors();
        }
    }

    /// Codex displays a completed plan as a separate assistant item, even when
    /// another assistant message in the turn has the same text.
    fn project_identified(
        &mut self,
        builder: &mut EventBuilder,
        payload: &Value,
        turn: &str,
        id: &str,
        timestamp: Option<DateTime<Utc>>,
    ) -> bool {
        if payload["type"] != "item_completed" {
            return push_response_item(builder, payload, timestamp, self);
        }
        if let Some(item_type @ ("Plan" | "ExitedReviewMode")) = payload["item"]["type"].as_str()
            && let Some(text) = payload["item"]["text"].as_str()
        {
            let content = json!({"type":item_type,"turn_id":turn,"id":id,"text":text});
            self.push_published_text(builder, content, timestamp);
            return false;
        }
        push_completed_item(builder, &payload["item"], timestamp, self)
    }

    fn push_published_text(
        &mut self,
        builder: &mut EventBuilder,
        payload: Value,
        timestamp: Option<DateTime<Utc>>,
    ) {
        self.message = None;
        if let Some(seq) = self.update
            && let Some(previous) = builder.event_at_mut(seq)
        {
            self.previous_message = Some((
                seq,
                PreviousDisplay::replace_payload(
                    previous,
                    EventKind::MessageAssistant,
                    payload,
                    "event_msg.item_completed",
                ),
            ));
            self.projected = Some(seq);
        } else {
            self.projected = Some(builder.checkpoint());
            builder.push(
                EventKind::MessageAssistant,
                payload,
                timestamp,
                ReplayPolicy::Contextual,
                Some("event_msg.item_completed".to_owned()),
                None,
            );
        }
    }

    fn take_tool_outcome(&mut self, id: &str, display: bool) -> Option<u64> {
        if self.tool_projection.wait_for_call {
            return None;
        }
        let position = if self.tool_projection.paginated || !display {
            self.tool_projection.peer
        } else {
            self.update
        };
        let entry = self.outcomes.take(
            id,
            display,
            position,
            self.tool_turn.as_deref(),
            self.tool_owner,
        )?;
        self.tool_projection.matched_turn = entry.turn;
        Some(entry.sequence)
    }

    fn push_historical(
        &mut self,
        builder: &mut EventBuilder,
        kind: EventKind,
        mut payload: Value,
        timestamp: Option<DateTime<Utc>>,
        raw_type: String,
        result_id: Option<&str>,
    ) {
        self.message = None;
        clear_projection_metadata(&mut payload);
        let display = raw_type == "event_msg.item_completed";
        if display && self.tool_projection.ambiguous {
            completion_positions::unattribute_display(&mut payload);
        }
        let result_id = result_id.filter(|id| {
            !id.is_empty()
                || self
                    .tool_turn
                    .as_deref()
                    .is_some_and(|turn| !is_implicit_turn(turn))
        });
        let matched = result_id.and_then(|id| self.take_tool_outcome(id, display));
        let physical = self.tool_projection.paginated || !display;
        let mirror = physical
            .then(|| self.tool_projection.peer.take().or(matched))
            .flatten();
        let original = if physical {
            self.update
        } else {
            self.update.or(matched).filter(|&sequence| {
                builder.event_at_mut(sequence).is_some_and(|event| {
                    if !completion_positions::model_output(event)
                        || completed_item::equivalent_model_receipt(
                            &payload,
                            &event.payload,
                            &self.shell_sources,
                        )
                    {
                        return true;
                    }
                    if payload["type"] == "FunctionCallOutput"
                        && event.payload["omnisession_codex_tool_display"]["type"]
                            == "FunctionCallOutput"
                    {
                        completion_positions::restore_model_output(event);
                    }
                    false
                })
            })
        };
        if let Some(sequence) = original
            && let Some(previous) = builder.event_at_mut(sequence)
        {
            let has_model = completion_positions::model_output(previous);
            let failed_display = previous.kind == EventKind::ToolFailed
                && (!has_model || model_result_kind(&previous.payload) != EventKind::ToolFailed);
            if display && has_model {
                if !self.tool_projection.paginated {
                    completed_item::annotate_model_receipt(&mut previous.payload, &payload);
                }
                // Codex shows a command's display output but gives the model the native result.
                previous.kind = if kind == EventKind::ToolFailed {
                    kind
                } else {
                    model_result_kind(&previous.payload)
                };
            } else {
                previous.kind = if !display && failed_display {
                    EventKind::ToolFailed
                } else {
                    kind
                };
                previous.payload = payload;
                previous.source.raw_record_type = Some(raw_type);
                previous.replay_policy = ReplayPolicy::HistoricalOnly;
            }
            self.finish_tool_projection(builder, sequence, display, mirror);
            return;
        }
        let sequence = builder.checkpoint();
        if let Some(id) = result_id.filter(|_| mirror.is_none() && !self.tool_projection.ambiguous)
        {
            self.outcomes.push(
                id,
                display,
                sequence,
                self.tool_turn.clone(),
                self.tool_owner,
            );
        }
        if (self.tool_projection.paginated || mirror.is_none())
            && !matches!(kind, EventKind::PlanUpdated | EventKind::ProviderEvent)
        {
            self.tool_events += 1;
        }
        builder.push(
            kind,
            payload,
            timestamp,
            ReplayPolicy::HistoricalOnly,
            Some(raw_type),
            None,
        );
        self.finish_tool_projection(builder, sequence, display, mirror);
    }

    fn prepare_identified(
        &mut self,
        payload: &Value,
        turn: &str,
        id: &str,
        native_turn: &str,
        projection: completion_positions::Projection,
        paginated: bool,
    ) -> Option<message_mirrors::ScopedMessageId> {
        let user = payload["item"]["type"] == "UserMessage";
        self.identity = MessageIdentity::completed(turn, id, user);
        let scoped = MessageIdentity::completed(native_turn, id, user);
        self.tool_projection = projection.tool_projection(paginated);
        self.tool_turn = Some(native_turn.to_owned());
        self.cited_display = matches!(
            payload["item"]["memory_citation"],
            Value::Object(_) | Value::Array(_)
        );
        match payload["item"]["type"].as_str() {
            Some("AgentMessage") => scoped.scoped(true),
            Some("UserMessage") => scoped.scoped(false),
            _ => None,
        }
    }
    fn finish_tool_projection(
        &mut self,
        builder: &mut EventBuilder,
        sequence: u64,
        display: bool,
        mirror: Option<u64>,
    ) {
        self.projected = Some(sequence);
        self.tool_projection.peer = mirror.filter(|&other| {
            let (slot, model) = if display {
                (sequence, other)
            } else {
                (other, sequence)
            };
            completion_positions::pair(builder, slot, model, &self.shell_sources)
        });
    }
}

fn clear_projection_metadata(payload: &mut Value) {
    if let Some(fields) = payload.as_object_mut() {
        fields.remove("omnisession_codex_rollback");
        fields.remove("omnisession_codex_checkpoint");
        fields.remove("omnisession_codex_prior_display");
        fields.remove("omnisession_codex_active_replies");
        fields.remove("omnisession_codex_checkpoint_position");
        fields.remove("omnisession_codex_tool_display");
    }
}

fn push_completed_item(
    builder: &mut EventBuilder,
    item: &Value,
    timestamp: Option<DateTime<Utc>>,
    items: &mut CodexItems,
) -> bool {
    let kind = match item["type"].as_str() {
        Some("UserMessage") => EventKind::MessageUser,
        Some("AgentMessage") => EventKind::MessageAssistant,
        _ => {
            if let Some((kind, payload)) = completed_item::historical_item(item) {
                let id = completed_item::model_receipt(item)
                    .then(|| item["id"].as_str())
                    .flatten();
                let payload = completed_item::with_source_receipt(payload, item);
                items.shell_sources.remember(&payload, item);
                items.push_historical(
                    builder,
                    kind,
                    payload,
                    timestamp,
                    "event_msg.item_completed".to_owned(),
                    id,
                );
            }
            return false;
        }
    };
    let Some(content) = item["content"].as_array() else {
        return false;
    };
    let parts = content
        .iter()
        .filter_map(|part| match kind {
            EventKind::MessageAssistant => completed_item::assistant::text(part),
            EventKind::MessageUser if part["type"] == "text" => part["text"].as_str(),
            _ => None,
        })
        .collect::<Vec<_>>();
    let contextual = parts.iter().any(|part| is_contextual_user_text(part));
    let alternate = (parts.len() > 1).then(|| parts.join("\n"));
    let text = omitted_input_media(&kind, &parts, content).unwrap_or_else(|| parts.concat());
    if text.is_empty() && !items.can_publish_empty(&kind) {
        return false;
    }
    let attachment = (kind == EventKind::MessageUser)
        .then(|| attachments::completed(content, &parts))
        .flatten();
    push_message_text(
        builder,
        kind,
        &text,
        timestamp,
        "event_msg.item_completed",
        MessageMatch {
            alternate: alternate.as_deref(),
            attachment,
            ..MessageMatch::default()
        },
        items,
    ) && !contextual
}

fn push_message_text<'a>(
    builder: &mut EventBuilder,
    kind: EventKind,
    text: &str,
    timestamp: Option<DateTime<Utc>>,
    raw_type: &'static str,
    matching: impl Into<MessageMatch<'a>>,
    items: &mut CodexItems,
) -> bool {
    let mut matching = matching.into();
    matching.identity = items.identity;
    let plan_text = (kind == EventKind::MessageAssistant
        && raw_type == "response_item.message"
        && items.plan_mode)
        .then_some(matching.alternate.unwrap_or(text));
    items.raw_proposal = plan_text.and_then(display_text::proposal_digest);
    let plan_display = plan_text.and_then(display_text::without_plan);
    let text = if let Some(visible) = &plan_display {
        // Plan-only replies have a dedicated completed Plan item and no assistant message.
        if visible.trim().is_empty() {
            return false;
        }
        matching.alternate = None;
        visible.as_str()
    } else {
        text
    };
    if kind == EventKind::MessageAssistant {
        matching.citation = if raw_type == "response_item.message" {
            message_mirrors::model_citation_spelling(matching.alternate.unwrap_or(text))
        } else {
            items
                .cited_display
                .then(|| message_mirrors::display_citation_spelling(text))
        };
    }
    let user_turn = kind == EventKind::MessageUser && !is_contextual_user_text(text);
    if text.is_empty()
        && !(raw_type == "event_msg.item_completed" && items.can_publish_empty(&kind))
    {
        return user_turn;
    }
    if !items.in_turn {
        // Without a named turn, only adjacent content can identify a mirror.
        matching.adjacent = Some(
            items
                .message
                .as_ref()
                .map_or_else(|| builder.checkpoint(), |message| message.sequence),
        );
    }
    let update = items
        .update
        .or_else(|| items.mirrors.native_match(&kind, items.identity, raw_type))
        .filter(|seq| builder.event_at_mut(*seq).is_some());
    let matched = if let Some(sequence) = update {
        items.mirrors.update(
            &kind,
            text,
            matching,
            raw_type,
            sequence,
            builder.checkpoint(),
        );
        Some(sequence)
    } else {
        items
            .mirrors
            .observe(&kind, text, matching, raw_type, builder.checkpoint())
    };
    let matched = matched.filter(|seq| {
        builder
            .event_at_mut(*seq)
            .is_some_and(|event| !completion_positions::model_output(event))
    });
    let sequence = matched.unwrap_or_else(|| builder.checkpoint());
    items.message = Some(VisibleMessage {
        kind: kind.clone(),
        sequence,
    });
    items.projected = Some(sequence);
    if let Some(old) = matched.and_then(|seq| builder.event_at_mut(seq))
        && (update.is_some() || raw_type != "response_item.message")
    {
        items.previous_message = PreviousDisplay::replace_message(old, kind, text, raw_type)
            .map(|previous| (sequence, previous));
        return false;
    }
    if matched.is_some() {
        return false;
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

fn hook_attribution(metadata: &Value, index: usize) -> Option<bool> {
    let source = &metadata["content_item_metadata"][index]["provenance"];
    match source["type"].as_str() {
        Some("user") => return Some(false),
        Some("harness") if source["hook"] == true => return Some(true),
        _ => {}
    }
    match metadata["content_item_kinds"][index].as_str() {
        Some("user.text") => Some(false),
        Some("hooks.additional_context") => Some(true),
        _ => None,
    }
}

/// Codex serializes each stop-hook fragment as one XML element with a run ID and escaped text.
fn is_hook_fragment(text: &str) -> bool {
    let Some(fragment) = text.trim().strip_prefix("<hook_prompt") else {
        return false;
    };
    let Some((attribute, body)) = fragment.split_once('>') else {
        return false;
    };
    let Some((name, value)) = attribute.split_once('=') else {
        return false;
    };
    let mut value = value.trim().chars();
    let Some(quote @ ('\'' | '"')) = value.next() else {
        return false;
    };
    attribute.starts_with(char::is_whitespace)
        && name.trim() == "hook_run_id"
        && value
            .as_str()
            .strip_suffix(quote)
            .is_some_and(|id| !id.trim().is_empty() && !id.contains([quote, '<']))
        && body
            .strip_suffix("</hook_prompt>")
            .is_some_and(|body| !body.contains('<'))
}

/// Codex adds framing text around attachments when it builds the model input.
fn message_parts(content: &[Value], mut keep: impl FnMut(usize, &str) -> bool) -> Vec<&str> {
    let part_type = |index: Option<usize>| {
        index
            .and_then(|index| content.get(index))
            .and_then(|part| part.get("type"))
            .and_then(Value::as_str)
    };
    content
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
            keep(*index, text)
                && !is_media_wrapper(
                    text,
                    part_type(index.checked_sub(1)),
                    part_type(Some(index + 1)),
                )
        })
        .map(|(_, text)| text)
        .collect()
}

fn omitted_input_media(kind: &EventKind, parts: &[&str], content: &[Value]) -> Option<String> {
    if *kind != EventKind::MessageUser || parts.iter().any(|part| !part.trim().is_empty()) {
        return None;
    }
    let images = content
        .iter()
        .filter(|part| {
            matches!(
                part["type"].as_str(),
                Some("input_image" | "image" | "local_image")
            )
        })
        .count();
    let audio = content.iter().any(|part| {
        matches!(
            part["type"].as_str(),
            Some("input_audio" | "audio" | "local_audio")
        )
    });
    omitted_media_text(images, audio)
}

fn omitted_media_text(images: usize, audio: bool) -> Option<String> {
    match (images, audio) {
        (0, false) => None,
        (0, true) => Some("[audio omitted]".to_owned()),
        (_, false) => Some(omitted_images_text(images)),
        (_, true) => Some(format!("{} [audio omitted]", omitted_images_text(images))),
    }
}

// Before per-part attribution, a hook message contained only hook fragments and contextual input.
fn legacy_hook_context(payload: &Value, content: &[Value], metadata: &Value) -> bool {
    !payload["id"]
        .as_str()
        .is_some_and(|id| id.starts_with("msg_input_"))
        && content.iter().enumerate().all(|(index, part)| {
            part["type"] == "input_text"
                && part["text"].as_str().is_some_and(|text| {
                    hook_attribution(metadata, index).unwrap_or_else(|| is_hook_fragment(text))
                        || is_contextual_user_text(text)
                })
        })
}

fn push_response_item(
    builder: &mut EventBuilder,
    payload: &Value,
    timestamp: Option<DateTime<Utc>>,
    items: &mut CodexItems,
) -> bool {
    let item_type = payload.get("type").and_then(Value::as_str).unwrap_or("");
    if paginated_rollback::is_private_instruction(payload) {
        return false;
    }
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
                None,
                items,
            );
        }
        let content = payload["content"].as_array().map_or(&[][..], Vec::as_slice);
        let metadata = &payload["internal_chat_message_metadata_passthrough"];
        let user = kind == EventKind::MessageUser;
        let hook_context = user && legacy_hook_context(payload, content, metadata);
        let mut omitted_hook = false;
        let mut inferred_hook = false;
        let parts = message_parts(content, |index, text| {
            let attribution = hook_attribution(metadata, index);
            let hook = user
                && content[index]["type"] == "input_text"
                && attribution.unwrap_or_else(|| hook_context && is_hook_fragment(text));
            omitted_hook |= hook;
            inferred_hook |= hook && attribution.is_none();
            !hook
        });
        if inferred_hook {
            builder.push(
                EventKind::ProviderEvent,
                json!({"content_omitted": true, "reason": "Legacy hook input has no user attribution."}),
                timestamp,
                ReplayPolicy::HistoricalOnly,
                Some("omnisession.codex_hook_input_omitted".to_owned()),
                None,
            );
        }
        let contextual = parts.iter().any(|text| is_contextual_user_text(text));
        let text = omitted_input_media(&kind, &parts, content).unwrap_or_else(|| parts.join("\n"));
        if omitted_hook && text.is_empty() {
            return false;
        }
        // Codex joins the original text parts without separators in display events.
        let alternate = (parts.len() > 1).then(|| parts.concat());
        let attachment = (kind == EventKind::MessageUser)
            .then(|| attachments::model(content, &parts))
            .flatten();
        return push_message_text(
            builder,
            kind,
            &text,
            timestamp,
            "response_item.message",
            MessageMatch {
                alternate: alternate.as_deref(),
                attachment,
                ..MessageMatch::default()
            },
            items,
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
        | "computer_call_output" => Some(model_result_kind(payload)),
        _ => None,
    };
    let Some(kind) = kind else {
        return false;
    };
    let scoped = items
        .tool_turn
        .as_deref()
        .is_some_and(|turn| !is_implicit_turn(turn));
    let result_id = scoped_model_result_id(payload, scoped);
    items.push_historical(
        builder,
        kind,
        completed_item::with_source_receipt(compact_tool_value(payload, None), payload),
        timestamp,
        format!("response_item.{item_type}"),
        result_id,
    );
    false
}

fn model_call_id(payload: &Value) -> Option<&str> {
    scoped_model_call_id(payload, false)
}

fn scoped_model_call_id(payload: &Value, scoped: bool) -> Option<&str> {
    match payload["type"].as_str()? {
        "function_call" | "custom_tool_call" | "web_search_call" | "computer_call"
        | "local_shell_call" => tool_wire_id(payload, scoped),
        _ => None,
    }
}

fn scoped_model_result_id(payload: &Value, scoped: bool) -> Option<&str> {
    match payload["type"].as_str()? {
        "function_call_output"
        | "custom_tool_call_output"
        | "local_shell_call_output"
        | "computer_call_output" => tool_wire_id(payload, scoped),
        _ => None,
    }
}

fn tool_wire_id(payload: &Value, scoped: bool) -> Option<&str> {
    if scoped && payload["call_id"].as_str() == Some("") {
        Some("")
    } else {
        string_at(payload, &[&["call_id"], &["id"]])
    }
}

fn model_result_kind(payload: &Value) -> EventKind {
    if matches!(payload["status"].as_str(), Some("failed" | "error")) {
        EventKind::ToolFailed
    } else {
        EventKind::ToolCompleted
    }
}

fn omitted_tool_value(value: &Value) -> Value {
    let mut omitted = json!({"content_omitted": true});
    if let Some(text) = value.as_str() {
        omitted["original_bytes"] = text.len().into();
    } else if let Some(items) = value.as_array() {
        omitted["original_items"] = items.len().into();
    }
    omitted
}

fn compact_tool_value(value: &Value, field: Option<&str>) -> Value {
    let output_part = field.filter(|key| *key == "output");
    if output_part.is_some()
        && matches!(
            value["type"].as_str(),
            Some("input_image" | "input_audio" | "encrypted_content")
        )
    {
        let mut part = json!({"type": value["type"]});
        for key in ["image_url", "file_id", "audio_url", "encrypted_content"] {
            if let Some(content) = value.get(key) {
                part[key] = omitted_tool_value(content);
            }
        }
        if let Some(detail) = value
            .get("detail")
            .filter(|v| checkpoint_wire::image_detail(v))
        {
            part["detail"] = detail.clone();
        }
        return part;
    }
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
        return omitted_tool_value(value);
    }
    match value {
        Value::String(value) => Value::String(compact_tool_string(value)),
        Value::Array(values) if values.len() > MAX_TOOL_ARRAY_ITEMS => {
            let edge = MAX_TOOL_ARRAY_ITEMS / 2;
            let mut compact = values
                .iter()
                .take(edge)
                .map(|value| compact_tool_value(value, output_part))
                .collect::<Vec<_>>();
            compact.push(json!({
                "content_omitted": true,
                "omitted_items": values.len() - MAX_TOOL_ARRAY_ITEMS,
            }));
            compact.extend(
                values
                    .iter()
                    .skip(values.len() - edge)
                    .map(|value| compact_tool_value(value, output_part)),
            );
            Value::Array(compact)
        }
        Value::Array(values) => Value::Array(
            values
                .iter()
                .map(|value| compact_tool_value(value, output_part))
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
    items: &mut CodexItems,
) -> bool {
    let event_type = payload.get("type").and_then(Value::as_str).unwrap_or("");
    if event_type == "item_completed" {
        return push_completed_item(builder, &payload["item"], timestamp, items);
    }
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
    let placeholder = if text.trim().is_empty() && kind == EventKind::MessageUser {
        let images = ["images", "file_ids", "local_images"]
            .into_iter()
            .filter_map(|field| payload.get(field).and_then(Value::as_array))
            .map(Vec::len)
            .sum();
        let audio = ["audio", "audio_references", "local_audio"]
            .into_iter()
            .any(|field| {
                payload[field]
                    .as_array()
                    .is_some_and(|audio| !audio.is_empty())
            });
        omitted_media_text(images, audio)
    } else {
        None
    };
    let attachment = (kind == EventKind::MessageUser)
        .then(|| attachments::legacy(payload, text))
        .flatten();
    push_message_text(
        builder,
        kind,
        placeholder.as_deref().unwrap_or(text),
        timestamp,
        "event_msg",
        MessageMatch {
            attachment,
            ..MessageMatch::default()
        },
        items,
    )
}

/// A tag typed without its matching attachment stays visible.
fn is_media_wrapper(text: &str, before: Option<&str>, after: Option<&str>) -> bool {
    if matches!(
        (text, before),
        ("</image>", Some("input_image")) | ("</audio>", Some("input_audio"))
    ) {
        return true;
    }
    match after {
        Some("input_image") => {
            text == "<image>" || (text.starts_with("<image name=") && text.ends_with('>'))
        }
        Some("input_audio") => {
            text == "<audio>" || (text.starts_with("<audio name=") && text.ends_with('>'))
        }
        _ => false,
    }
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
        self.index_rollout(session, native, source_path, full_read_bytes)
    }

    fn index_search_document(
        &self,
        session: &SessionRef,
        source_path: Option<&Path>,
        full_read_bytes: u64,
    ) -> Result<IndexRead> {
        validate_provider(session, Provider::Codex)?;
        let native = self.require_rollout(&session.id, source_path)?.session;
        self.index_rollout(session, native, source_path, full_read_bytes)
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
    fn rollback_scan_ignores_unicode_escapes() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let escaped = temporary.path().join("escaped.jsonl");
        fs::write(
            &escaped,
            "{\"type\":\"event_msg\",\"payload\":{\"text\":\"\\u001b[31mred\\u001b[0m\"}}\n",
        )
        .expect("escaped rollout");
        assert!(!contains_rollback_marker(&escaped).expect("scan escaped rollout"));

        let rolled_back = temporary.path().join("rolled-back.jsonl");
        fs::write(
            &rolled_back,
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"thread_rolled_back\",\"num_turns\":1}}\n",
        )
        .expect("rolled back rollout");
        assert!(contains_rollback_marker(&rolled_back).expect("scan rolled back rollout"));

        let escaped_marker = temporary.path().join("escaped-marker.jsonl");
        fs::write(
            &escaped_marker,
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"thread_rolled_\\u0062ack\"}}\n",
        )
        .expect("escaped marker rollout");
        assert!(contains_rollback_marker(&escaped_marker).expect("scan escaped marker rollout"));
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

    fn naive_rollback_marker(bytes: &[u8]) -> bool {
        const MARKER: &[u8] = b"thread_rolled_back";
        bytes.windows(MARKER.len()).any(|window| window == MARKER)
            || bytes.windows(4).any(|token| {
                &token[1..3] == b"14"
                    && (matches!(token[0], b'[' | b':') || token[0].is_ascii_whitespace())
                    && (matches!(token[3], b',' | b'}') || token[3].is_ascii_whitespace())
            })
            || bytes.windows(6).any(|window| {
                window.starts_with(b"\\u00")
                    && std::str::from_utf8(&window[4..])
                        .ok()
                        .and_then(|hex| u8::from_str_radix(hex, 16).ok())
                        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
            })
    }

    /// JSON unicode escape of one byte, e.g. the escaped `b` a writer may emit.
    fn json_escape(byte: u8) -> String {
        format!("\\u{byte:04x}")
    }

    fn rollback_filler() -> impl Strategy<Value = u8> {
        prop_oneof![
            8 => b'a'..=b'z',
            1 => Just(b'\\'),
            1 => Just(b'u'),
            1 => Just(b'0'),
            1 => Just(b'_'),
            1 => Just(b'"'),
            1 => Just(b'\n'),
        ]
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

        #[test]
        fn rollback_scan_matches_naive_search_across_chunk_boundaries(
            filler in proptest::collection::vec(rollback_filler(), 0..160),
            marker in prop_oneof![
                Just(String::new()),
                Just("thread_rolled_back".to_owned()),
                Just("[14,1]".to_owned()),
                Just(r#"{"num_turns":1,"type":14}"#.to_owned()),
                Just("[   14  ,1]".to_owned()),
                Just("[ 140 ,1]".to_owned()),
                Just(format!("thread_rolled_{}ack", json_escape(b'b'))),
                Just(format!("{}hread_rolled_back", json_escape(b't'))),
                Just(format!("{}[31m", json_escape(0x1b))),
            ],
            chunk in 18_usize..48,
            window in 0_usize..4,
            shift in 0_usize..24,
            random_offset in any::<proptest::sample::Index>(),
            near_boundary in any::<bool>(),
        ) {
            // After the first window, each read advances by `chunk` minus the 17-byte overlap.
            let boundary = chunk + window * (chunk - 17);
            let offset = if near_boundary {
                boundary.saturating_sub(shift).min(filler.len())
            } else {
                random_offset.index(filler.len() + 1)
            };
            let bytes = [&filler[..offset], marker.as_bytes(), &filler[offset..]].concat();
            let temporary = tempfile::tempdir().expect("temporary directory");
            let path = temporary.path().join("rollout.jsonl");
            fs::write(&path, &bytes).expect("synthetic rollout");

            prop_assert_eq!(
                contains_rollback_marker_with_chunk(&path, chunk).expect("chunked scan"),
                naive_rollback_marker(&bytes)
            );
        }
    }
}
