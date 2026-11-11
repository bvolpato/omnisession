mod branch;
mod input;

use std::{
    collections::{HashMap, HashSet},
    fs::File,
    io::{self, BufRead, BufReader, Read, Seek, SeekFrom},
    ops::Range,
    path::Path,
    path::PathBuf,
    sync::{Arc, LazyLock, Mutex, OnceLock, PoisonError},
    time::SystemTime,
};

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use memchr::memmem;
use omnis_ir::{EventKind, Provider, ReplayPolicy, SessionRef};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    LaunchPlan, LaunchTarget, NativeSession, ProviderAdapter, ProviderInstallation,
    support::{
        EventBuilder, ProviderFiles, nested_files_matching, omitted_images_text, parse_timestamp,
        paths_match, provider_executable, provider_file, provider_root, sample_files_concurrently,
        sort_sessions, string_at, validate_provider, value_at, visit_index_json_lines,
    },
};

const MAX_METADATA_RECORDS: usize = 32;
const MAX_METADATA_BYTES: u64 = 2 * 1024 * 1024;
// Legacy transcripts start with `summary` records. Current ones append title records near the end.
const TITLE_HEAD_BYTES: usize = 16 * 1024;
const TITLE_TAIL_BYTES: usize = 64 * 1024;
const MAX_PROMPT_TITLE_CHARACTERS: usize = 200;

#[derive(Clone, Debug)]
pub struct ClaudeAdapter {
    projects_root: Option<PathBuf>,
    session_files: Arc<OnceLock<Vec<(String, PathBuf)>>>,
    notes: Arc<Mutex<Vec<String>>>,
}

impl ClaudeAdapter {
    #[must_use]
    pub fn with_root(projects_root: impl Into<PathBuf>) -> Self {
        Self {
            projects_root: Some(projects_root.into()),
            session_files: Arc::default(),
            notes: Arc::default(),
        }
    }

    fn discover_session_files(&self) -> Vec<(String, PathBuf)> {
        let Some(root) = self.projects_root.as_deref() else {
            return Vec::new();
        };
        // Subagent logs and tool results outnumber transcripts, so skip them before the budgets apply.
        nested_files_matching(root, 8, &|path, is_dir| {
            if is_dir {
                return path
                    .file_name()
                    .is_none_or(|name| name != "subagents" && name != "tool-results");
            }
            path.extension()
                .is_some_and(|extension| extension == "jsonl")
                && path
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .is_some_and(|stem| Uuid::parse_str(stem).is_ok())
        })
        .into_iter()
        .filter_map(|path| Some((path.file_stem()?.to_str()?.to_owned(), path)))
        .collect()
    }

    fn session_files(&self) -> &[(String, PathBuf)] {
        self.session_files
            .get_or_init(|| self.discover_session_files())
    }

    fn find_session(&self, id: &str) -> Result<PathBuf> {
        Uuid::parse_str(id).context("Claude session ID must be a UUID")?;
        // Transcripts live at `<project>/<id>.jsonl`, so exact reads don't depend on discovery budgets.
        let direct = self.projects_root.as_deref().and_then(|root| {
            std::fs::read_dir(root)
                .ok()?
                .flatten()
                .filter(|entry| entry.file_type().is_ok_and(|file_type| file_type.is_dir()))
                .find_map(|entry| provider_file(root, &entry.path().join(format!("{id}.jsonl"))))
        });
        if let Some(path) = direct {
            return Ok(path);
        }
        self.session_files()
            .iter()
            .find_map(|(candidate, path)| (candidate == id).then(|| path.clone()))
            .or_else(|| {
                self.discover_session_files()
                    .into_iter()
                    .find_map(|(candidate, path)| (candidate == id).then_some(path))
            })
            .and_then(|path| {
                self.projects_root
                    .as_deref()
                    .and_then(|root| provider_file(root, &path))
            })
            .ok_or_else(|| anyhow!("Claude session `{id}` was not found"))
    }

    fn session_path(&self, id: &str, source_path: Option<&Path>) -> Result<PathBuf> {
        Uuid::parse_str(id).context("Claude session ID must be a UUID")?;
        let hinted = source_path.and_then(|source_path| {
            let root = self.projects_root.as_deref()?;
            let expected_name = format!("{id}.jsonl");
            if source_path.file_name()?.to_str()? != expected_name {
                return None;
            }
            provider_file(root, source_path)
        });
        hinted.map_or_else(|| self.find_session(id), Ok)
    }

    fn history_index(&self) -> ClaudeHistory {
        let Some(config_root) = self.projects_root.as_deref().and_then(Path::parent) else {
            return ClaudeHistory::default();
        };
        let Some(history) = provider_file(config_root, &config_root.join("history.jsonl")) else {
            return ClaudeHistory::default();
        };
        let mut sessions: HashMap<String, HistoryEntry> = HashMap::new();
        let scan = visit_index_json_lines(&history, |record| {
            let Some(id) = string_at(&record, &[&["sessionId"]]) else {
                return;
            };
            let Some(project) = string_at(&record, &[&["project"]]) else {
                return;
            };
            let entry = sessions.entry(id.to_owned()).or_default();
            // Later records update the workspace and latest activity. The prompt title and
            // creation time stay first.
            entry.project = PathBuf::from(project);
            if let Some(timestamp) = parse_timestamp(record.get("timestamp")) {
                entry.created_at = Some(
                    entry
                        .created_at
                        .map_or(timestamp, |first| first.min(timestamp)),
                );
                entry.updated_at = Some(
                    entry
                        .updated_at
                        .map_or(timestamp, |latest| latest.max(timestamp)),
                );
            }
            if entry.first_prompt.is_none() {
                entry.first_prompt = string_at(&record, &[&["display"]]).and_then(prompt_title);
            }
        });
        let notes = match scan {
            Ok(scan) => scan.notes("Claude history.jsonl"),
            Err(error) => vec![format!("Claude history.jsonl could not be read: {error}.")],
        };
        ClaudeHistory { sessions, notes }
    }
}

impl Default for ClaudeAdapter {
    fn default() -> Self {
        Self {
            projects_root: provider_root("CLAUDE_CONFIG_DIR", &[".claude"])
                .map(|root| root.join("projects")),
            session_files: Arc::default(),
            notes: Arc::default(),
        }
    }
}

#[derive(Default)]
struct HistoryEntry {
    project: PathBuf,
    created_at: Option<DateTime<Utc>>,
    updated_at: Option<DateTime<Utc>>,
    first_prompt: Option<String>,
}

#[derive(Default)]
struct ClaudeHistory {
    sessions: HashMap<String, HistoryEntry>,
    notes: Vec<String>,
}

/// Turns a history prompt into a one-line title.
fn prompt_title(display: &str) -> Option<String> {
    let title = display.split_whitespace().collect::<Vec<_>>().join(" ");
    (!title.is_empty()).then(|| title.chars().take(MAX_PROMPT_TITLE_CHARACTERS).collect())
}

#[derive(Default)]
struct ClaudeMetadata {
    project_path: Option<PathBuf>,
    git_branch: Option<String>,
    created_at: Option<DateTime<Utc>>,
    updated_at: Option<DateTime<Utc>>,
}

/// Title records seen so far. A custom title wins over the latest generated one.
#[derive(Default)]
struct TitleCandidates {
    custom: Option<String>,
    generated: Option<String>,
}

impl TitleCandidates {
    fn observe(&mut self, record: &Value) {
        let generated = match record.get("type").and_then(Value::as_str) {
            Some("summary") => string_at(record, &[&["summary"], &["title"]]),
            Some("ai-title") => string_at(record, &[&["aiTitle"]]),
            _ => None,
        };
        if let Some(title) = generated {
            self.generated = Some(title.to_owned());
        }
        if let Some(title) = string_at(record, &[&["customTitle"]]) {
            self.custom = Some(title.to_owned());
        }
    }

    fn title(self) -> Option<String> {
        self.custom.or(self.generated)
    }
}

