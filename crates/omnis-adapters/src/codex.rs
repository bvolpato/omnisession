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
    CodexWorkspaceProjection, IndexRead, LaunchPlan, LaunchTarget, NativeResumeSupport,
    NativeSession, NativeSessionName, ProviderAdapter, ProviderInstallation, RollbackTitle,
    support::{
        EventBuilder, JsonLineSkips, JsonLinesPreview, MAX_PREVIEW_TAIL_SIZE,
        MAX_STREAMED_TRANSCRIPT_FILE_SIZE, MAX_STREAMED_TRANSCRIPT_LINE_SIZE,
        json_lines_preview_with_omissions, omitted_images_text, parse_timestamp, paths_match,
        provider_executable, provider_file, sort_sessions, string_at, validate_provider, value_at,
        visit_index_json_lines_with_gaps,
    },
};

mod ancestor_paths;
mod history_base;
mod rollout_identity;

use history_base::{HistoryBase, HistoryScope, Lineage};
use rollout_identity::{IdKind, RolloutCatalog, RolloutSelector};

mod current_launch;
mod current_rollout;
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
const SCAN_DEPTH: usize = 5;
const UNAVAILABLE_ANCESTRY: &str = "omnisession.codex_history_base";
const MAX_CANONICAL_TOOL_EVENTS: usize = 256;
// Rollouts stream without a record budget, so retained events and turn checkpoints keep the newest.
const MAX_CANONICAL_EVENTS: usize = 100_000;
const MAX_TOOL_STRING_CHARACTERS: usize = 32 * 1024;
const MAX_TOOL_ARRAY_ITEMS: usize = 256;
const TOOL_COMPACTION_RECORD_INTERVAL: usize = 1_024;

#[derive(Clone, Debug, Default)]
struct CodexFileScan {
    files: Vec<PathBuf>,
    candidates: HashMap<String, Vec<usize>>,
    notes: Vec<String>,
    active_entries_complete: bool,
    // Native filename verification checks these link targets before authorizing an in-place resume.
    active_links: Vec<PathBuf>,
    ids: OnceLock<HashMap<String, Vec<usize>>>,
    skipped_directories: bool,
}

impl CodexFileScan {
    fn ancestor_paths<'a>(
        &'a self,
        home: &'a Path,
        id: &str,
    ) -> impl Iterator<Item = &'a Path> + 'a {
        self.indices(id)
            .iter()
            .map(|index| self.files[*index].as_path())
            .filter(move |path| rollout_identity::discovery_path(home, path))
    }

    fn indices(&self, id: &str) -> &[usize] {
        self.ids
            .get_or_init(|| {
                let mut ids = HashMap::<String, Vec<usize>>::new();
                for (index, path) in self.files.iter().enumerate() {
                    if let Some((thread, rollout)) = current_rollout::ids(path) {
                        ids.entry(thread.to_owned()).or_default().push(index);
                        if thread != rollout {
                            ids.entry(rollout.to_owned()).or_default().push(index);
                        }
                    }
                }
                ids
            })
            .get(id)
            .map(Vec::as_slice)
            .unwrap_or_default()
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
        anyhow::ensure!(
            !ambiguous_generations(self, id) || (row.is_none() && self.canonical_generations(id)),
            CurrentRolloutUnavailable
        );
        Ok(select_rollout(
            self.paths(id),
            |path| listed_session(path, cache.as_deref_mut()),
            invalid,
        ))
    }

    fn canonical_generations(&self, id: &str) -> bool {
        self.paths(id).all(current_rollout::canonical_name)
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
        indices.push(self.files.len());
        self.files.push(path.to_owned());
        sort_rollout_indices(indices, &self.files, active);
        self.ids.take();
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
                    shadowed: shadowed || (dual && !current_rollout::canonical_name(path)),
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
    sessions: Vec<CodexSession>,
    rollout_notes: Vec<String>,
    names: Arc<TitleIndex>,
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
    selector: Arc<RolloutSelector>,
    current: Arc<CurrentRollouts>,
    use_workspace_index: bool,
    /// Rollout header cache file. Explicit roots leave it unset.
    rollout_cache: Option<PathBuf>,
    session_scan: Arc<Mutex<Option<Arc<CodexFileScan>>>>,
    listing: Arc<Mutex<Option<CachedListing>>>,
    titles: Arc<Mutex<Option<CachedTitleIndex>>>,
}

