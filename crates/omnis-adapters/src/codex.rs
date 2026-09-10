use std::{
    collections::{HashMap, HashSet},
    fs,
    io::{ErrorKind, Read},
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
};

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use omnis_ir::{EventKind, Provider, ReplayPolicy, SessionRef};
use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::{
    LaunchPlan, LaunchTarget, NativeSession, ProviderAdapter, ProviderInstallation,
    support::{
        EventBuilder, executable, json_lines, json_lines_prefix, json_lines_preview,
        parse_timestamp, paths_match, provider_file, provider_root, sort_sessions, string_at,
        validate_provider, value_at, visit_json_lines,
    },
};

const SCAN_LIMIT: usize = 10_000;
const MAX_CANONICAL_TOOL_EVENTS: usize = 256;
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

#[derive(Clone, Debug)]
pub struct CodexAdapter {
    codex_home: Option<PathBuf>,
    session_scan: Arc<OnceLock<CodexFileScan>>,
    listing: Arc<OnceLock<CodexListing>>,
    titles: Arc<OnceLock<HashMap<String, String>>>,
}

impl CodexAdapter {
    #[must_use]
    pub fn with_root(codex_home: impl Into<PathBuf>) -> Self {
        Self {
            codex_home: Some(codex_home.into()),
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

    fn title_index(&self) -> &HashMap<String, String> {
        self.titles.get_or_init(|| {
            let Some(home) = self.codex_home.as_deref() else {
                return HashMap::new();
            };
            let Some(path) = provider_file(home, &home.join("session_index.jsonl")) else {
                return HashMap::new();
            };
            let mut rows: HashMap<String, (Option<DateTime<Utc>>, usize, String)> = HashMap::new();
            for (position, row) in json_lines(&path)
                .unwrap_or_default()
                .into_iter()
                .enumerate()
            {
                let Some(id) = string_at(&row, &[&["id"]]).filter(|id| Uuid::parse_str(id).is_ok())
                else {
                    continue;
                };
                let Some(title) = string_at(&row, &[&["thread_name"]]) else {
                    continue;
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
            }
            rows.into_iter()
                .map(|(id, (_, _, title))| (id, title))
                .collect()
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
        let titles = self.title_index();
        let mut sessions: HashMap<String, CodexSession> = HashMap::new();
        let mut subagents = 0_usize;
        let mut unreadable_files = 0_usize;
        let mut skipped_metadata = 0_usize;
        let mut first_unreadable: Option<String> = None;
        for path in &scan.files {
            match CodexSession::parse_metadata_path_result(path) {
                Ok(Some(mut session)) => {
                    if session.is_subagent {
                        subagents += 1;
                        continue;
                    }
                    if let Some(title) = titles.get(&session.id) {
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
        let sessions = sessions.into_values().collect::<Vec<_>>();
        CodexListing {
            notes: listing_notes(
                scan,
                sessions.len(),
                subagents,
                unreadable_files,
                skipped_metadata,
                first_unreadable.as_deref(),
            ),
            sessions,
        }
    }

    fn find_session(&self, id: &str) -> Result<CodexSession> {
        let path = self.find_session_path(id)?;
        let mut session = CodexSession::parse_metadata_path_result(&path)?
            .ok_or_else(|| anyhow!("Codex session `{id}` could not be parsed"))?;
        if let Some(title) = self.title_index().get(id) {
            session.title = Some(title.clone());
        }
        Ok(session)
    }

    fn find_session_metadata(&self, id: &str) -> Result<CodexSession> {
        let path = self.find_session_path(id)?;
        let mut session = CodexSession::parse_metadata_path(&path)
            .ok_or_else(|| anyhow!("Codex session `{id}` metadata could not be parsed"))?;
        if let Some(title) = self.title_index().get(id) {
            session.title = Some(title.clone());
        }
        Ok(session)
    }

    fn find_session_path(&self, id: &str) -> Result<PathBuf> {
        Uuid::parse_str(id).context("Codex session ID must be a UUID")?;
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
            .ok_or_else(|| anyhow!("Codex session `{id}` was not found"))
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

        let mut directories = HashSet::new();
        for timestamp in [started_at, Utc::now()] {
            directories.insert(
                home.join("sessions")
                    .join(timestamp.format("%Y/%m/%d").to_string()),
            );
        }

        let mut candidates = Vec::new();
        for directory in directories {
            let mut paths = Vec::new();
            let mut unreadable_dirs = 0;
            collect_jsonl(&directory, 0, &mut paths, SCAN_LIMIT, &mut unreadable_dirs);
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
        Self {
            codex_home: provider_root("CODEX_HOME", &[".codex"]),
            session_scan: Arc::default(),
            listing: Arc::default(),
            titles: Arc::default(),
        }
    }
}

fn scan_session_files(home: &Path, limit: usize) -> CodexFileScan {
    let mut files = Vec::new();
    let mut unreadable_dirs = 0_usize;
    let collect_limit = limit.saturating_add(1);
    collect_jsonl(
        &home.join("sessions"),
        5,
        &mut files,
        collect_limit,
        &mut unreadable_dirs,
    );
    if files.len() <= limit {
        collect_jsonl(
            &home.join("archived_sessions"),
            5,
            &mut files,
            collect_limit,
            &mut unreadable_dirs,
        );
    }
    let truncated = files.len() > limit;
    files.truncate(limit);
    let mut notes = Vec::new();
    if unreadable_dirs > 0 {
        notes.push(format!(
            "Codex skipped {unreadable_dirs} unreadable session director{}.",
            if unreadable_dirs == 1 { "y" } else { "ies" }
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
    let mut entries = entries
        .flatten()
        .filter_map(|entry| {
            let file_type = entry.file_type().ok()?;
            (!file_type.is_symlink()).then_some((entry.path(), file_type))
        })
        .collect::<Vec<_>>();
    entries.sort_by(|left, right| right.0.cmp(&left.0));
    for (path, file_type) in entries {
        if output.len() >= limit {
            break;
        }
        if file_type.is_dir() && depth > 0 {
            collect_jsonl(&path, depth - 1, output, limit, unreadable_dirs);
        } else if file_type.is_file()
            && path
                .extension()
                .is_some_and(|extension| extension == "jsonl")
        {
            output.push(path);
        }
    }
}

fn path_uuid(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let start = stem.len().checked_sub(36)?;
    let id = &stem[start..];
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
            .is_some(),
            created_at: parse_timestamp(record.get("timestamp")),
            updated_at,
        })
    }

    fn canonical_events(&self) -> Result<(EventBuilder, Option<PathBuf>)> {
        let mut builder = self.event_builder();
        let mut history = CodexHistory::new(self.project_path.clone());
        let mut records_seen = 0_usize;
        visit_json_lines(&self.path, |record| {
            history.push(&mut builder, &record)?;
            records_seen += 1;
            if records_seen.checked_rem(TOOL_COMPACTION_RECORD_INTERVAL) == Some(0) {
                history.omitted_tool_events +=
                    builder.retain_latest_tool_events(MAX_CANONICAL_TOOL_EVENTS);
            }
            Ok(())
        })?;
        let omitted_tool_events = history.omitted_tool_events
            + builder.retain_latest_tool_events(MAX_CANONICAL_TOOL_EVENTS);
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
        Ok((builder, history.project_path))
    }

    fn preview_events(&self) -> Result<EventBuilder> {
        const SAMPLE_RECORDS: usize = 1_024;
        Ok(self.events_from_records(json_lines_preview(&self.path, SAMPLE_RECORDS)?))
    }

    fn events_from_records(&self, records: Vec<Value>) -> EventBuilder {
        let mut builder = self.event_builder();
        let mut last_visible: Option<(EventKind, String, &'static str)> = None;
        for record in records {
            push_record(&mut builder, &record, &mut last_visible);
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
    tool_events: usize,
}

struct CodexHistory {
    project_path: Option<PathBuf>,
    last_visible: Option<(EventKind, String, &'static str)>,
    turns: Vec<TurnCheckpoint>,
    pending_turn: Option<TurnCheckpoint>,
    tool_events: usize,
    omitted_tool_events: usize,
}

impl CodexHistory {
    fn new(project_path: Option<PathBuf>) -> Self {
        Self {
            project_path,
            last_visible: None,
            turns: Vec::new(),
            pending_turn: None,
            tool_events: 0,
            omitted_tool_events: 0,
        }
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
                let retained = self
                    .turns
                    .len()
                    .saturating_sub(usize::try_from(count).unwrap_or(usize::MAX));
                if retained < self.turns.len() {
                    let checkpoint = self.turns.remove(retained);
                    self.turns.truncate(retained);
                    builder.truncate_from(checkpoint.sequence);
                    self.project_path = checkpoint.project_path;
                    self.tool_events = checkpoint.tool_events;
                    self.omitted_tool_events = self.omitted_tool_events.min(self.tool_events);
                }
                self.last_visible = None;
                self.pending_turn = None;
            }
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
            self.turns
                .push(self.pending_turn.take().unwrap_or(checkpoint));
        } else if record_type == Some("response_item")
            && record["payload"]["type"] != "message"
            && builder.checkpoint() > checkpoint.sequence
        {
            self.tool_events += 1;
        }
        Ok(())
    }
}

fn contains_rollback_marker(path: &Path) -> Result<bool> {
    const MARKER: &[u8] = b"thread_rolled_back";
    let mut file = fs::File::open(path)?;
    let mut buffer = vec![0_u8; 64 * 1024];
    let mut overlap = 0;
    loop {
        let read = file.read(&mut buffer[overlap..])?;
        if read == 0 {
            return Ok(false);
        }
        let length = overlap + read;
        if buffer[..length]
            .windows(MARKER.len())
            .any(|window| window == MARKER)
            || buffer[..length].windows(2).any(|window| window == b"\\u")
        {
            return Ok(true);
        }
        overlap = (MARKER.len() - 1).min(length);
        buffer.copy_within(length - overlap..length, 0);
    }
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
        let parts = payload
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|part| {
                matches!(
                    part.get("type").and_then(Value::as_str),
                    Some("input_text" | "output_text" | "text")
                )
            })
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>();
        let contextual = parts.iter().any(|text| is_contextual_user_text(text));
        let text = parts.join("\n");
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
    push_message_text(builder, kind, text, timestamp, "event_msg", last_visible)
}

impl ProviderAdapter for CodexAdapter {
    fn provider(&self) -> Provider {
        Provider::Codex
    }

    fn probe(&self) -> ProviderInstallation {
        ProviderInstallation {
            provider: Provider::Codex,
            installed: executable("codex").is_some()
                || self.codex_home.as_deref().is_some_and(Path::is_dir),
            executable: executable("codex"),
            data_root: self.codex_home.clone(),
        }
    }

    fn discovery_notes(&self) -> Vec<String> {
        self.listing().notes.clone()
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
            sessions.push(NativeSession {
                session: SessionRef::new(Provider::Codex, session.id.clone()),
                title: session.title.clone(),
                project_path: session.project_path.clone(),
                git_branch: session.git_branch.clone(),
                created_at: session.created_at,
                updated_at: session.updated_at,
                updated_at_approximate: false,
                event_count: 0,
                source_path: Some(session.path.clone()),
            });
        }
        sort_sessions(&mut sessions);
        Ok(sessions)
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
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;

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

        let file_root = temporary.path().join("not-a-directory");
        fs::write(&file_root, b"x").expect("file posing as Codex home");
        let unreadable = scan_session_files(&file_root, 10);
        assert!(unreadable.files.is_empty());
        assert!(
            unreadable
                .notes
                .iter()
                .any(|note| note.contains("unreadable session director"))
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
}