/// Title record keys: `"summary"`, plus `Title"` for `"aiTitle"` and `"customTitle"`.
static TITLE_KEYS: LazyLock<[memmem::Finder<'static>; 2]> = LazyLock::new(|| {
    [
        memmem::Finder::new(b"\"summary\""),
        memmem::Finder::new(b"Title\""),
    ]
});

/// Byte filter that skips parsing sampled lines which cannot hold a title.
fn may_carry_title(line: &[u8]) -> bool {
    TITLE_KEYS.iter().any(|key| key.find(line).is_some())
}

/// Observes title records in file order, parsing only lines that contain a title key.
fn observe_titles(titles: &mut TitleCandidates, records: &[u8]) {
    let mut starts = TITLE_KEYS
        .iter()
        .flat_map(|key| key.find_iter(records))
        .map(|position| {
            memchr::memrchr(b'\n', &records[..position]).map_or(0, |newline| newline + 1)
        })
        .collect::<Vec<_>>();
    starts.sort_unstable();
    starts.dedup();
    for start in starts {
        let line = &records[start..];
        let line = memchr::memchr(b'\n', line).map_or(line, |end| &line[..end]);
        if let Ok(record) = serde_json::from_slice::<Value>(line) {
            titles.observe(&record);
        }
    }
}

/// Replaces `buffer` with the bytes in `[start, end)`.
fn read_window(file: &mut File, start: u64, end: u64, buffer: &mut Vec<u8>) -> io::Result<()> {
    buffer.clear();
    file.seek(SeekFrom::Start(start))?;
    file.by_ref()
        .take(end.saturating_sub(start))
        .read_to_end(buffer)?;
    Ok(())
}

struct ClaudeEvent {
    kind: EventKind,
    payload: Value,
    timestamp: Option<DateTime<Utc>>,
    replay_policy: ReplayPolicy,
    raw_type: Option<String>,
    event_id: Option<Uuid>,
}

struct ClaudeRecord {
    branch: branch::Parent,
    parent: Option<Uuid>,
    events: Range<usize>,
}

struct PreservedContext {
    boundary: Uuid,
    anchor: Uuid,
    records: Vec<Uuid>,
}

fn compaction_preserved_context(
    boundary: &Value,
    records: &HashMap<Uuid, ClaudeRecord>,
) -> Result<Option<PreservedContext>> {
    let metadata = &boundary["compactMetadata"];
    let uuid = |value: Option<&Value>| {
        value
            .and_then(Value::as_str)
            .and_then(|value| Uuid::parse_str(value).ok())
            .ok_or_else(|| anyhow!("retained compaction context contains an invalid UUID"))
    };
    if let Some(preserved) = metadata
        .get("preservedMessages")
        .filter(|value| !value.is_null())
    {
        let anchor = uuid(preserved.get("anchorUuid"))?;
        let records = preserved
            .get("uuids")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("retained compaction context has no message list"))?
            .iter()
            .map(|value| uuid(Some(value)))
            .collect::<Result<Vec<_>>>()?;
        return Ok(Some(PreservedContext {
            boundary: uuid(boundary.get("uuid"))?,
            anchor,
            records,
        }));
    }
    let Some(segment) = metadata
        .get("preservedSegment")
        .filter(|value| !value.is_null())
    else {
        return Ok(None);
    };
    let anchor = uuid(segment.get("anchorUuid"))?;
    let head = uuid(segment.get("headUuid"))?;
    let mut current = uuid(segment.get("tailUuid"))?;
    let mut retained = Vec::new();
    let mut visited = HashSet::new();
    while visited.insert(current) {
        retained.push(current);
        let record = records
            .get(&current)
            .ok_or_else(|| anyhow!("retained compaction context references a missing record"))?;
        if current == head {
            retained.reverse();
            return Ok(Some(PreservedContext {
                boundary: uuid(boundary.get("uuid"))?,
                anchor,
                records: retained,
            }));
        }
        current = record
            .parent
            .ok_or_else(|| anyhow!("retained compaction context has a broken parent chain"))?;
    }
    Err(anyhow!(
        "retained compaction context has a cyclic parent chain"
    ))
}

fn claude_message_kind(record: &Value, role: Option<&str>) -> Option<EventKind> {
    if record.get("isCompactSummary").and_then(Value::as_bool) == Some(true) {
        return Some(EventKind::CompactionCreated);
    }
    match role {
        Some("user") => Some(EventKind::MessageUser),
        Some("assistant") => Some(EventKind::MessageAssistant),
        _ => None,
    }
}

impl ClaudeMetadata {
    fn observe(&mut self, record: &Value) {
        if let Some(cwd) = string_at(record, &[&["cwd"]]) {
            self.project_path = Some(PathBuf::from(cwd));
        }
        if let Some(branch) = string_at(record, &[&["gitBranch"]]) {
            self.git_branch = Some(branch.to_owned());
        }
        let timestamp = parse_timestamp(record.get("timestamp"));
        if let Some(timestamp) = timestamp {
            self.created_at = Some(
                self.created_at
                    .map_or(timestamp, |current| current.min(timestamp)),
            );
            self.updated_at = Some(
                self.updated_at
                    .map_or(timestamp, |current| current.max(timestamp)),
            );
        }
    }
}

/// Bounded transcript sample for discovery. Discovery never scans a whole transcript.
struct TranscriptSample {
    file: File,
    length: u64,
    modified: Option<SystemTime>,
    head_end: u64,
    workspace: ClaudeMetadata,
    titles: TitleCandidates,
}

impl TranscriptSample {
    /// Samples the head. `buffer` is reused across transcripts to avoid per-file allocations.
    fn head(path: &Path, need_workspace: bool, buffer: &mut Vec<u8>) -> Result<Self> {
        let file = File::open(path)?;
        let metadata = file.metadata()?;
        let mut sample = Self {
            file,
            length: metadata.len(),
            modified: metadata.modified().ok(),
            head_end: 0,
            workspace: ClaudeMetadata::default(),
            titles: TitleCandidates::default(),
        };
        if need_workspace {
            sample.scan_workspace_head()?;
        } else {
            sample.read_head_metadata(buffer)?;
        }
        Ok(sample)
    }

    /// Scans up to 32 records within 2 MiB for the first `cwd`, observing title records among the
    /// scanned records. The tail sample starts after them, so none are skipped.
    fn scan_workspace_head(&mut self) -> Result<()> {
        let mut reader = BufReader::new((&mut self.file).take(MAX_METADATA_BYTES));
        let mut line = Vec::new();
        for _ in 0..MAX_METADATA_RECORDS {
            line.clear();
            let read = u64::try_from(reader.read_until(b'\n', &mut line)?)?;
            if read == 0 {
                break;
            }
            let title_line = may_carry_title(&line);
            self.head_end += read;
            let Ok(record) = serde_json::from_slice::<Value>(&line) else {
                continue;
            };
            if title_line {
                self.titles.observe(&record);
            }
            let mut candidate = ClaudeMetadata::default();
            candidate.observe(&record);
            if candidate.project_path.is_some() {
                self.workspace = candidate;
                break;
            }
        }
        Ok(())
    }

    /// Reads complete records from the first 16 KiB.
    fn read_head_metadata(&mut self, buffer: &mut Vec<u8>) -> Result<()> {
        read_window(
            &mut self.file,
            0,
            self.length.min(TITLE_HEAD_BYTES as u64),
            buffer,
        )?;
        let complete = if buffer.len() as u64 == self.length {
            buffer.len()
        } else {
            memchr::memrchr(b'\n', buffer).map_or(0, |end| end + 1)
        };
        self.observe_sample(&buffer[..complete]);
        self.head_end = complete as u64;
        Ok(())
    }

