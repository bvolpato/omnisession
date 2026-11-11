use std::{
    borrow::Cow,
    cmp::Reverse,
    collections::{BinaryHeap, HashMap, HashSet, VecDeque},
    ffi::OsString,
    fs,
    io::{ErrorKind, Read},
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
};

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, TimeDelta, Utc};
use omnis_ir::{CanonicalSnapshot, EventKind, GitState, Provider, ReplayPolicy, SessionRef};
use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::{
    LaunchPlan, LaunchTarget, NativeSession, NativeWorkspace, ProviderAdapter,
    ProviderInstallation,
    support::{
        EventBuilder, MAX_PREVIEW_TAIL_SIZE, MAX_PROVIDER_RECORDS,
        MAX_STREAMED_TRANSCRIPT_FILE_SIZE, json_lines_prefix, json_lines_preview,
        omitted_images_text, parse_timestamp, paths_match, provider_executable, provider_file,
        provider_root, sort_sessions, string_at, validate_provider, value_at,
        visit_filtered_streamed_json_lines, visit_index_json_lines, visit_json_lines_tail,
        visit_streamed_json_lines,
    },
};

mod rollout_cache;
mod workspace_catalog;

use rollout_cache::{RolloutCache, RolloutHeader, RolloutStamp};
use workspace_catalog::WorkspaceCatalog;

const SCAN_LIMIT: usize = 10_000;
const MAX_CANONICAL_TOOL_EVENTS: usize = 256;
// Rollouts stream without a record budget, so retained events and turn checkpoints keep the newest.
const MAX_CANONICAL_EVENTS: usize = 100_000;
const MAX_TOOL_STRING_CHARACTERS: usize = 32 * 1024;
const MAX_TOOL_ARRAY_ITEMS: usize = 256;
const TOOL_COMPACTION_RECORD_INTERVAL: usize = 1_024;
const COMPACT_USER_TOKEN_LIMIT: usize = 20_000;

#[derive(Clone, Debug, Default)]
struct CodexFileScan {
    files: Vec<PathBuf>,
    notes: Vec<String>,
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
    /// Rollout header cache file. Explicit roots leave it unset.
    rollout_cache: Option<PathBuf>,
    workspace_catalog: Option<Arc<WorkspaceCatalog>>,
    session_scan: Arc<OnceLock<CodexFileScan>>,
    listing: Arc<OnceLock<CodexListing>>,
    titles: Arc<OnceLock<TitleIndex>>,
}

