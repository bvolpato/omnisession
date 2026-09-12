use std::{
    collections::HashMap,
    fs::File,
    io::{BufRead, BufReader, Read},
    path::Path,
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock, PoisonError},
};

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use omnis_ir::{EventKind, Provider, ReplayPolicy, SessionRef};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    LaunchPlan, LaunchTarget, NativeSession, ProviderAdapter, ProviderInstallation,
    support::{
        EventBuilder, MAX_COLLECTED_TRANSCRIPT_FILE_SIZE, executable, json_lines_preview,
        nested_files_matching, parse_timestamp, paths_match, provider_file, provider_root,
        sort_sessions, string_at, validate_provider, value_at, visit_index_json_lines,
        visit_json_lines,
    },
};

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
            // Later records update the workspace and timestamp.
            entry.project = PathBuf::from(project);
            entry.timestamp = parse_timestamp(record.get("timestamp"));
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
    timestamp: Option<DateTime<Utc>>,
}

#[derive(Default)]
struct ClaudeHistory {
    sessions: HashMap<String, HistoryEntry>,
    notes: Vec<String>,
}

#[derive(Default)]
struct ClaudeMetadata {
    title: Option<String>,
    project_path: Option<PathBuf>,
    git_branch: Option<String>,
    created_at: Option<DateTime<Utc>>,
    updated_at: Option<DateTime<Utc>>,
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

fn metadata(records: &[Value]) -> ClaudeMetadata {
    let mut metadata = ClaudeMetadata::default();
    for record in records {
        let record_type = record.get("type").and_then(Value::as_str);
        if record_type == Some("summary") {
            if let Some(title) = string_at(record, &[&["summary"], &["title"]]) {
                metadata.title = Some(title.to_owned());
            }
        }
        if let Some(title) = string_at(record, &[&["customTitle"]]) {
            metadata.title = Some(title.to_owned());
        }
        if let Some(cwd) = string_at(record, &[&["cwd"]]) {
            metadata.project_path = Some(PathBuf::from(cwd));
        }
        if let Some(branch) = string_at(record, &[&["gitBranch"]]) {
            metadata.git_branch = Some(branch.to_owned());
        }
        let timestamp = parse_timestamp(record.get("timestamp"));
        if let Some(timestamp) = timestamp {
            metadata.created_at = Some(
                metadata
                    .created_at
                    .map_or(timestamp, |current| current.min(timestamp)),
            );
            metadata.updated_at = Some(
                metadata
                    .updated_at
                    .map_or(timestamp, |current| current.max(timestamp)),
            );
        }
    }
    metadata
}

fn discovery_metadata(path: &Path) -> Result<ClaudeMetadata> {
    const MAX_METADATA_RECORDS: usize = 32;
    const MAX_METADATA_BYTES: u64 = 2 * 1024 * 1024;
    let file = File::open(path)?;
    let mut reader = BufReader::new(file.take(MAX_METADATA_BYTES));
    for _ in 0..MAX_METADATA_RECORDS {
        let mut line = Vec::new();
        if reader.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        let Ok(record) = serde_json::from_slice::<Value>(&line) else {
            continue;
        };
        let metadata = metadata(&[record]);
        if metadata.project_path.is_some() {
            return Ok(metadata);
        }
    }
    Ok(ClaudeMetadata::default())
}

fn text_payload(text: &str) -> Value {
    json!({ "text": text })
}

fn events(records: &[Value]) -> Vec<ClaudeEvent> {
    let mut events = Vec::new();
    for record in records {
        if record.get("isSidechain").and_then(Value::as_bool) == Some(true)
            || record.get("isMeta").and_then(Value::as_bool) == Some(true)
            || record.get("teamName").and_then(Value::as_str).is_some()
        {
            continue;
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
            continue;
        };
        if message_kind == EventKind::MessageAssistant {
            push_claude_session_metadata(&mut events, record, timestamp);
        }
        let Some(content) = value_at(record, &[&["message", "content"], &["content"]]) else {
            continue;
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
            continue;
        }
        let Some(parts) = content.as_array() else {
            continue;
        };
        for part in parts {
            match part.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(text) = part
                        .get("text")
                        .and_then(Value::as_str)
                        .filter(|text| !text.is_empty())
                    {
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
                Some("tool_use") => events.push(ClaudeEvent {
                    kind: EventKind::ToolCalled,
                    payload: json!({
                        "id": part.get("id").cloned().unwrap_or(Value::Null),
                        "name": part.get("name").cloned().unwrap_or(Value::Null),
                        "input": part.get("input").cloned().unwrap_or(Value::Null),
                    }),
                    timestamp,
                    replay_policy: ReplayPolicy::HistoricalOnly,
                    raw_type: raw_type.clone(),
                    event_id,
                }),
                Some("tool_result") => {
                    let failed = part.get("is_error").and_then(Value::as_bool) == Some(true);
                    events.push(ClaudeEvent {
                        kind: if failed {
                            EventKind::ToolFailed
                        } else {
                            EventKind::ToolCompleted
                        },
                        payload: json!({
                            "tool_use_id": part.get("tool_use_id").cloned().unwrap_or(Value::Null),
                            "content": part.get("content").cloned().unwrap_or(Value::Null),
                        }),
                        timestamp,
                        replay_policy: ReplayPolicy::HistoricalOnly,
                        raw_type: raw_type.clone(),
                        event_id,
                    });
                }
                _ => {}
            }
        }
    }
    events
}

fn push_claude_session_metadata(
    events: &mut Vec<ClaudeEvent>,
    record: &Value,
    timestamp: Option<DateTime<Utc>>,
) {
    let Some(payload) = claude_session_metadata(record) else {
        return;
    };
    events.push(ClaudeEvent {
        kind: EventKind::ProviderEvent,
        payload,
        timestamp,
        replay_policy: ReplayPolicy::HistoricalOnly,
        raw_type: Some("omnisession.session_metadata".to_owned()),
        event_id: None,
    });
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

fn is_sidechain_session(records: &[Value]) -> bool {
    let conversation = records.iter().filter(|record| {
        matches!(
            record.get("type").and_then(Value::as_str),
            Some("user" | "assistant")
        )
    });
    let mut found = false;
    for record in conversation {
        found = true;
        if record.get("isSidechain").and_then(Value::as_bool) != Some(true)
            && record.get("teamName").and_then(Value::as_str).is_none()
        {
            return false;
        }
    }
    found
}

fn snapshot_from_records(
    session: &SessionRef,
    records: &[Value],
    oversized_records: usize,
) -> Result<omnis_ir::CanonicalSnapshot> {
    if records.is_empty() {
        return Err(anyhow!(
            "Claude session `{}` contains no valid records",
            session.id
        ));
    }
    if is_sidechain_session(records) {
        return Err(anyhow!(
            "Claude session `{}` is a sidechain and cannot be resumed directly",
            session.id
        ));
    }
    let metadata = metadata(records);
    let captured_at = metadata.updated_at.unwrap_or_else(Utc::now);
    let mut builder = EventBuilder::new(Provider::Claude, &session.id);
    for event in events(records) {
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
        metadata.title,
        metadata.project_path,
        metadata.git_branch,
        captured_at,
    ))
}

impl ProviderAdapter for ClaudeAdapter {
    fn provider(&self) -> Provider {
        Provider::Claude
    }

