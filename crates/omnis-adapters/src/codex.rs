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
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    IndexRead, LaunchPlan, LaunchTarget, NativeResumeSupport, NativeSession, ProviderAdapter,
    ProviderInstallation,
    support::{
        EventBuilder, JsonLineSkips, MAX_PREVIEW_TAIL_SIZE, MAX_STREAMED_TRANSCRIPT_LINE_SIZE,
        MAX_TRANSCRIPT_LINE_SIZE, json_lines_preview_with_omissions, omitted_images_text,
        parse_timestamp, paths_match, provider_executable, provider_file, provider_root,
        sort_sessions, string_at, validate_provider, value_at, visit_index_json_lines,
    },
};

mod rollout_cache;
mod rollout_file;
mod workspace_search;

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
    files: Vec<PathBuf>,
    candidates: HashMap<String, Vec<usize>>,
    notes: Vec<String>,
}

impl CodexFileScan {
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
}

fn select_rollout<'a>(
    candidates: impl IntoIterator<Item = &'a Path>,
    mut read: impl FnMut(&Path) -> Result<Option<CodexSession>>,
    invalid: &mut InvalidHeaders,
) -> Option<SelectedRollout> {
    let mut shadowed = false;
    for path in candidates {
        match read(path) {
            Ok(Some(session)) => return Some(SelectedRollout { session, shadowed }),
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

#[derive(Debug)]
struct CodexListing {
    sessions: Vec<CodexSession>,
    notes: Vec<String>,
}

#[derive(Debug, Default)]
struct TitleIndex {
    titles: HashMap<String, String>,
    notes: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct CodexAdapter {
    codex_home: Option<PathBuf>,
    use_workspace_index: bool,
    /// Rollout header cache file. Explicit roots leave it unset.
    rollout_cache: Option<PathBuf>,
    session_scan: Arc<Mutex<Option<Arc<CodexFileScan>>>>,
    listing: Arc<OnceLock<CodexListing>>,
    titles: Arc<OnceLock<TitleIndex>>,
}

impl CodexAdapter {
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
            let (events, project) =
                native.reconstructed_preview(rollout_file::LEGACY_PROJECTION_LIMITS)?;
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
        validate_provider(session, Provider::Codex)?;
        anyhow::ensure!(
            path_uuid(path).as_deref() == Some(session.id.as_str()),
            "selected Codex rollout path has a different session ID"
        );
        if revision.contents.is_none() {
            return Ok(false);
        }
        let observed = ObservedRollout::read(path)
            .filter(|observed| observed.revision.matches_file_contents(revision))
            .context("selected Codex rollout changed before checking its search title")?;
        let native = CodexSession::parse_with_observation(path, observed)?
            .filter(|native| native.id == session.id)
            .context("selected Codex rollout has a different session header")?;
        native.read_unchanged(|native, _| native.withdrew_search_title(prior_title))
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

    fn listing(&self) -> &CodexListing {
        self.listing.get_or_init(|| self.build_listing())
    }

    fn sessions(&self) -> &[CodexSession] {
        &self.listing().sessions
    }

    fn build_listing(&self) -> CodexListing {
        let (scan, _) = self.session_scan(None);
        let title_index = self.title_index();
        let mut cache = self
            .rollout_cache
            .as_deref()
            .zip(self.codex_home.as_deref())
            .and_then(|(path, home)| RolloutCache::load(path, home));
        let mut sessions = Vec::new();
        let mut subagents = 0_usize;
        let mut invalid = InvalidHeaders {
            without_metadata: scan.files.len()
                - scan.candidates.values().map(Vec::len).sum::<usize>(),
            ..Default::default()
        };
        for indices in scan.candidates.values() {
            let paths = indices.iter().map(|&i| scan.files[i].as_path());
            if let Some(SelectedRollout { mut session, .. }) = select_rollout(
                paths,
                |path| listed_session(path, cache.as_mut()),
                &mut invalid,
            ) {
                if session.is_subagent {
                    subagents += 1;
                    continue;
                }
                session.title = title_index.titles.get(&session.id).cloned();
                sessions.push(session);
            }
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
        CodexListing { sessions, notes }
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
        let mut invalid = InvalidHeaders::default();
        let (scan, cached) = self.scan_for_lookup(id, source_path, home, &canonical_home);
        if let Some(selected) =
            select_live_rollout(home, &canonical_home, scan.paths(id), &mut invalid)
        {
            return Ok(Some(selected));
        }
        if cached {
            // A remaining malformed sibling does not prove that the last valid file is still here.
            let (fresh, _) = self.session_scan(Some(&scan));
            if !fresh.paths(id).eq(scan.paths(id)) {
                invalid = InvalidHeaders::default();
                if let Some(selected) =
                    select_live_rollout(home, &canonical_home, fresh.paths(id), &mut invalid)
                {
                    return Ok(Some(selected));
                }
            }
        }
        invalid.first_error.map_or(Ok(None), Err)
    }

    /// Finds user sessions created after a native fork command started.
    ///
    /// Codex does not currently persist a parent ID for ordinary CLI forks. This
    /// bounded read lets callers link a fork only when one new session matches
    /// the launch workspace. Multiple matches remain ambiguous.
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
                let session = selected.session;
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
                candidates.push(session);
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
}

fn apply_verified_workspace(
    session: &mut NativeSession,
    workspaces: &HashMap<Uuid, omnis_store::CodexWorkspace>,
    cached_header: bool,
) -> bool {
    if let Some(revision) = session.source_revision
        && let Some(workspace) = revision.file.and_then(|file| workspaces.get(&file))
        && revision.matches_file_contents(workspace.source_revision)
        && (!cached_header
            || session
                .source_path
                .as_deref()
                .and_then(CodexAdapter::index_file_revision)
                .is_some_and(|current| current.matches_file_contents(revision)))
    {
        session.apply_codex_workspace(workspace);
        return true;
    }
    false
}

impl Default for CodexAdapter {
    fn default() -> Self {
        let codex_home = provider_root("CODEX_HOME", &[".codex"]);
        Self {
            rollout_cache: codex_home.as_deref().and_then(rollout_cache::default_path),
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
    let mut unreadable_dirs = 0_usize;
    let mut depth_limited_dirs = 0_usize;
    let collect_limit = limit.saturating_add(1);
    collect_jsonl(
        &home.join("sessions"),
        5,
        &mut files,
        collect_limit,
        &mut unreadable_dirs,
        &mut depth_limited_dirs,
    );
    if files.len() <= limit {
        collect_jsonl(
            &home.join("archived_sessions"),
            5,
            &mut files,
            collect_limit,
            &mut unreadable_dirs,
            &mut depth_limited_dirs,
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
    unreadable_dirs: &mut usize,
    depth_limited_dirs: &mut usize,
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
    let stem = rollout_file::jsonl_file_name(path)?.file_stem()?.to_str()?;
    let start = stem.len().checked_sub(36)?;
    let id = stem.get(start..)?;
    Uuid::parse_str(id).ok().map(|_| id.to_owned())
}

#[derive(Debug)]
struct CodexSession {
    id: String,
    title: Option<String>,
    path: PathBuf,
    observed_revision: Option<SourceRevision>,
    project_path: Option<PathBuf>,
    git_branch: Option<String>,
    cli_version: Option<String>,
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
        if path_uuid(&path).as_deref() != Some(id) {
            return None;
        }
        Some(Self {
            id: id.to_owned(),
            title: None,
            path,
            observed_revision: Some(observed.revision),
            project_path: string_at(payload, &[&["cwd"]]).map(PathBuf::from),
            git_branch: string_at(payload, &[&["git", "branch"], &["git", "branch_name"]])
                .map(str::to_owned),
            cli_version: string_at(payload, &[&["cli_version"]]).map(str::to_owned),
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
            title: None,
            path: path.to_path_buf(),
            observed_revision: Some(observed.revision),
            project_path: header.project_path,
            git_branch: header.git_branch,
            cli_version: header.cli_version,
            is_subagent: header.is_subagent,
            created_at: header.created_at,
            updated_at: observed.modified,
        }
    }

    fn header(&self) -> RolloutHeader {
        RolloutHeader {
            id: self.id.clone(),
            project_path: self.project_path.clone(),
            git_branch: self.git_branch.clone(),
            cli_version: self.cli_version.clone(),
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
            ) {
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
        read: impl FnOnce(&mut dyn FnMut(Value, usize) -> Result<()>) -> Result<JsonLineSkips>,
    ) -> Result<(EventBuilder, Option<PathBuf>)> {
        let (events, history) = self.canonical_events_into(
            self.event_builder(),
            CodexHistory::new(self.project_path.clone()),
            read,
        )?;
        Ok((events, history.project_path))
    }

    fn canonical_events_into(
        &self,
        mut builder: EventBuilder,
        mut history: CodexHistory,
        read: impl FnOnce(&mut dyn FnMut(Value, usize) -> Result<()>) -> Result<JsonLineSkips>,
    ) -> Result<(EventBuilder, CodexHistory)> {
        let mut unread_ancestor = false;
        let mut first_gap = None;
        let unreadable = read(&mut |record, skipped_before| {
            if skipped_before > 0 && first_gap.is_none() {
                first_gap = Some(builder.checkpoint());
            }
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
        let prefix_end = first_gap.unwrap_or_else(|| builder.checkpoint());
        history.compact(&mut builder);
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
            report_unread_ancestor(&mut builder, self.updated_at);
        }
        Ok((builder, history))
    }

    fn branch_at(&self, project: Option<&Path>) -> Option<String> {
        (project == self.project_path.as_deref())
            .then(|| self.git_branch.clone())
            .flatten()
    }

    fn read(self, session: &SessionRef) -> Result<omnis_ir::CanonicalSnapshot> {
        let captured_at = self.updated_at.unwrap_or_else(Utc::now);
        let (events, project_path) = self.canonical_events()?;
        let branch = self.branch_at(project_path.as_deref());
        Ok(events.snapshot(
            session.clone(),
            self.title,
            project_path,
            branch,
            captured_at,
        ))
    }

    fn preview(self, session: &SessionRef) -> Result<omnis_ir::CanonicalSnapshot> {
        let captured_at = self.updated_at.unwrap_or_else(Utc::now);
        if rollout_file::compressed(&self.path) {
            return Ok(self
                .compressed_sample(session, MAX_PREVIEW_TAIL_SIZE)?
                .snapshot);
        }
        if contains_rollback_marker(&self.path)? {
            let (events, project_path) = self.reconstructed_preview(rollout_file::LIMITS)?;
            let branch = self.branch_at(project_path.as_deref());
            return Ok(events.snapshot(
                session.clone(),
                self.title,
                project_path,
                branch,
                captured_at,
            ));
        }
        Ok(self.preview_events()?.snapshot(
            session.clone(),
            self.title,
            self.project_path,
            self.git_branch,
            captured_at,
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
        let snapshot = if source_complete {
            self.read(session)?
        } else {
            self.preview(session)?
        };
        Ok(IndexRead {
            native_session,
            snapshot,
            source_complete,
            source_revision,
        })
    }

    fn compressed_sample(self, session: &SessionRef, budget: u64) -> Result<IndexRead> {
        let captured_at = self.updated_at.unwrap_or_else(Utc::now);
        let (events, project, source_complete) = match rollout_file::sample(&self.path, budget)
            .context("sampling compressed Codex rollout")?
        {
            rollout_file::Sample::Complete(bytes) => {
                let (events, project) = self.canonical_events_with(|visit| {
                    rollout_file::read_records(bytes.as_slice(), visit)
                })?;
                (events, project, true)
            }
            rollout_file::Sample::Reconstruct => {
                let (events, project) = self.reconstructed_preview(rollout_file::LIMITS)?;
                (events, project, false)
            }
            rollout_file::Sample::Edges {
                records,
                omitted,
                unreadable,
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
                (events, self.project_path.clone(), false)
            }
        };
        let branch = self.branch_at(project.as_deref());
        let snapshot = events.snapshot(session.clone(), self.title, project, branch, captured_at);
        Ok(IndexRead {
            native_session: None,
            snapshot,
            source_complete,
            source_revision: SourceRevision::default(),
        })
    }

    fn reconstructed_preview(
        &self,
        limits: crate::support::JsonLinesLimits,
    ) -> Result<(EventBuilder, Option<PathBuf>)> {
        let stamp = || fs::metadata(&self.path).map(|metadata| RolloutStamp::new(&metadata));
        let before = stamp()?;
        let per_edge = usize::try_from(MAX_PREVIEW_TAIL_SIZE)?;
        let mut builder = self.event_builder();
        builder.plan_preview(None);
        let ((selected, omitted), workspace_record) = {
            let (plan, history) =
                self.canonical_events_into(builder, CodexHistory::preview(), |visit| {
                    rollout_file::visit_with_limits(&self.path, limits, visit)
                })?;
            (
                plan.select_preview(rollout_file::SAMPLE_RECORDS, per_edge),
                history.workspace_record,
            )
        };
        let expected = selected.len();
        let mut builder = self.event_builder();
        builder.plan_preview(Some(selected.clone()));
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
            || events.select_preview(rollout_file::SAMPLE_RECORDS, per_edge) != (selected, omitted)
        {
            return Err(anyhow!(
                "Codex rollout changed while reconstructing its preview"
            ));
        }
        events.finish_preview(expected)?;
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
        Ok((events, project))
    }

    fn preview_events(&self) -> Result<EventBuilder> {
        const SAMPLE_RECORDS: usize = 1_024;
        let preview = json_lines_preview_with_omissions(&self.path, SAMPLE_RECORDS)?;
        let mut events = self.events_from_records(preview.records);
        events.push_json_line_omissions(
            preview.omissions,
            MAX_TRANSCRIPT_LINE_SIZE,
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
        Ok(events)
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
    workspace_record: Option<usize>,
    records_seen: usize,
    last_visible: Option<(EventKind, String, &'static str)>,
    turns: Vec<TurnCheckpoint>,
    pending_turn: Option<TurnCheckpoint>,
    tool_events: usize,
    omitted_tool_events: usize,
    omitted_events: usize,
}

impl CodexHistory {
    fn new(project_path: Option<PathBuf>) -> Self {
        Self {
            project_path,
            workspace_record: None,
            records_seen: 0,
            last_visible: None,
            turns: Vec::new(),
            pending_turn: None,
            tool_events: 0,
            omitted_tool_events: 0,
            omitted_events: 0,
        }
    }

    /// During preview, a checkpoint holds the position of the cwd record. The second pass reads the surviving path.
    fn preview() -> Self {
        Self {
            workspace_record: Some(1),
            ..Self::new(None)
        }
    }

    /// Applies the retained tool event, event, and turn checkpoint caps.
    fn compact(&mut self, builder: &mut EventBuilder) {
        self.omitted_tool_events += builder.retain_latest_tool_events(MAX_CANONICAL_TOOL_EVENTS);
        self.omitted_events += builder.retain_latest_events(MAX_CANONICAL_EVENTS);
        let excess = self.turns.len().saturating_sub(MAX_CANONICAL_EVENTS);
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
                let retained = self
                    .turns
                    .len()
                    .saturating_sub(usize::try_from(count).unwrap_or(usize::MAX));
                if retained < self.turns.len() {
                    let checkpoint = self.turns.remove(retained);
                    self.turns.truncate(retained);
                    builder.truncate_from(checkpoint.sequence);
                    truncated = Some(checkpoint.sequence);
                    self.project_path = checkpoint.project_path;
                    self.workspace_record = checkpoint.workspace_record;
                    self.tool_events = checkpoint.tool_events;
                    self.omitted_tool_events = self.omitted_tool_events.min(self.tool_events);
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
                && self
                    .last_visible
                    .as_ref()
                    .is_none_or(|(kind, _, _)| *kind != EventKind::MessageUser)
            {
                self.pending_turn = Some(self.checkpoint(builder));
            }
            if let Some(cwd) = string_at(record, &[&["payload", "cwd"]]) {
                if self.workspace_record.is_some() {
                    self.workspace_record = Some(self.records_seen);
                } else {
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

fn contains_rollback_marker(path: &Path) -> Result<bool> {
    contains_rollback_marker_with_chunk(path, 1024 * 1024)
}

fn contains_rollback_marker_with_chunk(path: &Path, chunk: usize) -> Result<bool> {
    const MARKER: &[u8] = b"thread_rolled_back";
    let marker = memchr::memmem::Finder::new(MARKER);
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
        self.listing().notes.clone()
    }

    fn discovery_is_complete(&self) -> bool {
        // A missing Codex home is not evidence that previously cached sessions were deleted.
        self.codex_home.as_deref().is_some_and(Path::is_dir) && self.listing().notes.is_empty()
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
            && let Some(file) = native.source_revision.and_then(|revision| revision.file)
            && let Ok(workspaces) = omnis_store::CodexWorkspace::read_index(Some(file))
        {
            apply_verified_workspace(&mut native, &workspaces, false);
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
            {
                NativeResumeSupport::ImportOnly
            } else if compressed {
                NativeResumeSupport::Since("0.137.0")
            } else {
                NativeResumeSupport::Direct
            },
        )
    }

    fn read_session(&self, session: &SessionRef) -> Result<omnis_ir::CanonicalSnapshot> {
        validate_provider(session, Provider::Codex)?;
        self.find_session(&session.id, None)?
            .read_unchanged(|native, _| native.read(session))
    }

    fn preview_session(&self, session: &SessionRef) -> Result<omnis_ir::CanonicalSnapshot> {
        validate_provider(session, Provider::Codex)?;
        self.find_session(&session.id, None)?
            .read_unchanged(|native, _| native.preview(session))
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
        let read = native
            .read_unchanged(|native, revision| native.index(session, full_read_bytes, revision));
        if changed {
            read.context("selected Codex rollout changed during indexing")
        } else {
            read
        }
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
                .sessions()
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
            candidates: HashMap::new(),
            notes: vec![
                "Codex scan stopped after 2 newest jsonl files; older sessions are omitted."
                    .to_owned(),
            ],
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