    /// Reads complete records from the last 64 KiB that the head did not cover.
    fn read_tail_metadata(&mut self, buffer: &mut Vec<u8>) -> Result<()> {
        let start = self
            .length
            .saturating_sub(TITLE_TAIL_BYTES as u64)
            .max(self.head_end);
        if start >= self.length {
            return Ok(());
        }
        // Read one byte early so the first segment is the partial record before `start`, if any.
        let aligned = start.saturating_sub(1);
        read_window(&mut self.file, aligned, self.length, buffer)?;
        let records = if start > 0 {
            buffer
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(&buffer[buffer.len()..], |end| &buffer[end + 1..])
        } else {
            &buffer[..]
        };
        self.observe_sample(records);
        Ok(())
    }

    fn observe_sample(&mut self, mut records: &[u8]) {
        observe_titles(&mut self.titles, records);
        // Branches can change. Read the last valid branch in the sampled records.
        while let Some(position) = memmem::rfind(records, b"\"gitBranch\"") {
            let start = memchr::memrchr(b'\n', &records[..position]).map_or(0, |index| index + 1);
            let end = memchr::memchr(b'\n', &records[position..])
                .map_or(records.len(), |index| position + index);
            if let Ok(record) = serde_json::from_slice::<Value>(&records[start..end])
                && let Some(branch) = string_at(&record, &[&["gitBranch"]])
            {
                self.workspace.git_branch = Some(branch.to_owned());
                break;
            }
            records = &records[..start];
        }
    }
}

fn text_payload(text: &str) -> Value {
    json!({ "text": text })
}

fn push_events(events: &mut Vec<ClaudeEvent>, responses: &mut ResponseMetadata, record: &Value) {
    if record.get("isSidechain").and_then(Value::as_bool) == Some(true)
        || record.get("isMeta").and_then(Value::as_bool) == Some(true)
        || record.get("teamName").and_then(Value::as_str).is_some()
    {
        return;
    }
    let raw_type = record
        .get("type")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let timestamp = parse_timestamp(record.get("timestamp"));
    let event_id = record
        .get("uuid")
        .and_then(Value::as_str)
        .and_then(|id| Uuid::parse_str(id).ok());
    let role = string_at(record, &[&["message", "role"], &["role"], &["type"]]);
    let Some(message_kind) = claude_message_kind(record, role) else {
        return;
    };
    if message_kind == EventKind::MessageAssistant {
        push_claude_session_metadata(events, responses, record, timestamp);
    }
    let Some(content) = value_at(record, &[&["message", "content"], &["content"]]) else {
        return;
    };
    if let Some(text) = content.as_str().filter(|text| !text.is_empty()) {
        events.push(ClaudeEvent {
            kind: message_kind,
            payload: text_payload(text),
            timestamp,
            replay_policy: ReplayPolicy::Contextual,
            raw_type,
            event_id,
        });
        return;
    }
    let Some(parts) = content.as_array() else {
        return;
    };
    let mut visible_text = false;
    let mut images = 0_usize;
    for part in parts {
        match part.get("type").and_then(Value::as_str) {
            Some("image") => images += 1,
            Some("text") => {
                if let Some(text) = part
                    .get("text")
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
                {
                    visible_text = true;
                    events.push(ClaudeEvent {
                        kind: message_kind.clone(),
                        payload: text_payload(text),
                        timestamp,
                        replay_policy: ReplayPolicy::Contextual,
                        raw_type: raw_type.clone(),
                        event_id,
                    });
                }
            }
            Some("tool_use" | "tool_result") => {
                events.push(tool_event(part, timestamp, raw_type.clone(), event_id));
            }
            _ => {}
        }
    }
    // An image-only turn keeps a placeholder so the reply still follows a request.
    if message_kind == EventKind::MessageUser && !visible_text && images > 0 {
        events.push(ClaudeEvent {
            kind: EventKind::MessageUser,
            payload: text_payload(&omitted_images_text(images)),
            timestamp,
            replay_policy: ReplayPolicy::Contextual,
            raw_type,
            event_id,
        });
    }
}

/// Historical record of one `tool_use` or `tool_result` content block.
fn tool_event(
    part: &Value,
    timestamp: Option<DateTime<Utc>>,
    raw_type: Option<String>,
    event_id: Option<Uuid>,
) -> ClaudeEvent {
    let field = |name: &str| part.get(name).cloned().unwrap_or(Value::Null);
    let (kind, payload) = if part.get("type").and_then(Value::as_str) == Some("tool_use") {
        (
            EventKind::ToolCalled,
            json!({ "id": field("id"), "name": field("name"), "input": field("input") }),
        )
    } else if part.get("is_error").and_then(Value::as_bool) == Some(true) {
        (
            EventKind::ToolFailed,
            json!({ "tool_use_id": field("tool_use_id"), "content": field("content") }),
        )
    } else {
        (
            EventKind::ToolCompleted,
            json!({ "tool_use_id": field("tool_use_id"), "content": field("content") }),
        )
    };
    ClaudeEvent {
        kind,
        payload,
        timestamp,
        replay_policy: ReplayPolicy::HistoricalOnly,
        raw_type,
        event_id,
    }
}

/// Metadata event index of each response, keyed by message ID and request ID.
type ResponseMetadata = HashMap<(String, String), usize>;

fn push_claude_session_metadata(
    events: &mut Vec<ClaudeEvent>,
    responses: &mut ResponseMetadata,
    record: &Value,
    timestamp: Option<DateTime<Utc>>,
) {
    let Some(payload) = claude_session_metadata(record) else {
        return;
    };
    // Claude Code writes each content block of one response as its own record, repeating usage.
    let response = string_at(record, &[&["message", "id"]]).map(|message| {
        (
            message.to_owned(),
            string_at(record, &[&["requestId"]])
                .unwrap_or_default()
                .to_owned(),
        )
    });
    if let Some(&index) = response.as_ref().and_then(|key| responses.get(key)) {
        merge_response_metadata(&mut events[index].payload, &payload);
        return;
    }
    if let Some(key) = response {
        responses.insert(key, events.len());
    }
    events.push(ClaudeEvent {
        kind: EventKind::ProviderEvent,
        payload,
        timestamp,
        replay_policy: ReplayPolicy::HistoricalOnly,
        raw_type: Some("omnisession.session_metadata".to_owned()),
        event_id: None,
    });
}

/// Folds a later chunk of one response into its metadata event.
///
/// Chunks repeat the response's usage, so the largest total wins instead of their sum.
fn merge_response_metadata(existing: &mut Value, chunk: &Value) {
    for field in ["model", "reasoning_mode"] {
        if existing[field].is_null() && !chunk[field].is_null() {
            existing[field] = chunk[field].clone();
        }
    }
    if let Some(tokens) = chunk["total_tokens"].as_u64()
        && existing["total_tokens"]
            .as_u64()
            .is_none_or(|current| tokens > current)
    {
        existing["total_tokens"] = Value::from(tokens);
    }
}

fn claude_session_metadata(record: &Value) -> Option<Value> {
    let message = record.get("message").unwrap_or(record);
    let model = string_at(message, &[&["model"], &["model_id"]]);
    let content = value_at(record, &[&["message", "content"], &["content"]]);
    let reasoning_mode = content
        .and_then(Value::as_array)
        .is_some_and(|parts| {
            parts.iter().any(|part| {
                matches!(
                    part.get("type").and_then(Value::as_str),
                    Some("thinking" | "redacted_thinking")
                )
            })
        })
        .then_some("thinking");
    let usage = message.get("usage");
    let total_tokens = usage
        .map(|usage| {
            [
                "input_tokens",
                "cache_creation_input_tokens",
                "cache_read_input_tokens",
                "output_tokens",
            ]
            .into_iter()
            .filter_map(|field| usage.get(field).and_then(Value::as_u64))
            .fold(0_u64, u64::saturating_add)
        })
        .filter(|tokens| *tokens > 0);
    (model.is_some() || reasoning_mode.is_some() || total_tokens.is_some()).then(|| {
        json!({
            "model": model,
            "reasoning_mode": reasoning_mode,
            "total_tokens": total_tokens,
            "token_usage": "incremental",
        })
    })
}