impl CodexAdapter {
    #[must_use]
    pub fn with_root(codex_home: impl Into<PathBuf>) -> Self {
        Self {
            codex_home: Some(codex_home.into()),
            rollout_cache: None,
            workspace_catalog: None,
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

    fn session_files(&self) -> &[PathBuf] {
        &self.session_scan().files
    }

    fn session_scan(&self) -> &CodexFileScan {
        self.session_scan
            .get_or_init(|| self.discover_session_scan())
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
        let scan = self.session_scan();
        let title_index = self.title_index();
        let mut cache = self
            .rollout_cache
            .as_deref()
            .zip(self.codex_home.as_deref())
            .and_then(|(path, home)| RolloutCache::load(path, home));
        let mut sessions: HashMap<String, CodexSession> = HashMap::new();
        let mut subagents = 0_usize;
        let mut unreadable_files = 0_usize;
        let mut skipped_metadata = 0_usize;
        let mut first_unreadable: Option<String> = None;
        for path in &scan.files {
            match listed_session(path, cache.as_mut()) {
                Ok(Some(mut session)) => {
                    if session.is_subagent {
                        subagents += 1;
                        continue;
                    }
                    if let Some(title) = title_index.titles.get(&session.id) {
                        session.title = Some(title.clone());
                    }
                    let replace = sessions
                        .get(&session.id)
                        .is_none_or(|current| session.updated_at > current.updated_at);
                    if replace {
                        sessions.insert(session.id.clone(), session);
                    }
                }
                Ok(None) => skipped_metadata += 1,
                Err(error) => {
                    unreadable_files += 1;
                    if first_unreadable.is_none() {
                        first_unreadable = Some(compact_discovery_error(&error));
                    }
                }
            }
        }
        if let Some(cache) = cache {
            cache.save();
        }
        let sessions = sessions.into_values().collect::<Vec<_>>();
        let mut notes = listing_notes(
            scan,
            sessions.len(),
            subagents,
            unreadable_files,
            skipped_metadata,
            first_unreadable.as_deref(),
        );
        notes.extend(title_index.notes.iter().cloned());
        CodexListing { sessions, notes }
    }

    fn find_session(&self, id: &str) -> Result<CodexSession> {
        let path = self.find_session_path(id)?;
        let mut session = CodexSession::parse_metadata_path_result(&path)?
            .ok_or_else(|| anyhow!("Codex session `{id}` could not be parsed"))?;
        if let Some(title) = self.title_index().titles.get(id) {
            session.title = Some(title.clone());
        }
        Ok(session)
    }

    fn find_session_metadata(&self, id: &str) -> Result<CodexSession> {
        let path = self.find_session_path(id)?;
        let mut session = CodexSession::parse_metadata_path(&path)
            .ok_or_else(|| anyhow!("Codex session `{id}` metadata could not be parsed"))?;
        if let Some(title) = self.title_index().titles.get(id) {
            session.title = Some(title.clone());
        }
        Ok(session)
    }

    fn find_session_path(&self, id: &str) -> Result<PathBuf> {
        Uuid::parse_str(id).context("Codex session ID must be a UUID")?;
        self.session_path(id)
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
        let Some(path) = self.session_path(id) else {
            return Ok(None);
        };
        let records = json_lines_prefix(&path, 1)?;
        Ok(records
            .first()
            .filter(|record| record.get("type").and_then(Value::as_str) == Some("session_meta"))
            .and_then(|record| record.get("payload"))
            .and_then(|payload| string_at(payload, &[&["model_provider"]]))
            .map(str::to_owned))
    }

    fn session_path(&self, id: &str) -> Option<PathBuf> {
        self.session_files()
            .iter()
            .find(|path| path_uuid(path).as_deref() == Some(id))
            .cloned()
            .or_else(|| {
                self.discover_session_scan()
                    .files
                    .into_iter()
                    .find(|path| path_uuid(path).as_deref() == Some(id))
            })
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
            );
            for path in paths {
                let Some(session) = CodexSession::parse_metadata_path_result(&path)? else {
                    continue;
                };
                if session.id == source.id
                    || session.forked_from_id != Some(parent)
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

impl Default for CodexAdapter {
    fn default() -> Self {
        let codex_home = provider_root("CODEX_HOME", &[".codex"]);
        Self {
            rollout_cache: codex_home.as_deref().and_then(rollout_cache::default_path),
            workspace_catalog: codex_home
                .as_deref()
                .and_then(WorkspaceCatalog::new)
                .map(Arc::new),
            codex_home,
            session_scan: Arc::default(),
            listing: Arc::default(),
            titles: Arc::default(),
        }
    }
}

/// Reads a rollout's listing metadata, reusing the cached header while the rollout is unchanged.
fn listed_session(path: &Path, cache: Option<&mut RolloutCache>) -> Result<Option<CodexSession>> {
    let Some(cache) = cache else {
        return CodexSession::parse_metadata_path_result(path);
    };
    // Stat before reading, so a rollout written in between is parsed again next time.
    let Some((metadata, stamp)) = fs::metadata(path)
        .ok()
        .and_then(|metadata| RolloutStamp::new(&metadata).map(|stamp| (metadata, stamp)))
    else {
        return CodexSession::parse_metadata_path_result(path);
    };
    if let Some(cached) = cache.get(path, stamp) {
        let updated_at = metadata.modified().ok().map(DateTime::<Utc>::from);
        return Ok(cached
            .header
            .clone()
            .map(|header| CodexSession::from_header(path, header, updated_at, stamp)));
    }
    let session = CodexSession::parse_metadata_path_result(path)?;
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
    CodexFileScan { files, notes }
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
        } else if file_type.is_file()
            && Path::new(&name)
                .extension()
                .is_some_and(|extension| extension == "jsonl")
        {
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
    let stem = path.file_stem()?.to_str()?;
    let start = stem.len().checked_sub(36)?;
    let id = stem.get(start..)?;
    Uuid::parse_str(id).ok().map(|_| id.to_owned())
}

struct RolloutWorkspace {
    directory: Option<PathBuf>,
    rolled_back: bool,
    recent_records: Option<VecDeque<Value>>,
}

#[derive(Debug)]
struct CodexSession {
    stamp: Option<RolloutStamp>,
    id: String,
    title: Option<String>,
    path: PathBuf,
    project_path: Option<PathBuf>,
    git: GitState,
    forked_from_id: Option<Uuid>,
    cli_version: Option<String>,
    is_subagent: bool,
    created_at: Option<DateTime<Utc>>,
    updated_at: Option<DateTime<Utc>>,
}

impl CodexSession {
    fn parse_metadata_path(path: &Path) -> Option<Self> {
        Self::parse_metadata_path_result(path).ok().flatten()
    }

    fn parse_metadata_path_result(path: &Path) -> Result<Option<Self>> {
        let metadata = fs::metadata(path)?;
        let records = json_lines_prefix(path, 1)?;
        Ok(records
            .first()
            .and_then(|record| Self::parse_metadata_record(path.to_path_buf(), record, &metadata)))
    }

    fn parse_metadata_record(
        path: PathBuf,
        record: &Value,
        metadata: &fs::Metadata,
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
        let updated_at = metadata.modified().ok().map(DateTime::<Utc>::from);
        Some(Self {
            stamp: RolloutStamp::new(metadata),
            id: id.to_owned(),
            title: None,
            path,
            project_path: string_at(payload, &[&["cwd"]]).map(PathBuf::from),
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
            forked_from_id: string_at(payload, &[&["forked_from_id"]])
                .and_then(|parent| Uuid::parse_str(parent).ok()),
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
            updated_at,
        })
    }

    fn from_header(
        path: &Path,
        header: RolloutHeader,
        updated_at: Option<DateTime<Utc>>,
        stamp: RolloutStamp,
    ) -> Self {
        Self {
            stamp: Some(stamp),
            id: header.id,
            title: None,
            path: path.to_path_buf(),
            project_path: header.project_path,
            git: header.git,
            forked_from_id: header.forked_from_id,
            cli_version: header.cli_version,
            is_subagent: header.is_subagent,
            created_at: header.created_at,
            updated_at,
        }
    }

    fn header(&self) -> RolloutHeader {
        RolloutHeader {
            id: self.id.clone(),
            project_path: self.project_path.clone(),
            git: self.git.clone(),
            forked_from_id: self.forked_from_id,
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
            git_branch: self.git.branch.clone(),
            created_at: self.created_at,
            updated_at: self.updated_at,
            updated_at_approximate: false,
            event_count: 0,
            source_path: Some(self.path.clone()),
        }
    }

    fn canonical_events(&self) -> Result<(EventBuilder, Option<PathBuf>)> {
        let mut builder = self.event_builder();
        let mut history = CodexHistory::new(self.project_path.clone());
        let mut records_seen = 0_usize;
        let oversized_records =
            visit_streamed_json_lines(&self.path, MAX_STREAMED_TRANSCRIPT_FILE_SIZE, |record| {
                history.push(&mut builder, &record)?;
                records_seen += 1;
                if records_seen.checked_rem(TOOL_COMPACTION_RECORD_INTERVAL) == Some(0) {
                    history.compact(&mut builder);
                }
                Ok(())
            })?;
        history.restore_legacy_users(&mut builder);
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
        builder.push_oversized_record_notice(oversized_records, self.updated_at);
        let directory = if oversized_records > 0 {
            self.recent_workspace(false)?.directory
        } else {
            history.project_path
        };
        Ok((builder, directory))
    }

    fn workspace_metadata(&self, for_preview: bool) -> Result<RolloutWorkspace> {
        let size = fs::metadata(&self.path)?.len();
        if size > MAX_STREAMED_TRANSCRIPT_FILE_SIZE
            || (!for_preview && size > MAX_PREVIEW_TAIL_SIZE)
        {
            let recent = self.recent_workspace(for_preview)?;
            if size > MAX_STREAMED_TRANSCRIPT_FILE_SIZE || recent.directory.is_some() {
                return Ok(recent);
            }
        }
        let mut directory = self.project_path.clone();
        let mut rolled_back = false;
        let oversized = visit_filtered_streamed_json_lines(
            &self.path,
            MAX_STREAMED_TRANSCRIPT_FILE_SIZE,
            workspace_record_filter(),
            |record| {
                match record["type"].as_str() {
                    Some("turn_context") => {
                        if let Some(cwd) = string_at(&record, &[&["payload", "cwd"]]) {
                            directory = Some(PathBuf::from(cwd));
                        }
                    }
                    Some("event_msg") if record["payload"]["type"] == "thread_rolled_back" => {
                        rolled_back |= record["payload"]["num_turns"].as_u64() != Some(0);
                    }
                    _ => {}
                }
                Ok(())
            },
        )?;
        if oversized > 0 {
            return self.recent_workspace(for_preview);
        }
        Ok(RolloutWorkspace {
            directory,
            rolled_back,
            recent_records: None,
        })
    }

    fn recent_workspace(&self, keep_preview: bool) -> Result<RolloutWorkspace> {
        // A later turn_context establishes the directory even when an earlier record was unreadable.
        let mut directory = None;
        let mut records = VecDeque::new();
        let is_metadata = workspace_record_filter();
        visit_json_lines_tail(
            &self.path,
            MAX_PREVIEW_TAIL_SIZE,
            |line| keep_preview || is_metadata(line),
            |_, record| {
                if record["type"] == "event_msg"
                    && record["payload"]["type"] == "thread_rolled_back"
                {
                    if record["payload"]["num_turns"].as_u64() != Some(0) {
                        records.clear();
                        directory = None;
                    }
                } else {
                    if record["type"] == "turn_context"
                        && let Some(cwd) = string_at(&record, &[&["payload", "cwd"]])
                    {
                        directory = Some(PathBuf::from(cwd));
                    }
                    if keep_preview {
                        if records.len() == 1_024 {
                            records.pop_front();
                        }
                        records.push_back(record);
                    }
                }
            },
        )?;
        Ok(RolloutWorkspace {
            directory,
            rolled_back: false,
            recent_records: keep_preview.then_some(records),
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
        catalog: Option<&WorkspaceCatalog>,
        session: &SessionRef,
        events: EventBuilder,
        project_path: Option<PathBuf>,
        captured_at: DateTime<Utc>,
    ) -> CanonicalSnapshot {
        let git = self.git_at(project_path.as_deref());
        if let Some(catalog) = catalog {
            catalog.record(self, project_path.as_deref(), git.branch.as_deref());
        }
        let mut snapshot = events.snapshot(
            session.clone(),
            self.title.clone(),
            project_path,
            None,
            captured_at,
        );
        snapshot.workspace.git = git;
        snapshot
    }

    fn preview_events(&self) -> Result<(EventBuilder, Option<PathBuf>)> {
        const SAMPLE_RECORDS: usize = 1_024;
        let mut history = CodexHistory::new(self.project_path.clone());
        let mut builder = self.event_builder();
        for record in json_lines_preview(&self.path, SAMPLE_RECORDS)? {
            history.push(&mut builder, &record)?;
        }
        history.restore_legacy_users(&mut builder);
        Ok((builder, history.project_path))
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
    tool_events: usize,
}

struct CodexHistory {
    project_path: Option<PathBuf>,
    last_visible: Option<(EventKind, String, &'static str)>,
    // Logical order can differ from file order when a checkpoint supplies missing requests.
    turns: Vec<u64>,
    next_turn: u64,
    checkpoints: Vec<(u64, TurnCheckpoint)>,
    // Each entry identifies a genuine user message in the canonical event stream.
    requests: Vec<(u64, u64)>,
    pending_turn: Option<TurnCheckpoint>,
    tool_events: usize,
    omitted_tool_events: usize,
    omitted_events: usize,
}

impl CodexHistory {
    fn new(project_path: Option<PathBuf>) -> Self {
        Self {
            project_path,
            last_visible: None,
            turns: Vec::new(),
            next_turn: 0,
            checkpoints: Vec::new(),
            requests: Vec::new(),
            pending_turn: None,
            tool_events: 0,
            omitted_tool_events: 0,
            omitted_events: 0,
        }
    }

    /// Applies the retained tool event, event, and turn checkpoint caps.
    fn compact(&mut self, builder: &mut EventBuilder) {
        self.omitted_tool_events += builder.retain_latest_tool_events(MAX_CANONICAL_TOOL_EVENTS);
        self.omitted_events += builder.retain_latest_events(MAX_CANONICAL_EVENTS);
        let excess = self.turns.len().saturating_sub(MAX_CANONICAL_EVENTS);
        if excess > 0 {
            let evicted: HashSet<_> = self.turns.drain(..excess).collect();
            self.requests.retain(|(_, id)| !evicted.contains(id));
        }
        let first = builder
            .events()
            .first()
            .map_or(builder.checkpoint(), |e| e.sequence);
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
    }

    fn new_turn(&mut self) -> u64 {
        let id = self.next_turn;
        self.next_turn += 1;
        id
    }

    fn rollback(&mut self, builder: &mut EventBuilder, count: u64) {
        let keep = self
            .turns
            .len()
            .saturating_sub(usize::try_from(count).unwrap_or(usize::MAX));
        let discarded: HashSet<_> = self.turns.drain(keep..).collect();
        if discarded.is_empty() {
            return;
        }
        let mut ranges = Vec::new();
        let mut removed_tools = 0;
        let mut points = std::mem::take(&mut self.checkpoints).into_iter().peekable();
        while let Some((id, mut point)) = points.next() {
            if discarded.contains(&id) {
                if ranges.is_empty() {
                    self.project_path = point.project_path;
                }
                let (end, tools) = points
                    .peek()
                    .map_or((builder.checkpoint(), self.tool_events), |(_, next)| {
                        (next.sequence, next.tool_events)
                    });
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
        builder.retain_events(|e| survives(e.sequence));
        self.tool_events -= removed_tools;
        self.omitted_tool_events = self.omitted_tool_events.min(self.tool_events);
        self.requests.retain(|(sequence, _)| survives(*sequence));
    }

    fn checkpoint(&self, builder: &EventBuilder) -> TurnCheckpoint {
        TurnCheckpoint {
            sequence: builder.checkpoint(),
            project_path: self.project_path.clone(),
            tool_events: self.tool_events,
        }
    }

    fn push(&mut self, builder: &mut EventBuilder, record: &Value) -> Result<()> {
        let record_type = record.get("type").and_then(Value::as_str);
        let event_type = (record_type == Some("event_msg"))
            .then(|| string_at(record, &[&["payload", "type"]]))
            .flatten();
        if event_type == Some("thread_rolled_back") {
            let count = record["payload"]["num_turns"]
                .as_u64()
                .context("Codex rollback marker has no valid num_turns")?;
            if count > 0 {
                self.rollback(builder, count);
                self.last_visible = None;
                self.pending_turn = None;
            }
            return Ok(());
        }
        if record_type == Some("compacted") {
            self.push_compaction(builder, record);
            return Ok(());
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
                self.project_path = Some(PathBuf::from(cwd));
            }
        }
        let checkpoint = self.checkpoint(builder);
        if push_record(builder, record, &mut self.last_visible) {
            let id = self.new_turn();
            self.turns.push(id);
            if builder.checkpoint() > checkpoint.sequence {
                self.requests.push((builder.checkpoint() - 1, id));
            }
            self.checkpoints
                .push((id, self.pending_turn.take().unwrap_or(checkpoint)));
        } else if record_type == Some("response_item")
            && record["payload"]["type"] != "message"
            && builder.checkpoint() > checkpoint.sequence
        {
            self.tool_events += 1;
        }
        Ok(())
    }

    fn push_compaction(&mut self, builder: &mut EventBuilder, record: &Value) {
        let timestamp = parse_timestamp(record.get("timestamp"));
        let payload = &record["payload"];
        let summary = payload["message"]
            .as_str()
            .filter(|text| !text.trim().is_empty());
        let history = payload["replacement_history"].as_array();
        let legacy = payload["replacement_history"].is_null();
        let items = history.map_or(&[][..], Vec::as_slice);
        let opaque = items.iter().any(|item| {
            matches!(
                item["type"].as_str(),
                Some("compaction" | "context_compaction" | "compaction_summary")
            )
        });
        let positioned_summary = replacement_summary_index(items, summary);
        let has_summary = positioned_summary.is_some() || summary.is_some();

        // A replacement is a new context window. Earlier events remain available for recovery.
        let boundary = summary
            .filter(|_| !legacy && positioned_summary.is_none())
            .unwrap_or(
                "Codex replaced the preceding conversation with a compacted context window.",
            );
        let start = self.checkpoint(builder);
        builder.push(
            EventKind::CompactionCreated,
            json!({"summary": boundary}),
            timestamp,
            ReplayPolicy::Contextual,
            Some(
                if legacy {
                    "compacted.legacy"
                } else {
                    "compacted"
                }
                .to_owned(),
            ),
            None,
        );
        let replacement_start = builder.checkpoint();
        let mut replacement = Vec::new();
        self.last_visible = None;
        for (index, item) in items.iter().enumerate() {
            if index != 0 && index.is_multiple_of(TOOL_COMPACTION_RECORD_INTERVAL) {
                self.compact(builder);
            }
            let selected = positioned_summary == Some(index);
            if let Some(text) = replacement_summary(item, summary.filter(|_| selected)) {
                builder.push(
                    EventKind::MessageAssistant,
                    json!({"text": text}),
                    timestamp,
                    ReplayPolicy::Contextual,
                    Some(
                        if selected {
                            "compacted.summary"
                        } else {
                            "response_item.message"
                        }
                        .to_owned(),
                    ),
                    None,
                );
                continue;
            }
            let checkpoint = self.checkpoint(builder);
            let user_turn = push_response_item(builder, item, timestamp, &mut self.last_visible);
            if user_turn && builder.checkpoint() > checkpoint.sequence {
                replacement.push((builder.checkpoint() - 1, checkpoint));
            } else if item["type"] != "message" && builder.checkpoint() > checkpoint.sequence {
                self.tool_events += 1;
            }
        }
        self.register_replacement(builder, start, replacement);
        if let Some(text) = summary.filter(|_| legacy) {
            builder.push(
                EventKind::MessageAssistant,
                json!({"text":text}),
                timestamp,
                ReplayPolicy::Contextual,
                Some("compacted.summary".to_owned()),
                None,
            );
        }
        let missing_summary =
            !has_summary && (legacy || opaque || builder.checkpoint() == replacement_start);
        if missing_summary || (history.is_none() && !payload["replacement_history"].is_null()) {
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
        self.last_visible = None;
    }

    fn request_id(&self, sequence: u64) -> Option<u64> {
        let index = self
            .requests
            .binary_search_by_key(&sequence, |(seq, _)| *seq)
            .ok()?;
        Some(self.requests[index].1)
    }

    fn restore_legacy_users(&self, builder: &mut EventBuilder) {
        let events = builder.events();
        let Some(end) = events
            .iter()
            .rposition(|e| e.kind == EventKind::CompactionCreated)
        else {
            return;
        };
        if events[end].source.raw_record_type.as_deref() != Some("compacted.legacy") {
            return;
        }
        let start = events[..end]
            .iter()
            .rposition(explicit_compaction)
            .map_or(0, |i| i + 1);
        let mut users = VecDeque::new();
        let mut tokens = 0;
        for e in &events[start..=end] {
            if e.kind == EventKind::CompactionCreated {
                trim_legacy_users(&mut users, &mut tokens);
            } else if self.request_id(e.sequence).is_some()
                && let Some(text) = e.payload["text"].as_str()
            {
                users.push_back((e.event_id, Cow::Borrowed(text)));
                tokens += text.len().div_ceil(4);
            }
        }
        let timestamp = events[end].timestamp;
        let retained: Vec<_> = users
            .into_iter()
            .map(|(id, text)| (id, text.into_owned()))
            .collect();
        // Older checkpoints retain their summaries and original requests for recovery.
        // Emit copies only for the last surviving window after all rollbacks have been applied.
        builder.insert_after(end, |builder| {
            for (id, text) in retained {
                builder.push(
                    EventKind::MessageUser,
                    json!({"text":text}),
                    timestamp,
                    ReplayPolicy::Contextual,
                    Some("compacted.user".to_owned()),
                    Some(id),
                );
            }
        });
    }

    fn register_replacement(
        &mut self,
        builder: &EventBuilder,
        start: TurnCheckpoint,
        replacement: Vec<(u64, TurnCheckpoint)>,
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
        for (sequence, _) in replacement.iter().rev() {
            let matched = events
                .binary_search_by_key(sequence, |e| e.sequence)
                .ok()
                .and_then(|i| events[i].payload["text"].as_str())
                .and_then(|text| {
                    if let Some(positions) = originals.get(text) {
                        let skip = positions.partition_point(|(seq, _)| *seq >= before);
                        if let Some(point) = positions.get(skip) {
                            return Some(*point);
                        }
                    }
                    (*sequence == replacement[0].0)
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
                    .map(|((seq, _), id)| (seq, id.unwrap())),
            );
            return;
        }

        // Some original turns are missing. Use the replacement for their boundaries and
        // associate copies of known requests with the same turn in both parts of the file.
        let mut previous = std::mem::take(&mut self.turns).into_iter();
        let mut missing = Vec::new();
        let mut window_start = Some(self.pending_turn.take().unwrap_or(start));
        for ((sequence, point), matched) in replacement.into_iter().zip(owners) {
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
            self.requests.push((sequence, id));
            self.checkpoints
                .push((id, window_start.take().unwrap_or(point)));
        }
        self.turns.extend(previous);
        self.turns.append(&mut missing);
    }
}

fn explicit_compaction(event: &omnis_ir::OmniEvent) -> bool {
    event.kind == EventKind::CompactionCreated
        && event.source.raw_record_type.as_deref() != Some("compacted.legacy")
}

fn trim_legacy_users(users: &mut VecDeque<(Uuid, Cow<'_, str>)>, tokens: &mut usize) {
    // Codex retains 20,000 approximate tokens and replaces the middle of the oldest partial request.
    while *tokens > COMPACT_USER_TOKEN_LIMIT {
        let Some((_, text)) = users.front_mut() else {
            break;
        };
        let size = text.len().div_ceil(4);
        let other = *tokens - size;
        if other >= COMPACT_USER_TOKEN_LIMIT {
            users.pop_front();
            *tokens = other;
            continue;
        }
        let (prefix, suffix) = retained_user_edges(text, COMPACT_USER_TOKEN_LIMIT - other);
        let marker = format!("…{} tokens truncated…", *tokens - COMPACT_USER_TOKEN_LIMIT);
        if text[prefix.len()..text.len() - suffix.len()] != marker {
            *text = Cow::Owned(format!("{prefix}{marker}{suffix}"));
        }
        *tokens = other + text.len().div_ceil(4);
        break;
    }
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
    let kinds = &item["internal_chat_message_metadata_passthrough"]["content_item_kinds"];
    (!text.trim().is_empty() && (Some(text) == summary || kinds[0] == "compaction.summary"))
        .then_some(text)
}

fn workspace_record_filter() -> impl Fn(&[u8]) -> bool {
    let cwd = memchr::memmem::Finder::new(b"turn_context");
    let rollback = memchr::memmem::Finder::new(b"thread_rolled_back");
    let escape = memchr::memmem::Finder::new(b"\\u00");
    move |line| {
        cwd.find(line).is_some()
            || rollback.find(line).is_some()
            || escape
                .find_iter(line)
                .any(|index| escapes_ascii_letter(line.get(index + 4..index + 6)))
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

fn push_record(
    builder: &mut EventBuilder,
    record: &Value,
    last_visible: &mut Option<(EventKind, String, &'static str)>,
) -> bool {
    let timestamp = parse_timestamp(record.get("timestamp"));
    match record.get("type").and_then(Value::as_str) {
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
                | "encrypted_function_args"
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
        let catalog = self.workspace_catalog.as_deref();
        let raw_sessions = self.sessions();
        if raw_sessions.is_empty() {
            return Ok(Vec::new());
        }
        let observations = catalog.map(WorkspaceCatalog::list).unwrap_or_default();
        let mut sessions = Vec::new();
        for raw in raw_sessions {
            let mut session = raw.native_session();
            if let Some((catalog, observation)) = catalog.zip(observations.get(&raw.id)) {
                catalog.apply(raw, &mut session, observation);
            }
            if project.is_some_and(|project| {
                session
                    .project_path
                    .as_deref()
                    .is_none_or(|directory| !paths_match(directory, project))
            }) {
                continue;
            }
            sessions.push(session);
        }
        sort_sessions(&mut sessions);
        Ok(sessions)
    }

    /// Reads the one matching rollout, so subagent threads that listings hide stay reachable.
    fn find_native_session(&self, id: &str) -> Result<Option<NativeSession>> {
        if Uuid::parse_str(id).is_err() {
            return Ok(None);
        }
        let Some(path) = self.session_path(id) else {
            return Ok(None);
        };
        let Some(mut session) = CodexSession::parse_metadata_path_result(&path)? else {
            return Ok(None);
        };
        session.title = self.title_index().titles.get(id).cloned();
        let mut native = session.native_session();
        if let Some(catalog) = self.workspace_catalog.as_deref()
            && let Some(observation) = catalog.find(id)
        {
            catalog.apply(&session, &mut native, &observation);
        }
        Ok(Some(native))
    }

    fn read_session(&self, session: &SessionRef) -> Result<omnis_ir::CanonicalSnapshot> {
        validate_provider(session, Provider::Codex)?;
        let native = self.find_session(&session.id)?;
        let captured_at = native.updated_at.unwrap_or_else(Utc::now);
        let (events, project_path) = native.canonical_events()?;
        Ok(native.snapshot(
            self.workspace_catalog.as_deref(),
            session,
            events,
            project_path,
            captured_at,
        ))
    }

    fn preview_session(&self, session: &SessionRef) -> Result<omnis_ir::CanonicalSnapshot> {
        const PREVIEW_EVENTS: usize = 1_024;
        validate_provider(session, Provider::Codex)?;
        let native = self.find_session_metadata(&session.id)?;
        let captured_at = native.updated_at.unwrap_or_else(Utc::now);
        // A sampled preview can miss a directory change between its prefix and tail.
        let workspace = if fs::metadata(&native.path)?.len() < MAX_PROVIDER_RECORDS as u64 {
            RolloutWorkspace {
                directory: None,
                rolled_back: contains_rollback_marker(&native.path)?,
                recent_records: None,
            }
        } else {
            native.workspace_metadata(true)?
        };
        if let Some(records) = workspace.recent_records {
            let mut events = native.event_builder();
            let mut history = CodexHistory::new(None);
            for record in records {
                history.push(&mut events, &record)?;
            }
            history.restore_legacy_users(&mut events);
            events.push(EventKind::ProviderEvent,
                json!({"omitted_events":1, "reason":"Only the recent complete Codex records were available"}),
                Some(captured_at), ReplayPolicy::HistoricalOnly, Some("omnisession.preview_limit".to_owned()), None);
            return Ok(native.snapshot(
                self.workspace_catalog.as_deref(),
                session,
                events,
                workspace.directory,
                captured_at,
            ));
        }
        if workspace.rolled_back {
            let (mut events, project_path) = native.canonical_events()?;
            events.retain_preview_events(PREVIEW_EVENTS, captured_at);
            return Ok(native.snapshot(
                self.workspace_catalog.as_deref(),
                session,
                events,
                project_path,
                captured_at,
            ));
        }
        let (events, project_path) = native.preview_events()?;
        Ok(native.snapshot(
            self.workspace_catalog.as_deref(),
            session,
            events,
            workspace.directory.or(project_path),
            captured_at,
        ))
    }

    fn native_workspace(&self, session: &SessionRef) -> Result<Option<NativeWorkspace>> {
        validate_provider(session, Provider::Codex)?;
        let native = self.find_session_metadata(&session.id)?;
        let workspace = native.workspace_metadata(false)?;
        let directory = if workspace.rolled_back {
            native.canonical_events()?.1
        } else {
            workspace.directory
        };
        let git = native.git_at(directory.as_deref());
        if let Some(catalog) = self.workspace_catalog.as_deref() {
            catalog.record(&native, directory.as_deref(), git.branch.as_deref());
        }
        Ok(Some(NativeWorkspace { directory, git }))
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
