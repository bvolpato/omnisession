use std::{
    collections::HashMap,
    fs::File,
    io::{self, BufRead, BufReader, Read, Seek, SeekFrom},
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
        EventBuilder, MAX_COLLECTED_TRANSCRIPT_FILE_SIZE, ProviderFiles, json_lines_preview,
        nested_files_matching, omitted_images_text, parse_timestamp, paths_match,
        provider_executable, provider_file, provider_root, sample_files_concurrently,
        sort_sessions, string_at, validate_provider, value_at, visit_index_json_lines,
        visit_json_lines,
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
            sample.read_head_titles(buffer)?;
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

    /// Observes title records among the complete records in the first 16 KiB.
    fn read_head_titles(&mut self, buffer: &mut Vec<u8>) -> Result<()> {
        read_window(
            &mut self.file,
            0,
            self.length.min(TITLE_HEAD_BYTES as u64),
            buffer,
        )?;
        // The tail then starts on a record boundary.
        let complete = buffer
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |end| end + 1);
        observe_titles(&mut self.titles, &buffer[..complete]);
        self.head_end = complete as u64;
        Ok(())
    }

    /// Observes title records in the last 64 KiB that the head did not cover.
    fn read_tail_titles(&mut self, buffer: &mut Vec<u8>) -> Result<()> {
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
        observe_titles(&mut self.titles, records);
        Ok(())
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
    metadata: ClaudeMetadata,
    titles: TitleCandidates,
    events: Vec<ClaudeEvent>,
    responses: ResponseMetadata,
    has_records: bool,
    has_conversation: bool,
    has_main_conversation: bool,
}

impl ClaudeTranscript {
    fn observe(&mut self, record: &Value) {
        self.has_records = true;
        self.metadata.observe(record);
        self.titles.observe(record);
        if matches!(
            record.get("type").and_then(Value::as_str),
            Some("user" | "assistant")
        ) {
            self.has_conversation = true;
            self.has_main_conversation |= record.get("isSidechain").and_then(Value::as_bool)
                != Some(true)
                && record.get("teamName").and_then(Value::as_str).is_none();
        }
        push_events(&mut self.events, &mut self.responses, record);
    }

    fn snapshot(
        self,
        session: &SessionRef,
        oversized_records: usize,
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
        builder.push_oversized_record_notice(oversized_records, metadata.updated_at);
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
                let fallback = sample
                    .as_mut()
                    .map(|sample| std::mem::take(&mut sample.workspace))
                    .unwrap_or_default();
                let project_path = indexed
                    .map(|entry| entry.project.clone())
                    .or(fallback.project_path);
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
                        let _ = sample.read_tail_titles(&mut buffer);
                        std::mem::take(&mut sample.titles).title()
                    })
                    .or_else(|| indexed.and_then(|entry| entry.first_prompt.clone()));
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
        let mut transcript = ClaudeTranscript::default();
        let oversized_records =
            visit_json_lines(&path, MAX_COLLECTED_TRANSCRIPT_FILE_SIZE, |record| {
                transcript.observe(&record);
                Ok(())
            })?;
        transcript.snapshot(session, oversized_records)
    }

    fn preview_session(&self, session: &SessionRef) -> Result<omnis_ir::CanonicalSnapshot> {
        self.preview_session_at(session, None)
    }

    fn preview_session_at(
        &self,
        session: &SessionRef,
        source_path: Option<&Path>,
    ) -> Result<omnis_ir::CanonicalSnapshot> {
        const SAMPLE_RECORDS: usize = 1_024;
        validate_provider(session, Provider::Claude)?;
        let path = self.session_path(&session.id, source_path)?;
        let records = json_lines_preview(&path, SAMPLE_RECORDS)?;
        let mut transcript = ClaudeTranscript::default();
        for record in records {
            transcript.observe(&record);
        }
        transcript.snapshot(session, 0)
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
    use super::{ClaudeAdapter, ClaudeTranscript};
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
                0,
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
                0,
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
            .snapshot(&session, 0)
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
