use std::{
    collections::HashMap,
    path::Path,
    path::PathBuf,
    sync::{Arc, OnceLock},
};

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use omnis_ir::{EventKind, Provider, ReplayPolicy, SessionRef};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    LaunchPlan, LaunchTarget, NativeSession, ProviderAdapter, ProviderInstallation,
    support::{
        EventBuilder, executable, json_lines, json_lines_preview, nested_files, parse_timestamp,
        paths_match, provider_file, provider_root, sort_sessions, string_at, validate_provider,
        value_at,
    },
};

#[derive(Clone, Debug)]
pub struct ClaudeAdapter {
    projects_root: Option<PathBuf>,
    session_files: Arc<OnceLock<Vec<(String, PathBuf)>>>,
}

impl ClaudeAdapter {
    #[must_use]
    pub fn with_root(projects_root: impl Into<PathBuf>) -> Self {
        Self {
            projects_root: Some(projects_root.into()),
            session_files: Arc::default(),
        }
    }

    fn discover_session_files(&self) -> Vec<(String, PathBuf)> {
        self.projects_root
            .as_deref()
            .map(|root| nested_files(root, 8, None))
            .unwrap_or_default()
            .into_iter()
            .filter_map(|path| {
                if path
                    .extension()
                    .is_none_or(|extension| extension != "jsonl")
                    || path
                        .components()
                        .any(|component| component.as_os_str() == "subagents")
                {
                    return None;
                }
                let id = path.file_stem()?.to_str()?;
                Uuid::parse_str(id).ok()?;
                Some((id.to_owned(), path))
            })
            .collect()
    }

    fn session_files(&self) -> &[(String, PathBuf)] {
        self.session_files
            .get_or_init(|| self.discover_session_files())
    }

    fn find_session(&self, id: &str) -> Result<PathBuf> {
        Uuid::parse_str(id).context("Claude session ID must be a UUID")?;
        self.session_files()
            .iter()
            .find_map(|(candidate, path)| (candidate == id).then(|| path.clone()))
            .or_else(|| {
                self.discover_session_files()
                    .into_iter()
                    .find_map(|(candidate, path)| (candidate == id).then_some(path))
            })
            .ok_or_else(|| anyhow!("Claude session `{id}` was not found"))
    }

    fn history_index(&self) -> HashMap<String, (PathBuf, Option<DateTime<Utc>>)> {
        let Some(config_root) = self.projects_root.as_deref().and_then(Path::parent) else {
            return HashMap::new();
        };
        let Some(history) = provider_file(config_root, &config_root.join("history.jsonl")) else {
            return HashMap::new();
        };
        let mut index = HashMap::new();
        for record in json_lines(&history).unwrap_or_default() {
            let Some(id) = string_at(&record, &[&["sessionId"]]) else {
                continue;
            };
            let Some(project) = string_at(&record, &[&["project"]]) else {
                continue;
            };
            index.insert(
                id.to_owned(),
                (
                    PathBuf::from(project),
                    parse_timestamp(record.get("timestamp")),
                ),
            );
        }
        index
    }
}

impl Default for ClaudeAdapter {
    fn default() -> Self {
        Self {
            projects_root: provider_root("CLAUDE_CONFIG_DIR", &[".claude"])
                .map(|root| root.join("projects")),
            session_files: Arc::default(),
        }
    }
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

    fn list_sessions(&self, project: Option<&Path>) -> Result<Vec<NativeSession>> {
        let history = self.history_index();
        let mut sessions = Vec::new();
        for (id, path) in self.session_files() {
            let indexed = history.get(id);
            if project.is_some_and(|project| {
                indexed
                    .map(|(recorded, _)| recorded.as_path())
                    .is_none_or(|recorded| !paths_match(recorded, project))
            }) {
                continue;
            }
            sessions.push(NativeSession {
                session: SessionRef::new(Provider::Claude, id.clone()),
                title: None,
                project_path: indexed.map(|(project, _)| project.clone()),
                git_branch: None,
                created_at: indexed.and_then(|(_, timestamp)| *timestamp),
                updated_at: std::fs::metadata(path)
                    .ok()
                    .and_then(|metadata| metadata.modified().ok())
                    .map(DateTime::<Utc>::from)
                    .or_else(|| indexed.and_then(|(_, timestamp)| *timestamp)),
                event_count: 0,
                source_path: Some(path.clone()),
            });
        }
        sort_sessions(&mut sessions);
        Ok(sessions)
    }

    fn read_session(&self, session: &SessionRef) -> Result<omnis_ir::CanonicalSnapshot> {
        validate_provider(session, Provider::Claude)?;
        let path = self.find_session(&session.id)?;
        let records = json_lines(&path)?;
        snapshot_from_records(session, &records)
    }

    fn preview_session(&self, session: &SessionRef) -> Result<omnis_ir::CanonicalSnapshot> {
        const SAMPLE_RECORDS: usize = 1_024;
        validate_provider(session, Provider::Claude)?;
        let path = self.find_session(&session.id)?;
        let records = json_lines_preview(&path, SAMPLE_RECORDS)?;
        snapshot_from_records(session, &records)
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