#[derive(Default)]
struct ClaudeTranscript {
    branch: branch::Branch,
    metadata: ClaudeMetadata,
    titles: TitleCandidates,
    events: Vec<ClaudeEvent>,
    responses: ResponseMetadata,
    records: HashMap<Uuid, ClaudeRecord>,
    duplicate_records: HashSet<Uuid>,
    preserved_context: Option<Result<PreservedContext>>,
    has_records: bool,
    has_conversation: bool,
    has_main_conversation: bool,
}

impl ClaudeTranscript {
    fn observe(&mut self, record: &Value) {
        self.has_records = true;
        self.metadata.observe(record);
        self.titles.observe(record);
        let main_record = record.get("isSidechain").and_then(Value::as_bool) != Some(true)
            && record.get("teamName").and_then(Value::as_str).is_none();
        if matches!(
            record.get("type").and_then(Value::as_str),
            Some("user" | "assistant")
        ) {
            self.has_conversation = true;
            self.has_main_conversation |= main_record;
        }
        if !main_record {
            return;
        }
        if record.get("type").and_then(Value::as_str) == Some("system")
            && record.get("subtype").and_then(Value::as_str) == Some("compact_boundary")
        {
            // Claude reconnects retained records after the summary through boundary metadata.
            self.preserved_context =
                compaction_preserved_context(record, &self.records).transpose();
        }
        let start = self.events.len();
        if record["type"] != "progress" {
            push_events(&mut self.events, &mut self.responses, record);
        }
        let branch = self.branch.observe(
            record,
            &self.records,
            self.preserved_context
                .as_ref()
                .and_then(|p| p.as_ref().ok()),
            start..self.events.len(),
        );
        if let Some(id) = record
            .get("uuid")
            .and_then(Value::as_str)
            .and_then(|id| Uuid::parse_str(id).ok())
        {
            let record = ClaudeRecord {
                branch,
                parent: record
                    .get("parentUuid")
                    .and_then(Value::as_str)
                    .and_then(|id| Uuid::parse_str(id).ok()),
                events: start..self.events.len(),
            };
            if self.records.insert(id, record).is_some() {
                self.duplicate_records.insert(id);
            }
        }
    }

    fn restore_preserved_context(&mut self, order: &mut Vec<usize>) -> Result<()> {
        let Some(preserved) = self.preserved_context.take() else {
            return Ok(());
        };
        let preserved = preserved?;
        if self.duplicate_records.contains(&preserved.boundary)
            || self.duplicate_records.contains(&preserved.anchor)
        {
            return Err(anyhow!(
                "retained compaction context has an ambiguous anchor"
            ));
        }
        let anchor = self
            .records
            .get(&preserved.anchor)
            .ok_or_else(|| anyhow!("retained compaction context has no summary anchor"))?;
        if !self.events[anchor.events.clone()]
            .iter()
            .any(|event| event.kind == EventKind::CompactionCreated)
        {
            return Err(anyhow!(
                "retained compaction context anchor is not a summary"
            ));
        }
        if anchor.parent != Some(preserved.boundary) {
            return Err(anyhow!(
                "retained compaction summary belongs to another boundary"
            ));
        }
        let mut seen = HashSet::new();
        let mut selected = Vec::new();
        let tip = self.branch.retained_tip();
        let mut retained_end = None;
        for id in preserved.records {
            if !seen.insert(id) {
                return Err(anyhow!("retained compaction context repeats a record"));
            }
            if self.duplicate_records.contains(&id) {
                return Err(anyhow!(
                    "retained compaction context references an ambiguous record"
                ));
            }
            let record = self.records.get(&id).ok_or_else(|| {
                anyhow!("retained compaction context references a missing record")
            })?;
            if record.events.end > anchor.events.start {
                return Err(anyhow!(
                    "retained compaction context references a record after its summary"
                ));
            }
            for index in record.events.clone() {
                if self.events[index].kind == EventKind::CompactionCreated {
                    return Err(anyhow!(
                        "retained compaction context contains another summary"
                    ));
                }
                selected.push(index);
            }
            if tip == Some(id) {
                retained_end = Some(selected.len());
            }
        }
        if !selected.is_empty() {
            let selected_set: HashSet<_> = selected.iter().copied().collect();
            if order.contains(&anchor.events.start) {
                order.retain(|index| !selected_set.contains(index));
                let insertion = order
                    .iter()
                    .rposition(|index| anchor.events.contains(index))
                    .expect("validated compaction anchor")
                    + 1;
                if let Some(end) = retained_end {
                    selected.truncate(end);
                }
                order.splice(insertion..insertion, selected);
            }
        }
        Ok(())
    }

    fn snapshot(
        mut self,
        session: &SessionRef,
        coverage: input::InputCoverage,
    ) -> Result<omnis_ir::CanonicalSnapshot> {
        if !self.has_records {
            return Err(anyhow!(
                "Claude session `{}` contains no valid records",
                session.id
            ));
        }
        if self.has_conversation && !self.has_main_conversation {
            return Err(anyhow!(
                "Claude session `{}` is a sidechain and cannot be resumed directly",
                session.id
            ));
        }
        let preservation_error = if self.branch.forked() || self.preserved_context.is_some() {
            let mut order = self.branch.event_order(&self.events);
            let error = self.restore_preserved_context(&mut order).err();
            let mut slots = self.events.into_iter().map(Some).collect::<Vec<_>>();
            self.events = order
                .into_iter()
                .map(|i| slots[i].take().expect("unique Claude event index"))
                .collect();
            error
        } else {
            None
        };
        // Response chunks may be nonadjacent, so retain their metadata indexes until EOF.
        drop(self.responses);
        let metadata = self.metadata;
        let captured_at = metadata.updated_at.unwrap_or_else(Utc::now);
        let mut builder = EventBuilder::new(Provider::Claude, &session.id);
        for event in self.events {
            builder.push(
                event.kind,
                event.payload,
                event.timestamp,
                event.replay_policy,
                event.raw_type,
                event.event_id,
            );
        }
        coverage.report(&mut builder, metadata.updated_at);
        if self.branch.missing_ancestor() {
            builder.push(
                EventKind::ProviderEvent,
                json!({"native_history_unverified": true, "reason": "Claude's active conversation references a missing parent"}),
                metadata.updated_at,
                ReplayPolicy::HistoricalOnly,
                Some("omnisession.missing_parent".to_owned()),
                None,
            );
        }
        if let Some(error) = preservation_error {
            builder.push(
                EventKind::ProviderEvent,
                json!({"omitted_events": 1, "reason": error.to_string()}),
                metadata.updated_at,
                ReplayPolicy::HistoricalOnly,
                Some("omnisession.compaction_preservation_unavailable".to_owned()),
                None,
            );
        }
        Ok(builder.snapshot(
            session.clone(),
            self.titles.title(),
            metadata.project_path,
            metadata.git_branch,
            captured_at,
        ))
    }
}

impl ProviderAdapter for ClaudeAdapter {
    fn provider(&self) -> Provider {
        Provider::Claude
    }

    fn probe(&self) -> ProviderInstallation {
        let executable = provider_executable(Provider::Claude);
        ProviderInstallation {
            provider: Provider::Claude,
            installed: executable.is_some()
                || self.projects_root.as_deref().is_some_and(Path::is_dir),
            executable,
            data_root: self.projects_root.clone(),
        }
    }