impl CodexAdapter {
    /// A leaf rollout cannot supply the history stored in an unresolved ancestor.
    #[must_use]
    pub fn has_unavailable_ancestry(snapshot: &omnis_ir::CanonicalSnapshot) -> bool {
        snapshot.session.provider == Provider::Codex
            && snapshot
                .events
                .iter()
                .any(|event| event.source.raw_record_type.as_deref() == Some(UNAVAILABLE_ANCESTRY))
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
        if native.needs_visible_projection() {
            return Ok(None);
        }
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
        Self {
            codex_home: Some(codex_home.into()),
            selector: Arc::default(),
            current: Arc::default(),
            use_workspace_index: false,
            rollout_cache: None,
            session_scan: Arc::default(),
            listing: Arc::default(),
            titles: Arc::default(),
        }
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

    fn scan_for_lookup(
        &self,
        id: &str,
        source_path: Option<&Path>,
        home: &Path,
        canonical_home: &Path,
    ) -> (Arc<CodexFileScan>, bool) {
        let (scan, reused) = self.session_scan(None);
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
                    titles,
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
        if !observed.has_file_identity
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
                    rollout_notes: vec![format!(
                        "Codex could not verify the current rollouts: {}.",
                        compact_discovery_error(&error)
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
            let mut session = selected.session;
            if current_rollout::ids(&session.path).is_none()
                && legacy
                    .get(id)
                    .is_some_and(|files| files.contains(&session.path.as_path()))
            {
                invalid.without_metadata -= 1;
            }
            if row.is_some_and(current_rollout::CurrentRollout::required) {
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
            session.title = title_index.titles.get(&session.id).cloned();
            sessions.push(session);
        }
        if let Some(cache) = cache {
            cache.save();
        }
        let rollout_notes = listing_notes(
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
        CodexListing {
            sessions,
            rollout_notes,
            names: title_index,
            selected_files,
            requires_scan,
        }
    }

    fn find_session(&self, id: &str, source_path: Option<&Path>) -> Result<CodexSession> {
        let mut session = self.require_rollout(id, source_path)?.session;
        session.title = self.title_index().titles.get(id).cloned();
        Ok(session)
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
        let Some(home) = self.codex_home.as_deref() else {
            return Ok(None);
        };
        let Ok(canonical_home) = fs::canonicalize(home) else {
            return Ok(None);
        };
        let rows = self.current.rows(home, [id])?;
        let row = rows.get(id);
        if let Some(row) = row {
            match row.resolve(home, id, std::iter::empty(), false) {
                Ok(Some(file)) => return Ok(Some(file.into())),
                Err(error) if row.has_legacy_name() => return Err(error),
                _ => {}
            }
        }
        let mut invalid = InvalidHeaders::default();
        let (mut scan, cached) = self.scan_for_lookup(id, source_path, home, &canonical_home);
        if let Some(row) = row {
            if cached && row.required() {
                // A restored copy can appear outside the recorded directory between lookups.
                scan = self.session_scan(Some(&scan)).0;
            }
            if let Some(file) = row.resolve(home, id, scan.paths(id), scan.notes.is_empty())? {
                return Ok(Some(file.into()));
            }
        }
        anyhow::ensure!(
            !ambiguous_generations(&scan, id) || (row.is_none() && scan.canonical_generations(id)),
            CurrentRolloutUnavailable
        );
        if let Some(selected) =
            select_live_rollout(home, &canonical_home, scan.paths(id), &mut invalid)
        {
            return Ok(Some(selected));
        }
        if cached {
            // A remaining malformed sibling does not prove that the last valid file is still here.
            let (fresh, _) = self.session_scan(Some(&scan));
            if !fresh.paths(id).eq(scan.paths(id)) {
                anyhow::ensure!(
                    !ambiguous_generations(&fresh, id)
                        || (row.is_none() && fresh.canonical_generations(id)),
                    CurrentRolloutUnavailable
                );
                invalid = InvalidHeaders::default();
                if let Some(selected) =
                    select_live_rollout(home, &canonical_home, fresh.paths(id), &mut invalid)
                {
                    return Ok(Some(selected));
                }
            }
        }
        if !scan.notes.is_empty()
            && invalid.first_error.is_none()
            && let Some(path) = self.selector.find_exact(home, id, IdKind::Thread)?
        {
            return Ok(select_live_rollout(
                home,
                &canonical_home,
                std::iter::once(path.as_path()),
                &mut invalid,
            ));
        }
        invalid.first_error.map_or(Ok(None), Err)
    }

    fn ancestor_path(
        &self,
        id: &str,
        catalog: Option<&RolloutCatalog<'_>>,
    ) -> Result<Option<PathBuf>> {
        let Some(home) = self.codex_home.as_deref() else {
            return Ok(None);
        };
        let (scan, _) = self.session_scan(None);
        if let Some(path) =
            RolloutSelector::select(home, scan.ancestor_paths(home, id), id, IdKind::Rollout)?
        {
            return Ok(Some(path));
        }
        if let Some(catalog) = catalog {
            return catalog.find_unlisted_ancestor(id);
        }
        self.selector.find_exact(home, id, IdKind::Rollout)
    }

    fn index_session_in(
        &self,
        session: &SessionRef,
        source_path: Option<&Path>,
        full_read_bytes: u64,
        catalog: Option<&RolloutCatalog<'_>>,
    ) -> Result<IndexRead> {
        validate_provider(session, Provider::Codex)?;
        let native = self.find_session(&session.id, source_path)?;
        let changed = source_path.is_some_and(|path| path != native.path);
        let read = self.read_selected(native, |native, revision| {
            let selected_path = native.path.clone();
            let mut read = if native.needs_visible_projection() {
                let mut read = native.referenced_index(self, session, full_read_bytes, catalog)?;
                read.source_revision = SourceRevision {
                    missing_ancestor: read.source_revision.missing_ancestor,
                    ..revision
                };
                read
            } else {
                native.index(session, full_read_bytes, revision)?
            };
            read.observe_inherited_workspace(&selected_path)?;
            Ok(read)
        });
        if changed {
            read.context("selected Codex rollout changed during indexing")
        } else {
            read
        }
    }

    /// Finds user sessions created after a native fork command started.
    ///
    /// A fork starts a thread with its own rollout and records the source as its parent.
    /// A revert only replaces a rollout within an existing thread.
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
        let parent = Uuid::parse_str(&source.id).context("Codex fork parent must be a UUID")?;
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
        for directory in directories {
            let mut paths = Vec::new();
            let mut unreadable_dirs = 0;
            let mut depth_limited_dirs = 0;
            collect_jsonl(
                &directory,
                0,
                &mut paths,
                SCAN_LIMIT,
                &mut unreadable_dirs,
                &mut depth_limited_dirs,
                None,
            );
            for ((_, name), siblings) in physical_rollouts(&paths) {
                if current_rollout::ids(name).is_none_or(|(thread, rollout)| thread != rollout) {
                    continue;
                }
                let Some(session) = verified_fork(home, &siblings, parent)? else {
                    continue;
                };
                if session.id == source.id
                    || session.is_subagent
                    || session
                        .created_at
                        .is_none_or(|created_at| created_at < started_at)
                    || session
                        .project_path
                        .as_deref()
                        .is_none_or(|recorded| !paths_match(recorded, project))
                {
                    continue;
                }
                let current = self.find_session(&session.id, Some(&session.path))?;
                if current.observed_revision == session.observed_revision {
                    candidates.push(session);
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

fn physical_rollouts(files: &[PathBuf]) -> HashMap<(&Path, &Path), Vec<&Path>> {
    let mut rollouts = HashMap::<_, Vec<&Path>>::new();
    for path in files {
        if let (Some(directory), Some(name)) = (path.parent(), rollout_file::jsonl_file_name(path))
        {
            rollouts.entry((directory, name)).or_default().push(path);
        }
    }
    for siblings in rollouts.values_mut() {
        siblings.sort_unstable_by_key(|path| Reverse(rollout_file::representation_priority(path)));
    }
    rollouts
}

fn verified_fork(home: &Path, siblings: &[&Path], parent: Uuid) -> Result<Option<CodexSession>> {
    let mut read_error = None;
    for path in siblings {
        let Some(path) = rollout_identity::managed_path(home, path) else {
            continue;
        };
        match CodexSession::parse_observed_metadata(&path) {
            Ok(Some(session))
                if current_rollout::ids(&path).is_some_and(|(id, _)| id == session.id) =>
            {
                return Ok((session.forked_from_id == Some(parent)).then_some(session));
            }
            Ok(_) => {}
            Err(error) => {
                read_error.get_or_insert(error);
            }
        }
    }
    read_error.map_or(Ok(None), Err)
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
        let codex_home = std::env::var_os("CODEX_HOME")
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
            .or_else(Self::default_home);
        Self {
            rollout_cache: codex_home.as_deref().and_then(rollout_cache::default_path),
            selector: Arc::default(),
            current: Arc::new(CurrentRollouts::from_env()),
            codex_home,
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
    let mut unreadable_dirs = 0_usize;
    let mut depth_limited_dirs = 0_usize;
    let collect_limit = limit.saturating_add(1);
    collect_jsonl(
        &home.join("sessions"),
        SCAN_DEPTH,
        &mut files,
        collect_limit,
        &mut unreadable_dirs,
        &mut depth_limited_dirs,
        Some(&mut active_links),
    );
    let active_entries_complete = unreadable_dirs == 0
        && depth_limited_dirs == 0
        && files.len() <= limit
        && active_links.len() <= limit;
    if files.len() <= limit {
        collect_jsonl(
            &home.join("archived_sessions"),
            SCAN_DEPTH,
            &mut files,
            collect_limit,
            &mut unreadable_dirs,
            &mut depth_limited_dirs,
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
        files,
        candidates,
        notes,
        active_entries_complete,
        active_links,
        ids: OnceLock::new(),
        skipped_directories: unreadable_dirs != 0 || depth_limited_dirs != 0,
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
        sort_rollout_indices(indices, files, active);
    }
    candidates
}

fn sort_rollout_indices(indices: &mut [usize], files: &[PathBuf], active: &Path) {
    if indices
        .iter()
        .all(|&i| current_rollout::canonical_name(&files[i]))
    {
        indices.sort_unstable_by(|&a, &b| {
            rollout_identity::rollout_priority(&files[b], active)
                .cmp(&rollout_identity::rollout_priority(&files[a], active))
        });
    } else {
        indices.sort_unstable_by(|&a, &b| {
            rollout_priority(&files[b], active).cmp(&rollout_priority(&files[a], active))
        });
    }
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
    if paths.iter().all(|p| current_rollout::canonical_name(p)) {
        paths.sort_unstable_by(|a, b| {
            rollout_identity::rollout_priority(b, &active)
                .cmp(&rollout_identity::rollout_priority(a, &active))
        });
    } else {
        paths.sort_unstable_by(|a, b| {
            rollout_priority(b, &active).cmp(&rollout_priority(a, &active))
        });
    }
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
    tail.components().count() <= SCAN_DEPTH + 1
        && tail
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)))
        && path.is_file()
        && fs::canonicalize(path).is_ok_and(|actual| actual == canonical_home.join(relative))
}

fn uniquely_scoped_rollout(home: &Path, path: &Path) -> bool {
    let Ok(actual) = fs::canonicalize(path) else {
        return false;
    };
    // Nested Codex homes can discover the same file. The legacy catalog did not store its home.
    let mut homes = actual
        .ancestors()
        .skip(1)
        .take(SCAN_DEPTH + 1)
        .filter(|dir| {
            dir.file_name()
                .is_some_and(|name| name == "sessions" || name == "archived_sessions")
        })
        .filter_map(Path::parent);
    homes.next() == Some(home) && homes.next().is_none()
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
    unreadable_dirs: &mut usize,
    depth_limited_dirs: &mut usize,
    mut skipped_links: Option<&mut Vec<PathBuf>>,
) {
    if output.len() >= limit {
        return;
    }
    if fs::symlink_metadata(root).is_ok_and(|meta| meta.file_type().is_symlink()) {
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
                    unreadable_dirs,
                    depth_limited_dirs,
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
    history_base: HistoryBase,
    subagent_history_start: Option<u64>,
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
        let history_base = HistoryBase::from_header(&path, record);
        Some(Self {
            history_base,
            subagent_history_start: history_base::subagent_history_start(record),
            id: id.to_owned(),
            forked_from_id: string_at(payload, &[&["forked_from_id"]])
                .and_then(|id| Uuid::parse_str(id).ok()),
            title: None,
            path,
            observed_revision: Some(observed.revision),
            project_path: string_at(payload, &[&["cwd"]]).map(PathBuf::from),
            git_branch: string_at(payload, &[&["git", "branch"], &["git", "branch_name"]])
                .map(str::to_owned),
            cli_version: string_at(payload, &[&["cli_version"]]).map(str::to_owned),
            history_mode: if history_base.mode_is_unverified() {
                CodexHistoryMode::Unknown
            } else {
                CodexHistoryMode::from_payload(payload)
            },
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
            history_base: header.history_base,
            subagent_history_start: header.subagent_history_start,
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
            history_base: self.history_base,
            subagent_history_start: self.subagent_history_start,
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

    fn indexed_workspace(
        &self,
        index: &omnis_store::CodexWorkspaceIndex,
        cached_header: bool,
    ) -> (NativeSession, bool) {
        let mut session = self.native_session();
        let verified = !self.needs_visible_projection()
            && session.apply_codex_workspace_index(index, cached_header);
        (session, verified)
    }

    fn needs_visible_projection(&self) -> bool {
        self.history_base.is_some() || self.subagent_history_start.is_some()
    }

    fn native_header_is_usable(&self) -> Result<bool> {
        use rollout_identity::{HeaderEvidence, HistoryMode};

        let header = rollout_file::header(&self.path)?;
        Ok(header.first().is_some_and(|record| {
            match (HeaderEvidence::read(record, &self.id), self.history_mode) {
                (HeaderEvidence::Native(HistoryMode::Legacy), CodexHistoryMode::Legacy) => {
                    self.subagent_history_start.is_none()
                }
                (HeaderEvidence::Native(HistoryMode::Paginated), CodexHistoryMode::Paginated) => {
                    record["payload"]["cwd"].is_string()
                }
                _ => false,
            }
        }))
    }

    fn lineage(
        &self,
        adapter: &CodexAdapter,
        scope: HistoryScope,
        catalog: Option<&RolloutCatalog<'_>>,
    ) -> Result<Lineage> {
        self.history_base.resolve(
            &self.path,
            scope,
            self.subagent_history_start,
            adapter,
            catalog,
        )
    }

    fn check_saved_opener(&self, prior_title: &str, check: SavedOpenerCheck) -> Result<bool> {
        if self.needs_visible_projection() {
            return Ok(false);
        }
        let mut builder = self.event_builder();
        builder.plan_preview(None);
        builder.track_saved_opener(prior_title);
        let mut matched_source = false;
        let (events, _) = self.canonical_events_into(
            &Lineage::default(),
            builder,
            CodexHistory::preview(),
            None,
            |visit| {
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
            },
        )?;
        Ok(match check {
            SavedOpenerCheck::Withdrawal => events.rollback_title().withdraws(prior_title),
            _ => matched_source && events.confirms_saved_local_opener(prior_title),
        })
    }

    fn withdrew_search_title(&self, prior_title: &str) -> Result<bool> {
        let lineage = self.visible_leaf()?;
        let mut builder = self.event_builder();
        let mut history = CodexHistory::preview();
        let mut opener: Option<(u64, bool)> = None;
        let mut withdrew = false;
        let mut row = 0;
        let complete = rollout_file::visit_all_records(&self.path, |record| {
            row += 1;
            if history.records_seen == 0 {
                anyhow::ensure!(
                    record["type"] == "session_meta"
                        && string_at(&record, &[&["payload", "id"], &["payload", "session_id"]])
                            == Some(self.id.as_str()),
                    "selected Codex rollout has a different session header"
                );
            } else if record["type"] == "session_meta" {
                return Ok(false);
            }
            match lineage.leaf_record_visibility(&record, row) {
                Some(true) => {}
                Some(false) => return Ok(true),
                None => return Ok(false),
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
        let (events, directory, _) = self.canonical_events_with(&Lineage::default(), |visit| {
            rollout_file::visit(&self.path, visit)
        })?;
        Ok((events, directory))
    }

    fn canonical_events_with(
        &self,
        lineage: &Lineage,
        read: impl FnOnce(&mut dyn FnMut(Value, JsonLineSkips) -> Result<()>) -> Result<JsonLineSkips>,
    ) -> Result<(EventBuilder, Option<PathBuf>, bool)> {
        let mut builder = self.event_builder();
        builder.track_rollback_opener(true);
        let (mut events, history) = self.canonical_events_into(
            lineage,
            builder,
            CodexHistory::new(self.project_path.clone()),
            None,
            read,
        )?;
        let unknown = history.workspace_unknown() || history.settings.has_owned();
        let workspace_verified =
            history.settings.has_owned() || !history.workspace_history_incomplete;
        let project = history.settings.directory(history.project_path);
        if unknown && project.is_none() {
            events.mark_codex_workspace_unknown(self.updated_at);
        }
        Ok((events, project, workspace_verified))
    }

    fn canonical_events_into(
        &self,
        lineage: &Lineage,
        mut builder: EventBuilder,
        mut history: CodexHistory,
        workspace_record: Option<usize>,
        read: impl FnOnce(&mut dyn FnMut(Value, JsonLineSkips) -> Result<()>) -> Result<JsonLineSkips>,
    ) -> Result<(EventBuilder, CodexHistory)> {
        // Reserve the first position for the child creation directory during ancestor replay.
        if self.needs_visible_projection() {
            history.records_seen = 1;
        }
        let mut first_gap = None;
        let mut selected_project = None;
        let mut observe =
            |record: Value, mut skipped_before: JsonLineSkips, inherited: usize| -> Result<()> {
                if workspace_record == Some(history.records_seen + 1) {
                    selected_project =
                        string_at(&record, &[&["payload", "cwd"]]).map(PathBuf::from);
                }
                if builder.tracks_rollback_opener()
                    && !ordinary_rollback_record(&record, history.records_seen == 0)
                {
                    builder.discard_rollback_opener();
                }
                if skipped_before.total() + inherited > 0 && first_gap.is_none() {
                    first_gap = Some(builder.checkpoint());
                    builder.note_rollback_history_gap();
                }
                history.observe_workspace_gaps(skipped_before.workspace_gaps() + inherited);
                skipped_before.possible_settings += inherited;
                // The latest settings for this thread survive conversation rollback.
                history.settings.observe_gaps(skipped_before);
                history.settings.observe(&self.id, &record);
                history.push(&mut builder, &record)?;
                if history
                    .records_seen
                    .checked_rem(TOOL_COMPACTION_RECORD_INTERVAL)
                    == Some(0)
                {
                    history.compact(&mut builder);
                }
                Ok(())
            };
        let inherited =
            lineage.visit(|record, skipped| observe(record, JsonLineSkips::default(), skipped))?;
        let mut unreadable = lineage.visit_leaf(&self.id, read, |record, skipped| {
            observe(record, skipped, inherited)
        })?;
        history.observe_workspace_gaps(unreadable.workspace_gaps() + inherited);
        unreadable.possible_settings += inherited;
        unreadable.oversized += inherited;
        history.settings.observe_gaps(unreadable);
        if workspace_record.is_some() {
            history.project_path = selected_project.or_else(|| {
                (history.workspace_record == Some(1))
                    .then(|| self.project_path.clone())
                    .flatten()
            });
        }
        let prefix_end = first_gap.unwrap_or_else(|| builder.checkpoint());
        history.compact(&mut builder);
        history.source_prefix_end = if lineage.unavailable.is_some() || history.omitted_events > 0 {
            Some(0)
        } else {
            (unreadable.total() > 0).then_some(prefix_end)
        };
        if unreadable.total() != 0 {
            builder.note_rollback_history_gap();
        }
        if self.needs_visible_projection() || self.is_subagent {
            builder.discard_rollback_opener();
        }
        self.report_replay_omissions(&mut builder, &mut history, lineage, unreadable, prefix_end);
        Ok((builder, history))
    }

    fn report_replay_omissions(
        &self,
        builder: &mut EventBuilder,
        history: &mut CodexHistory,
        lineage: &Lineage,
        unreadable: JsonLineSkips,
        prefix_end: u64,
    ) {
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
        if let Some(reason) = lineage.unavailable {
            history.workspace_history_incomplete |=
                history.workspace_record.is_none_or(|record| record <= 1);
            builder.push(
                EventKind::ProviderEvent,
                json!({"omitted_events": 1, "native_history_unverified": true, "reason": reason}),
                self.updated_at,
                ReplayPolicy::HistoricalOnly,
                Some(if self.history_base.mode_is_unverified() {
                    "omnisession.codex_history_mode".to_owned()
                } else {
                    UNAVAILABLE_ANCESTRY.to_owned()
                }),
                None,
            );
        }
        for record_type in lineage.hidden_context_types() {
            builder.push(
                EventKind::ProviderEvent,
                json!({}),
                self.updated_at,
                ReplayPolicy::HistoricalOnly,
                Some(record_type.to_owned()),
                None,
            );
        }
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
            source_revision,
            source_complete,
        })
    }

    fn referenced_index(
        self,
        adapter: &CodexAdapter,
        session: &SessionRef,
        budget: u64,
        catalog: Option<&RolloutCatalog<'_>>,
    ) -> Result<IndexRead> {
        let mut lineage = self.lineage(adapter, HistoryScope::Visible, catalog)?;
        let (events, project, retained_all, workspace_verified) =
            lineage.project(adapter, catalog, |lineage| {
                self.index_lineage(lineage, budget)
            })?;
        let captured_at = self.updated_at.unwrap_or_else(Utc::now);
        let branch = self.branch_at(project.as_deref());
        Ok(IndexRead {
            native_session: (!self.is_subagent).then(|| self.native_session()),
            rollback_title: events.rollback_title(),
            workspace_projection: CodexWorkspaceProjection::Observed(
                project.clone().filter(|_| workspace_verified),
            ),
            snapshot: events.snapshot(session.clone(), self.title, project, branch, captured_at),
            source_complete: retained_all,
            source_revision: SourceRevision {
                missing_ancestor: lineage.missing_rollout,
                ..Default::default()
            },
        })
    }

    fn index_lineage(
        &self,
        lineage: &Lineage,
        budget: u64,
    ) -> Result<(EventBuilder, Option<PathBuf>, bool, bool)> {
        if self.exceeds_index_budget(lineage, budget)? {
            return self.reconstructed_lineage_preview(lineage, rollout_file::LIMITS);
        }
        let remaining = budget - lineage.inherited_bytes();
        let (events, project, workspace_verified) =
            self.canonical_events_with(lineage, |visit| {
                let mut reader = rollout_file::open(&self.path)?.take(remaining.saturating_add(1));
                let read = rollout_file::read_records(&mut reader, visit);
                anyhow::ensure!(reader.limit() != 0, "Codex rollout changed during indexing");
                read
            })?;
        Ok((events, project, true, workspace_verified))
    }

    fn exceeds_index_budget(&self, lineage: &Lineage, budget: u64) -> Result<bool> {
        if budget == u64::MAX {
            return Ok(false);
        }
        let Some(remaining) = budget.checked_sub(lineage.inherited_bytes()) else {
            return Ok(true);
        };
        if !rollout_file::compressed(&self.path) {
            return Ok(fs::metadata(&self.path)?.len() > remaining);
        }
        // Compressed file sizes do not measure the decoded delta. Stop as soon as it exceeds the budget.
        let mut reader = rollout_file::open(&self.path)?
            .take(remaining.min(MAX_STREAMED_TRANSCRIPT_FILE_SIZE) + 1);
        Ok(std::io::copy(&mut reader, &mut std::io::sink())? > remaining)
    }

    fn compressed_sample(self, session: &SessionRef, budget: u64) -> Result<IndexRead> {
        let captured_at = self.updated_at.unwrap_or_else(Utc::now);
        let (events, project, source_complete, workspace_reconstructed) =
            match rollout_file::sample(&self.path, budget, self.project_path.as_deref(), &self.id)
                .context("sampling compressed Codex rollout")?
            {
                rollout_file::Sample::Complete(bytes) => {
                    let (events, project, _) = self
                        .canonical_events_with(&Lineage::default(), |visit| {
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
                        JsonLinesPreview {
                            records,
                            omissions: unreadable,
                            sampled: omitted > 0,
                            prefix_records,
                        },
                        Some(omitted),
                        MAX_STREAMED_TRANSCRIPT_LINE_SIZE,
                    );
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
            source_revision: SourceRevision::default(),
        })
    }

    fn reconstructed_preview(
        &self,
        limits: crate::support::JsonLinesLimits,
    ) -> Result<(EventBuilder, Option<PathBuf>, bool)> {
        let lineage = self.visible_leaf()?;
        let (events, directory, _, verified) =
            self.reconstructed_lineage_preview(&lineage, limits)?;
        Ok((events, directory, verified))
    }

    fn reconstructed_lineage_preview(
        &self,
        lineage: &Lineage,
        limits: crate::support::JsonLinesLimits,
    ) -> Result<(EventBuilder, Option<PathBuf>, bool, bool)> {
        let stamp = || {
            lineage
                .source_paths(&self.path)
                .map(|path| fs::metadata(path).map(|metadata| RolloutStamp::new(&metadata)))
                .collect::<std::io::Result<Vec<_>>>()
        };
        let before = stamp()?;
        let per_edge = usize::try_from(MAX_PREVIEW_TAIL_SIZE)?;
        let mut builder = self.event_builder();
        builder.plan_preview(None);
        builder.track_rollback_opener(false);
        let (
            (selected, omitted, prefix_end),
            workspace_record,
            source_prefix_end,
            first_settings,
            first_title,
        ) = {
            let (plan, history) = self.canonical_events_into(
                lineage,
                builder,
                CodexHistory::preview(),
                None,
                |visit| rollout_file::visit_with_limits(&self.path, limits, visit),
            )?;
            (
                plan.select_preview(rollout_file::SAMPLE_RECORDS, per_edge),
                history.workspace_record,
                history.source_prefix_end,
                history.settings,
                plan.rollback_title(),
            )
        };
        let expected = selected.len();
        let mut builder = self.event_builder();
        builder.plan_preview(Some(selected.clone()));
        builder.track_rollback_opener(false);
        let (mut events, history) = self.canonical_events_into(
            lineage,
            builder,
            CodexHistory::preview(),
            workspace_record,
            |visit| rollout_file::visit_with_limits(&self.path, limits, visit),
        )?;
        if stamp()? != before
            || history.workspace_record != workspace_record
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
        let workspace_unknown = history.workspace_unknown();
        let project = history.settings.directory(history.project_path);
        let owned_settings = history.settings.has_owned();
        if project.is_none() && (workspace_unknown || owned_settings) {
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
            omitted == 0,
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
        let events = self.events_from_preview(preview, None, MAX_STREAMED_TRANSCRIPT_LINE_SIZE);
        Ok((events, directory, true))
    }

    fn events_from_preview(
        &self,
        preview: JsonLinesPreview,
        omitted: Option<usize>,
        line_limit: u64,
    ) -> EventBuilder {
        let mut builder = self.event_builder();
        let mut last_visible = None;
        let unread_ancestor = preview.records.iter().any(|record| {
            record["type"] == "session_meta"
                && HistoryBase::from_header(&self.path, record).is_some()
        });
        if !self.is_subagent
            && preview.prefix_records > 0
            && preview
                .records
                .iter()
                .enumerate()
                .all(|(i, record)| ordinary_rollback_record(record, i == 0))
        {
            builder.track_rollback_opener(false);
            builder.note_rollback_history_gap();
        }
        let mut records = preview.records.into_iter();
        for record in records.by_ref().take(preview.prefix_records) {
            push_record(&mut builder, &record, &mut last_visible);
        }
        let prefix_end = builder.checkpoint();
        for record in records {
            push_record(&mut builder, &record, &mut last_visible);
        }
        if unread_ancestor {
            report_unread_ancestor(&mut builder, self.updated_at);
        }
        builder.push_json_line_omissions(
            preview.omissions,
            line_limit,
            Some(prefix_end),
            self.updated_at,
        );
        self.finish_preview_prefix(
            &mut builder,
            if preview.sampled { omitted } else { Some(0) },
            prefix_end,
            preview.omissions.total() == 0 && !unread_ancestor,
        );
        builder
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
    source_prefix_end: Option<u64>,
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
            source_prefix_end: None,
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
            builder.note_rollback_history_gap();
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
    record["type"] == "session_meta"
        && !record["payload"]["history_base"].is_null()
        && !matches!(
            rollout_identity::HistoryMode::read(&record["payload"]["history_mode"]),
            Some(rollout_identity::HistoryMode::Legacy)
        )
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
        let current = self.title_index();
        current.complete
            && self
                .listing
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
                .is_none_or(|cached| Arc::ptr_eq(&cached.listing.names, &current))
    }

    fn native_session_name(&self, id: &str) -> NativeSessionName {
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
                    && Self::index_file_revision(path) == Some(*selected)
                {
                    return prior
                        .and_then(|revision| revision.file)
                        .is_some_and(|file| Some(file) != selected.file);
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
                Some(row) if row.required() => {
                    // A custom filename cannot identify the home of an older source receipt.
                    if row.has_legacy_name()
                        && !revision
                            .and_then(|revision| revision.file)
                            .is_some_and(|file| {
                                scan.paths(id).any(|path| {
                                    rollout_identity::discovery_path(home, path)
                                        && Self::index_file_id(path) == Some(file)
                                }) || row.recorded_file_is(home, file)
                            })
                    {
                        continue;
                    }
                    match row.resolve(home, id, scan.paths(id), scan.notes.is_empty()) {
                        Err(error) if error.is::<CurrentRolloutUnavailable>() => {
                            unavailable.insert(id.clone());
                        }
                        Err(error) if error.is::<current_rollout::CurrentRolloutUnconfirmed>() => {
                            if row.has_legacy_name() {
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
                None if ambiguous_generations(&scan, id) && !scan.canonical_generations(id) => {
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
        let Some(selected) = self.selected_rollout(id, None)? else {
            return Ok(None);
        };
        let mut session = selected.session;
        session.title = self.title_index().titles.get(id).cloned();
        let mut native = session.native_session();
        if self.use_workspace_index
            && !session.needs_visible_projection()
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
        let native = &selected.session;
        let path = &native.path;
        let home = self
            .codex_home
            .as_deref()
            .context("Codex home was not found")?;
        if !selected.from_registry
            && !current_launch::source_filename_selects(
                self,
                &session.id,
                path,
                native.history_mode == CodexHistoryMode::Paginated,
            )?
        {
            return Ok(NativeResumeSupport::ImportOnly);
        }
        if !native.native_header_is_usable()? {
            return Ok(NativeResumeSupport::ImportOnly);
        }
        if native.history_mode == CodexHistoryMode::Paginated
            && native.history_base.is_some()
            && !native
                .lineage(self, HistoryScope::ModelContext, None)?
                .provider_can_read(home, &self.session_scan(None).0)?
        {
            return Ok(NativeResumeSupport::ImportOnly);
        }
        let archived = self
            .codex_home
            .as_ref()
            .is_some_and(|home| path.starts_with(home.join("archived_sessions")));
        let compressed = rollout_file::compressed(path);
        Ok(
            if selected.shadowed
                || archived
                || path.extension().is_some_and(|ext| ext == "gz")
                || (compressed && fs::symlink_metadata(path.with_extension("")).is_ok())
            {
                NativeResumeSupport::ImportOnly
            } else {
                match selected.session.history_mode {
                    CodexHistoryMode::Paginated if current_rollout::canonical_name(path) => {
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
            if native.needs_visible_projection() {
                Ok(native
                    .referenced_index(self, session, u64::MAX, None)?
                    .snapshot)
            } else {
                native.read(session).map(|(snapshot, _)| snapshot)
            }
        })
    }

    fn preview_session(&self, session: &SessionRef) -> Result<omnis_ir::CanonicalSnapshot> {
        validate_provider(session, Provider::Codex)?;
        self.read_selected(self.find_session(&session.id, None)?, |native, _| {
            if native.needs_visible_projection() {
                Ok(native
                    .referenced_index(self, session, MAX_PREVIEW_TAIL_SIZE, None)?
                    .snapshot)
            } else {
                native.preview(session).map(|(snapshot, _, _)| snapshot)
            }
        })
    }

    fn index_batch(&self, _ids: &[&str]) -> Option<Box<crate::IndexSessionReader<'_>>> {
        let home = self.codex_home.as_deref()?;
        let (scan, cached) = self.session_scan(None);
        let mut catalog = RolloutCatalog::new(home);
        if !cached && scan.notes.is_empty() {
            catalog.fresh_scan = Some(scan);
        }
        Some(Box::new(move |session, hint, budget| {
            self.index_session_in(session, hint, budget, Some(&catalog))
        }))
    }

    fn available_index_sources(&self, requested: &HashSet<Uuid>) -> HashSet<Uuid> {
        self.codex_home
            .as_deref()
            .and_then(|home| self.selector.available_rollouts(home, requested).ok())
            .unwrap_or_default()
    }

    fn index_session(
        &self,
        session: &SessionRef,
        source_path: Option<&Path>,
        full_read_bytes: u64,
    ) -> Result<IndexRead> {
        self.index_session_in(session, source_path, full_read_bytes, None)
    }

    fn new_session_plan(&self, target: &LaunchTarget) -> Result<LaunchPlan> {
        Ok(LaunchPlan {
            program: "codex".to_owned(),
            args: target.prompt.iter().cloned().collect(),
            cwd: target.cwd.clone(),
            env: Vec::new(),
        })
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
        Ok(LaunchPlan {
            program: "codex".to_owned(),
            args,
            cwd: target.cwd.clone(),
            env: Vec::new(),
        })
    }

    fn codex_continuation_sqlite_home(
        &self,
        session: Option<&SessionRef>,
        cwd: &Path,
        overrides: &crate::CodexLaunchOverrides,
    ) -> Result<Option<PathBuf>> {
        if let Some(session) = session {
            return current_launch::verified_sqlite_home(self, session, cwd, overrides);
        }
        let home = self
            .codex_home
            .as_deref()
            .context("Codex home was not found")?;
        self.current.root_for_launch(home, cwd, overrides).map(Some)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;
    use proptest::prelude::*;

    #[test]
    fn exact_id_lookup_reaches_an_older_rollout_after_the_listing_limit() -> Result<()> {
        let home = tempfile::tempdir()?;
        let sessions = home.path().join("sessions");
        fs::create_dir(&sessions)?;
        let older = "10000000-0000-4000-8000-000000000001";
        let newer = "20000000-0000-4000-8000-000000000002";
        for (day, id) in [("01", older), ("02", newer)] {
            fs::write(
                sessions.join(format!("rollout-2026-01-{day}T00-00-00-{id}.jsonl")),
                json!({"type": "session_meta", "payload": {"id":id}}).to_string(),
            )?;
        }
        let adapter = CodexAdapter::with_root(home.path());
        *adapter.session_scan.lock().unwrap() = Some(Arc::new(scan_session_files(home.path(), 1)));
        let listed = adapter.list_sessions(None)?;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].session.id, newer);
        let known = adapter
            .find_native_session(older)?
            .expect("requested rollout");
        assert_eq!(known.session.id, older);
        Ok(())
    }

    #[test]
    fn indexed_workspace_cannot_restore_a_subagents_copied_directory() -> Result<()> {
        let home = tempfile::tempdir()?;
        let sessions = home.path().join("sessions");
        fs::create_dir(&sessions)?;
        let id = "019a3b1c-0000-7000-8000-000000000001";
        let path = sessions.join(format!("rollout-2026-09-15T10-00-00-{id}.jsonl"));
        for (mode_key, cutoff, expected) in [
            ("history_mode", None, "/copied"),
            ("history_mode", Some(3), "/creation"),
            ("history_mxde", Some(3), "/creation"),
        ] {
            let header = json!({"type":"session_meta","ordinal":0,"payload":{
                "id":id,"cwd":"/creation",(mode_key):"paginated","subagent_history_start_ordinal":cutoff
            }});
            let turn =
                |n, cwd: &str| json!({"type":"turn_context","ordinal":n,"payload":{"cwd":cwd}});
            fs::write(
                &path,
                format!("{header}\n{}\n{}\n", turn(1, "/copied"), turn(3, "/local")),
            )?;
            let session = CodexSession::parse_observed_metadata(&path)?.unwrap();
            let revision = session.observed_revision.unwrap();
            let cached = omnis_store::CodexWorkspace {
                source_revision: revision,
                project_path: Some(PathBuf::from("/copied")),
                git_branch: None,
            };
            let mut cache = omnis_store::CodexWorkspaceIndex::default();
            cache.verified.insert(revision.file.unwrap(), cached);
            assert_eq!(
                session
                    .indexed_workspace(&cache, true)
                    .0
                    .project_path
                    .as_deref(),
                Some(Path::new(expected))
            );
            let adapter = CodexAdapter::with_root(home.path());
            let snapshot = adapter.read_session(&SessionRef::new(Provider::Codex, id))?;
            assert_eq!(snapshot.workspace.current_dir, Path::new("/local"));
        }
        Ok(())
    }

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
            candidates: HashMap::new(),
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
