use std::{
    borrow::Cow,
    cmp::Reverse,
    collections::{BinaryHeap, HashMap, HashSet, VecDeque},
    ffi::OsString,
    fs,
    io::{ErrorKind, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, TimeDelta, Utc};
use omnis_core::{CODEX_ROLLOUT_DIRECTORY_DEPTH, unique_codex_rollout_home};
use omnis_ir::{
    CanonicalSnapshot, EventKind, GitState, Provider, ReplayPolicy, SessionRef, SourceRevision,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    CodexWorkspaceProjection, EnvChange, IndexRead, LaunchPlan, LaunchTarget, NativeResumeSupport,
    NativeSession, NativeSessionName, NativeWorkspace, ProviderAdapter, ProviderInstallation,
    RollbackTitle,
    support::{
        EventBuilder, JsonLine, JsonLineSkips, JsonlRecord, MAX_PREVIEW_TAIL_SIZE,
        MAX_STREAMED_TRANSCRIPT_FILE_SIZE, MAX_STREAMED_TRANSCRIPT_LINE_SIZE,
        json_lines_preview_with_omissions, omitted_images_text, parse_timestamp, paths_match,
        provider_executable, provider_file, sort_sessions, string_at, validate_provider, value_at,
        visit_index_json_lines_with_gaps, visit_json_lines_tail,
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
mod paginated_compaction;
mod paginated_preview;
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
mod workspace_catalog;
mod workspace_check;
mod workspace_damage;
mod workspace_records;
mod workspace_search;
mod workspace_settings;

use current_rollout::{
    CurrentFile, CurrentRolloutStamp, CurrentRolloutUnavailable, CurrentRollouts,
};
use paginated_answers::{PaginatedAnswers, ToolId};
use paginated_compaction::{
    DisplayEvidence, PaginatedCompactions, ProjectedReplacement, WindowKind,
};
use paginated_rollback::{InputPosition, LegacyPrivateInputs, PaginatedRollback, Withdrawal};
use rollout_cache::{ObservedRollout, RolloutCache, RolloutHeader, RolloutStamp};
use rollout_lines::{Admission, OrdinalGap, RolloutLines, Wire};
use tool_history::{NativeCalls, PendingOutcomes};
use tool_revisions::{DisplayRevisions, PreviousDisplay};
use turn_aliases::{MessageIds, TurnAliases};
use workspace_catalog::WorkspaceCatalog;
use workspace_check::WorkspaceSettings;
use workspace_records::WorkspaceRecords;

const SCAN_LIMIT: usize = 10_000;
const UNAVAILABLE_ANCESTRY: &str = "omnisession.codex_history_base";
const MAX_CANONICAL_TOOL_EVENTS: usize = 256;
// Rollouts stream without a record budget, so retained events and turn checkpoints keep the newest.
const MAX_CANONICAL_EVENTS: usize = 100_000;
const MAX_TOOL_STRING_CHARACTERS: usize = 32 * 1024;
const MAX_TOOL_ARRAY_ITEMS: usize = 256;
const TOOL_COMPACTION_RECORD_INTERVAL: usize = 1_024;
const COMPACT_USER_TOKEN_LIMIT: usize = 20_000;
// Codex recognizes unannotated local summaries by this complete prompt line.
// Source: codex-rs/prompts/templates/compact/summary_prefix.md.
const LOCAL_SUMMARY_PREFIX: &str = concat!(
    "Another language model started to solve this problem and produced a summary of its ",
    "thinking process. You also have access to the state of the tools that were used by ",
    "that language model. Use this to build on the work that has already been done and ",
    "avoid duplicating work. Here is the summary produced by the other language model, ",
    "use the information in this summary to assist with your own analysis:",
);

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

    fn observed_file_paths(&self) -> HashMap<Uuid, &Path> {
        self.files
            .iter()
            .filter_map(|path| Some((CodexAdapter::index_file_id(path)?, path.as_path())))
            .collect()
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
    name: NativeSessionName,
    session: CodexSession,
    shadowed: bool,
    from_registry: bool,
}

impl From<CurrentFile> for SelectedRollout {
    fn from(file: CurrentFile) -> Self {
        debug_assert_eq!(file.session.observed_revision, Some(file.revision));
        Self {
            name: NativeSessionName::Unknown,
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
                    name: NativeSessionName::Unknown,
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
    listed_names: HashMap<String, (usize, NativeSessionName)>,
    registry_rows: HashMap<String, current_rollout::CurrentRollout>,
    rollout_notes: Vec<String>,
    names: Arc<TitleIndex>,
    selected_files: HashMap<String, (PathBuf, SourceRevision)>,
    legacy_revisions: Vec<(PathBuf, Option<SourceRevision>)>,
    requires_scan: bool,
}

impl CodexListing {
    fn matching_names(
        &self,
        sessions: &[(&str, &Path, SourceRevision)],
        registry_matches: impl Fn(&str) -> bool,
    ) -> HashMap<String, NativeSessionName> {
        let mut result = HashMap::new();
        let mut seen = HashSet::with_capacity(sessions.len());
        for &(id, path, revision) in sessions {
            if !seen.insert(id) {
                result.remove(id);
                continue;
            }
            let Some((position, name)) = self.listed_names.get(id) else {
                continue;
            };
            let selected = &self.sessions[*position];
            if selected.path == path
                && revision.contents.is_some()
                && selected
                    .observed_revision
                    .is_some_and(|observed| observed.matches_file_contents(revision))
                && registry_matches(id)
                && CodexAdapter::index_file_revision(path)
                    .is_some_and(|current| current.matches_file_contents(revision))
            {
                result.insert(id.to_owned(), name.clone());
            }
        }
        result
    }
}

#[derive(Debug)]
struct CachedListing {
    database: Option<CurrentRolloutStamp>,
    listing: Arc<CodexListing>,
}

#[derive(Debug, Default)]
struct TitleIndex {
    titles: Arc<HashMap<String, String>>,
    prior_titles: HashMap<String, String>,
    notes: Vec<String>,
    complete: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TitleIndexSource {
    Missing,
    Unavailable,
    File(RolloutStamp),
}

impl TitleIndexSource {
    fn observe(home: &Path, path: &Path) -> Self {
        match fs::symlink_metadata(path) {
            Ok(meta) if meta.is_file() => {
                RolloutStamp::new(&meta).map_or(Self::Unavailable, Self::File)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && home.is_dir() => {
                Self::Missing
            }
            _ => Self::Unavailable,
        }
    }
}

#[derive(Debug)]
struct CachedTitleIndex {
    source: TitleIndexSource,
    index: Arc<TitleIndex>,
}

#[derive(Clone, Debug)]
pub struct CodexAdapter {
    codex_home: Option<PathBuf>,
    relative_home: bool,
    current: Arc<CurrentRollouts>,
    use_workspace_index: bool,
    workspace_catalog: Option<Arc<WorkspaceCatalog>>,
    /// Rollout header cache file. Explicit roots leave it unset.
    rollout_cache: Option<PathBuf>,
    session_scan: Arc<Mutex<Option<Arc<CodexFileScan>>>>,
    listing: Arc<Mutex<Option<CachedListing>>>,
    titles: Arc<Mutex<Option<CachedTitleIndex>>>,
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

    fn header_is_current(path: &Path, previous: Option<SourceRevision>) -> bool {
        previous.is_some_and(|old| {
            old.contents.is_some()
                && Self::index_file_revision(path)
                    .is_some_and(|current| old.matches_file_contents(current))
        })
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
            let project = project.filter(|path| path.is_absolute());
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
            let mut snapshot = native.snapshot(session, events, project, read.snapshot.captured_at);
            snapshot.title.clone_from(&read.snapshot.title);
            let document = omnis_core::trajectory_search_document(&snapshot);
            if Sha256::digest(document.text.as_bytes())[..] != *expected_hash {
                return Ok(None);
            }
            Ok(Some(IndexRead {
                ownership_revision: None,
                verified_prefix_end: None,
                database_aliases: std::sync::Arc::default(),
                search_record_groups: Vec::new(),
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
            workspace_catalog: None,
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

    fn title_index(&self) -> Arc<TitleIndex> {
        let Some(home) = self.codex_home.as_deref() else {
            return Arc::default();
        };
        let path = home.join("session_index.jsonl");
        let mut cache = self
            .titles
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let source = TitleIndexSource::observe(home, &path);
        if let Some(cached) = cache.as_ref().filter(|cached| cached.source == source) {
            return Arc::clone(&cached.index);
        }
        let index = Arc::new(if matches!(source, TitleIndexSource::File(_)) {
            provider_file(home, &path)
                .map_or_else(TitleIndex::default, |path| Self::read_title_index(&path))
        } else {
            TitleIndex {
                complete: source == TitleIndexSource::Missing,
                ..TitleIndex::default()
            }
        });
        if TitleIndexSource::observe(home, &path) != source {
            return Arc::default();
        }
        *cache = Some(CachedTitleIndex {
            source,
            index: Arc::clone(&index),
        });
        index
    }

    fn read_title_index(path: &Path) -> TitleIndex {
        let mut titles = HashMap::new();
        let mut prior_titles = HashMap::new();
        let scan = visit_index_json_lines_with_gaps(path, |row| {
            let Some(row) = row else {
                // This row could rename any thread. Only later assignments can confirm a name.
                prior_titles.extend(std::mem::take(&mut titles));
                return;
            };
            let Some(id) = string_at(&row, &[&["id"]]).filter(|id| Uuid::parse_str(id).is_ok())
            else {
                return;
            };
            if let Some(title) =
                string_at(&row, &[&["thread_name"]]).filter(|title| !title.trim().is_empty())
            {
                // Codex resolves the last appended name, including when the clock moves back.
                titles.insert(id.to_owned(), title.to_owned());
            }
        });
        match scan {
            Ok(scan) => {
                let mut notes = scan.notes("Codex session_index.jsonl");
                if scan.malformed_records > 0 {
                    notes.push(format!(
                        "Codex session_index.jsonl skipped {} malformed JSON record(s).",
                        scan.malformed_records
                    ));
                }
                if let Some(note) = notes.last_mut() {
                    note.push_str(
                        " Only names assigned after the last skipped row can be confirmed.",
                    );
                }
                TitleIndex {
                    titles: Arc::new(titles),
                    prior_titles,
                    complete: notes.is_empty(),
                    notes,
                }
            }
            Err(error) => TitleIndex {
                notes: vec![format!(
                    "Codex session_index.jsonl could not be read: {}.",
                    compact_discovery_error(&error)
                )],
                ..TitleIndex::default()
            },
        }
    }

    fn confirms_legacy_name_source(&self, home: &Path, reported: &NativeSession) -> bool {
        let Some(path) = reported.source_path.as_deref() else {
            return false;
        };
        let Some(observed) = ObservedRollout::read(path) else {
            return false;
        };
        if observed.physical_identity.is_none()
            || observed.revision.contents.is_none()
            || reported.source_revision != Some(observed.revision)
            || !uniquely_scoped_rollout(home, path)
        {
            return false;
        }
        self.selected_rollout(&reported.session.id, Some(path))
            .is_ok_and(|selected| {
                selected.is_some_and(|current| {
                    current.session.id == reported.session.id
                        && current.session.observed_revision == Some(observed.revision)
                })
            })
    }

    fn header_cache(&self, home: &Path) -> Option<RolloutCache> {
        self.rollout_cache
            .as_deref()
            .filter(|_| cfg!(unix))
            .and_then(|path| RolloutCache::load(path, home))
    }

    fn selected_name(
        &self,
        selected: &SelectedRollout,
        row: Option<&current_rollout::CurrentRollout>,
        index: &TitleIndex,
        candidates: impl FnOnce() -> Option<Vec<PathBuf>>,
    ) -> NativeSessionName {
        let session = &selected.session;
        let indexed = || {
            let Some(title) = index.titles.get(&session.id) else {
                return if index.complete {
                    NativeSessionName::Absent
                } else {
                    NativeSessionName::Unknown
                };
            };
            let Some(paths) = candidates() else {
                return NativeSessionName::Unknown;
            };
            if paths.as_slice() == [session.path.as_path()] {
                return NativeSessionName::Named(title.clone());
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
                return NativeSessionName::Named(title.clone());
            }
            NativeSessionName::Absent
        };
        match row {
            Some(row) if selected.from_registry || row.ordinary_path_is(&session.path) => {
                row.observed_name(indexed)
            }
            Some(_) => NativeSessionName::Absent,
            None => indexed(),
        }
    }

    /// Refreshes cached names and metadata from the currently selected rollouts.
    pub fn refresh_session_titles(
        &self,
        sessions: &mut [NativeSession],
    ) -> HashMap<String, NativeSessionName> {
        let mut names = HashMap::new();
        if !sessions
            .iter()
            .any(|s| s.session.provider == Provider::Codex)
        {
            return names;
        }
        let Some(home) = self.codex_home.as_deref() else {
            return names;
        };
        let (scan, _) = self.current_session_scan();
        let ids = sessions
            .iter()
            .filter(|s| s.session.provider == Provider::Codex)
            .map(|s| s.session.id.as_str());
        let index = self.title_index();
        let rows = self.current.rows_with_titles(home, ids, &index.titles);
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
                && Self::header_is_current(path, cached.source_revision)
            {
                let indexed = || match index.titles.get(id) {
                    Some(title) => NativeSessionName::Named(title.clone()),
                    None if index.complete => NativeSessionName::Absent,
                    None => NativeSessionName::Unknown,
                };
                let name = row.map_or_else(indexed, |row| row.observed_name(indexed));
                if name != NativeSessionName::Unknown {
                    names.insert(id.to_owned(), name.clone());
                }
                cached.title = name.into_title();
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
            let name = self.selected_name(&selected, row, &index, || {
                scan.index_paths(&selected, &legacy)
            });
            if name != NativeSessionName::Unknown {
                names.insert(id.to_owned(), name.clone());
            }
            let current = selected.session;
            if cached
                .source_revision
                .zip(current.observed_revision)
                .is_some_and(|(old, new)| old.contents.is_some() && old.matches_file_contents(new))
            {
                cached.source_path = Some(current.path);
            } else {
                *cached = current.native_session();
            }
            cached.title = name.into_title();
        }
        names
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
                    && Arc::ptr_eq(&cached.listing.names, &self.title_index())
                    && cached.listing.scan.current()
                    && cached
                        .listing
                        .legacy_revisions
                        .iter()
                        .all(|(path, previous)| Self::header_is_current(path, *previous))
                    && cached.listing.sessions.iter().all(|session| {
                        cached.listing.selected_files.contains_key(&session.id)
                            || Self::header_is_current(&session.path, session.observed_revision)
                    })
                    && cached
                        .listing
                        .selected_files
                        .values()
                        .all(|(path, revision)| Self::header_is_current(path, Some(*revision))))
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
        let title_index = self.title_index();
        let rows = match self.current.rows_with_titles(
            home,
            scan.thread_ids(&legacy).map(String::as_str),
            &title_index.titles,
        ) {
            Ok(rows) => rows,
            Err(error) => {
                return CodexListing {
                    rollout_notes: vec![format!(
                        "Codex could not verify the current rollouts: {}.",
                        compact_discovery_error(&anyhow!("{error:#}"))
                    )],
                    scan,
                    names: title_index,
                    requires_scan: true,
                    ..CodexListing::default()
                };
            }
        };
        let mut sessions = Vec::new();
        let mut listed_names = HashMap::new();
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
            let name = self.selected_name(&selected, row, &title_index, || {
                scan.index_paths(&selected, &legacy)
            });
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
            session.title = name.clone().into_title();
            listed_names.insert(id.clone(), (sessions.len(), name));
            sessions.push(session);
        }
        if let Some(cache) = cache {
            cache.save();
        }
        let rollout_notes = listing_notes(&scan, sessions.len(), subagents, &invalid);
        CodexListing {
            legacy_revisions: legacy.revisions,
            scan,
            sessions,
            listed_names,
            registry_rows: rows,
            rollout_notes,
            names: title_index,
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

    fn remember_workspace(
        &self,
        native: &CodexSession,
        snapshot: &CanonicalSnapshot,
        projection: &CodexWorkspaceProjection,
        complete: bool,
    ) {
        let Some(catalog) = self.workspace_catalog.as_deref() else {
            return;
        };
        let directory = match projection {
            CodexWorkspaceProjection::Observed(directory) => directory.as_deref(),
            CodexWorkspaceProjection::Unchecked if complete => {
                let directory = snapshot.workspace.current_dir.as_path();
                (!directory.as_os_str().is_empty()).then_some(directory)
            }
            CodexWorkspaceProjection::Unchecked => return,
        };
        let directory = directory.filter(|path| path.is_absolute());
        catalog.record(
            native,
            directory,
            native.git_at(directory).branch.as_deref(),
        );
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
        self.read_selected(selected.session, |native, _| {
            let records = rollout_file::header(&native.path)?;
            Ok(records
                .first()
                .filter(|record| record.get("type").and_then(Value::as_str) == Some("session_meta"))
                .and_then(|record| record.get("payload"))
                .filter(|payload| string_at(payload, &[&["id"], &["session_id"]]) == Some(id))
                .and_then(|payload| string_at(payload, &[&["model_provider"]]))
                .map(str::to_owned))
        })
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
        let title_index = with_title.then(|| self.title_index());
        let rows = if let Some(index) = &title_index {
            self.current.rows_with_titles(home, [id], &index.titles)?
        } else {
            self.current.rows(home, [id])?
        };
        let row = rows.get(id);
        let finish = |mut selected: SelectedRollout| {
            if let Some(index) = &title_index {
                selected.name = self.selected_name(&selected, row, index, || {
                    let (scan, _) = self.current_session_scan();
                    let legacy = scan.legacy_paths(home, None);
                    scan.index_paths(&selected, &legacy)
                });
                selected.session.title = selected.name.clone().into_title();
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
                    .is_some_and(|path| path.is_absolute() && paths_match(path, project))
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
        let directory = workspace
            .project_path
            .as_deref()
            .filter(|path| path.is_absolute());
        let branch = directory.and(workspace.git_branch.as_deref());
        if self.session.provider == Provider::Codex
            && self.source_revision.is_some_and(|revision| {
                revision.contents.is_some()
                    && revision.matches_file_contents(workspace.source_revision)
            })
            && (self.project_path.as_deref() != directory || self.git_branch.as_deref() != branch)
        {
            self.project_path = directory.map(Path::to_path_buf);
            self.git_branch = branch.map(str::to_owned);
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
            workspace_catalog: codex_home
                .as_deref()
                .and_then(WorkspaceCatalog::new)
                .map(Arc::new),
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
        CODEX_ROLLOUT_DIRECTORY_DEPTH,
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
            CODEX_ROLLOUT_DIRECTORY_DEPTH,
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
    tail.components().count() <= CODEX_ROLLOUT_DIRECTORY_DEPTH + 1
        && tail
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)))
        && path.is_file()
        && fs::canonicalize(path).is_ok_and(|actual| {
            omnis_core::codex_rollout_path_in_home(canonical_home, relative).as_deref()
                == Some(&actual)
        })
}

fn uniquely_scoped_rollout(home: &Path, path: &Path) -> bool {
    let Ok(actual) = fs::canonicalize(path) else {
        return false;
    };
    unique_codex_rollout_home(&actual) == Some(home)
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
    invalid: &InvalidHeaders,
) -> Vec<String> {
    let unreadable_files = invalid.unreadable;
    let skipped_metadata = invalid.without_metadata;
    let mut notes = scan.notes.clone();
    if unreadable_files > 0 {
        notes.push(
            match invalid.first_error.as_ref().map(compact_discovery_error) {
                Some(error) if !error.is_empty() => {
                    format!("Codex skipped {unreadable_files} unreadable session file(s): {error}.")
                }
                _ => format!("Codex skipped {unreadable_files} unreadable session file(s)."),
            },
        );
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

#[derive(Default)]
struct RolloutWorkspace {
    directory: Option<PathBuf>,
    settings: WorkspaceSettings,
    recent_records: Option<VecDeque<JsonlRecord>>,
}

#[derive(Clone, Debug)]
struct CodexSession {
    id: String,
    forked_from_id: Option<Uuid>,
    title: Option<String>,
    path: PathBuf,
    observed_revision: Option<SourceRevision>,
    stamp: Option<RolloutStamp>,
    project_path: Option<PathBuf>,
    git: GitState,
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
            stamp: observed.stamp,
            project_path: string_at(payload, &[&["cwd"]])
                .map(PathBuf::from)
                .filter(|path| path.is_absolute()),
            git: GitState {
                branch: string_at(payload, &[&["git", "branch"], &["git", "branch_name"]])
                    .map(str::to_owned),
                head: string_at(payload, &[&["git", "commit_hash"]])
                    .filter(|head| {
                        matches!(head.len(), 40 | 64)
                            && head.bytes().all(|byte| byte.is_ascii_hexdigit())
                    })
                    .map(str::to_ascii_lowercase),
                remote_fingerprint: string_at(payload, &[&["git", "repository_url"]])
                    .filter(|url| !url.is_empty())
                    .map(|url| omnis_core::fingerprint(url.as_bytes())),
                ..GitState::default()
            },
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
            stamp: observed.stamp,
            project_path: header.project_path,
            git: header.git,
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
            git: self.git.clone(),
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
            git_branch: self.git_at(self.project_path.as_deref()).branch,
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
            history.observe_workspace_gaps(skipped_before.workspace_gaps(), builder.checkpoint());
            // The latest settings for this thread survive conversation rollback.
            history.settings.observe_gaps(skipped_before);
            if self.history_mode != CodexHistoryMode::Paginated
                || WorkspaceRecords::accepts(true, &record)
            {
                history.settings.observe(&self.id, &record);
            }
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
        history.observe_workspace_gaps(unreadable.workspace_gaps(), builder.checkpoint());
        history.settings.observe_gaps(unreadable);
        let prefix_end = first_gap.unwrap_or_else(|| builder.checkpoint());
        history.compact(&mut builder);
        history.source_prefix_end = if unread_ancestor || history.omitted_events > 0 {
            Some(0)
        } else {
            (unreadable.total() > 0).then_some(prefix_end)
        };
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

    fn recent_workspace(&self, keep_preview: bool) -> Result<RolloutWorkspace> {
        let mut workspace =
            WorkspaceRecords::from_header(None, self.history_mode == CodexHistoryMode::Paginated);
        let mut records = VecDeque::new();
        let is_workspace_record = workspace_record_filter();
        visit_json_lines_tail(
            &self.path,
            MAX_PREVIEW_TAIL_SIZE,
            |line| keep_preview || is_workspace_record(line),
            |_, line| {
                workspace.observe(&self.id, &line);
                if !keep_preview {
                    return;
                }
                if self.history_mode != CodexHistoryMode::Paginated
                    && let JsonLine::Record(record) | JsonLine::Unterminated(record) = &line
                    && encoded_rollback(record)
                {
                    if rollout_lines::rollback_turns(&record["payload"]) != Some(0) {
                        records.clear();
                    }
                } else if let Some(record) = line.into_record() {
                    if records.len() == 1_024 {
                        records.pop_front();
                    }
                    records.push_back(record);
                }
            },
        )?;
        let mut workspace = workspace.finish(&self.path)?;
        workspace.recent_records = keep_preview.then_some(records);
        Ok(workspace)
    }

    fn recent_preview(&self, records: VecDeque<JsonlRecord>) -> Result<EventBuilder> {
        let mut events = self.event_builder();
        let mut history = CodexHistory::new(None);
        history.paginated = self.history_mode == CodexHistoryMode::Paginated;
        for line in records {
            let (mut record, terminated) = match line {
                JsonlRecord::Complete(record) => (record, true),
                JsonlRecord::Unterminated(record) => (record, false),
                JsonlRecord::Malformed => {
                    history.rollout_lines.skipped +=
                        usize::from(self.history_mode == CodexHistoryMode::Paginated);
                    continue;
                }
            };
            if self.history_mode == CodexHistoryMode::Paginated {
                // An unread prefix can contain a higher display ordinal than any row in the suffix.
                if history
                    .rollout_lines
                    .prepare_wire(&mut record, true, Some(true))
                    != Wire::Invalid
                {
                    history.push_admitted(&mut events, &record, Admission::ModelOnly)?;
                }
            } else {
                history.push(&mut events, &mut record, terminated)?;
            }
        }
        history.finish(&mut events);
        let reason = if self.history_mode == CodexHistoryMode::Paginated {
            "The source prefix was unavailable, so the public display ordinals in its suffix could not be verified."
        } else {
            "Only the recent Codex records were available."
        };
        events.push(
            EventKind::ProviderEvent,
            json!({"omitted_events":1, "reason":reason}),
            self.updated_at,
            ReplayPolicy::HistoricalOnly,
            Some("omnisession.preview_limit".to_owned()),
            None,
        );
        Ok(events)
    }

    fn live_workspace(&self) -> Result<NativeWorkspace> {
        if !rollout_file::compressed(&self.path)
            && let Some(stamp) = self.stamp
            && stamp.len > MAX_PREVIEW_TAIL_SIZE
        {
            let mut recent = self.recent_workspace(false)?;
            if stamp.len > MAX_STREAMED_TRANSCRIPT_FILE_SIZE || recent.settings.has_owned() {
                if !recent.settings.has_owned() {
                    recent.directory = None;
                }
                return Ok(NativeWorkspace {
                    git: self.git_at(recent.directory.as_deref()),
                    directory: recent.directory,
                    startup_directory: recent.settings.startup,
                });
            }
            if recent.directory.is_some() && !contains_settings_marker(&self.path)? {
                return Ok(NativeWorkspace {
                    git: self.git_at(recent.directory.as_deref()),
                    directory: recent.directory,
                    startup_directory: self.project_path.clone(),
                });
            }
        }
        let (directory, startup_directory) = self.effective_and_startup_directories()?;
        Ok(NativeWorkspace {
            git: self.git_at(directory.as_deref()),
            directory,
            startup_directory,
        })
    }

    fn git_at(&self, directory: Option<&Path>) -> GitState {
        // Codex records Git only for the creation directory.
        if directory.is_some()
            && directory.map(Path::as_os_str) == self.project_path.as_deref().map(Path::as_os_str)
        {
            self.git.clone()
        } else {
            GitState::default()
        }
    }

    fn snapshot(
        &self,
        session: &SessionRef,
        events: EventBuilder,
        project: Option<PathBuf>,
        captured_at: DateTime<Utc>,
    ) -> CanonicalSnapshot {
        let project = project.filter(|path| path.is_absolute());
        let git = self.git_at(project.as_deref());
        let mut snapshot = events.snapshot(
            session.clone(),
            self.title.clone(),
            project,
            None,
            captured_at,
        );
        snapshot.workspace.git = git;
        snapshot
    }

    fn read(self, session: &SessionRef) -> Result<(omnis_ir::CanonicalSnapshot, RollbackTitle)> {
        let captured_at = self.updated_at.unwrap_or_else(Utc::now);
        let (events, project_path) = self.canonical_events()?;
        let title = events.rollback_title();
        Ok((
            self.snapshot(session, events, project_path, captured_at),
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
        let (events, project, reconstructed) = if self
            .stamp
            .is_some_and(|stamp| stamp.len > MAX_STREAMED_TRANSCRIPT_FILE_SIZE)
        {
            let mut workspace = self.recent_workspace(true)?;
            if !workspace.settings.has_owned() {
                workspace.directory = None;
            }
            (
                self.recent_preview(workspace.recent_records.unwrap_or_default())?,
                workspace.directory,
                true,
            )
        } else if self.history_mode == CodexHistoryMode::Paginated {
            let workspace = self.live_workspace()?;
            (self.preview_paginated_events()?, workspace.directory, true)
        } else if self.history_mode == CodexHistoryMode::Legacy {
            let scan = self.scan_workspace(true)?;
            if scan.reconstruct_history || contains_rollback_marker(&self.path)? {
                self.reconstructed_preview(rollout_file::LIMITS)?
            } else {
                self.preview_events(scan.directory)?
            }
        } else {
            self.reconstructed_preview(rollout_file::LIMITS)?
        };
        let project = project.filter(|path| path.is_absolute());
        let title = events.rollback_title();
        let workspace =
            CodexWorkspaceProjection::from_reconstruction(reconstructed, project.as_deref());
        Ok((
            self.snapshot(session, events, project, captured_at),
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
            ownership_revision: None,
            verified_prefix_end: None,
            database_aliases: std::sync::Arc::default(),
            search_record_groups: Vec::new(),
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
                    prefix_records,
                    directory,
                } => {
                    let events = self.events_from_preview(
                        records,
                        prefix_records,
                        unreadable,
                        Some(omitted),
                    )?;
                    (events, directory, false, true)
                }
            };
        let project = project.filter(|path| path.is_absolute());
        let rollback_title = events.rollback_title();
        let workspace_projection = CodexWorkspaceProjection::from_reconstruction(
            workspace_reconstructed,
            project.as_deref(),
        );
        let snapshot = self.snapshot(session, events, project, captured_at);
        Ok(IndexRead {
            ownership_revision: None,
            verified_prefix_end: None,
            database_aliases: std::sync::Arc::default(),
            search_record_groups: Vec::new(),
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
        let (omitted, prefix_end) = events.retain_bounded_preview(
            rollout_file::SAMPLE_RECORDS,
            usize::try_from(MAX_PREVIEW_TAIL_SIZE)?,
        );
        self.finish_preview_prefix(
            &mut events,
            Some(omitted),
            history
                .source_prefix_end
                .map_or(prefix_end, |end| end.min(prefix_end)),
            history.source_prefix_end.is_none(),
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
        let (
            (selected, omitted, prefix_end),
            workspace_record,
            source_prefix_end,
            first_settings,
            first_title,
        ) = {
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
                history.source_prefix_end,
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
            || history.source_prefix_end != source_prefix_end
            || events.select_preview(rollout_file::SAMPLE_RECORDS, per_edge)
                != (selected, omitted, prefix_end)
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
        self.finish_preview_prefix(
            &mut events,
            Some(omitted),
            source_prefix_end.map_or(prefix_end, |end| end.min(prefix_end)),
            source_prefix_end.is_none(),
        );
        Ok((
            events,
            project,
            owned_settings || !history.workspace.incomplete,
        ))
    }

    fn preview_paginated_events(&self) -> Result<EventBuilder> {
        const SAMPLE_RECORDS: usize = 1_024;
        let Some(records) = crate::support::complete_small_json_records(&self.path)? else {
            return paginated_preview::read(self, SAMPLE_RECORDS);
        };
        let mut history = CodexHistory::new(self.project_path.clone());
        let mut builder = self.event_builder();
        let mut has_history_base = false;
        for (index, line) in records.into_iter().enumerate() {
            let (mut record, terminated) = match line {
                JsonlRecord::Complete(record) => (record, true),
                JsonlRecord::Unterminated(record) => (record, false),
                JsonlRecord::Malformed => {
                    history.rollout_lines.malformed();
                    continue;
                }
            };
            if index == 0 {
                has_history_base = records_history_base(&record);
            }
            history.push(&mut builder, &mut record, terminated)?;
        }
        if has_history_base {
            report_unread_ancestor(&mut builder, self.updated_at);
        }
        history.finish(&mut builder);
        Ok(builder)
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
        let events = self.events_from_preview(
            preview.records,
            preview.prefix_records,
            preview.omissions,
            (!preview.sampled).then_some(0),
        )?;
        Ok((events, directory, true))
    }

    fn events_from_preview(
        &self,
        records: Vec<Value>,
        prefix_records: usize,
        omissions: JsonLineSkips,
        omitted: Option<usize>,
    ) -> Result<EventBuilder> {
        let mut builder = self.event_builder();
        let mut history = CodexHistory::new(self.project_path.clone());
        let unread_ancestor = records.iter().any(records_history_base);
        if !self.is_subagent
            && prefix_records > 0
            && records
                .iter()
                .enumerate()
                .all(|(i, record)| ordinary_rollback_record(record, i == 0))
        {
            builder.track_rollback_opener(false);
            builder.note_rollback_history_gap();
        }
        let mut records = records.into_iter();
        for mut record in records.by_ref().take(prefix_records) {
            history.push(&mut builder, &mut record, true)?;
        }
        let prefix_end = builder.checkpoint();
        for mut record in records {
            history.push(&mut builder, &mut record, true)?;
        }
        if unread_ancestor {
            report_unread_ancestor(&mut builder, self.updated_at);
        }
        history.finish(&mut builder);
        builder.push_json_line_omissions(
            omissions,
            MAX_STREAMED_TRANSCRIPT_LINE_SIZE,
            Some(prefix_end),
            self.updated_at,
        );
        self.finish_preview_prefix(
            &mut builder,
            omitted,
            prefix_end,
            omissions.total() == 0 && !unread_ancestor,
        );
        Ok(builder)
    }

    fn finish_preview_prefix(
        &self,
        builder: &mut EventBuilder,
        omitted: Option<usize>,
        prefix_end: u64,
        complete: bool,
    ) {
        if omitted == Some(0) {
            builder.push_codex_preview_prefix(prefix_end, complete, self.updated_at);
            return;
        }
        let mut payload = json!({"prefix_end_sequence": prefix_end});
        if let Some(count) = omitted {
            payload["omitted_events"] = count.into();
        } else {
            payload["native_history_unverified"] = true.into();
        }
        builder.push(
            EventKind::ProviderEvent,
            payload,
            self.updated_at,
            ReplayPolicy::HistoricalOnly,
            Some("omnisession.preview_limit".to_owned()),
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
    admission: Option<InputPosition>,
    project_path: Option<PathBuf>,
    workspace_record: Option<usize>,
    tool_events: usize,
}

struct ReplacementRequest<'a> {
    sequence: u64,
    checkpoint: TurnCheckpoint,
    item: &'a Value,
}

impl TurnCheckpoint {
    fn admitted_sequence(&self) -> Option<u64> {
        // A completed display alone does not establish a native instruction boundary.
        self.admission.map(|_| self.sequence)
    }
}

fn revision_owner<'a>(
    late: &'a HashMap<u64, u64>,
    original: &'a HashMap<u64, u64>,
) -> impl Fn(u64) -> u64 + 'a {
    |seq| {
        late.get(&seq)
            .and_then(|id| original.get(id))
            .copied()
            .unwrap_or(seq)
    }
}

fn confirm_admission(
    checkpoints: &mut [(u64, TurnCheckpoint)],
    requests: &[(u64, u64)],
    sequence: Option<u64>,
    position: InputPosition,
) {
    if let Some(seq) = sequence
        && let Ok(i) = requests.binary_search_by_key(&seq, |(s, _)| *s)
    {
        let (sequence, owner) = requests[i];
        let i = checkpoints.partition_point(|(_, cp)| cp.sequence <= sequence);
        if let Some((_, cp)) = checkpoints[..i]
            .iter_mut()
            .rev()
            .find(|(id, _)| *id == owner)
        {
            cp.admission = Some(position);
        }
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
    // Logical order can differ from file order when a checkpoint supplies missing requests.
    turns: Vec<u64>,
    next_turn: u64,
    checkpoints: Vec<(u64, TurnCheckpoint)>,
    // Each entry identifies a genuine user message in the canonical event stream.
    requests: Vec<(u64, u64)>,
    original_requests: HashMap<u64, u64>,
    pending_turn: Option<TurnCheckpoint>,
    active_turn_id: Option<String>,
    source_ordinal: u64,
    paginated: bool,
    paginated_inputs: PaginatedRollback,
    paginated_workspace: paginated_workspace::PaginatedWorkspace,
    paginated_answers: PaginatedAnswers,
    compactions: PaginatedCompactions,
    checkpoint_turns: paginated_checkpoint::CheckpointTurns,
    withdrawn: HashMap<u64, (Withdrawal, u64)>,
    explicit_display_turn: bool,
    legacy_display: MessageIdentity,
    legacy_request: Option<u64>,
    uncertain_legacy_rollback: bool,
    legacy_private: LegacyPrivateInputs,
    rollout_lines: RolloutLines,
    // Codex updates a completed item in place using its turn and item IDs.
    completed: HashMap<String, CompletedTurn>,
    // Native turn names and delayed events refer to logical turns, including their replacement copies.
    named_turns: HashMap<String, u64>,
    plan_turns: HashSet<String>,
    plan_receipts: display_text::PlanReceipts,
    message_ids: MessageIds,
    aliases: TurnAliases,
    calls: NativeCalls,
    occurrences: tool_occurrences::ToolOccurrences,
    display_revisions: DisplayRevisions,
    late_owners: HashMap<u64, u64>,
    late_tool_counts: HashMap<u64, usize>,
    display_positions: HashMap<u64, (u64, u64)>,
    ambiguous_receipts: usize,
    deferred_receipts: deferred_receipts::DeferredReceipts,
    omitted_tool_events: usize,
    omitted_events: usize,
    source_prefix_end: Option<u64>,
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
            next_turn: 0,
            checkpoints: Vec::new(),
            requests: Vec::new(),
            original_requests: HashMap::new(),
            pending_turn: None,
            active_turn_id: None,
            source_ordinal: 0,
            paginated: false,
            paginated_inputs: PaginatedRollback::default(),
            paginated_answers: PaginatedAnswers::default(),
            compactions: PaginatedCompactions::default(),
            checkpoint_turns: paginated_checkpoint::CheckpointTurns::default(),
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
            late_tool_counts: HashMap::new(),
            display_positions: HashMap::new(),
            ambiguous_receipts: 0,
            deferred_receipts: deferred_receipts::DeferredReceipts::default(),
            omitted_tool_events: 0,
            omitted_events: 0,
            source_prefix_end: None,
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
            self.compactions.discard_before(first);
            self.withdrawn.retain(|seq, _| *seq >= first);
        }
        let excess = self.turns.len().saturating_sub(MAX_CANONICAL_EVENTS);
        if excess > 0 || self.omitted_events > 0 || self.omitted_tool_events > 0 {
            builder.note_rollback_history_gap();
        }
        self.workspace.checkpoints_pruned |= excess > 0;
        if excess > 0 {
            let evicted: HashSet<_> = self.turns.drain(..excess).collect();
            self.requests.retain(|(_, id)| !evicted.contains(id));
            self.named_turns.retain(|_, id| !evicted.contains(id));
            self.original_requests.retain(|id, _| !evicted.contains(id));
            self.aliases.retain(&self.named_turns);
            self.late_tool_counts.retain(|id, _| !evicted.contains(id));
        }
        if excess > 0 || self.plan_turns.len() > MAX_CANONICAL_EVENTS {
            self.plan_turns.retain(|id| {
                self.named_turns.contains_key(id) || self.active_turn_id.as_deref() == Some(id)
            });
        }
        let first = builder.first_sequence();
        let stale = self
            .requests
            .partition_point(|(sequence, _)| *sequence < first);
        self.requests.drain(..stale);
        let stale = self
            .checkpoints
            .partition_point(|(_, point)| point.sequence < first);
        let excess = self.checkpoints.len().saturating_sub(MAX_CANONICAL_EVENTS);
        self.checkpoints
            .drain(..excess.max(stale.saturating_sub(1)));
        if tools_removed > 0 || events_removed > 0 || excess > 0 {
            let oldest = self
                .checkpoints
                .first()
                .map_or(0, |(_, turn)| turn.sequence);
            self.display_revisions.prune(builder, oldest);
        }
    }

    fn new_turn(&mut self) -> u64 {
        let id = self.next_turn;
        self.next_turn += 1;
        id
    }

    fn observe_workspace_gaps(&mut self, skipped: usize, sequence: u64) {
        if skipped != self.workspace.gaps {
            self.workspace.observe_gaps(skipped, self.records_seen);
            self.paginated_workspace.new_segment(sequence);
        }
    }

    fn verify_legacy_rollback(&mut self, first_removed: u64, count: u64) {
        // A replacement can give the same logical request another physical checkpoint.
        let mut admitted = HashMap::new();
        for (id, point) in &self.checkpoints {
            if let Some(position) = point.admission {
                admitted.entry(*id).or_insert(position);
            }
        }
        self.uncertain_legacy_rollback |= self.turns.iter().any(|id| !admitted.contains_key(id));
        if let Some(&cut) = admitted.get(&first_removed) {
            let excess = count.saturating_sub(self.turns.len() as u64);
            self.uncertain_legacy_rollback |= self.legacy_private.crosses(cut, excess);
        }
    }

    fn rollback(&mut self, builder: &mut EventBuilder, count: u64) {
        if self.workspace.checkpoints_pruned && count > self.turns.len() as u64 {
            self.workspace.incomplete = true;
        }
        let keep = self
            .turns
            .len()
            .saturating_sub(usize::try_from(count).unwrap_or(usize::MAX));
        let Some(&first_removed) = self.turns.get(keep) else {
            if self.workspace.gaps > 0 {
                self.workspace.record = None;
                self.workspace.path = None;
            }
            return;
        };
        self.verify_legacy_rollback(first_removed, count);
        let discarded: HashSet<_> = self.turns.drain(keep..).collect();
        let mut ranges = Vec::new();
        let mut removed_tools = 0;
        let mut points = std::mem::take(&mut self.checkpoints).into_iter().peekable();
        while let Some((id, mut point)) = points.next() {
            if discarded.contains(&id) {
                if ranges.is_empty() {
                    self.workspace.record = point
                        .workspace_record
                        .filter(|position| *position > self.workspace.records_before_gap);
                    self.workspace.path = self.workspace.record.and(point.project_path);
                    self.last_rollback_boundary = Some(point.sequence);
                }
                let (end, tools) = points.peek().map_or(
                    (builder.checkpoint(), self.items.tool_events),
                    |(_, next)| (next.sequence, next.tool_events),
                );
                ranges.push(point.sequence..end);
                removed_tools += tools - point.tool_events;
            } else {
                point.tool_events -= removed_tools;
                self.checkpoints.push((id, point));
            }
        }
        let survives = |sequence: u64| {
            let index = ranges.partition_point(|r| r.end <= sequence);
            ranges.get(index).is_none_or(|r| sequence < r.start)
        };
        builder.rollback_events(|sequence| {
            self.late_owners
                .get(&sequence)
                .map_or_else(|| survives(sequence), |owner| !discarded.contains(owner))
        });
        self.named_turns.retain(|_, id| !discarded.contains(id));
        for (owner, point) in self.checkpoints.iter().rev() {
            if let Some(name) = &point.id {
                self.named_turns.entry(name.clone()).or_insert(*owner);
            }
        }
        self.original_requests
            .retain(|id, _| !discarded.contains(id));
        self.plan_receipts
            .rollback(|owner| discarded.contains(&owner));
        self.aliases.retain(&self.named_turns);
        self.late_tool_counts
            .retain(|id, _| !discarded.contains(id));
        self.items.tool_events -= removed_tools;
        if self.omitted_tool_events > 0 {
            let total_tools =
                self.items.tool_events + self.late_tool_counts.values().sum::<usize>();
            let visible_tools = builder
                .events()
                .iter()
                .filter(|event| {
                    matches!(
                        event.kind,
                        EventKind::CommandExecuted
                            | EventKind::ToolCalled
                            | EventKind::ToolCompleted
                            | EventKind::ToolFailed
                    )
                })
                .count();
            self.omitted_tool_events = self
                .omitted_tool_events
                .min(total_tools.saturating_sub(visible_tools));
        }
        self.display_revisions
            .rollback(builder, |sequence| !survives(sequence));
        self.deferred_receipts.retain(|_, owner| survives(owner));
        self.prune_completed(builder);
        self.requests.retain(|(sequence, _)| survives(*sequence));
    }

    fn checkpoint(&self, builder: &EventBuilder) -> TurnCheckpoint {
        TurnCheckpoint {
            id: self.active_turn_id.clone(),
            sequence: builder.checkpoint(),
            admission: None,
            project_path: self.workspace.path.clone(),
            tool_events: self.items.tool_events,
            workspace_record: self.workspace.record,
        }
    }

    fn remember_turn(&mut self, mut checkpoint: TurnCheckpoint, sequence: Option<u64>) {
        let id = self.new_turn();
        self.turns.push(id);
        self.original_requests
            .insert(id, sequence.unwrap_or(checkpoint.sequence));
        if let Some(name) = &checkpoint.id {
            let name = self.aliases.resolve(name).to_owned();
            self.named_turns.insert(name.clone(), id);
            checkpoint.id = Some(name);
        }
        if let Some(sequence) = sequence {
            self.requests.push((sequence, id));
        }
        self.checkpoints.push((id, checkpoint));
    }

    fn prune_completed(&mut self, builder: &mut EventBuilder) {
        self.deferred_receipts
            .retain(|seq, _| builder.event_at_mut(seq).is_some());
        self.items
            .outcomes
            .retain(|seq| builder.event_at_mut(seq).is_some());
        self.items
            .unclaimed_tools
            .retain(|seq| builder.event_at_mut(*seq).is_some());
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

    fn noncurrent_owner(&self, turn: &str) -> Option<u64> {
        let turn = self.aliases.resolve(turn);
        self.named_turns.get(turn).copied().filter(|id| {
            self.turns.last() != Some(id)
                || self
                    .active_turn_id
                    .as_deref()
                    .is_some_and(|active| self.aliases.resolve(active) != turn)
        })
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
            .or_else(|| {
                let (owner, checkpoint) = self.checkpoints.last()?;
                (Some(owner) == self.turns.last()).then_some(())?;
                checkpoint.id.as_deref()
            })
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
            && let Some((owner, checkpoint)) = self.checkpoints.last_mut()
            && Some(&*owner) == self.turns.last()
            && checkpoint.id.as_deref().is_none_or(|id| id == turn)
        {
            checkpoint.id = Some(turn.to_owned());
            self.named_turns.insert(turn.to_owned(), *owner);
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

    fn end_display_turn(&mut self) {
        self.legacy_request = None;
        self.explicit_display_turn = false;
        self.legacy_display = MessageIdentity::default();
        self.items.plan_mode = false;
        self.items.in_turn = false;
        self.pending_turn = None;
        self.active_turn_id = None;
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
        let disposition = self
            .rollout_lines
            .prepare(record, terminated, Some(self.paginated));
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
                if record["type"] == "turn_context" {
                    self.observe_turn_directory(record, record["payload"]["turn_id"].as_str());
                }
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
            self.start_copied_segment(builder);
        }
        if self.consume_model_record(record, event_type, builder)? {
            return Ok(());
        }
        let turn_id = payload["turn_id"].as_str();
        if event_type == Some("thread_rolled_back") {
            let count = rollout_lines::rollback_turns(payload)
                .context("Codex rollback marker has no valid num_turns")?;
            if count > 0 {
                self.rollback(builder, u64::from(count));
                self.end_display_turn();
                self.plan_turns
                    .retain(|id| self.named_turns.contains_key(id));
                self.items.clear_mirrors();
            }
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
            self.end_display_turn();
            self.items.turn_boundary(turn_id);
        }
        if record_type == Some("turn_context") {
            self.push_turn_context(builder, record, turn_id);
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
                (turn, id),
                parse_timestamp(record.get("timestamp")),
                None,
            );
        } else if !self.push_model_tool(builder, record) {
            self.push_message_or_metadata(builder, record, event_type);
        }
        Ok(())
    }

    fn push_turn_context(&mut self, builder: &EventBuilder, record: &Value, turn_id: Option<&str>) {
        let payload = &record["payload"];
        if let Some(mode) = payload["collaboration_mode"]["mode"].as_str() {
            self.set_plan_mode(turn_id, mode == "plan");
        } else if turn_id.is_some()
            && turn_id.map(|id| self.aliases.resolve(id)) != self.current_turn_id()
        {
            self.items.plan_mode =
                turn_id.is_some_and(|turn| self.plan_turns.contains(self.aliases.resolve(turn)));
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
        self.observe_turn_directory(record, turn_id);
    }

    fn observe_turn_directory(&mut self, record: &Value, turn_id: Option<&str>) {
        if let Some(cwd) = record["payload"]["cwd"].as_str() {
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

    fn start_copied_segment(&mut self, builder: &mut EventBuilder) {
        if let Some((ordinal, end)) = self.compactions.latest() {
            self.paginated_answers
                .resolve_turns(self.paginated_inputs.resolved_turns());
            self.paginated_answers.resolve_positions(builder, ordinal);
            self.paginated_answers.finish_answers();
            self.paginated_answers.project_revisions(
                builder,
                self.compactions.public_history(),
                &self.paginated_inputs,
                ordinal,
                end,
            );
        }
        self.paginated_inputs.segment();
        self.paginated_workspace.new_segment(builder.checkpoint());
        self.paginated_answers.segment();
    }

    fn consume_model_record(
        &mut self,
        record: &Value,
        event_type: Option<&str>,
        builder: &mut EventBuilder,
    ) -> Result<bool> {
        let payload = &record["payload"];
        if self.paginated {
            self.checkpoint_turns.observe(record, self.source_ordinal);
        }
        if record["type"] == "compacted" {
            self.push_compaction(builder, record);
            return Ok(true);
        }
        if !self.paginated {
            return Ok(self.legacy_private.observe(record, self.source_ordinal));
        }
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
                let reserved = self.compactions.has_model_window().then(|| {
                    let start = builder.reserve_sequence();
                    if plan_mode {
                        builder.reserve_sequence();
                    }
                    start
                });
                let active = self
                    .active_turn_id
                    .as_deref()
                    .filter(|turn| !is_implicit_turn(turn));
                self.paginated_answers.model(
                    record,
                    self.source_ordinal,
                    reserved,
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

    fn withdraw_displayed_inputs(&mut self, builder: &mut EventBuilder, count: usize) {
        let effective = self.compactions.apply_rollback(
            &self.paginated_inputs,
            count,
            self.checkpoint_turns.selected(),
        );
        let aliases = self.paginated_inputs.resolved_turns();
        let floor = self
            .compactions
            .selected_checkpoint()
            .map_or(0, paginated_checkpoint::PaginatedCheckpoint::source_ordinal);
        let mut owners = self.paginated_inputs.withdraw(
            effective.input_count,
            &aliases,
            effective.boundary,
            floor,
        );
        if effective.changed {
            self.workspace.path = self.paginated_workspace.surviving(
                &self.aliases,
                &aliases,
                self.checkpoints.iter().filter_map(|(id, turn)| {
                    let seq = *self.original_requests.get(id)?;
                    let baseline = owners
                        .get(&seq)
                        .copied()
                        .or_else(|| self.withdrawn.get(&seq).map(|&(s, _)| s));
                    let status = self.compactions.public_status(seq, Some(seq), baseline);
                    if turn.id.is_some()
                        && self
                            .compactions
                            .selected_checkpoint()
                            .is_some_and(|w| w.applies_to(seq))
                        && let Some(status @ (Withdrawal::Confirmed | Withdrawal::Unverified)) =
                            status
                    {
                        owners.insert(seq, status);
                    }
                    Some((seq, turn.id.as_deref(), status))
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
            if self.compactions.contains(event.sequence) {
                return;
            }
            if let Some(&withdrawal) = replies.get(&event.sequence) {
                if let Some(status) = withdrawal {
                    self.withdrawn
                        .entry(event.sequence)
                        .or_insert((status, self.source_ordinal));
                }
                return;
            }
            if let Some(status) = self
                .event_request(event.sequence)
                .and_then(|seq| owners.get(&seq))
            {
                self.withdrawn
                    .entry(event.sequence)
                    .or_insert((*status, self.source_ordinal));
            }
        });
    }

    fn event_request(&self, sequence: u64) -> Option<u64> {
        let owner = self.late_owners.get(&sequence).copied().or_else(|| {
            let point = self
                .checkpoints
                .partition_point(|(_, checkpoint)| checkpoint.sequence <= sequence)
                .checked_sub(1)
                .map(|i| (self.checkpoints[i].1.sequence, self.checkpoints[i].0));
            let request = self
                .requests
                .partition_point(|(seq, _)| *seq <= sequence)
                .checked_sub(1)
                .map(|i| self.requests[i]);
            point.max(request).map(|(_, id)| id)
        });
        owner.and_then(|id| self.original_requests.get(&id).copied())
    }

    fn finish(&mut self, builder: &mut EventBuilder) {
        self.compactions.finish(builder);
        let answers = {
            self.paginated_answers
                .resolve_turns(self.paginated_inputs.resolved_turns());
            self.paginated_answers.finish_answers()
        };
        if let Some((ordinal, end)) = self.compactions.latest() {
            self.paginated_answers
                .finish_checkpoint(builder, ordinal, end);
            self.paginated_answers.project_revisions(
                builder,
                self.compactions.public_history(),
                &self.paginated_inputs,
                ordinal,
                end,
            );
        }
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
        self.rollout_lines.finish_segment();
        self.report_unverified_history(builder);
        if self.paginated {
            self.annotate_model_window(builder, &answers);
        } else {
            builder.retain_events(|event| {
                event.source.raw_record_type.as_deref()
                    != Some("omnisession.codex_tool_display_mirror")
            });
        }
        self.restore_legacy_users(builder);
        builder.visit_events_mut(|event| completed_item::clear_source_receipt(&mut event.payload));
        builder.order_delayed_messages(&self.display_positions);
    }

    fn report_unverified_history(&self, builder: &mut EventBuilder) {
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
    }

    fn annotate_model_window(
        &self,
        builder: &mut EventBuilder,
        answers: &HashMap<u64, Option<Withdrawal>>,
    ) {
        builder.visit_events_mut(|event| {
            let realtime =
                event.source.raw_record_type.as_deref() == Some(rollout_lines::TRANSCRIPT_SEGMENT);
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
            if self.compactions.contains(seq) {
                return;
            }
            let status = if realtime {
                self.compactions.public_status(seq, None, None)
            } else {
                let baseline = answers
                    .get(&seq)
                    .copied()
                    .unwrap_or_else(|| self.withdrawn.get(&seq).map(|&(status, _)| status));
                self.compactions
                    .public_status(seq, self.event_request(seq), baseline)
            };
            let (status, prior) = if event.kind == EventKind::MessageAssistant && !realtime {
                let projection = self.paginated_answers.project_answer(
                    seq,
                    self.compactions.public_history(),
                    status,
                );
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
    }

    fn push_message_or_metadata(
        &mut self,
        builder: &mut EventBuilder,
        record: &Value,
        event_type: Option<&str>,
    ) {
        let payload = &record["payload"];
        let checkpoint = self.checkpoint(builder);
        self.items.projected = None;
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
            self.remember_turn(checkpoint, self.items.projected);
        }
        if event_type == Some("user_message") {
            self.legacy_request = self.items.projected;
        } else if event_type == Some("agent_message")
            && let Some(sequence) = self.items.projected
            && let Some(request) = self.legacy_request
            && let Some(owner) = self.request_id(request)
        {
            self.place_early_answer(builder, owner, sequence);
        }
    }

    fn push_compaction(&mut self, builder: &mut EventBuilder, record: &Value) {
        let timestamp = parse_timestamp(record.get("timestamp"));
        let payload = &record["payload"];
        let history = payload["replacement_history"].as_array();
        let legacy = payload["replacement_history"].is_null() && payload["message"].is_string();
        let summary = match payload["message"].as_str() {
            Some("") if legacy => Some("(no summary available)"),
            text => text.filter(|text| !text.trim().is_empty()),
        };
        let items = history.map_or(&[][..], Vec::as_slice);
        let (public, scoped_sources) = self
            .paginated
            .then(|| paginated_checkpoint::PaginatedCheckpoint::read(self, builder, payload))
            .flatten()
            .unzip();
        let start = self.checkpoint(builder);
        let positioned_summary =
            push_compaction_boundary(builder, history, legacy, summary, self.paginated, timestamp);
        let evidence =
            (self.paginated && !items.is_empty()).then(|| DisplayEvidence::collect(builder));
        let replacement_start = builder.checkpoint();
        let mut projection = ProjectedReplacement::default();
        let mut replacement = Vec::new();
        self.items.identity = MessageIdentity::default();
        self.items.clear_mirrors();
        for (index, item) in items.iter().enumerate() {
            if index != 0 && index.is_multiple_of(TOOL_COMPACTION_RECORD_INTERVAL) {
                self.compact(builder);
            }
            if self.paginated {
                projection.record_start(index, builder);
            }
            if paginated_rollback::is_private_instruction(item) || opaque_checkpoint_tool(item) {
                continue;
            }
            let selected = positioned_summary == Some(index);
            if let Some(text) = replacement_summary(item, summary.filter(|_| selected)) {
                push_summary_message(builder, text, timestamp, selected, self.paginated);
                continue;
            }
            if let Some(sources) = scoped_sources.as_ref()
                && self.project_checkpoint_plan(builder, index, item, sources, timestamp)
            {
                continue;
            }
            if self.paginated
                && item["type"]
                    .as_str()
                    .is_some_and(|kind| model_tool_kind(kind, item).is_some())
            {
                self.project_checkpoint_tool(
                    builder,
                    &mut projection,
                    index,
                    scoped_sources.as_ref(),
                    timestamp,
                );
                continue;
            }
            let source = scoped_sources
                .as_ref()
                .and_then(|sources| sources.items.get(&index).copied());
            if let Some(request) = self.project_replacement_item(
                builder,
                item,
                &payload["replacement_history_metadata"][index],
                evidence.as_ref(),
                source,
                timestamp,
            ) {
                replacement.push(request);
            }
        }
        if self.paginated {
            projection.record_start(items.len(), builder);
            self.track_pending_plan_window(items);
        }
        let first = start.sequence;
        self.register_replacement(builder, start, replacement);
        let summary_sequence = summary.filter(|_| legacy).map(|_| builder.checkpoint());
        if let Some(text) = summary.filter(|_| legacy) {
            push_summary_message(builder, text, timestamp, true, self.paginated);
        }
        let empty = builder.checkpoint() == replacement_start;
        let missing_context = history.map_or(!legacy || summary.is_none(), |items| {
            missing_replacement_context(items, empty, self.paginated)
        });
        if missing_context {
            report_unavailable_compaction(builder, timestamp);
        }
        let kind = WindowKind::from_checkpoint(legacy, history.is_some(), summary_sequence);
        self.record_compaction_window(
            first..builder.checkpoint(),
            kind,
            public,
            projection,
            payload["message"].as_str().filter(|_| legacy),
        );
        self.items.clear_mirrors();
    }

    fn record_compaction_window(
        &mut self,
        range: std::ops::Range<u64>,
        kind: WindowKind,
        evidence: Option<paginated_checkpoint::PaginatedCheckpoint>,
        projection: ProjectedReplacement,
        legacy_message: Option<&str>,
    ) {
        if self.paginated {
            let window = paginated_compaction::Window {
                range,
                ordinal: self.source_ordinal,
                kind,
                projection,
            };
            self.compactions.record(
                window,
                evidence,
                legacy_message,
                &self.paginated_inputs,
                self.checkpoint_turns.selected(),
            );
        }
    }

    fn project_checkpoint_tool(
        &self,
        builder: &mut EventBuilder,
        projection: &mut ProjectedReplacement,
        index: usize,
        sources: Option<&paginated_checkpoint::MatchedDisplays>,
        timestamp: Option<DateTime<Utc>>,
    ) {
        let source = sources
            .and_then(|matched| matched.items.get(&index))
            .map(|(seq, _)| *seq);
        projection.project_tool(builder, index, source, timestamp);
        if let Some(&(seq, spelling)) =
            sources.and_then(|matched| matched.async_questions.get(&index))
            && let Some(text) = self.replacement_text(builder, Some((seq, Some(spelling))))
        {
            projection.project_question(builder, seq, text, timestamp);
        }
    }

    fn track_pending_plan_window(&mut self, items: &[Value]) {
        let pending =
            self.paginated_answers
                .pending_plan_window(self.source_ordinal, items, |item| {
                    self.model_plan_mode(item)
                });
        self.paginated_answers.store_plan_window(pending);
    }

    fn project_replacement_item<'a>(
        &mut self,
        builder: &mut EventBuilder,
        item: &'a Value,
        metadata: &Value,
        evidence: Option<&DisplayEvidence>,
        source: Option<paginated_checkpoint::PublicCopy>,
        timestamp: Option<DateTime<Utc>>,
    ) -> Option<ReplacementRequest<'a>> {
        let mut checkpoint = self.checkpoint(builder);
        checkpoint.admission = InputPosition::user_item(item, metadata, self.source_ordinal);
        self.items.projected = None;
        let live_plan_mode = std::mem::take(&mut self.items.plan_mode);
        let user_turn = push_response_item(builder, item, timestamp, &mut self.items);
        self.items.plan_mode = live_plan_mode;
        if let Some(evidence) = evidence
            && let Some(seq) = self.items.projected
            && item["type"] == "message"
            && matches!(item["role"].as_str(), Some("user" | "assistant"))
            && !self.confirm_replacement_message(builder, seq, item, evidence, source)
        {
            return None;
        }
        (user_turn && builder.checkpoint() > checkpoint.sequence).then(|| ReplacementRequest {
            sequence: builder.checkpoint() - 1,
            checkpoint,
            item,
        })
    }

    fn replacement_text(
        &self,
        builder: &mut EventBuilder,
        source: Option<paginated_checkpoint::PublicCopy>,
    ) -> Option<String> {
        source.and_then(|(seq, spelling)| {
            let current = builder.event_at_mut(seq)?.payload["text"].as_str()?;
            match spelling {
                None => Some(current.to_owned()),
                Some(digest)
                    if <[u8; 32]>::from(sha2::Sha256::digest(current.as_bytes())) == digest =>
                {
                    Some(current.to_owned())
                }
                Some(digest) => self
                    .paginated_answers
                    .public_spelling(seq, digest)
                    .map(str::to_owned),
            }
        })
    }

    fn project_checkpoint_plan(
        &mut self,
        builder: &mut EventBuilder,
        index: usize,
        item: &Value,
        sources: &paginated_checkpoint::MatchedDisplays,
        timestamp: Option<DateTime<Utc>>,
    ) -> bool {
        if !self.model_plan_mode(item) {
            return false;
        }
        let Some((narrative, _)) = display_text::model_plan(item) else {
            return false;
        };
        for is_plan in [false, true] {
            if !is_plan && narrative.is_none() {
                continue;
            }
            let source = if is_plan {
                sources
                    .plans
                    .get(&index)
                    .map(|&(seq, digest)| (seq, Some(digest)))
            } else {
                sources.items.get(&index).copied()
            };
            if let Some(text) = self.replacement_text(builder, source) {
                builder.push(
                    EventKind::MessageAssistant,
                    json!({"text":text,"omnisession_codex_replacement":true}),
                    timestamp,
                    ReplayPolicy::Contextual,
                    Some("response_item.message".to_owned()),
                    None,
                );
            } else {
                let seq = builder.checkpoint();
                builder.push(
                    EventKind::ProviderEvent,
                    json!({"omitted_events": 1, "reason": "The Codex checkpoint has no matching published reply for this part of the proposal."}),
                    timestamp,
                    ReplayPolicy::HistoricalOnly,
                    Some("omnisession.codex_compaction_unavailable".into()),
                    None,
                );
                self.paginated_answers.pending_replacement_plan(
                    seq,
                    index,
                    item,
                    self.source_ordinal,
                    is_plan,
                );
            }
        }
        true
    }

    fn confirm_replacement_message(
        &mut self,
        builder: &mut EventBuilder,
        sequence: u64,
        item: &Value,
        evidence: &DisplayEvidence,
        scoped_source: Option<(u64, Option<[u8; 32]>)>,
    ) -> bool {
        let source_turn = item["internal_chat_message_metadata_passthrough"]["turn_id"].as_str();
        let source_id = item["id"].as_str();
        let public_text = self.replacement_text(builder, scoped_source);
        let confirmed = public_text.is_some()
            || (scoped_source.is_none()
                && source_turn.zip(source_id).is_none()
                && (item["role"] != "user" || item["id"].is_null())
                && evidence
                    .unique_unscoped(builder, sequence)
                    .is_some_and(|shown| {
                        item["role"] != "user"
                            || (self.paginated_inputs.display_is_plain_text(shown)
                                && item["content"].as_array().is_some_and(|parts| {
                                    parts.iter().all(|part| part["type"] == "input_text")
                                }))
                    }));
        if let Some(e) = builder.event_at_mut(sequence) {
            if confirmed {
                if let Some(text) = public_text {
                    e.payload["text"] = text.into();
                }
                e.payload["omnisession_codex_replacement"] = true.into();
            } else {
                if e.kind == EventKind::MessageAssistant
                    && let Some(text) = e.payload["text"].as_str()
                {
                    self.paginated_answers.pending_replacement(
                        sequence,
                        item,
                        text,
                        self.source_ordinal,
                    );
                }
                e.kind = EventKind::ProviderEvent;
                e.payload = json!({"omitted_events":1,"reason":"The Codex replacement contains a message without a matching display item."});
                e.source.raw_record_type =
                    Some("omnisession.codex_compaction_unavailable".to_owned());
                e.replay_policy = ReplayPolicy::HistoricalOnly;
            }
        }
        confirmed
    }

    fn legacy_revision_input(&self, owner: Option<u64>) -> Option<&TurnCheckpoint> {
        if self.paginated {
            return None;
        }
        let id = owner.or_else(|| self.turns.last().copied())?;
        let original = self.original_requests.get(&id)?;
        let i = self
            .checkpoints
            .partition_point(|(_, point)| point.sequence <= *original);
        self.checkpoints[..i]
            .iter()
            .rev()
            .find(|(owner, _)| *owner == id)
            .map(|(_, point)| point)
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
            revision_owner(&self.late_owners, &self.original_requests),
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
        let owner = stamped.and_then(|turn| self.noncurrent_owner(turn));
        let input = self
            .legacy_revision_input(owner)
            .and_then(TurnCheckpoint::admitted_sequence);
        let plan_mode = stamped.map_or(self.items.plan_mode, |turn| {
            self.plan_turns.contains(turn)
                || (self.items.plan_mode
                    && self.current_turn_id().is_none_or(|active| active == turn))
        });
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
            let owner = owner.or_else(|| self.turns.last().copied());
            self.plan_receipts.proposal(plan_scope, proposal, owner);
        }
        if !new_turn && let Some(position) = InputPosition::user(record, self.source_ordinal) {
            confirm_admission(
                &mut self.checkpoints,
                &self.requests,
                items.projected,
                position,
            );
        }
        self.display_revisions.observe_message(
            builder,
            items.previous_message.take(),
            input,
            revision_owner(&self.late_owners, &self.original_requests),
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
        self.turns.last() == Some(owner) && !completed
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
            if let Some(owner) = turn.and_then(|turn| self.noncurrent_owner(turn)) {
                self.late_owners.insert(sequence, owner);
                let mut late = CodexItems::default();
                push_response_item(builder, payload, stamp, &mut late);
                *self.late_tool_counts.entry(owner).or_default() += late.tool_events;
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
            let owner = call.as_ref().and_then(|call| call.owner);
            self.push_identified_item(builder, payload, (turn, id), stamp, owner)
        } else {
            self.items.projected = None;
            self.items.tool_owner = self.turns.last().copied().filter(|&id| {
                self.legacy_revision_input(Some(id))
                    .is_some_and(|input| input.admission.is_some())
                    && self
                        .original_requests
                        .get(&id)
                        .zip(call.as_ref())
                        .is_some_and(|(&request, call)| call.sequence >= request)
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
                self.claim_tool(owner, sequence);
                self.claim_tool(owner, call.sequence);
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
            self.withdrawn
                .entry(sequence)
                .or_insert((status, self.source_ordinal));
        }
    }

    fn push_identified_item(
        &mut self,
        builder: &mut EventBuilder,
        payload: &Value,
        (turn, id): (&str, &str),
        timestamp: Option<DateTime<Utc>>,
        call_owner: Option<u64>,
    ) -> Option<u64> {
        let display = payload["type"] == "item_completed";
        if display {
            push_codex_session_metadata(builder, payload, timestamp, true);
        }
        let native_turn = self.aliases.resolve(turn).to_owned();
        let kind = CompletedKind::for_item(payload);
        let projection = self.completion_position(builder, &native_turn, id, kind, payload);
        let original = projection.original;
        let owner = self.noncurrent_owner(&native_turn);
        let input = self.legacy_revision_input(owner).filter(|_| display);
        let message_input = input.and_then(TurnCheckpoint::admitted_sequence);
        let revision_input = input.map(|turn| turn.sequence);
        let previous_display = revision_input
            .filter(|_| owner.is_none())
            .and_then(|input| self.display_revisions.before(builder, original, input));
        // A late result cannot act as a message or tool mirror in the current turn.
        let mut late = self.items.for_late_turn();
        let checkpoint = self.checkpoint(builder);
        // A synthesized display turn can still mirror earlier model items, even if the
        // last logical request in the file is newer. Its native turn owns the result.
        let current_mirrors =
            owner.is_none() || self.current_turn_id() == Some(native_turn.as_str());
        let items = if current_mirrors {
            &mut self.items
        } else {
            &mut late
        };
        let message_id =
            items.prepare_identified(payload, turn, id, &native_turn, projection, self.paginated);
        items.tool_owner = call_owner.or_else(|| self.named_turns.get(&native_turn).copied());
        items.update =
            original.or_else(|| message_id.and_then(|key| self.message_ids.get(&key).copied()));
        items.projected = None;
        let new_turn = items.project_identified(builder, payload, turn, id, timestamp);
        let previous_message = items.previous_message.take();
        let peer = items.tool_projection.peer.take();
        let projected = items.finish_identified();
        let was_displayed = self.observe_reply_revision(builder, previous_message, message_input);
        if let Some(owner) = owner {
            self.count_late_outcome(
                call_owner.unwrap_or(owner),
                projected,
                current_mirrors,
                checkpoint.tool_events,
                late.tool_events,
            );
        }
        let sequence = projected?;
        self.detach_tool_display(builder, projection, peer);
        if display {
            let key = (native_turn.as_str(), id);
            self.record_deferred_receipt(key, sequence, owner, revision_input, projection);
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
        let item_type = payload["item"]["type"].as_str();
        if display && !matches!(item_type, Some("AgentMessage" | "UserMessage" | "Plan")) {
            self.claim_display_outcome(&native_turn, peer.unwrap_or(sequence), checkpoint.sequence);
        }
        let new_slot = original.is_none() || sequence >= checkpoint.sequence;
        let owner = call_owner.or(owner.filter(|_| new_slot));
        if !self.claim_item_owner(
            &native_turn,
            sequence,
            item_type,
            owner,
            new_slot,
            message_id.is_some(),
        ) {
            if new_turn {
                let mut checkpoint = self.pending_turn.take().unwrap_or(checkpoint);
                checkpoint.id = Some(turn.to_owned());
                self.remember_turn(checkpoint, Some(sequence));
            } else if item_type == Some("UserMessage") && original.is_none() {
                self.name_request_turn(turn, sequence);
            }
        }
        if display
            && original.is_none()
            && !was_displayed
            && item_type == Some("AgentMessage")
            && let Some(&owner) = self.named_turns.get(&native_turn)
        {
            self.place_early_answer(builder, owner, sequence);
        }
        self.remember_completion(builder, (turn, id), kind, projection, sequence, peer);
        Some(sequence)
    }

    fn record_deferred_receipt(
        &mut self,
        key: (&str, &str),
        sequence: u64,
        owner: Option<u64>,
        revision_input: Option<u64>,
        projection: completion_positions::Projection,
    ) {
        self.deferred_receipts.observe(
            key,
            sequence,
            revision_input
                .or_else(|| owner.and_then(|id| self.original_requests.get(&id).copied()))
                .unwrap_or(sequence),
            projection.reconsider,
            projection.ambiguous && projection.reconsider.is_some(),
            projection.display_identity,
        );
    }

    fn claim_item_owner(
        &mut self,
        native_turn: &str,
        sequence: u64,
        item_type: Option<&str>,
        owner: Option<u64>,
        new_slot: bool,
        scoped_message: bool,
    ) -> bool {
        let item_owner = (new_slot && !matches!(item_type, Some("UserMessage" | "AgentMessage")))
            .then(|| self.named_turns.get(native_turn).copied())
            .flatten();
        let Some(owner) = owner.or(item_owner) else {
            return false;
        };
        if scoped_message || matches!(item_type, Some("Plan" | "ExitedReviewMode")) {
            self.late_owners.entry(sequence).or_insert(owner);
        } else {
            self.claim_tool(owner, sequence);
        }
        true
    }

    fn count_late_outcome(
        &mut self,
        owner: u64,
        projected: Option<u64>,
        current_mirrors: bool,
        initial_tools: usize,
        late_tools: usize,
    ) {
        let added = if current_mirrors {
            let added = self.items.tool_events - initial_tools;
            self.items.tool_events = initial_tools;
            added
        } else {
            late_tools
        };
        if added > 0
            && let Some(seq) = projected
        {
            self.items.unclaimed_tools.remove(&seq);
            self.late_owners.entry(seq).or_insert(owner);
            *self.late_tool_counts.entry(owner).or_default() += added;
        }
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
        self.display_revisions.observe_message(
            builder,
            previous,
            input,
            revision_owner(&self.late_owners, &self.original_requests),
        )
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
            let name = self.aliases.resolve(turn);
            let owner = self
                .named_turns
                .get(name)
                .copied()
                .or_else(|| self.turns.last().copied());
            self.plan_receipts.completed(name, text, owner);
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
                Some("Plan") | None => None,
                _ => self
                    .paginated_answers
                    .display_tool(sequence, turn, id, item),
            };
            if let Some(status) = withdrawal {
                self.withdrawn
                    .entry(sequence)
                    .or_insert((status, self.source_ordinal));
            }
        }
    }

    fn place_early_answer(&mut self, builder: &EventBuilder, owner: u64, sequence: u64) {
        let Some(&request) = self.original_requests.get(&owner) else {
            return;
        };
        if sequence >= request || self.display_positions.contains_key(&sequence) {
            return;
        }
        let next = self
            .checkpoints
            .partition_point(|(_, point)| point.sequence <= request);
        let boundary = self
            .checkpoints
            .get(next)
            .map_or_else(|| builder.checkpoint(), |(_, point)| point.sequence);
        self.display_positions
            .insert(sequence, (boundary, self.source_ordinal));
        self.late_owners.entry(sequence).or_insert(owner);
    }

    /// A migrated request can precede its generated display turn.
    fn name_request_turn(&mut self, turn: &str, sequence: u64) {
        let Some(owner) = self.request_id(sequence) else {
            return;
        };
        let Some(&source_request) = self.original_requests.get(&owner) else {
            return;
        };
        let Some(index) = self
            .checkpoints
            .partition_point(|(_, c)| c.sequence <= source_request)
            .checked_sub(1)
        else {
            return;
        };
        let (id, checkpoint) = &mut self.checkpoints[index];
        if *id != owner {
            return;
        }
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
        self.named_turns.insert(canonical.to_owned(), owner);
        if self
            .pending_turn
            .as_ref()
            .is_some_and(|pending| sequence < pending.sequence)
        {
            self.pending_turn = None;
        }
    }

    fn claim_display_outcome(&mut self, turn: &str, sequence: u64, checkpoint: u64) {
        if let Some(&owner) = self.named_turns.get(turn) {
            if let Some(call) = self.calls.linked.remove(&sequence) {
                self.claim_tool(owner, call);
            }
            if sequence < checkpoint {
                self.claim_tool(owner, sequence);
            }
        }
    }

    fn claim_tool(&mut self, owner: u64, sequence: u64) {
        let std::collections::hash_map::Entry::Vacant(entry) = self.late_owners.entry(sequence)
        else {
            return;
        };
        entry.insert(owner);
        if !self.items.unclaimed_tools.remove(&sequence) {
            return;
        }
        let later = self
            .checkpoints
            .partition_point(|(_, point)| point.sequence <= sequence);
        // A display item can identify the native owner of an earlier model call and result.
        // Count the record with that owner, including after the visible tool window evicts it.
        *self.late_tool_counts.entry(owner).or_default() += 1;
        self.items.tool_events -= 1;
        for (_, point) in &mut self.checkpoints[later..] {
            point.tool_events -= 1;
        }
        if let Some(point) = self
            .pending_turn
            .as_mut()
            .filter(|point| point.sequence > sequence)
        {
            point.tool_events -= 1;
        }
    }

    fn request_id(&self, sequence: u64) -> Option<u64> {
        let index = self
            .requests
            .binary_search_by_key(&sequence, |(seq, _)| *seq)
            .ok()?;
        Some(self.requests[index].1)
    }

    fn restore_legacy_users(&mut self, builder: &mut EventBuilder) {
        let events = builder.events();
        let Some(end) = events.iter().rposition(|e| {
            e.kind == EventKind::CompactionCreated
                && e.payload["omnisession_codex_rollback"] != "withdrawn"
        }) else {
            return;
        };
        if events[end].source.raw_record_type.as_deref() != Some("compacted.legacy") {
            return;
        }
        let retained = if self.paginated {
            self.compactions
                .legacy_users(events[end].sequence)
                .filter_map(|(seq, budget)| {
                    let event = &events[events.binary_search_by_key(&seq, |e| e.sequence).ok()?];
                    let text = event.payload["text"].as_str()?;
                    let text = budget.map_or_else(
                        || text.to_owned(),
                        |budget| shorten_legacy_user(text, budget).into_owned(),
                    );
                    legacy_message_source(event).map(|summary| (event.event_id, summary, text))
                })
                .collect()
        } else {
            self.unpaginated_legacy_users(&events[..=end])
        };
        let timestamp = events[end].timestamp;
        let boundary = events[end].sequence;
        let inserted = retained.len() as u64;
        // Older checkpoints retain their summaries and original requests for recovery.
        // Emit copies only for the last surviving window after all rollbacks have been applied.
        builder.insert_after(end, |builder| {
            for (id, summary, text) in retained {
                let (kind, wire) = if summary {
                    (EventKind::MessageAssistant, "compacted.summary")
                } else {
                    (EventKind::MessageUser, "compacted.user")
                };
                builder.push(
                    kind,
                    replacement_payload(&text, self.paginated),
                    timestamp,
                    ReplayPolicy::Contextual,
                    Some(wire.to_owned()),
                    Some(id),
                );
            }
        });
        if inserted > 0 {
            self.shift_legacy_positions(builder, boundary, inserted);
        }
    }

    fn shift_legacy_positions(&mut self, builder: &mut EventBuilder, boundary: u64, inserted: u64) {
        let adjust = |seq| if seq > boundary { seq + inserted } else { seq };
        if !self.display_positions.is_empty() {
            self.display_positions = self
                .display_positions
                .drain()
                .map(|(seq, (before, ordinal))| (adjust(seq), (adjust(before), ordinal)))
                .collect();
        }
        if self.paginated {
            builder.visit_events_mut(|event| {
                if matches!(
                    event.kind,
                    EventKind::MessageUser | EventKind::MessageAssistant
                ) && event.source.raw_record_type.as_deref() == Some("event_msg.item_completed")
                    && let Some(anchor) = event
                        .payload
                        .pointer_mut("/omnisession_codex_checkpoint_position/0")
                    && let Some(before) = anchor.as_u64()
                {
                    *anchor = adjust(before).into();
                }
            });
        }
    }

    fn unpaginated_legacy_users(
        &self,
        events: &[omnis_ir::OmniEvent],
    ) -> Vec<(Uuid, bool, String)> {
        let start = events
            .iter()
            .rposition(explicit_compaction)
            .map_or(0, |i| i + 1);
        let mut users = VecDeque::new();
        let mut tokens = 0;
        for e in &events[start..] {
            if e.kind == EventKind::CompactionCreated {
                trim_legacy_users(&mut users, &mut tokens);
            } else if let Some(summary) = legacy_message_source(e)
                && let Some(text) = e.payload["text"].as_str()
                && (self.request_id(e.sequence).is_some()
                    || (summary && !paginated_rollback::contextual_fragment(text)))
            {
                users.push_back((e.event_id, summary, Cow::Borrowed(text)));
                tokens += text.len().div_ceil(4);
            }
        }
        users
            .into_iter()
            .map(|(id, summary, text)| (id, summary, text.into_owned()))
            .collect()
    }

    fn register_replacement(
        &mut self,
        builder: &EventBuilder,
        start: TurnCheckpoint,
        replacement: Vec<ReplacementRequest<'_>>,
    ) {
        if replacement.is_empty() {
            return;
        }
        let events = builder.events();
        let before = events.partition_point(|e| e.sequence < start.sequence);
        let mut originals: HashMap<&str, Vec<(u64, u64)>> = HashMap::new();
        for e in events[..before]
            .iter()
            .rev()
            .take_while(|e| !explicit_compaction(e))
        {
            if let Some(id) = self.request_id(e.sequence)
                && let Some(text) = e.payload["text"].as_str()
            {
                originals.entry(text).or_default().push((e.sequence, id));
            }
        }
        // Work backward so the newest repeated requests match the newest surviving turns.
        let mut before = start.sequence;
        let mut owners = Vec::with_capacity(replacement.len());
        for request in replacement.iter().rev() {
            let matched = events
                .binary_search_by_key(&request.sequence, |e| e.sequence)
                .ok()
                .and_then(|i| events[i].payload["text"].as_str())
                .and_then(|text| {
                    if let Some(point) = request_origin(text, request.item, before, &originals) {
                        return Some(point);
                    }
                    (request.sequence == replacement[0].sequence)
                        .then(|| shortened_request_origin(text, before, &originals))
                        .flatten()
                });
            if let Some((sequence, _)) = matched {
                before = sequence;
            }
            owners.push(matched.map(|(_, id)| id));
        }
        owners.reverse();
        if owners.iter().all(Option::is_some) {
            self.requests.extend(
                replacement
                    .into_iter()
                    .zip(owners)
                    .map(|(request, id)| (request.sequence, id.unwrap())),
            );
            return;
        }

        // Some original turns are missing. Use the replacement for their boundaries and
        // associate copies of known requests with the same turn in both parts of the file.
        let mut previous = std::mem::take(&mut self.turns).into_iter();
        let mut missing = Vec::new();
        let mut window_start = Some(self.pending_turn.take().unwrap_or(start));
        for (request, matched) in replacement.into_iter().zip(owners) {
            let id = if let Some(id) = matched {
                self.turns
                    .extend(previous.by_ref().take_while(|old| *old != id));
                self.turns.append(&mut missing);
                self.turns.push(id);
                id
            } else {
                let id = self.new_turn();
                missing.push(id);
                id
            };
            self.requests.push((request.sequence, id));
            self.original_requests.entry(id).or_insert(request.sequence);
            let admission = request.checkpoint.admission;
            let mut point = window_start.take().unwrap_or(request.checkpoint);
            point.admission = admission;
            self.checkpoints.push((id, point));
        }
        self.turns.extend(previous);
        self.turns.append(&mut missing);
    }
}

fn request_origin(
    text: &str,
    item: &Value,
    before: u64,
    originals: &HashMap<&str, Vec<(u64, u64)>>,
) -> Option<(u64, u64)> {
    let previous = |text: &str| {
        let positions = originals.get(text)?;
        let skip = positions.partition_point(|(seq, _)| *seq >= before);
        positions.get(skip).copied()
    };
    let parts = item["content"]
        .as_array()
        .filter(|parts| parts.len() > 1)
        .map(|parts| {
            message_parts(parts, |i, _| {
                hook_attribution(&item["internal_chat_message_metadata_passthrough"], i)
                    != Some(true)
            })
        });
    // Display items concatenate the same parts that a model response joins with newlines.
    let display = parts
        .filter(|parts| parts.len() > 1)
        .and_then(|parts| previous(&parts.concat()));
    previous(text).max(display)
}

fn explicit_compaction(event: &omnis_ir::OmniEvent) -> bool {
    event.kind == EventKind::CompactionCreated
        && event.payload["omnisession_codex_rollback"] != "withdrawn"
        && event.source.raw_record_type.as_deref() != Some("compacted.legacy")
}

fn legacy_message_source(event: &omnis_ir::OmniEvent) -> Option<bool> {
    if event.kind == EventKind::MessageUser {
        Some(false)
    } else {
        (event.kind == EventKind::MessageAssistant
            && event.source.raw_record_type.as_deref() == Some("compacted.summary"))
        .then_some(true)
    }
}

fn trim_legacy_users(users: &mut VecDeque<(Uuid, bool, Cow<'_, str>)>, tokens: &mut usize) {
    // Codex retains 20,000 approximate tokens and replaces the middle of the oldest partial request.
    while *tokens > COMPACT_USER_TOKEN_LIMIT {
        let Some((_, _, text)) = users.front_mut() else {
            break;
        };
        let size = text.len().div_ceil(4);
        let other = *tokens - size;
        if other >= COMPACT_USER_TOKEN_LIMIT {
            users.pop_front();
            *tokens = other;
            continue;
        }
        if let Cow::Owned(shortened) = shorten_legacy_user(text, COMPACT_USER_TOKEN_LIMIT - other) {
            *text = Cow::Owned(shortened);
        }
        *tokens = other + text.len().div_ceil(4);
        break;
    }
}

fn shorten_legacy_user(text: &str, budget: usize) -> Cow<'_, str> {
    let omitted = text.len().div_ceil(4).saturating_sub(budget);
    if omitted > 0 {
        let (prefix, suffix) = retained_user_edges(text, budget);
        let marker = format!("…{omitted} tokens truncated…");
        if text[prefix.len()..text.len() - suffix.len()] != marker {
            return Cow::Owned(format!("{prefix}{marker}{suffix}"));
        }
    }
    Cow::Borrowed(text)
}

fn retained_user_edges(text: &str, tokens: usize) -> (&str, &str) {
    let half = 2 * tokens;
    let mut left = half;
    let mut right = text.len() - half;
    while !text.is_char_boundary(left) {
        left -= 1;
    }
    while !text.is_char_boundary(right) {
        right += 1;
    }
    (&text[..left], &text[right..])
}

fn shortened_request_origin(
    text: &str,
    before: u64,
    originals: &HashMap<&str, Vec<(u64, u64)>>,
) -> Option<(u64, u64)> {
    let (prefix, count, suffix) =
        text.match_indices(" tokens truncated…")
            .find_map(|(offset, end)| {
                let (prefix, count) = text[..offset].rsplit_once('…')?;
                let suffix = &text[offset + end.len()..];
                let count: usize = count.parse().ok()?;
                // The two retained halves have the same byte budget. UTF-8 boundaries can differ by three bytes.
                (count > 0 && prefix.len().abs_diff(suffix.len()) <= 3)
                    .then_some((prefix, count, suffix))
            })?;
    // Match the exact head and tail produced by Codex. Newer rollouts store the notice
    // in its own text block, which the visible projection joins with a newline on each side.
    originals
        .iter()
        .filter_map(|(source, positions)| {
            let kept = source.len().div_ceil(4).checked_sub(count)?;
            if !(1..=COMPACT_USER_TOKEN_LIMIT).contains(&kept) {
                return None;
            }
            let (left, right) = retained_user_edges(source, kept);
            if left.is_empty() && right.is_empty() {
                return None;
            }
            if (prefix == left && suffix == right)
                || (prefix.strip_suffix('\n') == Some(left)
                    && suffix.strip_prefix('\n') == Some(right))
            {
                let skip = positions.partition_point(|(seq, _)| *seq >= before);
                return positions.get(skip).copied();
            }
            None
        })
        .max()
}

fn replacement_summary_index(items: &[Value], summary: Option<&str>) -> Option<usize> {
    items
        .iter()
        .rposition(|item| replacement_summary(item, None).is_some())
        .or_else(|| {
            summary.and_then(|text| {
                items
                    .iter()
                    .rposition(|item| replacement_summary(item, Some(text)) == Some(text))
            })
        })
}

fn report_unavailable_compaction(builder: &mut EventBuilder, timestamp: Option<DateTime<Utc>>) {
    builder.push(
        EventKind::ProviderEvent,
        json!({"omitted_events": 1, "reason":
                "Codex compaction context is unavailable"}),
        timestamp,
        ReplayPolicy::HistoricalOnly,
        Some("omnisession.codex_compaction_unavailable".to_owned()),
        None,
    );
}

fn opaque_checkpoint_tool(item: &Value) -> bool {
    matches!(
        item["type"].as_str(),
        Some("computer_call" | "computer_call_output" | "local_shell_call_output")
    )
}

fn native_replacement_items(items: &[Value], paginated: bool) -> impl Iterator<Item = &Value> {
    items
        .iter()
        .filter(move |item| paginated || !checkpoint_wire::legacy_ghost(item))
}

fn missing_replacement_context(items: &[Value], empty_projection: bool, paginated: bool) -> bool {
    native_replacement_items(items, paginated).any(|item| {
        matches!(
            item["type"].as_str(),
            Some("compaction" | "context_compaction" | "compaction_summary")
        ) || (empty_projection && !opaque_checkpoint_tool(item))
    })
}

fn push_compaction_boundary(
    builder: &mut EventBuilder,
    history: Option<&Vec<Value>>,
    legacy: bool,
    summary: Option<&str>,
    paginated: bool,
    timestamp: Option<DateTime<Utc>>,
) -> Option<usize> {
    let start = builder.checkpoint();
    let items = history.map_or(&[][..], Vec::as_slice);
    let positioned_summary = replacement_summary_index(items, summary);
    if history.is_some() {
        // The model receives only the replacement items. A separate summary is documentary.
        let count = if paginated {
            items.len()
        } else {
            native_replacement_items(items, false).count()
        };
        let mut boundary = json!({"replacement_items":count});
        if let Some(text) = summary
            && positioned_summary.and_then(|i| replacement_summary(&items[i], summary))
                != Some(text)
        {
            boundary["summary"] = text.into();
        }
        builder.push(
            EventKind::CompactionCreated,
            boundary,
            timestamp,
            ReplayPolicy::HistoricalOnly,
            Some("compacted".to_owned()),
            None,
        );
    } else {
        builder.push(EventKind::CompactionCreated,
                json!({"summary":"Codex replaced the preceding conversation with a compacted context window."}),
                timestamp, ReplayPolicy::Contextual, Some(if legacy {"compacted.legacy"} else {"compacted"}.to_owned()), None);
    }
    if paginated && let Some(fence) = builder.event_at_mut(start) {
        fence.payload["omnisession_codex_paginated"] = true.into();
    }
    positioned_summary
}

fn replacement_payload(text: &str, paginated: bool) -> Value {
    let mut payload = json!({"text":text});
    if paginated {
        payload["omnisession_codex_replacement"] = true.into();
    }
    payload
}

fn push_summary_message(
    builder: &mut EventBuilder,
    text: &str,
    timestamp: Option<DateTime<Utc>>,
    latest: bool,
    paginated: bool,
) {
    let kind = if latest {
        "compacted.summary"
    } else {
        "response_item.message"
    };
    builder.push(
        EventKind::MessageAssistant,
        replacement_payload(text, paginated),
        timestamp,
        ReplayPolicy::Contextual,
        Some(kind.to_owned()),
        None,
    );
}

/// Local compaction stores its summary as a user message at its position in the replacement.
fn replacement_summary<'a>(item: &'a Value, summary: Option<&str>) -> Option<&'a str> {
    if item["type"] != "message" || item["role"] != "user" {
        return None;
    }
    let text = if let Some(text) = item["content"].as_str() {
        text
    } else {
        let [part] = item["content"].as_array()?.as_slice() else {
            return None;
        };
        if !matches!(part["type"].as_str(), Some("input_text" | "text")) {
            return None;
        }
        part["text"].as_str()?
    };
    let metadata = &item["internal_chat_message_metadata_passthrough"];
    if hook_attribution(metadata, 0).is_some() {
        return None;
    }
    let kinds = &metadata["content_item_kinds"];
    let legacy_summary = kinds[0].is_null()
        && Some(text) == summary
        && text
            .split_once('\n')
            .is_some_and(|(prefix, _)| prefix == LOCAL_SUMMARY_PREFIX);
    (!text.trim().is_empty() && (legacy_summary || kinds[0] == "compaction.summary"))
        .then_some(text)
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

fn workspace_record_filter() -> impl Fn(&[u8]) -> bool {
    let cwd = memchr::memmem::Finder::new(b"turn_context");
    let settings = memchr::memmem::Finder::new(b"thread_settings_applied");
    let rollback = workspace_marker_filter(b"thread_rolled_back");
    move |line| {
        cwd.find(line).is_some()
            || settings.find(line).is_some()
            || rollback(line)
            || workspace_damage::damaged_workspace_record(line).is_some()
    }
}

fn contains_rollback_marker(path: &Path) -> Result<bool> {
    if !contains_workspace_marker(path, b"thread_rolled_back", 1024 * 1024)? {
        return Ok(false);
    }
    let mut rollback = false;
    rollout_file::visit(path, |record, _, _| {
        rollback |= encoded_rollback(&record);
        Ok(())
    })?;
    Ok(rollback)
}

fn contains_settings_marker(path: &Path) -> Result<bool> {
    contains_workspace_marker(path, b"thread_settings_applied", 1024 * 1024)
}

fn contains_workspace_marker(path: &Path, name: &[u8], chunk: usize) -> Result<bool> {
    let reader = rollout_file::open(path)?.take(MAX_STREAMED_TRANSCRIPT_FILE_SIZE);
    reader_contains_workspace_marker(reader, name, chunk)
}

fn oversized_settings_marker(
    path: &Path,
    locations: &[crate::support::JsonLineLocation],
) -> Result<bool> {
    let mut file = fs::File::open(path)?;
    for record in locations {
        file.seek(SeekFrom::Start(record.offset))?;
        if reader_contains_workspace_marker(
            (&mut file).take(record.length),
            b"thread_settings_applied",
            1024 * 1024,
        )? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn reader_contains_workspace_marker(
    mut reader: impl Read,
    name: &[u8],
    chunk: usize,
) -> Result<bool> {
    let matches = workspace_marker_filter(name);
    // Overlap the reads so a marker split across them is still visible.
    let mut buffer = vec![0_u8; chunk.max(name.len())];
    let mut overlap = 0;
    loop {
        let read = reader.read(&mut buffer[overlap..])?;
        if read == 0 {
            return Ok(false);
        }
        let length = overlap + read;
        let window = &buffer[..length];
        if matches(window) {
            return Ok(true);
        }
        overlap = (name.len() - 1).min(length);
        buffer.copy_within(length - overlap..length, 0);
    }
}

fn workspace_marker_filter(name: &[u8]) -> impl Fn(&[u8]) -> bool + '_ {
    let marker = memchr::memmem::Finder::new(name);
    let tag = rollout_lines::ROLLBACK_TAG.to_string();
    let numeric = (name == b"thread_rolled_back")
        .then(|| memchr::memmem::Finder::new(tag.as_bytes()).into_owned());
    let escape = memchr::memmem::Finder::new(b"\\u00");
    move |bytes| {
        marker.find(bytes).is_some()
            // A possible numeric tag selects the reader that checks its enclosing event.
            || numeric.as_ref().is_some_and(|numeric| numeric.find_iter(bytes).any(|start| {
                start.checked_sub(1).and_then(|i| bytes.get(i)).is_some_and(|b|
                    matches!(b, b':' | b'[') || b.is_ascii_whitespace()
                ) && bytes.get(start + tag.len()).is_some_and(|b|
                    matches!(b, b',' | b'}') || b.is_ascii_whitespace()
                )
            }))
            || escape
                .find_iter(bytes)
                .any(|index| escapes_ascii_letter(bytes.get(index + 4..index + 6)))
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
    // Mirrors and documentary items can acquire a turn without adding a tool event.
    unclaimed_tools: HashSet<u64>,
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
            self.unclaimed_tools.insert(sequence);
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
        for key in [
            "omnisession_codex_rollback",
            "omnisession_codex_checkpoint",
            "omnisession_codex_prior_display",
            "omnisession_codex_before_compaction",
            "omnisession_codex_positioned_reply",
            "omnisession_codex_active_replies",
            "omnisession_codex_checkpoint_position",
            "omnisession_codex_replacement",
            "omnisession_codex_paginated",
            "omnisession_codex_summary_withdrawn",
            "omnisession_codex_tool_display",
        ] {
            fields.remove(key);
        }
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

    let Some(kind) = model_tool_kind(item_type, payload) else {
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

fn model_tool_kind(item_type: &str, payload: &Value) -> Option<EventKind> {
    match item_type {
        "function_call" | "custom_tool_call" | "web_search_call" | "computer_call" => {
            Some(EventKind::ToolCalled)
        }
        "local_shell_call" => Some(EventKind::CommandExecuted),
        "function_call_output"
        | "custom_tool_call_output"
        | "local_shell_call_output"
        | "computer_call_output" => Some(model_result_kind(payload)),
        _ => None,
    }
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
                | "encrypted_function_args"
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

impl CodexSession {
    fn selected_workspace(&self, catalog: Option<&WorkspaceCatalog>) -> Result<NativeWorkspace> {
        #[cfg(unix)]
        if self
            .stamp
            .is_some_and(|stamp| stamp.len > MAX_PREVIEW_TAIL_SIZE)
            && let Some(observation) = catalog.and_then(|catalog| catalog.verified_workspace(self))
        {
            return Ok(NativeWorkspace {
                git: self.git_at(observation.project_path.as_deref()),
                directory: observation.project_path,
                startup_directory: None,
            });
        }
        let workspace = self.live_workspace()?;
        if let Some(catalog) = catalog {
            catalog.record(
                self,
                workspace.directory.as_deref(),
                workspace.git.branch.as_deref(),
            );
        }
        Ok(workspace)
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
        let listing = self.listing(false).0;
        listing
            .rollout_notes
            .iter()
            .chain(&listing.names.notes)
            .cloned()
            .collect()
    }

    fn discovery_is_complete(&self) -> bool {
        // A missing Codex home is not evidence that previously cached sessions were deleted.
        self.codex_home.as_deref().is_some_and(Path::is_dir)
            && self.listing(false).0.rollout_notes.is_empty()
    }

    fn session_titles_are_complete(&self) -> bool {
        self.observed_session_names()
            .is_some_and(|observed| observed.complete)
    }

    fn observed_session_names(&self) -> Option<crate::ObservedSessionNames> {
        let current = self.title_index();
        self.listing
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .is_none_or(|cached| Arc::ptr_eq(&cached.listing.names, &current))
            .then(|| crate::ObservedSessionNames {
                names: Arc::clone(&current.titles),
                complete: current.complete,
            })
    }

    fn listed_session_names(
        &self,
        sessions: &[(&str, &Path, SourceRevision)],
    ) -> HashMap<String, NativeSessionName> {
        if sessions.is_empty() {
            return HashMap::new();
        }
        let (listing, _) = self.listing(true);
        listing.matching_names(sessions, |_| true)
    }

    fn listed_session_names_still_current(
        &self,
        sessions: &[(&str, &Path, SourceRevision)],
    ) -> HashMap<String, NativeSessionName> {
        if sessions.is_empty() {
            return HashMap::new();
        }
        let Some(home) = self.codex_home.as_deref() else {
            return HashMap::new();
        };
        let cache = self
            .listing
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(cached) = cache.as_ref() else {
            return HashMap::new();
        };
        let listing = &cached.listing;
        if !Arc::ptr_eq(&listing.names, &self.title_index()) || !listing.scan.current() {
            return HashMap::new();
        }
        if listing.requires_scan
            || listing.legacy_revisions.iter().any(|(path, previous)| {
                previous.is_none() || Self::index_file_revision(path) != *previous
            })
        {
            drop(cache);
            return self.listed_session_names(sessions);
        }
        let rows = if cached.database.is_some() && self.current.stamp(home) == cached.database {
            None
        } else {
            let ids = sessions.iter().map(|&(id, _, _)| id);
            let Ok(rows) = self
                .current
                .rows_with_titles(home, ids, &listing.names.titles)
            else {
                return HashMap::new();
            };
            Some(rows)
        };
        listing.matching_names(sessions, |id| {
            rows.as_ref()
                .is_none_or(|rows| rows.get(id) == listing.registry_rows.get(id))
        })
    }

    fn native_session_name(&self, id: &str) -> NativeSessionName {
        match self.resolve_rollout(id, None, true) {
            Ok(Some(selected)) => return selected.name,
            Ok(None) => {}
            Err(_) => return NativeSessionName::Unknown,
        }
        let index = self.title_index();
        match index.titles.get(id) {
            Some(name) => NativeSessionName::Named(name.clone()),
            None if index.complete => NativeSessionName::Absent,
            None => NativeSessionName::Unknown,
        }
    }

    fn prior_session_names(&self, sessions: &[&NativeSession]) -> HashMap<String, String> {
        if sessions.is_empty() {
            return HashMap::new();
        }
        let index = self.title_index();
        if index.prior_titles.is_empty() {
            return HashMap::new();
        }
        let Some(home) = self
            .codex_home
            .as_deref()
            .and_then(|path| fs::canonicalize(path).ok())
        else {
            return HashMap::new();
        };
        let names = sessions
            .iter()
            .filter_map(|session| {
                let id = &session.session.id;
                let name = index.prior_titles.get(id)?;
                (session.session.provider == Provider::Codex
                    && session.title.is_none()
                    && !index.titles.contains_key(id)
                    && self.confirms_legacy_name_source(&home, session))
                .then(|| (id.clone(), name.clone()))
            })
            .collect();
        if Arc::ptr_eq(&index, &self.title_index()) {
            names
        } else {
            HashMap::new()
        }
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
                    && Self::header_is_current(path, Some(*selected))
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
        let observed_paths = std::cell::LazyCell::new(|| scan.observed_file_paths());
        let mut replacements = Vec::new();
        for (id, revision) in prior {
            match rows.get(id) {
                Some(row) if row.requires_selection(home) => {
                    match row.resolve(home, id, scan.paths(id), scan.notes.is_empty()) {
                        Err(error) if error.is::<CurrentRolloutUnavailable>() => {
                            unavailable.insert(id.clone());
                        }
                        Err(error) if error.is::<current_rollout::CurrentRolloutUnconfirmed>() => {
                            let Some(file) = revision.and_then(|revision| revision.file) else {
                                continue;
                            };
                            if row.has_legacy_name() {
                                if let Some(prior) = scan
                                    .paths(id)
                                    .find(|path| Self::index_file_id(path) == Some(file))
                                    .or_else(|| observed_paths.get(&file).copied())
                                    && row.recorded_file_replaces(home, prior, file)
                                {
                                    replacements.push((id, row, prior, file));
                                }
                            } else if let Some(selected) = row.generation(id)
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
        if !replacements.is_empty() {
            let current = self
                .current
                .rows(home, replacements.iter().map(|(id, ..)| id.as_str()))?;
            for (id, row, prior, file) in replacements {
                if current.get(id) == Some(row) && row.recorded_file_replaces(home, prior, file) {
                    unavailable.insert(id.clone());
                }
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
        if let Some(catalog) = self.workspace_catalog.as_deref()
            && let Some(observation) = catalog.find(id)
        {
            catalog.apply(&selected.session, &mut native, &observation, None);
        }
        if self.use_workspace_index
            && let Some(file) = native.source_revision.and_then(|revision| revision.file)
            && let Ok(workspaces) = omnis_store::CodexWorkspace::discovery_index(Some(file))
        {
            native.apply_codex_workspace_index(&workspaces, false);
        }
        Ok(Some(native))
    }

    fn search_transcript_path(
        &self,
        session: &SessionRef,
        hint: Option<&Path>,
    ) -> Result<Option<PathBuf>> {
        let path = self.find_session(&session.id, hint)?.path;
        anyhow::ensure!(
            hint.is_none_or(|hint| hint == path),
            "selected Codex rollout changed during indexing"
        );
        Ok(Some(path))
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
        let native = self.find_session(&session.id, None)?;
        let (snapshot, _) = self.read_selected(native.clone(), |native, _| native.read(session))?;
        self.remember_workspace(
            &native,
            &snapshot,
            &CodexWorkspaceProjection::Unchecked,
            true,
        );
        Ok(snapshot)
    }

    fn preview_session(&self, session: &SessionRef) -> Result<omnis_ir::CanonicalSnapshot> {
        validate_provider(session, Provider::Codex)?;
        let native = self.find_session(&session.id, None)?;
        let (snapshot, workspace) = self.read_selected(native.clone(), |native, _| {
            let small_plain_rollout = !rollout_file::compressed(&native.path)
                && fs::metadata(&native.path)?.len() <= MAX_PREVIEW_TAIL_SIZE;
            let (mut snapshot, _, workspace) = native.preview(session)?;
            if small_plain_rollout
                && snapshot.events.last().is_some_and(|event| {
                    event.source.raw_record_type.as_deref()
                        == Some("omnisession.codex_preview_prefix")
                        && event.payload["prefix_complete"].as_bool() == Some(true)
                        && event.payload["prefix_end_sequence"].as_u64() == Some(event.sequence)
                })
            {
                snapshot.events.pop();
            }
            Ok((snapshot, workspace))
        })?;
        self.remember_workspace(&native, &snapshot, &workspace, false);
        Ok(snapshot)
    }

    fn native_workspace(&self, session: &SessionRef) -> Result<Option<NativeWorkspace>> {
        validate_provider(session, Provider::Codex)?;
        self.read_selected(self.find_session(&session.id, None)?, |native, _| {
            native.selected_workspace(self.workspace_catalog.as_deref())
        })
        .map(Some)
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
        _prior: Option<crate::PriorSearchDocument>,
    ) -> Result<crate::IndexedSearchDocument> {
        validate_provider(session, Provider::Codex)?;
        let native = self.require_rollout(&session.id, source_path)?.session;
        self.index_rollout(session, native, source_path, full_read_bytes)
            .map(Into::into)
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
    fn a_long_lived_adapter_rechecks_renamed_and_unreadable_name_indexes() -> Result<()> {
        let home = tempfile::tempdir()?;
        let id = "019f0000-0000-7000-8000-000000000001";
        let adapter = CodexAdapter::with_root(home.path());
        let path = home.path().join("session_index.jsonl");
        assert!(adapter.confirms_session_title(id, None));
        for name in ["Amber Heron", "Violet Quokka"] {
            fs::write(
                &path,
                serde_json::json!({"id": id, "thread_name": name}).to_string(),
            )?;
            assert!(adapter.confirms_session_title(id, Some(name)));
            assert!(!adapter.confirms_session_title(id, None));
        }
        adapter.list_sessions(None)?;
        fs::write(&path, "{\n")?;
        assert!(!adapter.confirms_session_title(id, None));
        fs::write(&path, " \t\r\n")?;
        assert!(adapter.confirms_session_title(id, None));
        assert!(!adapter.session_titles_are_complete());
        fs::remove_file(&path)?;
        fs::create_dir(&path)?;
        assert!(!adapter.confirms_session_title(id, Some("Violet Quokka")));
        assert!(!adapter.confirms_session_title(id, None));
        fs::remove_dir(&path)?;
        assert!(adapter.confirms_session_title(id, None));
        Ok(())
    }

    #[test]
    fn codex_confirms_only_names_assigned_after_the_last_unreadable_row() -> Result<()> {
        let home = tempfile::tempdir()?;
        let id = "019f0000-0000-7000-8000-000000000001";
        let other = "019f0000-0000-7000-8000-000000000002";
        let adapter = CodexAdapter::with_root(home.path());
        let path = home.path().join("session_index.jsonl");
        let name = |id, title, time| json!({"id": id, "thread_name": title, "updated_at": time});
        let original = name(id, "Amber Heron", "2026-01-03T00:00:00Z");
        let unrelated = name(other, "Blue Juniper", "2026-01-02T00:00:00Z");
        let spelling = " Violet    Quokka ";
        let renamed = name(id, spelling, "2026-01-01T00:00:00Z");
        for gap in ["{".to_owned(), "x".repeat(2 * 1024 * 1024 + 1)] {
            fs::write(&path, format!("{original}\n{unrelated}\n{gap}"))?;
            assert_eq!(adapter.native_session_name(id), NativeSessionName::Unknown);

            fs::write(
                &path,
                format!("{original}\n{unrelated}\n{gap}\n{renamed}\n"),
            )?;
            assert_eq!(
                adapter.native_session_name(id),
                NativeSessionName::Named(spelling.to_owned())
            );
            assert_eq!(
                adapter.native_session_name(other),
                NativeSessionName::Unknown
            );
        }
        let blank = name(id, " \t ", "2026-01-04T00:00:00Z");
        fs::write(&path, format!("{original}\n{renamed}\n{blank}\n \t\n"))?;
        assert!(adapter.confirms_session_title(id, Some(spelling)));
        assert_eq!(
            adapter.native_session_name(other),
            NativeSessionName::Absent
        );
        Ok(())
    }

    #[test]
    fn unannotated_local_summary_uses_the_codex_prompt_prefix() {
        let text = format!("{LOCAL_SUMMARY_PREFIX}\nRetain the latest question.");
        let user =
            json!({"type":"message", "role":"user", "content":[{"type":"input_text","text":text}]});
        assert_eq!(replacement_summary(&user, Some(&text)), Some(text.as_str()));
        let mut ordinary = user;
        ordinary["internal_chat_message_metadata_passthrough"] =
            json!({"content_item_kinds":["user.text"]});
        assert_eq!(replacement_summary(&ordinary, Some(&text)), None);
    }

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
        let write_rollout = |cwd: &Path| {
            let header = json!({
                "timestamp": "2026-09-15T10:00:00Z",
                "type": "session_meta",
                "payload": {"id": id, "cwd": cwd}
            });
            fs::write(&rollout, format!("{header}\n")).expect("rollout");
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

        let original = temporary.path().join("original");
        let renamed = temporary.path().join("renamed-longer");
        fs::create_dir(&original).expect("original project");
        fs::create_dir(&renamed).expect("renamed project");
        write_rollout(&original);
        assert_eq!(listed_projects(), [Some(original.clone())]);
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

            let cached_project = temporary.path().join("cached");
            let mut cached: Value =
                serde_json::from_slice(&fs::read(&cache).expect("cache file")).expect("cache JSON");
            let rollouts = cached["rollouts"].as_object_mut().expect("cached rollouts");
            assert_eq!(rollouts.len(), 1);
            let entry = rollouts.values_mut().next().expect("cached rollout");
            entry["header"]["project_path"] = json!(cached_project);
            fs::write(&cache, cached.to_string()).expect("edited cache");
            assert_eq!(listed_projects(), [Some(cached_project)]);

            cached["format"] = json!("0-stale");
            fs::write(&cache, cached.to_string()).expect("stale cache");
            assert_eq!(listed_projects(), [Some(original)]);
        }

        write_rollout(&renamed);
        assert_eq!(listed_projects(), [Some(renamed)]);
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
        let notes = listing_notes(
            &scan,
            0,
            1,
            &super::InvalidHeaders {
                unreadable: 1,
                first_error: Some(anyhow::anyhow!("permission denied")),
                ..Default::default()
            },
        );
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

        let readable = listing_notes(&scan, 4, 0, &super::InvalidHeaders::default());
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
                contains_workspace_marker(&path, b"thread_rolled_back", chunk).expect("chunked scan"),
                naive_rollback_marker(&bytes)
            );
        }
    }
}