    fn discovery_notes(&self) -> Vec<String> {
        self.notes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn list_sessions(&self, project: Option<&Path>) -> Result<Vec<NativeSession>> {
        let history = self.history_index();
        let mut sessions = sample_files_concurrently(self.session_files(), |session_files| {
            let mut files = self.projects_root.as_deref().map(ProviderFiles::new);
            let mut buffer = Vec::with_capacity(TITLE_TAIL_BYTES + 1);
            let mut sessions = Vec::new();
            for (id, path) in session_files {
                let Some(path) = files.as_mut().and_then(|files| files.resolve(path)) else {
                    continue;
                };
                let indexed = history.sessions.get(id);
                let mut sample = TranscriptSample::head(&path, indexed.is_none(), &mut buffer).ok();
                let project_path = indexed
                    .map(|entry| entry.project.clone())
                    .or_else(|| sample.as_mut()?.workspace.project_path.take());
                if project.is_some_and(|project| {
                    project_path
                        .as_deref()
                        .is_none_or(|recorded| !paths_match(recorded, project))
                }) {
                    continue;
                }
                let title = sample
                    .as_mut()
                    .and_then(|sample| {
                        // Head titles still apply when the tail cannot be read.
                        let _ = sample.read_tail_metadata(&mut buffer);
                        std::mem::take(&mut sample.titles).title()
                    })
                    .or_else(|| indexed.and_then(|entry| entry.first_prompt.clone()));
                let fallback = sample
                    .as_mut()
                    .map(|sample| std::mem::take(&mut sample.workspace))
                    .unwrap_or_default();
                let created_at = indexed
                    .and_then(|entry| entry.created_at)
                    .or(fallback.created_at);
                let file_updated_at = sample
                    .as_ref()
                    .and_then(|sample| sample.modified)
                    .or_else(|| {
                        std::fs::metadata(&path)
                            .ok()
                            .and_then(|metadata| metadata.modified().ok())
                    })
                    .map(DateTime::<Utc>::from);
                sessions.push(NativeSession {
                    session: SessionRef::new(Provider::Claude, id.clone()),
                    title,
                    project_path,
                    git_branch: fallback.git_branch,
                    created_at,
                    updated_at: file_updated_at
                        .or_else(|| indexed.and_then(|entry| entry.updated_at))
                        .or(fallback.updated_at),
                    updated_at_approximate: file_updated_at.is_some(),
                    event_count: 0,
                    source_path: Some(path),
                });
            }
            sessions
        });
        *self.notes.lock().unwrap_or_else(PoisonError::into_inner) = history.notes;
        sort_sessions(&mut sessions);
        Ok(sessions)
    }

    fn read_session(&self, session: &SessionRef) -> Result<omnis_ir::CanonicalSnapshot> {
        self.read_session_at(session, None)
    }

    fn read_session_at(
        &self,
        session: &SessionRef,
        source_path: Option<&Path>,
    ) -> Result<omnis_ir::CanonicalSnapshot> {
        validate_provider(session, Provider::Claude)?;
        let path = self.session_path(&session.id, source_path)?;
        let (transcript, coverage) = input::read(&path, false)?;
        transcript.snapshot(session, coverage)
    }

    fn preview_session(&self, session: &SessionRef) -> Result<omnis_ir::CanonicalSnapshot> {
        self.preview_session_at(session, None)
    }

    fn preview_session_at(
        &self,
        session: &SessionRef,
        source_path: Option<&Path>,
    ) -> Result<omnis_ir::CanonicalSnapshot> {
        validate_provider(session, Provider::Claude)?;
        let path = self.session_path(&session.id, source_path)?;
        let (transcript, coverage) = input::read(&path, true)?;
        transcript.snapshot(session, coverage)
    }

    fn new_session_plan(&self, target: &LaunchTarget) -> Result<LaunchPlan> {
        Ok(LaunchPlan {
            program: "claude".to_owned(),
            args: target.prompt.iter().cloned().collect(),
            cwd: target.cwd.clone(),
            env: Vec::new(),
        })
    }

    fn launch_plan(&self, session: &SessionRef, target: &LaunchTarget) -> Result<LaunchPlan> {
        validate_provider(session, Provider::Claude)?;
        let mut args = vec!["--resume".to_owned(), session.id.clone()];
        if target.fork {
            args.push("--fork-session".to_owned());
        }
        if let Some(prompt) = &target.prompt {
            args.push(prompt.clone());
        }
        Ok(LaunchPlan {
            program: "claude".to_owned(),
            args,
            cwd: target.cwd.clone(),
            env: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{ClaudeAdapter, ClaudeTranscript, input::InputCoverage};
    use crate::ProviderAdapter;
    use omnis_ir::{EventKind, Provider, ReplayPolicy, SessionRef};
    use serde_json::json;

    fn transcript_from_records(records: &[serde_json::Value]) -> ClaudeTranscript {
        let mut transcript = ClaudeTranscript::default();
        for record in records {
            transcript.observe(record);
        }
        transcript
    }

    fn snapshot_from_records(records: &[serde_json::Value]) -> omnis_ir::CanonicalSnapshot {
        transcript_from_records(records)
            .snapshot(
                &SessionRef::new(Provider::Claude, "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"),
                InputCoverage::default(),
            )
            .expect("synthetic Claude snapshot")
    }

    #[test]
    fn wrong_session_hint_uses_the_requested_claude_transcript() {
        let temporary = tempfile::tempdir().expect("temporary Claude root");
        let root = temporary.path();
        let project = root.join("project");
        std::fs::create_dir_all(&project).expect("Claude project directory");
        let requested = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
        let other = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
        let actual = project.join(format!("{requested}.jsonl"));
        let wrong = project.join(format!("{other}.jsonl"));
        std::fs::write(
            &actual,
            "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"correct session\"}}\n",
        )
        .expect("requested synthetic transcript");
        std::fs::write(
            &wrong,
            "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"wrong session\"}}\n",
        )
        .expect("other synthetic transcript");
        let adapter = ClaudeAdapter::with_root(root);
        let session = SessionRef::new(Provider::Claude, requested);

        let snapshot = adapter
            .read_session_at(&session, Some(&wrong))
            .expect("fall back to requested session");

        assert_eq!(snapshot.events.len(), 1);
        assert_eq!(snapshot.events[0].payload["text"], "correct session");
    }

    #[test]
    fn outside_root_path_hint_is_rejected() {
        let root = tempfile::tempdir().expect("temporary Claude root");
        let outside = tempfile::tempdir().expect("outside path");
        let id = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
        let path = outside.path().join(format!("{id}.jsonl"));
        std::fs::write(
            &path,
            "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"outside\"}}\n",
        )
        .expect("outside synthetic transcript");
        let adapter = ClaudeAdapter::with_root(root.path());
        let session = SessionRef::new(Provider::Claude, id);

        assert!(adapter.read_session_at(&session, Some(&path)).is_err());
    }

    #[test]
    fn fixture_canonicalizes_visible_messages_and_historical_tools() {
        let records = include_str!("../tests/fixtures/claude-session.jsonl")
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect::<Vec<_>>();
        let transcript = transcript_from_records(&records);
        let metadata = transcript.metadata;
        let events = transcript.events;

        assert_eq!(
            metadata.project_path.as_deref(),
            Some(std::path::Path::new("/workspace/demo"))
        );
        assert_eq!(events.len(), 5);
        assert_eq!(events[0].kind, EventKind::MessageUser);
        assert_eq!(events[1].kind, EventKind::ProviderEvent);
        assert_eq!(events[2].kind, EventKind::MessageAssistant);
        assert_eq!(events[3].kind, EventKind::ToolCalled);
        assert_eq!(events[3].replay_policy, ReplayPolicy::HistoricalOnly);
        assert_eq!(events[4].kind, EventKind::ToolCompleted);
    }

    #[test]
    fn sidechain_records_are_not_canonicalized_as_main_context() {
        let records = vec![serde_json::json!({
            "type": "assistant",
            "isSidechain": true,
            "message": {"role": "assistant", "content": "subagent-only"}
        })];

        let transcript = transcript_from_records(&records);
        assert!(transcript.events.is_empty());
        let error = transcript
            .snapshot(
                &SessionRef::new(Provider::Claude, "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"),
                InputCoverage::default(),
            )
            .expect_err("sidechain sessions cannot be resumed");
        assert!(error.to_string().contains("is a sidechain"));
    }

    #[test]
    fn compact_summary_is_contextual_compaction_not_a_user_request() {
        let records = vec![serde_json::json!({
            "type": "user",
            "uuid": "44444444-4444-4444-8444-444444444444",
            "isCompactSummary": true,
            "message": {
                "role": "user",
                "content": "Current objective: finish synthetic migration"
            }
        })];

        let events = transcript_from_records(&records).events;

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, EventKind::CompactionCreated);
        assert_eq!(events[0].replay_policy, ReplayPolicy::Contextual);
        assert_eq!(
            events[0].payload["text"],
            "Current objective: finish synthetic migration"
        );
    }

    #[test]
    fn rewind_follows_the_active_branch_through_progress_and_parallel_results() {
        let id = |i| uuid::Uuid::from_u128(i);
        let message = |i, parent: Option<u128>, role: &str, content| json!({"type": role, "uuid": id(i), "parentUuid": parent.map(id), "message": {"role":role, "content":content}});
        let mut first_call = message(
            2,
            Some(1),
            "assistant",
            json!([{"type":"tool_use","id":"a","name":"Read"}]),
        );
        first_call["message"]["id"] = json!("parallel");
        let mut second_call = message(
            3,
            Some(2),
            "assistant",
            json!([{"type":"tool_use","id":"b","name":"Read"}]),
        );
        second_call["message"]["id"] = json!("parallel");
        let records = vec![
            message(1, None, "user", json!("Shared task")),
            first_call,
            second_call,
            message(
                4,
                Some(2),
                "user",
                json!([{"type":"tool_result","tool_use_id":"a","content":"first"}]),
            ),
            message(
                5,
                Some(3),
                "user",
                json!([{"type":"tool_result","tool_use_id":"b","content":"second"}]),
            ),
            message(6, Some(4), "assistant", json!("Shared reply")),
            message(7, Some(6), "user", json!("Discarded request")),
            message(8, Some(7), "assistant", json!("Discarded reply")),
            json!({"type":"progress","uuid":id(9),"parentUuid":id(6)}),
            message(10, Some(9), "user", json!("Replacement request")),
        ];
        let snapshot = snapshot_from_records(&records);
        let items = snapshot
            .events
            .iter()
            .map(|event| {
                event.payload["text"]
                    .as_str()
                    .or(event.payload["id"].as_str())
                    .or(event.payload["content"].as_str())
                    .unwrap()
            })
            .collect::<Vec<_>>();
        assert_eq!(
            items,
            [
                "Shared task",
                "a",
                "b",
                "second",
                "first",
                "Shared reply",
                "Replacement request"
            ]
        );
    }

    #[test]
    fn missing_parent_does_not_import_an_unrelated_exchange() {
        let id = |n| uuid::Uuid::from_u128(n);
        let records = [
            json!({"uuid":id(1),"type":"user","message":{"content":"Unrelated exchange"}}),
            json!({"uuid":id(2),"parentUuid":id(42),"type":"progress"}),
            json!({"uuid":id(3),"parentUuid":id(2),"type":"user","message":{"content":"Keep this request"}}),
            json!({"type":"assistant","message":{"content":"Legacy reply without a parent field"}}),
        ];
        let snapshot = snapshot_from_records(&records);
        assert_eq!(
            snapshot
                .events
                .iter()
                .filter_map(|event| event.payload["text"].as_str())
                .collect::<Vec<_>>(),
            ["Keep this request", "Legacy reply without a parent field"]
        );
        assert_eq!(
            snapshot.events.last().unwrap().payload["native_history_unverified"],
            true
        );
    }

    #[test]
    fn rewinding_to_a_compacted_suffix_keeps_the_summary_and_selected_turns() {
        let id = |n| uuid::Uuid::from_u128(n);
        let message = |n, parent, role: &str, text: &str| {
            json!({
                "uuid":id(n),"parentUuid":id(parent),"type":role,"message":{"role":role,"content":text}
            })
        };
        let records = [
            json!({"uuid":id(1),"parentUuid":null,"type":"user","message":{"content":"Initial task"}}),
            message(2, 1, "assistant", "Retained reply"),
            json!({"uuid":id(3),"type":"system","subtype":"compact_boundary","logicalParentUuid":id(2),
                "compactMetadata":{"preservedMessages":{"anchorUuid":id(4),"uuids":[id(2)]}}}),
            json!({"uuid":id(4),"parentUuid":id(3),"type":"user","isCompactSummary":true,"message":{"content":"Summary"}}),
            message(5, 2, "user", "Discarded question"),
            message(6, 5, "assistant", "Discarded answer"),
            message(7, 2, "user", "Replacement question"),
        ];
        let snapshot = snapshot_from_records(&records);
        assert_eq!(
            snapshot
                .events
                .iter()
                .map(|event| event.payload["text"].as_str().unwrap())
                .collect::<Vec<_>>(),
            [
                "Initial task",
                "Summary",
                "Retained reply",
                "Replacement question"
            ]
        );
    }

    #[test]
    fn rewinding_into_the_middle_of_a_retained_suffix_discards_its_old_tail() {
        let id = |n| uuid::Uuid::from_u128(n);
        let records = [
            json!({"uuid":id(1),"parentUuid":null,"type":"user","message":{"content":"Original task"}}),
            json!({"uuid":id(2),"parentUuid":id(1),"type":"assistant","message":{"content":"First result"}}),
            json!({"uuid":id(3),"parentUuid":id(2),"type":"user","message":{"content":"Abandoned follow-up"}}),
            json!({"uuid":id(4),"type":"system","subtype":"compact_boundary","logicalParentUuid":id(3),
                "compactMetadata":{"preservedMessages":{"anchorUuid":id(5),"uuids":[id(2),id(3)]}}}),
            json!({"uuid":id(5),"parentUuid":id(4),"type":"user","isCompactSummary":true,"message":{"content":"Summary"}}),
            json!({"uuid":id(6),"parentUuid":id(2),"type":"progress"}),
            json!({"uuid":id(7),"parentUuid":id(6),"type":"user","message":{"content":"Replacement follow-up"}}),
        ];
        let snapshot = snapshot_from_records(&records);
        assert_eq!(
            snapshot
                .events
                .iter()
                .filter_map(|event| event.payload["text"].as_str())
                .collect::<Vec<_>>(),
            [
                "Original task",
                "Summary",
                "First result",
                "Replacement follow-up"
            ]
        );
    }

    #[test]
    fn retained_compaction_context_follows_the_summary_once() {
        let retained = "11111111-1111-4111-8111-111111111111";
        let result = "22222222-2222-4222-8222-222222222222";
        let boundary = "33333333-3333-4333-8333-333333333333";
        let anchor = "44444444-4444-4444-8444-444444444444";
        for metadata in [
            json!({"preservedMessages": {"anchorUuid": anchor, "uuids": [retained, result], "allUuids": [retained, result]}}),
            json!({"preservedSegment": {"headUuid": retained, "anchorUuid": anchor, "tailUuid": result}}),
        ] {
            let records = vec![
                json!({"type": "user", "message": {"role": "user", "content": "Finish the migration"}}),
                json!({"type": "assistant", "uuid": retained, "message": {"role": "assistant", "content": [{"type": "text", "text": "The migration is complete"}, {"type": "tool_use", "id": "verify", "name": "Bash", "input": {"command": "synthetic verification"}}]}}),
                json!({"type": "user", "uuid": result, "parentUuid": retained, "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "verify", "content": "passed"}]}}),
                json!({"type": "system", "uuid": boundary, "subtype": "compact_boundary", "compactMetadata": metadata}),
                json!({"type": "user", "uuid": anchor, "parentUuid": boundary, "isCompactSummary": true, "message": {"role": "user", "content": "The migration still needs a final report"}}),
                json!({"type": "user", "parentUuid": result, "message": {"role": "user", "content": "Start the next task"}}),
            ];
            let snapshot = snapshot_from_records(&records);

            let kinds = snapshot
                .events
                .iter()
                .map(|event| event.kind.clone())
                .collect::<Vec<_>>();
            assert_eq!(
                kinds,
                [
                    EventKind::MessageUser,
                    EventKind::CompactionCreated,
                    EventKind::MessageAssistant,
                    EventKind::ToolCalled,
                    EventKind::ToolCompleted,
                    EventKind::MessageUser
                ]
            );
            assert_eq!(
                snapshot.events[2].payload["text"],
                "The migration is complete"
            );
            assert_eq!(
                snapshot.events[3].replay_policy,
                ReplayPolicy::HistoricalOnly
            );
            assert_eq!(
                snapshot.events[4].replay_policy,
                ReplayPolicy::HistoricalOnly
            );
            assert!(
                snapshot
                    .events
                    .windows(2)
                    .all(|events| events[0].sequence < events[1].sequence)
            );
        }
    }

    #[test]
    fn retained_segment_walks_opaque_records_without_importing_another_branch() {
        let head = "11111111-1111-4111-8111-111111111111";
        let bridge = "22222222-2222-4222-8222-222222222222";
        let tail = "33333333-3333-4333-8333-333333333333";
        let anchor = "44444444-4444-4444-8444-444444444444";
        let boundary = "55555555-5555-4555-8555-555555555555";
        let records = vec![
            json!({"type": "user", "uuid": head, "message": {"role": "user", "content": "Check the result"}}),
            json!({"type": "attachment", "uuid": bridge, "parentUuid": head, "attachment": {"type": "future_unknown", "content": "opaque metadata"}}),
            json!({"type": "assistant", "uuid": tail, "parentUuid": bridge, "message": {"role": "assistant", "content": "The result is ready"}}),
            json!({"type": "assistant", "parentUuid": head, "message": {"role": "assistant", "content": "A superseded branch"}}),
            json!({"type": "system", "uuid": boundary, "subtype": "compact_boundary", "compactMetadata": {"preservedSegment": {"headUuid": head, "anchorUuid": anchor, "tailUuid": tail}}}),
            json!({"type": "user", "uuid": anchor, "parentUuid": boundary, "isCompactSummary": true, "message": {"role": "user", "content": "Context before the result"}}),
        ];
        let snapshot = snapshot_from_records(&records);

        let text = snapshot
            .events
            .iter()
            .map(|event| event.payload["text"].as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            text,
            [
                Some("A superseded branch"),
                Some("Context before the result"),
                Some("Check the result"),
                Some("The result is ready")
            ]
        );
    }

    #[test]
    fn invalid_retained_compaction_context_reports_incomplete_fidelity() {
        let head = "11111111-1111-4111-8111-111111111111";
        let missing = "22222222-2222-4222-8222-222222222222";
        let anchor = "44444444-4444-4444-8444-444444444444";
        let boundary = "55555555-5555-4555-8555-555555555555";
        for metadata in [
            json!({"preservedMessages": {"anchorUuid": anchor, "uuids": [missing]}}),
            json!({"preservedMessages": {"anchorUuid": missing, "uuids": [head]}}),
            json!({"preservedMessages": {"anchorUuid": anchor, "uuids": [head, head]}}),
            json!({"preservedMessages": {"anchorUuid": anchor, "uuids": [anchor]}}),
            json!({"preservedMessages": {"anchorUuid": anchor, "uuids": ["invalid"]}}),
            json!({"preservedSegment": {"headUuid": missing, "anchorUuid": anchor, "tailUuid": head}}),
        ] {
            let records = vec![
                json!({"type": "assistant", "uuid": head, "message": {"role": "assistant", "content": "Keep historical context"}}),
                json!({"type": "system", "uuid": boundary, "subtype": "compact_boundary", "compactMetadata": metadata}),
                json!({"type": "user", "uuid": anchor, "parentUuid": boundary, "isCompactSummary": true, "message": {"role": "user", "content": "Current summary"}}),
            ];
            let snapshot = snapshot_from_records(&records);

            assert_eq!(snapshot.events.len(), 3);
            assert_eq!(
                snapshot.events[0].payload["text"],
                "Keep historical context"
            );
            assert_eq!(snapshot.events[1].kind, EventKind::CompactionCreated);
            assert_eq!(
                snapshot.events[2].source.raw_record_type.as_deref(),
                Some("omnisession.compaction_preservation_unavailable")
            );
            assert_eq!(snapshot.events[2].payload["omitted_events"], 1);
        }
    }

    #[test]
    fn only_the_latest_compaction_boundary_restores_retained_context() {
        let first = "11111111-1111-4111-8111-111111111111";
        let second = "22222222-2222-4222-8222-222222222222";
        let first_anchor = "33333333-3333-4333-8333-333333333333";
        let second_anchor = "44444444-4444-4444-8444-444444444444";
        let first_boundary = "55555555-5555-4555-8555-555555555555";
        let second_boundary = "66666666-6666-4666-8666-666666666666";
        for latest_metadata in [
            json!({}),
            json!({"preservedMessages": {"anchorUuid": second_anchor, "uuids": [second]}}),
        ] {
            let restores_latest = latest_metadata.get("preservedMessages").is_some();
            let records = vec![
                json!({"type": "assistant", "uuid": first, "message": {"role": "assistant", "content": "First result"}}),
                json!({"type": "system", "uuid": first_boundary, "subtype": "compact_boundary", "compactMetadata": {"preservedMessages": {"anchorUuid": first_anchor, "uuids": [first]}}}),
                json!({"type": "user", "uuid": first_anchor, "parentUuid": first_boundary, "isCompactSummary": true, "message": {"role": "user", "content": "First summary"}}),
                json!({"type": "assistant", "uuid": second, "message": {"role": "assistant", "content": "Second result"}}),
                json!({"type": "system", "uuid": second_boundary, "subtype": "compact_boundary", "compactMetadata": latest_metadata}),
                json!({"type": "user", "uuid": second_anchor, "parentUuid": second_boundary, "isCompactSummary": true, "message": {"role": "user", "content": "Second summary"}}),
            ];
            let snapshot = snapshot_from_records(&records);
            let text = snapshot
                .events
                .iter()
                .map(|event| event.payload["text"].as_str())
                .collect::<Vec<_>>();
            let expected = if restores_latest {
                [
                    Some("First result"),
                    Some("First summary"),
                    Some("Second summary"),
                    Some("Second result"),
                ]
            } else {
                [
                    Some("First result"),
                    Some("First summary"),
                    Some("Second result"),
                    Some("Second summary"),
                ]
            };
            assert_eq!(text, expected);
        }
    }

    #[test]
    fn retained_message_list_controls_order_and_ignores_stream_only_ids() {
        let first = "11111111-1111-4111-8111-111111111111";
        let second = "22222222-2222-4222-8222-222222222222";
        let stream_only = "33333333-3333-4333-8333-333333333333";
        let anchor = "44444444-4444-4444-8444-444444444444";
        let boundary = "55555555-5555-4555-8555-555555555555";
        let records = vec![
            json!({"type": "assistant", "uuid": first, "message": {"role": "assistant", "content": "First physical record"}}),
            json!({"type": "user", "uuid": second, "message": {"role": "user", "content": "Second physical record"}}),
            json!({"type": "system", "uuid": boundary, "subtype": "compact_boundary", "compactMetadata": {"preservedMessages": {"anchorUuid": anchor, "uuids": [second, first], "allUuids": [first, second, stream_only]}}}),
            json!({"type": "user", "uuid": anchor, "parentUuid": boundary, "isCompactSummary": true, "message": {"role": "user", "content": "Summary"}}),
        ];
        let snapshot = snapshot_from_records(&records);
        let text = snapshot
            .events
            .iter()
            .map(|event| event.payload["text"].as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            text,
            [
                Some("Summary"),
                Some("Second physical record"),
                Some("First physical record")
            ]
        );
    }

    #[test]
    fn ambiguous_retained_records_and_unrelated_anchors_are_reported() {
        let retained = "11111111-1111-4111-8111-111111111111";
        let anchor = "44444444-4444-4444-8444-444444444444";
        let boundary = "55555555-5555-4555-8555-555555555555";
        for ambiguous_record in [Some(0), Some(1), Some(2), None] {
            let mut records = vec![
                json!({"type": "assistant", "uuid": retained, "message": {"role": "assistant", "content": "Historical result"}}),
                json!({"type": "system", "uuid": boundary, "subtype": "compact_boundary", "compactMetadata": {"preservedMessages": {"anchorUuid": anchor, "uuids": [retained]}}}),
                json!({"type": "user", "uuid": anchor, "parentUuid": boundary, "isCompactSummary": true, "message": {"role": "user", "content": "Summary"}}),
            ];
            if let Some(index) = ambiguous_record {
                records.push(records[index].clone());
            } else {
                records[2]["parentUuid"] = serde_json::Value::Null;
            }
            let snapshot = snapshot_from_records(&records);
            let notice = snapshot.events.last().expect("fidelity notice");
            assert_eq!(
                notice.source.raw_record_type.as_deref(),
                Some("omnisession.compaction_preservation_unavailable")
            );
            assert_eq!(notice.payload["omitted_events"], 1);
        }
    }

    #[test]
    fn streamed_chunks_of_one_response_count_usage_once() {
        // Claude Code writes one record per content block, each repeating the response's usage.
        let usage =
            json!({"input_tokens": 100, "cache_read_input_tokens": 20, "output_tokens": 30});
        let chunk = |content: serde_json::Value| {
            json!({
                "type": "assistant", "requestId": "req_1",
                "message": {"id": "msg_1", "role": "assistant", "model": "claude-test", "usage": usage, "content": [content]}
            })
        };
        let records = vec![
            json!({"type": "user", "message": {"role": "user", "content": "question"}}),
            chunk(json!({"type": "thinking", "thinking": "hidden"})),
            chunk(json!({"type": "text", "text": "answer"})),
            json!({
                "type": "assistant", "requestId": "req_2",
                "message": {"id": "msg_2", "role": "assistant", "usage": {"input_tokens": 10, "output_tokens": 5}, "content": "follow-up"}
            }),
        ];
        let snapshot = transcript_from_records(&records)
            .snapshot(
                &SessionRef::new(Provider::Claude, "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"),
                InputCoverage::default(),
            )
            .expect("Claude snapshot");

        let preview = omnis_core::session_preview(&snapshot);
        assert_eq!(preview.total_tokens, Some(165));
        assert_eq!(preview.reasoning_mode.as_deref(), Some("thinking"));
        assert_eq!(preview.model.as_deref(), Some("claude-test"));
    }

    #[test]
    fn nonadjacent_chunks_update_the_first_metadata_event_without_reordering() {
        let records = vec![
            json!({
                "type": "assistant", "requestId": "req_1", "timestamp": "2026-01-01T00:00:00Z",
                "message": {"id": "msg_1", "usage": {"output_tokens": 10}, "content": "first chunk"}
            }),
            json!({
                "type": "assistant", "requestId": "req_2",
                "message": {"id": "msg_1", "model": "other-model", "usage": {"output_tokens": 5}, "content": "other response"}
            }),
            json!({
                "type": "assistant", "requestId": "req_1", "timestamp": "2026-01-01T00:01:00Z",
                "message": {"id": "msg_1", "model": "claude-test", "usage": {"output_tokens": 30}, "content": [
                    {"type": "thinking", "thinking": "not visible"},
                    {"type": "text", "text": "later chunk"}
                ]}
            }),
            json!({
                "type": "assistant", "requestId": "req_1",
                "message": {"id": "msg_1", "model": "ignored-later-model", "usage": {"output_tokens": 20}}
            }),
        ];
        let session = SessionRef::new(Provider::Claude, "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa");
        let snapshot = transcript_from_records(&records)
            .snapshot(&session, InputCoverage::default())
            .expect("nonadjacent response chunks");

        assert_eq!(snapshot.events.len(), 5);
        assert_eq!(snapshot.events[0].payload["total_tokens"], 30);
        assert_eq!(snapshot.events[0].payload["model"], "claude-test");
        assert_eq!(snapshot.events[0].payload["reasoning_mode"], "thinking");
        assert_eq!(
            snapshot.events[0].timestamp,
            super::parse_timestamp(Some(&records[0]["timestamp"]))
        );
        assert_eq!(snapshot.events[1].payload["text"], "first chunk");
        assert_eq!(snapshot.events[2].payload["total_tokens"], 5);
        assert_eq!(snapshot.events[3].payload["text"], "other response");
        assert_eq!(snapshot.events[4].payload["text"], "later chunk");
        let thread_id = uuid::Uuid::parse_str(&session.id).expect("session UUID");
        for (index, event) in snapshot.events.iter().enumerate() {
            assert_eq!(event.sequence, index as u64);
            assert_eq!(
                event.event_id,
                uuid::Uuid::new_v5(&thread_id, index.to_string().as_bytes())
            );
        }
    }

    #[test]
    fn full_read_and_preview_preserve_late_metadata_from_excluded_records() {
        let temporary = tempfile::tempdir().expect("temporary Claude root");
        let project = temporary.path().join("project");
        std::fs::create_dir(&project).expect("Claude project directory");
        let session = SessionRef::new(Provider::Claude, "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa");
        let records = [
            json!({
                "type": "assistant", "isSidechain": true, "timestamp": "2026-01-01T00:02:00Z",
                "cwd": "/workspace/earlier", "gitBranch": "earlier", "message": {"content": "excluded"}
            }),
            json!({"type": "custom-title", "customTitle": "Chosen title"}),
            json!({
                "type": "user", "timestamp": "2026-01-01T00:01:00Z",
                "uuid": "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb", "message": {"content": "visible"}
            }),
            json!({
                "type": "assistant", "isMeta": true, "timestamp": "2026-01-01T00:03:00Z",
                "cwd": "/workspace/latest", "gitBranch": "latest", "message": {"content": "excluded"}
            }),
            json!({"type": "ai-title", "aiTitle": "Generated title"}),
            json!({"type": "summary", "summary": "Later generated title", "timestamp": "2026-01-01T00:00:00Z"}),
        ];
        let source = project.join(format!("{}.jsonl", session.id));
        let input = records
            .iter()
            .map(serde_json::Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&source, input).expect("synthetic Claude transcript");
        let adapter = ClaudeAdapter::with_root(temporary.path());
        let full = adapter.read_session(&session).expect("full transcript");
        let preview = adapter
            .preview_session(&session)
            .expect("transcript preview");

        assert_eq!(
            serde_json::to_value(&full).expect("full snapshot"),
            serde_json::to_value(preview).expect("preview snapshot")
        );
        assert_eq!(full.title.as_deref(), Some("Chosen title"));
        assert_eq!(
            full.workspace.root,
            std::path::Path::new("/workspace/latest")
        );
        assert_eq!(full.workspace.git.branch.as_deref(), Some("latest"));
        assert_eq!(
            Some(full.captured_at),
            super::parse_timestamp(Some(&records[3]["timestamp"]))
        );
        assert_eq!(full.events.len(), 1);
        assert_eq!(full.events[0].payload["text"], "visible");
        let thread_id = uuid::Uuid::parse_str(&session.id).expect("session UUID");
        assert_eq!(
            full.events[0].event_id,
            uuid::Uuid::new_v5(&thread_id, b"0:bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb")
        );
    }

    #[test]
    fn image_only_user_turn_keeps_a_placeholder() {
        let image = json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "iVBORw0KGgo="}});
        let records = vec![
            json!({"type": "user", "message": {"role": "user", "content": [image, image]}}),
            json!({"type": "assistant", "message": {"role": "assistant", "content": [{"type": "text", "text": "Two screenshots"}]}}),
        ];

        let events = transcript_from_records(&records).events;

        assert_eq!(events[0].kind, EventKind::MessageUser);
        assert_eq!(events[0].payload, json!({"text": "[2 images omitted]"}));
        assert_eq!(events[1].kind, EventKind::MessageAssistant);
    }
}
