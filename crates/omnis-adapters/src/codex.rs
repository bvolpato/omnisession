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
        EventBuilder, JsonlRecord, MAX_STREAMED_TRANSCRIPT_FILE_SIZE, json_lines_prefix,
        json_lines_preview, omitted_images_text, parse_timestamp, paths_match, provider_executable,
        provider_file, provider_root, sort_sessions, string_at, validate_provider, value_at,
        visit_index_json_lines, visit_streamed_json_records,
    },
};

mod attachments;
mod checkpoint_wire;
mod completed_item;
mod completion_positions;
mod display_text;
mod message_mirrors;
mod model_cut;
use message_mirrors::{MessageIdentity, MessageMatch, MessageMirrors};
mod paginated_answers;
mod paginated_checkpoint;
mod paginated_rollback;
mod paginated_workspace;
mod rollout_cache;
mod rollout_lines;
mod tool_history;
mod tool_revisions;
mod turn_aliases;
mod wire_enums;

use paginated_answers::{PaginatedAnswers, ToolId};
use paginated_rollback::{InputPosition, LegacyPrivateInputs, PaginatedRollback, Withdrawal};
use rollout_cache::{RolloutCache, RolloutHeader, RolloutStamp};
use rollout_lines::{Admission, OrdinalGap, RolloutLines};
use tool_history::{NativeCalls, PendingOutcomes};
use tool_revisions::{DisplayRevisions, PreviousDisplay};
use turn_aliases::{MessageIds, TurnAliases};

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
            source_revision: None,
        }
    }

    fn canonical_events(&self) -> Result<(EventBuilder, Option<PathBuf>)> {
        let mut builder = self.event_builder();
        let mut history = CodexHistory::new(self.project_path.clone());
        let mut records_seen = 0_usize;
        let mut has_history_base = false;
        let oversized_records =
            visit_streamed_json_records(&self.path, MAX_STREAMED_TRANSCRIPT_FILE_SIZE, |line| {
                let (mut record, terminated) = match line {
                    JsonlRecord::Complete(value) => (value, true),
                    JsonlRecord::Unterminated(value) => (value, false),
                    JsonlRecord::Malformed => {
                        history.rollout_lines.malformed();
                        return Ok(());
                    }
                };
                if records_seen == 0 {
                    has_history_base = declares_history_base(&record);
                }
                history.push(&mut builder, &mut record, terminated)?;
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
        if has_history_base {
            self.report_unread_history_base(&mut builder);
        }
        history.finish(&mut builder);
        Ok((builder, history.project_path))
    }

    fn preview_events(&self) -> Result<EventBuilder> {
        const SAMPLE_RECORDS: usize = 1_024;
        self.events_from_records(json_lines_preview(&self.path, SAMPLE_RECORDS)?)
    }

    fn events_from_records(&self, records: Vec<Value>) -> Result<EventBuilder> {
        let mut builder = self.event_builder();
        let mut history = CodexHistory::new(self.project_path.clone());
        let has_history_base = records.first().is_some_and(declares_history_base);
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

fn declares_history_base(record: &Value) -> bool {
    record["type"] == "session_meta" && !record["payload"]["history_base"].is_null()
}

struct TurnCheckpoint {
    id: Option<String>,
    sequence: u64,
    request_sequence: u64,
    admission: Option<InputPosition>,
    project_path: Option<PathBuf>,
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
    model: Option<u64>,
    kind: CompletedKind,
    sequence: u64,
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
enum CompletionScope {
    Published,
    Historical(CompletedKind),
}

type CompletedTurn = HashMap<(CompletionScope, String), CompletedPosition>;

struct CodexHistory {
    project_path: Option<PathBuf>,
    items: CodexItems,
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
    display_revisions: DisplayRevisions,
    late_owners: HashMap<u64, u64>,
    display_positions: HashMap<u64, (u64, u64)>,
    omitted_tool_events: usize,
    omitted_events: usize,
}

impl CodexHistory {
    fn new(project_path: Option<PathBuf>) -> Self {
        Self {
            paginated_workspace: paginated_workspace::PaginatedWorkspace::new(project_path.clone()),
            project_path,
            items: CodexItems::default(),
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
            display_revisions: DisplayRevisions::default(),
            late_owners: HashMap::new(),
            display_positions: HashMap::new(),
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
            let first = builder.first_sequence();
            self.items.mirrors.discard_before(first);
            self.paginated_inputs.discard_before(first);
            self.paginated_answers.discard_before(first);
            self.withdrawn.retain(|seq, _| *seq >= first);
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
            project_path: self.project_path.clone(),
            tool_events: self.items.tool_events,
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
        self.items
            .outcomes
            .retain(|seq| builder.event_at_mut(seq).is_some());
        self.calls.retain(|seq| builder.event_at_mut(seq).is_some());
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
            self.project_path = checkpoint.project_path;
            self.prune_completed(builder);
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
    ) -> Result<()> {
        if self.source_ordinal == 0 {
            self.rollout_lines.start_segment(record);
            self.paginated = self.rollout_lines.paginated();
            self.items.tool_projection.paginated = self.paginated;
        }
        let disposition =
            self.rollout_lines
                .prepare(record, terminated, self.paginated.then_some(true));
        self.push_admitted(builder, record, disposition)
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
            Admission::Complete => self.push_verified(builder, record, index),
            Admission::ModelOnly => {
                let event_type = (record["type"] == "event_msg")
                    .then(|| record["payload"]["type"].as_str())
                    .flatten();
                self.consume_model_record(record, event_type, builder)?;
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
        if record_type == Some("session_meta") {
            self.observe_history_mode(payload, index, builder.checkpoint());
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
                && self
                    .items
                    .message
                    .as_ref()
                    .is_none_or(|message| message.kind != EventKind::MessageUser)
            {
                self.pending_turn = Some(self.checkpoint(builder));
            }
            if let Some(cwd) = string_at(record, &[&["payload", "cwd"]]) {
                if self.paginated {
                    self.paginated_workspace
                        .observe(turn_id.or(self.active_turn_id.as_deref()), Path::new(&cwd));
                }
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
            self.push_message_or_metadata(builder, record, event_type);
        }
        Ok(())
    }

    fn observe_history_mode(&mut self, payload: &Value, index: u64, sequence: u64) {
        if self.paginated && index > 0 {
            self.paginated_inputs.segment();
            self.paginated_workspace.new_segment(sequence);
            self.paginated_answers.segment();
        }
        self.paginated |= payload["history_mode"] == "paginated";
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
            self.project_path = self.paginated_workspace.surviving(
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
        }
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
            // A generated turn can remain open after the next raw input has arrived.
            // Use it only while its confirmed request is the most recent checkpoint.
            let turn = stamped.or(active).or_else(|| {
                let named = self.aliases.resolve(current?);
                let owner = self.named_turns.get(named)?;
                let completed = self.completed.get(named).is_some_and(|items| {
                    items
                        .get(&(CompletionScope::Published, id.to_owned()))
                        .is_some_and(|p| p.kind == CompletedKind::Tool)
                        || items.contains_key(&(
                            CompletionScope::Historical(CompletedKind::Tool),
                            id.to_owned(),
                        ))
                });
                (self.turns.last()?.sequence == *owner && !completed).then_some(named)
            });
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
                .push(id, turn.map(str::to_owned), sequence, owner);
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
            push_response_item(builder, payload, stamp, &mut self.items);
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
        let projection = self.completion_position(builder, &native_turn, id, kind, display);
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
        let mut late = CodexItems::default();
        let checkpoint = self.checkpoint(builder);
        let items = if owner.is_some() {
            &mut late
        } else {
            &mut self.items
        };
        let user = payload["item"]["type"] == "UserMessage";
        items.identity = MessageIdentity::completed(turn, id, user);
        let scoped = MessageIdentity::completed(&native_turn, id, user);
        items.tool_projection.paginated = self.paginated;
        items.tool_projection.peer = projection.peer;
        items.tool_projection.matched_turn = None;
        items.tool_turn = Some(native_turn.clone());
        items.tool_owner = self.named_turns.get(&native_turn).copied();
        items.cited_display = matches!(
            payload["item"]["memory_citation"],
            Value::Object(_) | Value::Array(_)
        );
        let message_id = match payload["item"]["type"].as_str() {
            Some("AgentMessage") => scoped.scoped(true),
            Some("UserMessage") => scoped.scoped(false),
            _ => None,
        };
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
        if let Some(owner) = owner.filter(|_| original.is_none()).or(item_owner) {
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
        self.remember_completion(turn, id, kind, projection, sequence, peer);
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
        if matches!(item["type"].as_str(), Some("Plan" | "AgentMessage")) {
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

fn contains_rollback_marker(path: &Path) -> Result<bool> {
    contains_rollback_marker_with_chunk(path, 1024 * 1024)
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
struct ToolProjection {
    paginated: bool,
    peer: Option<u64>,
    matched_turn: Option<String>,
}

#[derive(Default)]
struct CodexItems {
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
        if payload["item"]["type"] == "Plan"
            && let Some(text) = payload["item"]["text"].as_str()
        {
            self.push_plan(builder, turn, id, text, timestamp);
            return false;
        }
        push_completed_item(builder, &payload["item"], timestamp, self)
    }

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
        if let Some(fields) = payload.as_object_mut() {
            fields.remove("omnisession_codex_rollback");
            fields.remove("omnisession_codex_checkpoint");
            fields.remove("omnisession_codex_prior_display");
            fields.remove("omnisession_codex_active_replies");
            fields.remove("omnisession_codex_checkpoint_position");
        }
        let display = raw_type == "event_msg.item_completed";
        let result_id = result_id.filter(|id| {
            !id.is_empty()
                || self
                    .tool_turn
                    .as_deref()
                    .is_some_and(|turn| !is_implicit_turn(turn))
        });
        let matched = result_id.and_then(|id| {
            self.outcomes.take(
                id,
                display,
                if self.tool_projection.paginated {
                    self.tool_projection.peer
                } else {
                    self.update
                },
                self.tool_turn.as_deref(),
                self.tool_owner,
            )
        });
        let matched = matched.map(|entry| {
            self.tool_projection.matched_turn = entry.turn;
            entry.sequence
        });
        let mirror = self
            .tool_projection
            .paginated
            .then(|| self.tool_projection.peer.take().or(matched))
            .flatten();
        let original = if self.tool_projection.paginated {
            self.update
        } else {
            self.update.or(matched)
        };
        if let Some(sequence) = original
            && let Some(previous) = builder.event_at_mut(sequence)
        {
            let has_model = completion_positions::model_output(previous);
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
            self.finish_tool_projection(builder, sequence, display, mirror);
            return;
        }
        let sequence = builder.checkpoint();
        if let Some(id) = result_id.filter(|_| mirror.is_none()) {
            self.outcomes.push(
                id,
                display,
                sequence,
                self.tool_turn.clone(),
                self.tool_owner,
            );
        }
        if !matches!(kind, EventKind::PlanUpdated | EventKind::ProviderEvent) {
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
            completion_positions::pair(builder, slot, model)
        });
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
        compact_tool_value(payload, None),
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
        // A revision in the unsampled middle can invalidate a newer-looking item at the tail.
        let header = json_lines_prefix(&native.path, 1)?;
        if header.first().is_some_and(|row| {
            row["type"] == "session_meta" && row["payload"]["history_mode"] == "paginated"
        }) || contains_rollback_marker(&native.path)?
        {
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
