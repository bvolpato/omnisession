use std::{
    cmp::Reverse,
    collections::{BinaryHeap, HashMap, HashSet},
    ffi::OsString,
    fs,
    io::{ErrorKind, Read},
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
};

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, TimeDelta, Utc};
use omnis_ir::{EventKind, Provider, ReplayPolicy, SessionRef};
use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::{
    LaunchPlan, LaunchTarget, NativeSession, ProviderAdapter, ProviderInstallation,
    support::{
        EventBuilder, MAX_STREAMED_TRANSCRIPT_FILE_SIZE, json_lines_prefix, json_lines_preview,
        omitted_images_text, parse_timestamp, paths_match, provider_executable, provider_file,
        provider_root, sort_sessions, string_at, validate_provider, value_at,
        visit_index_json_lines, visit_streamed_json_lines,
    },
};

mod completed_item;
mod display_text;
mod failed_attachments;
mod message_mirrors;
use message_mirrors::{MessageIdentity, MessageMatch, MessageMirrors, ScopedMessageId};
mod rollout_cache;
mod tool_history;

use rollout_cache::{RolloutCache, RolloutHeader, RolloutStamp};
use tool_history::{NativeCalls, PendingOutcomes};

const SCAN_LIMIT: usize = 10_000;
const MAX_CANONICAL_TOOL_EVENTS: usize = 256;
// Rollouts stream without a record budget, so retained events and turn checkpoints keep the newest.
const MAX_CANONICAL_EVENTS: usize = 100_000;
const MAX_TOOL_STRING_CHARACTERS: usize = 32 * 1024;
const MAX_TOOL_ARRAY_ITEMS: usize = 256;
const TOOL_COMPACTION_RECORD_INTERVAL: usize = 1_024;

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
            for path in paths {
                let Some(session) = CodexSession::parse_metadata_path_result(&path)? else {
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
            .map(|header| CodexSession::from_header(path, header, updated_at)));
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

#[derive(Debug)]
struct CodexSession {
    id: String,
    title: Option<String>,
    path: PathBuf,
    project_path: Option<PathBuf>,
    git_branch: Option<String>,
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
        let records = json_lines_prefix(path, 1)?;
        Ok(records
            .first()
            .and_then(|record| Self::parse_metadata_record(path.to_path_buf(), record)))
    }

    fn parse_metadata_record(path: PathBuf, record: &Value) -> Option<Self> {
        if record.get("type").and_then(Value::as_str) != Some("session_meta") {
            return None;
        }
        let payload = record.get("payload")?;
        let id = string_at(payload, &[&["id"], &["session_id"]])?;
        Uuid::parse_str(id).ok()?;
        if path_uuid(&path).as_deref() != Some(id) {
            return None;
        }
        let updated_at = fs::metadata(&path)
            .ok()
            .and_then(|metadata| metadata.modified().ok())
            .map(DateTime::<Utc>::from);
        Some(Self {
            id: id.to_owned(),
            title: None,
            path,
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
            updated_at,
        })
    }

    fn from_header(path: &Path, header: RolloutHeader, updated_at: Option<DateTime<Utc>>) -> Self {
        Self {
            id: header.id,
            title: None,
            path: path.to_path_buf(),
            project_path: header.project_path,
            git_branch: header.git_branch,
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
        Ok((builder, history.project_path))
    }

    fn preview_events(&self) -> Result<EventBuilder> {
        const SAMPLE_RECORDS: usize = 1_024;
        self.events_from_records(json_lines_preview(&self.path, SAMPLE_RECORDS)?)
    }

    fn events_from_records(&self, records: Vec<Value>) -> Result<EventBuilder> {
        let mut builder = self.event_builder();
        let mut history = CodexHistory::new(self.project_path.clone());
        for record in records {
            history.push(&mut builder, &record)?;
        }
        Ok(builder)
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
    project_path: Option<PathBuf>,
    tool_events: usize,
}

struct CodexHistory {
    project_path: Option<PathBuf>,
    items: CodexItems,
    turns: Vec<TurnCheckpoint>,
    pending_turn: Option<TurnCheckpoint>,
    active_turn_id: Option<String>,
    source_ordinal: u64,
    explicit_display_turn: bool,
    legacy_display: MessageIdentity,
    // Codex updates a completed item in place using its turn and item IDs.
    completed: HashMap<String, HashMap<String, u64>>,
    named_turns: HashMap<String, u64>,
    plan_turns: HashSet<String>,
    message_ids: HashMap<ScopedMessageId, u64>,
    calls: NativeCalls,
    late_owners: HashMap<u64, u64>,
    omitted_tool_events: usize,
    omitted_events: usize,
}

impl CodexHistory {
    fn new(project_path: Option<PathBuf>) -> Self {
        Self {
            project_path,
            items: CodexItems::default(),
            turns: Vec::new(),
            pending_turn: None,
            active_turn_id: None,
            source_ordinal: 0,
            explicit_display_turn: false,
            legacy_display: MessageIdentity::default(),
            completed: HashMap::new(),
            named_turns: HashMap::new(),
            plan_turns: HashSet::new(),
            message_ids: HashMap::new(),
            calls: NativeCalls::default(),
            late_owners: HashMap::new(),
            omitted_tool_events: 0,
            omitted_events: 0,
        }
    }

    /// Applies the retained tool event, event, and turn checkpoint caps.
    fn compact(&mut self, builder: &mut EventBuilder) {
        let tools_removed = builder.retain_latest_tool_events(MAX_CANONICAL_TOOL_EVENTS);
        let events_removed = builder.retain_latest_events(MAX_CANONICAL_EVENTS);
        self.omitted_tool_events += tools_removed;
        self.omitted_events += events_removed;
        if tools_removed > 0 || events_removed > 0 {
            self.prune_completed(builder);
            self.items.mirrors.discard_before(builder.first_sequence());
        }
        let excess = self.turns.len().saturating_sub(MAX_CANONICAL_EVENTS);
        if self.plan_turns.len() > MAX_CANONICAL_EVENTS {
            self.plan_turns.retain(|id| {
                self.named_turns.contains_key(id) || self.active_turn_id.as_deref() == Some(id)
            });
        }
        if excess > 0 {
            self.turns.drain(..excess);
            let earliest = self.turns[0].sequence;
            self.named_turns.retain(|_, seq| *seq >= earliest);
            self.plan_turns
                .retain(|id| self.named_turns.contains_key(id));
        }
    }

    fn checkpoint(&self, builder: &EventBuilder) -> TurnCheckpoint {
        TurnCheckpoint {
            id: self.active_turn_id.clone(),
            sequence: builder.checkpoint(),
            project_path: self.project_path.clone(),
            tool_events: self.items.tool_events,
        }
    }

    fn remember_turn(&mut self, checkpoint: TurnCheckpoint) {
        if let Some(id) = &checkpoint.id {
            self.named_turns.insert(id.clone(), checkpoint.sequence);
        }
        self.turns.push(checkpoint);
    }

    fn prune_completed(&mut self, builder: &mut EventBuilder) {
        self.items
            .outcomes
            .retain(|seq| builder.event_at_mut(seq).is_some());
        self.calls.retain(|seq| builder.event_at_mut(seq).is_some());
        self.message_ids
            .retain(|_, seq| builder.event_at_mut(*seq).is_some());
        self.completed.retain(|_, items| {
            items.retain(|_, sequence| builder.event_at_mut(*sequence).is_some());
            !items.is_empty()
        });
        self.late_owners
            .retain(|seq, _| builder.event_at_mut(*seq).is_some());
    }

    fn rollback(&mut self, builder: &mut EventBuilder, count: u64) {
        if count == 0 {
            return;
        }
        let retained = self
            .turns
            .len()
            .saturating_sub(usize::try_from(count).unwrap_or(usize::MAX));
        if retained < self.turns.len() {
            let checkpoint = self.turns.remove(retained);
            self.turns.truncate(retained);
            // A migrated turn can have both its original and generated ID.
            self.named_turns.retain(|_, seq| *seq < checkpoint.sequence);
            for turn in &self.turns {
                if let Some(id) = &turn.id {
                    self.named_turns.insert(id.clone(), turn.sequence);
                }
            }
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
            self.project_path = checkpoint.project_path;
            self.prune_completed(builder);
        }
        self.plan_turns
            .retain(|id| self.named_turns.contains_key(id));
        self.items.clear_mirrors();
        self.items.in_turn = false;
        self.explicit_display_turn = false;
        self.legacy_display = MessageIdentity::default();
        self.items.plan_mode = false;
        self.pending_turn = None;
        self.active_turn_id = None;
    }

    fn set_plan_mode(&mut self, turn: Option<&str>, plan: bool) {
        self.items.plan_mode = plan;
        if let Some(turn) = turn {
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
    }

    fn observe_context_turn(&mut self, turn: &str, sequence: u64) {
        if self.active_turn_id.as_deref() == Some(turn) {
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

    fn push(&mut self, builder: &mut EventBuilder, record: &Value) -> Result<()> {
        let index = self.source_ordinal;
        self.source_ordinal += 1;
        let record_type = record.get("type").and_then(Value::as_str);
        let payload = &record["payload"];
        let event_type = (record_type == Some("event_msg"))
            .then(|| payload["type"].as_str())
            .flatten();
        let turn_id = payload["turn_id"].as_str();
        if event_type == Some("thread_rolled_back") {
            let count = payload["num_turns"]
                .as_u64()
                .context("Codex rollback marker has no valid num_turns")?;
            self.rollback(builder, count);
            return Ok(());
        }
        if matches!(event_type, Some("task_started" | "turn_started")) {
            self.explicit_display_turn = true;
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
            .is_none_or(|(target, current)| target == current)
        {
            self.explicit_display_turn = false;
            self.legacy_display = MessageIdentity::default();
            self.pending_turn = None;
            self.active_turn_id = None;
            self.items.turn_boundary(turn_id);
            self.items.in_turn = false;
            self.items.plan_mode = false;
        }
        if record_type == Some("turn_context") {
            if let Some(mode) = payload["collaboration_mode"]["mode"].as_str() {
                self.set_plan_mode(turn_id, mode == "plan");
            } else if turn_id.is_some() && turn_id != self.current_turn_id() {
                self.items.plan_mode = turn_id.is_some_and(|turn| self.plan_turns.contains(turn));
            }
            if let Some(turn) = turn_id {
                self.observe_context_turn(turn, builder.checkpoint());
            }
            if self.pending_turn.is_none()
                && self
                    .items
                    .message
                    .as_ref()
                    .is_none_or(|message| message.kind != EventKind::MessageUser)
            {
                self.pending_turn = Some(self.checkpoint(builder));
            }
            if let Some(cwd) = string_at(record, &[&["payload", "cwd"]]) {
                self.project_path = Some(PathBuf::from(cwd));
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
            let checkpoint = self.checkpoint(builder);
            if self.push_record(builder, record) {
                let mut checkpoint = self.pending_turn.take().unwrap_or(checkpoint);
                // The migrator can emit the next raw request before closing a synthesized turn.
                if checkpoint
                    .id
                    .as_deref()
                    .is_some_and(|id| is_implicit_turn(id) && self.named_turns.contains_key(id))
                {
                    checkpoint.id = None;
                }
                self.remember_turn(checkpoint);
            }
        }
        Ok(())
    }

    fn push_record(&mut self, builder: &mut EventBuilder, record: &Value) -> bool {
        let payload = &record["payload"];
        if record["type"] != "response_item" || payload["type"] != "message" {
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
            return push_record(builder, record, &mut self.items);
        }
        let role = payload["role"].as_str();
        if !matches!(role, Some("assistant" | "user")) {
            return false;
        }
        let stamped = payload["internal_chat_message_metadata_passthrough"]["turn_id"]
            .as_str()
            .filter(|turn| !turn.is_empty());
        // A generated turn can close after the next raw message has already arrived.
        let turn = stamped.or_else(|| {
            self.active_turn_id
                .as_deref()
                .filter(|id| !is_implicit_turn(id))
        });
        let identity = MessageIdentity::new(turn, payload["id"].as_str(), false);
        let key = identity.scoped(role == Some("assistant"));
        let owner = stamped
            .filter(|turn| self.current_turn_id().is_some_and(|active| active != *turn))
            .and_then(|turn| self.named_turns.get(turn).copied());
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
        let previous_plan_mode = std::mem::replace(&mut items.plan_mode, plan_mode);
        items.identity = identity;
        items.cited_display = false;
        items.update = key.and_then(|key| self.message_ids.get(&key).copied());
        items.projected = None;
        let new_turn = push_record(builder, record, items);
        items.plan_mode = previous_plan_mode;
        if let Some(seq) = items.projected {
            if let Some(key) = key {
                self.message_ids.insert(key, seq);
            }
            if let Some(owner) = owner {
                self.late_owners.insert(seq, owner);
            }
        }
        items.update = None;
        items.identity = MessageIdentity::default();
        new_turn && owner.is_none()
    }

    fn push_model_tool(&mut self, builder: &mut EventBuilder, record: &Value) -> bool {
        if record["type"] != "response_item" {
            return false;
        }
        let payload = &record["payload"];
        let stamp = parse_timestamp(record.get("timestamp"));
        let stamped = payload["internal_chat_message_metadata_passthrough"]["turn_id"]
            .as_str()
            .filter(|turn| !turn.is_empty());
        let active = self
            .active_turn_id
            .as_deref()
            .filter(|turn| !is_implicit_turn(turn));
        if let Some(id) = model_call_id(payload) {
            let turn = stamped.or(active);
            let sequence = builder.checkpoint();
            let owner = turn
                .filter(|turn| self.current_turn_id() != Some(*turn))
                .and_then(|turn| self.named_turns.get(turn).copied());
            if let Some(owner) = owner {
                self.late_owners.insert(sequence, owner);
                let mut late = CodexItems::default();
                push_response_item(builder, payload, stamp, &mut late);
                self.items.tool_events += late.tool_events;
            } else {
                push_response_item(builder, payload, stamp, &mut self.items);
            }
            self.calls.push(id, turn.map(str::to_owned), sequence);
            return true;
        }
        let Some(id) = model_result_id(payload) else {
            return false;
        };
        let call = self.calls.take(id, stamped, active);
        let turn = stamped
            .or_else(|| call.as_ref()?.turn.as_deref())
            .or(active)
            .map(str::to_owned);
        let sequence = if let Some(turn) = &turn {
            self.push_identified_item(builder, payload, turn, id, stamp)
        } else {
            self.items.projected = None;
            push_response_item(builder, payload, stamp, &mut self.items);
            self.items.projected
        };
        if let Some(sequence) = sequence
            && let Some(call) = call.filter(|call| call.turn.is_none())
        {
            if let Some(owner) = turn.as_ref().and_then(|turn| self.named_turns.get(turn)) {
                self.late_owners.insert(call.sequence, *owner);
            } else {
                self.calls.linked.insert(sequence, call.sequence);
            }
        }
        true
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
        let original = self
            .completed
            .get(turn)
            .and_then(|items| items.get(id))
            .copied();
        let owner = self
            .named_turns
            .get(turn)
            .copied()
            .filter(|_| self.current_turn_id() != Some(turn));
        // A late result cannot act as a message or tool mirror in the current turn.
        let mut late = CodexItems::default();
        let checkpoint = self.checkpoint(builder);
        let items = if owner.is_some() {
            &mut late
        } else {
            &mut self.items
        };
        items.identity =
            MessageIdentity::completed(turn, id, payload["item"]["type"] == "UserMessage");
        items.tool_turn = Some(turn.to_owned());
        items.tool_owner = self.named_turns.get(turn).copied();
        items.cited_display = payload["item"]["memory_citation"].is_object();
        let message_id = match payload["item"]["type"].as_str() {
            Some("AgentMessage") => items.identity.scoped(true),
            Some("UserMessage") => items.identity.scoped(false),
            _ => None,
        };
        items.update =
            original.or_else(|| message_id.and_then(|key| self.message_ids.get(&key).copied()));
        items.projected = None;
        let new_turn = if display
            && !turn.is_empty()
            && !id.is_empty()
            && payload["item"]["type"] == "Plan"
            && let Some(text) = payload["item"]["text"].as_str()
        {
            items.push_plan(builder, turn, id, text, timestamp);
            false
        } else if display {
            push_completed_item(builder, &payload["item"], timestamp, items)
        } else {
            push_response_item(builder, payload, timestamp, items)
        };
        let projected = items.projected;
        items.tool_turn = None;
        items.tool_owner = None;
        items.update = None;
        items.identity = MessageIdentity::default();
        if owner.is_some() {
            self.items.tool_events += late.tool_events;
        }
        let sequence = projected?;
        if display
            && let Some(call) = self.calls.linked.remove(&sequence)
            && let Some(owner) = self.named_turns.get(turn)
        {
            self.late_owners.insert(call, *owner);
        }
        if let Some(key) = message_id {
            self.message_ids.insert(key, sequence);
        }
        let early_outcome = display
            .then(|| self.named_turns.get(turn).copied())
            .flatten()
            .filter(|boundary| {
                sequence < *boundary
                    && payload["item"]["type"] != "UserMessage"
                    && payload["item"]["type"] != "AgentMessage"
            });
        if let Some(owner) = owner.or(early_outcome) {
            self.late_owners.insert(sequence, owner);
        } else if new_turn {
            let mut checkpoint = self.pending_turn.take().unwrap_or(checkpoint);
            checkpoint.id = Some(turn.to_owned());
            self.remember_turn(checkpoint);
        } else if payload["item"]["type"] == "UserMessage" && original.is_none() {
            self.name_request_turn(turn, sequence);
        }
        self.completed
            .entry(turn.to_owned())
            .or_default()
            .insert(id.to_owned(), sequence);
        Some(sequence)
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
        checkpoint.id = Some(turn.to_owned());
        self.named_turns
            .insert(turn.to_owned(), checkpoint.sequence);
        if self
            .pending_turn
            .as_ref()
            .is_some_and(|pending| sequence < pending.sequence)
        {
            self.pending_turn = None;
        }
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

fn push_record(builder: &mut EventBuilder, record: &Value, items: &mut CodexItems) -> bool {
    let timestamp = parse_timestamp(record.get("timestamp"));
    match record.get("type").and_then(Value::as_str) {
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
struct CodexItems {
    message: Option<VisibleMessage>,
    mirrors: MessageMirrors,
    identity: MessageIdentity,
    cited_display: bool,
    plan_mode: bool,
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
    fn push_plan(
        &mut self,
        builder: &mut EventBuilder,
        turn: &str,
        id: &str,
        text: &str,
        timestamp: Option<DateTime<Utc>>,
    ) {
        self.message = None;
        let payload = json!({"type": "Plan", "turn_id": turn, "id": id, "text": text});
        if let Some(seq) = self.update
            && let Some(previous) = builder.event_at_mut(seq)
        {
            previous.kind = EventKind::MessageAssistant;
            previous.payload = payload;
            previous.replay_policy = ReplayPolicy::Contextual;
            previous.source.raw_record_type = Some("event_msg.item_completed".to_owned());
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

    fn push_historical(
        &mut self,
        builder: &mut EventBuilder,
        kind: EventKind,
        payload: Value,
        timestamp: Option<DateTime<Utc>>,
        raw_type: String,
        result_id: Option<&str>,
    ) {
        self.message = None;
        let display = raw_type == "event_msg.item_completed";
        let result_id = result_id.filter(|id| !id.is_empty());
        let matched = result_id.and_then(|id| {
            self.outcomes.take(
                id,
                display,
                self.update,
                self.tool_turn.as_deref(),
                self.tool_owner,
            )
        });
        let original = self.update.or(matched);
        if let Some(sequence) = original
            && let Some(previous) = builder.event_at_mut(sequence)
        {
            let has_model = previous
                .source
                .raw_record_type
                .as_deref()
                .is_some_and(|raw| raw.starts_with("response_item.") && raw.ends_with("_output"));
            let failed_display = previous.kind == EventKind::ToolFailed
                && (!has_model || model_result_kind(&previous.payload) != EventKind::ToolFailed);
            if display && has_model {
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
            self.projected = Some(sequence);
            return;
        }
        let sequence = builder.checkpoint();
        if let Some(id) = result_id {
            self.outcomes.push(
                id,
                display,
                sequence,
                self.tool_turn.clone(),
                self.tool_owner,
            );
        }
        self.projected = Some(sequence);
        if kind != EventKind::PlanUpdated {
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
    }
}

fn push_completed_item(
    builder: &mut EventBuilder,
    item: &Value,
    timestamp: Option<DateTime<Utc>>,
    items: &mut CodexItems,
) -> bool {
    let (kind, text_tag) = match item["type"].as_str() {
        Some("UserMessage") => (EventKind::MessageUser, "text"),
        Some("AgentMessage") => (EventKind::MessageAssistant, "Text"),
        _ => {
            if let Some((kind, payload)) = completed_item::historical_item(item) {
                let id = (kind != EventKind::PlanUpdated)
                    .then(|| item["id"].as_str())
                    .flatten();
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
        .filter(|part| part["type"] == text_tag)
        .filter_map(|part| part["text"].as_str())
        .collect::<Vec<_>>();
    let contextual = parts.iter().any(|part| is_contextual_user_text(part));
    let alternate = (parts.len() > 1).then(|| parts.join("\n"));
    let text = omitted_input_media(&kind, &parts, content).unwrap_or_else(|| parts.concat());
    if text.is_empty()
        && (kind != EventKind::MessageAssistant
            || parts.is_empty()
            || items.identity.scoped(true).is_none())
    {
        return false;
    }
    let attachment = (kind == EventKind::MessageUser)
        .then(|| failed_attachments::completed(content, &parts))
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
    let plan_display = (kind == EventKind::MessageAssistant
        && raw_type == "response_item.message"
        && items.plan_mode)
        .then(|| display_text::without_plan(matching.alternate.unwrap_or(text)))
        .flatten();
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
        && !(raw_type == "event_msg.item_completed"
            && kind == EventKind::MessageAssistant
            && items.identity.scoped(true).is_some())
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
    let matched = update.or_else(|| {
        items
            .mirrors
            .observe(&kind, text, matching, raw_type, builder.checkpoint())
    });
    let sequence = matched.unwrap_or_else(|| builder.checkpoint());
    items.message = Some(VisibleMessage {
        kind: kind.clone(),
        sequence,
    });
    items.projected = Some(sequence);
    if update.is_some() {
        items
            .mirrors
            .update(&kind, text, matching, raw_type, sequence);
    }
    if let Some(old) = matched.and_then(|seq| builder.event_at_mut(seq))
        && (update.is_some() || raw_type != "response_item.message")
    {
        // The display preserves the native text, including how it joins multipart messages.
        let priority = |wire| match wire {
            Some("event_msg.item_completed") => 2,
            Some("event_msg") => 1,
            _ => 0,
        };
        if priority(Some(raw_type)) < priority(old.source.raw_record_type.as_deref()) {
            return false;
        }
        old.kind = kind;
        old.payload = json!({ "text": text });
        old.replay_policy = ReplayPolicy::Contextual;
        old.source.raw_record_type = Some(raw_type.to_owned());
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
            .then(|| failed_attachments::model(content, &parts))
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
    let result_id = model_result_id(payload);
    items.push_historical(
        builder,
        kind,
        compact_tool_value(payload, None),
        timestamp,
        format!("response_item.{item_type}"),
        result_id,
    );
    false
}

fn model_call_id(payload: &Value) -> Option<&str> {
    match payload["type"].as_str()? {
        "function_call" | "custom_tool_call" | "web_search_call" | "computer_call"
        | "local_shell_call" => {
            string_at(payload, &[&["call_id"], &["id"]]).filter(|id| !id.is_empty())
        }
        _ => None,
    }
}

fn model_result_id(payload: &Value) -> Option<&str> {
    match payload["type"].as_str()? {
        "function_call_output"
        | "custom_tool_call_output"
        | "local_shell_call_output"
        | "computer_call_output" => {
            string_at(payload, &[&["call_id"], &["id"]]).filter(|id| !id.is_empty())
        }
        _ => None,
    }
}

fn model_result_kind(payload: &Value) -> EventKind {
    if matches!(payload["status"].as_str(), Some("failed" | "error")) {
        EventKind::ToolFailed
    } else {
        EventKind::ToolCompleted
    }
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
        .then(|| failed_attachments::legacy(payload, text))
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
        self.listing().notes.clone()
    }

    fn discovery_is_complete(&self) -> bool {
        // A missing Codex home is not evidence that previously cached sessions were deleted.
        self.codex_home.as_deref().is_some_and(Path::is_dir) && self.listing().notes.is_empty()
    }

    fn list_sessions(&self, project: Option<&Path>) -> Result<Vec<NativeSession>> {
        let mut sessions = Vec::new();
        for session in self.sessions() {
            if project.is_some_and(|project| {
                session
                    .project_path
                    .as_deref()
                    .is_none_or(|recorded| !paths_match(recorded, project))
            }) {
                continue;
            }
            sessions.push(session.native_session());
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
        Ok(Some(session.native_session()))
    }

    fn read_session(&self, session: &SessionRef) -> Result<omnis_ir::CanonicalSnapshot> {
        validate_provider(session, Provider::Codex)?;
        let native = self.find_session(&session.id)?;
        let captured_at = native.updated_at.unwrap_or_else(Utc::now);
        let (events, project_path) = native.canonical_events()?;
        Ok(events.snapshot(
            session.clone(),
            native.title,
            project_path,
            native.git_branch,
            captured_at,
        ))
    }

    fn preview_session(&self, session: &SessionRef) -> Result<omnis_ir::CanonicalSnapshot> {
        const PREVIEW_EVENTS: usize = 1_024;
        validate_provider(session, Provider::Codex)?;
        let native = self.find_session_metadata(&session.id)?;
        let captured_at = native.updated_at.unwrap_or_else(Utc::now);
        if contains_rollback_marker(&native.path)? {
            let (mut events, project_path) = native.canonical_events()?;
            events.retain_preview_events(PREVIEW_EVENTS, captured_at);
            return Ok(events.snapshot(
                session.clone(),
                native.title,
                project_path,
                native.git_branch,
                captured_at,
            ));
        }
        Ok(native.preview_events()?.snapshot(
            session.clone(),
            native.title,
            native.project_path,
            native.git_branch,
            captured_at,
        ))
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