    fn probe(&self) -> ProviderInstallation {
        ProviderInstallation {
            provider: Provider::Claude,
            installed: executable("claude").is_some()
                || self.projects_root.as_deref().is_some_and(Path::is_dir),
            executable: executable("claude"),
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
        let mut sessions = Vec::new();
        for (id, path) in self.session_files() {
            let Some(path) = self
                .projects_root
                .as_deref()
                .and_then(|root| provider_file(root, path))
            else {
                continue;
            };
            let indexed = history.sessions.get(id);
            let fallback = if indexed.is_none() {
                discovery_metadata(&path).unwrap_or_default()
            } else {
                ClaudeMetadata::default()
            };
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
            let created_at = indexed
                .and_then(|entry| entry.timestamp)
                .or(fallback.created_at);
            let file_updated_at = std::fs::metadata(&path)
                .ok()
                .and_then(|metadata| metadata.modified().ok())
                .map(DateTime::<Utc>::from);
            sessions.push(NativeSession {
                session: SessionRef::new(Provider::Claude, id.clone()),
                title: None,
                project_path,
                git_branch: fallback.git_branch,
                created_at,
                updated_at: file_updated_at
                    .or_else(|| indexed.and_then(|entry| entry.timestamp))
                    .or(fallback.updated_at),
                updated_at_approximate: file_updated_at.is_some(),
                event_count: 0,
                source_path: Some(path),
            });
        }
        *self.notes.lock().unwrap_or_else(PoisonError::into_inner) = history.notes;
        sort_sessions(&mut sessions);
        Ok(sessions)
    }

    fn read_session(&self, session: &SessionRef) -> Result<omnis_ir::CanonicalSnapshot> {
        validate_provider(session, Provider::Claude)?;
        let path = self.find_session(&session.id)?;
        let mut records = Vec::new();
        let oversized_records =
            visit_json_lines(&path, MAX_COLLECTED_TRANSCRIPT_FILE_SIZE, |record| {
                records.push(record);
                Ok(())
            })?;
        snapshot_from_records(session, &records, oversized_records)
    }

    fn preview_session(&self, session: &SessionRef) -> Result<omnis_ir::CanonicalSnapshot> {
        const SAMPLE_RECORDS: usize = 1_024;
        validate_provider(session, Provider::Claude)?;
        let path = self.find_session(&session.id)?;
        let records = json_lines_preview(&path, SAMPLE_RECORDS)?;
        snapshot_from_records(session, &records, 0)
    }

    fn new_session_plan(&self, target: &LaunchTarget) -> Result<LaunchPlan> {
        Ok(LaunchPlan {
            program: "claude".to_owned(),
            args: target.prompt.iter().cloned().collect(),
            cwd: target.cwd.clone(),
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
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{events, is_sidechain_session, metadata};
    use omnis_ir::{EventKind, ReplayPolicy};

    #[test]
    fn fixture_canonicalizes_visible_messages_and_historical_tools() {
        let records = include_str!("../tests/fixtures/claude-session.jsonl")
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect::<Vec<_>>();
        let metadata = metadata(&records);
        let events = events(&records);

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

        assert!(is_sidechain_session(&records));
        assert!(events(&records).is_empty());
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

        let events = events(&records);

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, EventKind::CompactionCreated);
        assert_eq!(events[0].replay_policy, ReplayPolicy::Contextual);
        assert_eq!(
            events[0].payload["text"],
            "Current objective: finish synthetic migration"
        );
    }
}
