use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use omnis_ir::{EventKind, Provider, ReplayPolicy, SessionRef};
use serde_json::{Value, json};

use crate::{
    LaunchPlan, LaunchTarget, NativeSession, ProviderAdapter, ProviderInstallation,
    support::{
        EventBuilder, MAX_COLLECTED_TRANSCRIPT_FILE_SIZE, json_lines_preview, nested_files,
        parse_timestamp, paths_match, provider_executable, provider_file, provider_root, read_json,
        sort_sessions, sqlite_snapshot, string_at, validate_provider, value_at, visit_json_lines,
    },
};

#[derive(Clone, Debug)]
pub struct GrokAdapter {
    sessions_root: Option<PathBuf>,
}

impl GrokAdapter {
    #[must_use]
    pub fn with_root(sessions_root: impl Into<PathBuf>) -> Self {
        Self {
            sessions_root: Some(sessions_root.into()),
        }
    }

    fn summaries(&self) -> Vec<PathBuf> {
        self.sessions_root
            .as_deref()
            .map(|root| nested_files(root, 2, Some("summary.json")))
            .unwrap_or_default()
    }

    fn find_summary(&self, id: &str) -> Result<(Value, PathBuf)> {
        let read_matching = |path: PathBuf| {
            let value = read_json(&path).ok()?;
            (session_id(&value, &path).as_deref() == Some(id)).then_some((value, path))
        };
        self.direct_summary(id)
            .and_then(read_matching)
            .or_else(|| {
                self.summaries().into_iter().find_map(|path| {
                    let value = read_json(&path).ok()?;
                    (session_id(&value, &path).as_deref() == Some(id)).then_some((value, path))
                })
            })
            .ok_or_else(|| anyhow!("Grok session `{id}` was not found"))
    }

    fn direct_summary(&self, id: &str) -> Option<PathBuf> {
        uuid::Uuid::parse_str(id).ok()?;
        let root = self.sessions_root.as_deref()?;
        let entries = fs::read_dir(root).ok()?;
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_dir() || file_type.is_symlink() {
                continue;
            }
            let candidate = entry.path().join(id).join("summary.json");
            if let Some(candidate) = provider_file(root, &candidate) {
                return Some(candidate);
            }
        }
        None
    }

    fn catalog_sessions(&self, project: Option<&Path>) -> Option<Vec<NativeSession>> {
        let root = self.sessions_root.as_deref()?;
        let database = provider_file(root, &root.join("session_search.sqlite"))?;
        let snapshot = sqlite_snapshot(root, &database).ok()?;
        let mut statement = snapshot
            .connection
            .prepare("SELECT session_id, cwd, updated_at, title FROM session_docs")
            .ok()?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            })
            .ok()?;
        let mut sessions = Vec::new();
        for row in rows.flatten() {
            let (id, cwd, updated_at, title) = row;
            if uuid::Uuid::parse_str(&id).is_err() {
                continue;
            }
            let project_path = PathBuf::from(cwd);
            if project.is_some_and(|project| !paths_match(&project_path, project)) {
                continue;
            }
            sessions.push(NativeSession {
                session: SessionRef::new(Provider::Grok, id),
                title,
                project_path: Some(project_path),
                git_branch: None,
                created_at: None,
                updated_at: DateTime::from_timestamp(updated_at, 0),
                updated_at_approximate: false,
                event_count: 0,
                source_path: None,
            });
        }
        Some(sessions)
    }
}

impl Default for GrokAdapter {
    fn default() -> Self {
        Self {
            sessions_root: provider_root("GROK_HOME", &[".grok"]).map(|root| root.join("sessions")),
        }
    }
}

#[derive(Default)]
struct GrokMetadata {
    title: Option<String>,
    project_path: Option<PathBuf>,
    git_branch: Option<String>,
    created_at: Option<DateTime<Utc>>,
    updated_at: Option<DateTime<Utc>>,
}

struct PendingMessage {
    kind: EventKind,
    text: String,
    content: Vec<Value>,
    timestamp: Option<DateTime<Utc>>,
    raw_type: Option<String>,
    message_id: Option<String>,
}

fn session_id(summary: &Value, path: &Path) -> Option<String> {
    string_at(
        summary,
        &[
            &["id"],
            &["session_id"],
            &["sessionId"],
            &["info", "id"],
            &["session", "id"],
        ],
    )
    .map(str::to_owned)
    .or_else(|| {
        path.parent()?
            .file_name()?
            .to_str()
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
    })
}

fn metadata(summary: &Value) -> GrokMetadata {
    GrokMetadata {
        title: string_at(
            summary,
            &[
                &["generated_title"],
                &["session_summary"],
                &["title"],
                &["name"],
                &["session", "title"],
                &["metadata", "title"],
            ],
        )
        .map(str::to_owned),
        project_path: string_at(
            summary,
            &[
                &["info", "cwd"],
                &["cwd"],
                &["project_path"],
                &["projectPath"],
                &["session", "cwd"],
                &["metadata", "cwd"],
            ],
        )
        .map(PathBuf::from),
        git_branch: string_at(
            summary,
            &[
                &["git_branch"],
                &["gitBranch"],
                &["head_branch"],
                &["git", "branch"],
                &["metadata", "git_branch"],
            ],
        )
        .map(str::to_owned),
        created_at: parse_timestamp(value_at(
            summary,
            &[
                &["created_at"],
                &["createdAt"],
                &["created"],
                &["time", "created"],
            ],
        )),
        updated_at: parse_timestamp(value_at(
            summary,
            &[
                &["updated_at"],
                &["updatedAt"],
                &["updated"],
                &["time", "updated"],
            ],
        )),
    }
}

fn declared_message_count(summary: &Value) -> u64 {
    value_at(
        summary,
        &[
            &["num_chat_messages"],
            &["num_messages"],
            &["message_count"],
            &["session", "num_messages"],
        ],
    )
    .and_then(Value::as_u64)
    .unwrap_or_default()
}

#[derive(Default)]
struct GrokUpdateProcessor {
    pending: Option<PendingMessage>,
    preservation: GrokPreservationBudget,
    unsupported_text_records: usize,
}

struct GrokPreservationBudget {
    bytes_left: usize,
    records_left: usize,
    omitted_events: usize,
}

impl Default for GrokPreservationBudget {
    fn default() -> Self {
        Self {
            bytes_left: 64 * 1024 * 1024,
            records_left: 100_000,
            omitted_events: 0,
        }
    }
}

impl GrokPreservationBudget {
    fn retain(&mut self, value: &Value) -> bool {
        let bytes = estimated_value_size(value, self.bytes_left);
        if self.records_left == 0 || bytes > self.bytes_left {
            self.omitted_events = self.omitted_events.saturating_add(1);
            return false;
        }
        self.records_left -= 1;
        self.bytes_left -= bytes;
        true
    }
}

fn estimated_value_size(value: &Value, limit: usize) -> usize {
    // Charge JSON allocations conservatively without serializing large historical payloads.
    let mut size = 256_usize;
    match value {
        Value::String(text) => size = size.saturating_add(text.len()),
        Value::Array(values) => {
            for value in values {
                size = size.saturating_add(estimated_value_size(value, limit.saturating_sub(size)));
                if size > limit {
                    break;
                }
            }
        }
        Value::Object(values) => {
            for (key, value) in values {
                size = size
                    .saturating_add(key.len())
                    .saturating_add(estimated_value_size(value, limit.saturating_sub(size)));
                if size > limit {
                    break;
                }
            }
        }
        _ => {}
    }
    size
}

impl GrokUpdateProcessor {
    fn push_record(&mut self, builder: &mut EventBuilder, mut record: Value) {
        let timestamp = parse_timestamp(value_at(
            &record,
            &[&["timestamp"], &["created_at"], &["createdAt"]],
        ));
        let nested_update = record
            .as_object_mut()
            .and_then(|object| object.get_mut("params"))
            .and_then(Value::as_object_mut)
            .and_then(|params| params.remove("update"));
        let update = nested_update.unwrap_or(record);
        let raw_type =
            string_at(&update, &[&["sessionUpdate"], &["type"], &["event"]]).map(str::to_owned);
        let update_type = raw_type.as_deref().unwrap_or("");
        let message_kind = match update_type {
            "user_message_chunk" | "user" | "user_message" => Some(EventKind::MessageUser),
            "agent_message_chunk" | "assistant" | "assistant_message" => {
                Some(EventKind::MessageAssistant)
            }
            "agent_thought_chunk" => Some(EventKind::ReasoningSummary),
            _ => None,
        };
        if let Some(kind) = message_kind {
            if kind == EventKind::ReasoningSummary && !self.preservation.retain(&update) {
                flush_message(builder, &mut self.pending);
                return;
            }
            let message_id = string_at(
                &update,
                &[&["messageId"], &["message_id"], &["message", "id"]],
            )
            .map(str::to_owned);
            let (text, content) = message_content(&update, &mut self.preservation);
            match &mut self.pending {
                Some(pending) if pending.kind == kind && pending.message_id == message_id => {
                    pending.text.push_str(&text);
                    pending.content.extend(content);
                }
                _ => {
                    flush_message(builder, &mut self.pending);
                    self.pending = Some(PendingMessage {
                        kind,
                        text,
                        content,
                        timestamp,
                        raw_type,
                        message_id,
                    });
                }
            }
            return;
        }

        flush_message(builder, &mut self.pending);
        let kind = nonmessage_kind(update_type, string_at(&update, &[&["status"]]));
        if let Some(kind) = kind {
            if matches!(kind, EventKind::ProviderEvent | EventKind::PlanUpdated)
                && update_type != "tool_call_update"
                && !self.preservation.retain(&update)
            {
                return;
            }
            if kind == EventKind::ProviderEvent && update_type != "tool_call_update" {
                self.unsupported_text_records = self.unsupported_text_records.saturating_add(1);
            }
            let mut update = update;
            if kind == EventKind::PlanUpdated
                && let Some(entries) = update
                    .as_object_mut()
                    .and_then(|object| object.remove("entries"))
            {
                update["plan"] = entries;
            }
            builder.push(
                kind,
                update,
                timestamp,
                ReplayPolicy::HistoricalOnly,
                raw_type,
                None,
            );
        }
    }

    fn finish(&mut self, builder: &mut EventBuilder) {
        flush_message(builder, &mut self.pending);
        let omitted_events = std::mem::take(&mut self.preservation.omitted_events);
        if omitted_events > 0 {
            builder.push(EventKind::ProviderEvent,
                json!({"omitted_events": omitted_events,
                    "reason": "Grok nontext, reasoning, plan, and opaque historical records exceeded the collection budget"}),
                None, ReplayPolicy::HistoricalOnly,
                Some("omnisession.collection_limit".to_owned()), None);
        }
        let unsupported_text_records = std::mem::take(&mut self.unsupported_text_records);
        if unsupported_text_records > 0 {
            builder.push(EventKind::ProviderEvent,
                json!({"unsupported_text_records": unsupported_text_records,
                    "raw_records_preserved": true,
                    "reason": "Unknown Grok updates remain historical metadata; their text cannot be transferred safely"}),
                None, ReplayPolicy::HistoricalOnly,
                Some("omnisession.unsupported_history".to_owned()), None);
        }
    }
}

fn nonmessage_kind(update_type: &str, status: Option<&str>) -> Option<EventKind> {
    match update_type {
        "tool_call" | "tool_use" => Some(EventKind::ToolCalled),
        "tool_result" | "tool_completed" => {
            let failed = status.is_some_and(|status| matches!(status, "failed" | "error"));
            Some(if failed {
                EventKind::ToolFailed
            } else {
                EventKind::ToolCompleted
            })
        }
        "tool_call_update" => Some(match status {
            Some("failed" | "error") => EventKind::ToolFailed,
            Some("completed" | "complete" | "success" | "succeeded" | "done") => {
                EventKind::ToolCompleted
            }
            _ => EventKind::ProviderEvent,
        }),
        "tool_error" | "tool_failed" => Some(EventKind::ToolFailed),
        "command" | "command_executed" => Some(EventKind::CommandExecuted),
        "plan" => Some(EventKind::PlanUpdated),
        "available_commands_update" | "current_mode_update" | "config_option_update" => None,
        _ => Some(EventKind::ProviderEvent),
    }
}

fn message_content(
    update: &Value,
    preservation: &mut GrokPreservationBudget,
) -> (String, Vec<Value>) {
    let mut text = String::new();
    let mut historical_content = Vec::new();
    let mut push = |block: &Value| {
        if let Some(chunk) = block
            .as_str()
            .or_else(|| block.get("text").and_then(Value::as_str))
        {
            text.push_str(chunk);
        } else if !block.is_null() && preservation.retain(block) {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(match block.get("type").and_then(Value::as_str) {
                Some("image") => "[Source image attachment preserved as historical metadata]",
                Some("audio") => "[Source audio attachment preserved as historical metadata]",
                _ => "[Source nontext content preserved as historical metadata]",
            });
            historical_content.push(block.clone());
        }
    };
    if let Some(content) = update.get("content") {
        if let Some(blocks) = content.as_array() {
            for block in blocks {
                push(block);
            }
        } else {
            push(content);
        }
    } else if let Some(fallback) = value_at(
        update,
        &[&["text"], &["message", "content"], &["message", "text"]],
    ) {
        push(fallback);
    }
    (text, historical_content)
}

fn push_updates(builder: &mut EventBuilder, updates: impl IntoIterator<Item = Value>) {
    let mut processor = GrokUpdateProcessor::default();
    for record in updates {
        processor.push_record(builder, record);
    }
    processor.finish(builder);
}

fn push_grok_session_metadata(builder: &mut EventBuilder, summary: &Value) {
    let model = string_at(
        summary,
        &[
            &["current_model_id"],
            &["model"],
            &["model_id"],
            &["metadata", "model"],
        ],
    );
    let reasoning_mode = string_at(
        summary,
        &[
            &["reasoning_effort"],
            &["reasoning_mode"],
            &["metadata", "reasoning_effort"],
        ],
    );
    let total_tokens = value_at(
        summary,
        &[
            &["total_tokens"],
            &["usage", "total_tokens"],
            &["metadata", "total_tokens"],
        ],
    )
    .and_then(Value::as_u64);
    let parent_session_id = summary.get("parent_session_id").and_then(Value::as_str);
    let session_kind = summary.get("session_kind").and_then(Value::as_str);
    if model.is_none()
        && reasoning_mode.is_none()
        && total_tokens.is_none()
        && parent_session_id.is_none()
        && session_kind.is_none()
    {
        return;
    }
    builder.push(
        EventKind::ProviderEvent,
        json!({
            "model": model,
            "reasoning_mode": reasoning_mode,
            "total_tokens": total_tokens,
            "token_usage": "cumulative",
            "parent_session_id": parent_session_id,
            "session_kind": session_kind,
        }),
        None,
        ReplayPolicy::HistoricalOnly,
        Some("omnisession.session_metadata".to_owned()),
        None,
    );
}

fn push_grok_state(builder: &mut EventBuilder, root: &Path, summary_path: &Path) {
    let Some(directory) = summary_path.parent() else {
        return;
    };
    for (filename, kind, record_type, field) in [
        (
            "plan.json",
            EventKind::PlanUpdated,
            "grok.plan_state",
            "plan",
        ),
        (
            "goal/state.json",
            EventKind::ProviderEvent,
            "grok.goal_state",
            "goal",
        ),
        (
            "usage.json",
            EventKind::ProviderEvent,
            "grok.usage_state",
            "usage",
        ),
    ] {
        let Some(path) = provider_file(root, &directory.join(filename)) else {
            continue;
        };
        let Ok(value) = read_json(&path) else {
            continue;
        };
        builder.push(
            kind,
            json!({field: value}),
            None,
            ReplayPolicy::HistoricalOnly,
            Some(record_type.to_owned()),
            None,
        );
    }
}

fn flush_message(builder: &mut EventBuilder, pending: &mut Option<PendingMessage>) {
    let Some(pending) = pending.take() else {
        return;
    };
    if !pending.text.is_empty() {
        let mut payload = json!({ "text": pending.text });
        let nontext_blocks = pending.content.len();
        if !pending.content.is_empty() {
            payload["content"] = Value::Array(pending.content);
        }
        let replay_policy = if pending.kind == EventKind::ReasoningSummary {
            ReplayPolicy::HistoricalOnly
        } else {
            ReplayPolicy::Contextual
        };
        builder.push(
            pending.kind,
            payload,
            pending.timestamp,
            replay_policy,
            pending.raw_type,
            None,
        );
        if nontext_blocks > 0 {
            builder.push(
                EventKind::ProviderEvent,
                json!({
                    "omitted_events": nontext_blocks,
                    "event_kind": "attachment",
                    "reason": "Attachments remain in canonical historical metadata; native transfer carries documentary text only",
                }),
                pending.timestamp,
                ReplayPolicy::HistoricalOnly,
                Some("omnisession.unsupported_content".to_owned()),
                None,
            );
        }
    }
}

impl ProviderAdapter for GrokAdapter {
    fn provider(&self) -> Provider {
        Provider::Grok
    }

    fn probe(&self) -> ProviderInstallation {
        let executable = provider_executable(Provider::Grok);
        ProviderInstallation {
            provider: Provider::Grok,
            installed: executable.is_some()
                || self.sessions_root.as_deref().is_some_and(Path::is_dir),
            executable,
            data_root: self.sessions_root.clone(),
        }
    }

    fn list_sessions(&self, project: Option<&Path>) -> Result<Vec<NativeSession>> {
        let mut sessions = self.catalog_sessions(project).unwrap_or_default();
        let positions = sessions
            .iter()
            .enumerate()
            .map(|(index, session)| (session.session.id.clone(), index))
            .collect::<HashMap<_, _>>();
        let mut seen_summaries = HashSet::new();
        for path in self.summaries() {
            let summary: Value = match read_json(&path) {
                Ok(summary) => summary,
                Err(_) => continue,
            };
            let Some(id) = session_id(&summary, &path) else {
                continue;
            };
            let metadata = metadata(&summary);
            if !seen_summaries.insert(id.clone()) {
                continue;
            }
            let index = positions.get(&id).copied();
            let session = NativeSession {
                session: SessionRef::new(Provider::Grok, id),
                title: metadata.title,
                project_path: metadata.project_path,
                git_branch: metadata.git_branch,
                created_at: metadata.created_at,
                updated_at: metadata.updated_at,
                updated_at_approximate: false,
                event_count: 0,
                source_path: Some(path),
            };
            if let Some(index) = index {
                sessions[index] = session;
            } else {
                sessions.push(session);
            }
        }
        sessions.retain(|session| {
            project.is_none_or(|project| {
                session
                    .project_path
                    .as_deref()
                    .is_some_and(|recorded| paths_match(recorded, project))
            })
        });
        sort_sessions(&mut sessions);
        Ok(sessions)
    }

    fn fork_candidates_created_since(
        &self,
        source: &SessionRef,
        project: &Path,
        started_at: DateTime<Utc>,
    ) -> Result<Vec<SessionRef>> {
        validate_provider(source, Provider::Grok)?;
        uuid::Uuid::parse_str(&source.id).context("Grok fork source is not a UUID")?;
        let mut candidates = Vec::new();
        for path in self.summaries() {
            let Ok(summary) = read_json(&path) else {
                continue;
            };
            if summary.get("parent_session_id").and_then(Value::as_str) != Some(source.id.as_str())
            {
                continue;
            }
            let Some(id) = session_id(&summary, &path) else {
                continue;
            };
            if id == source.id || uuid::Uuid::parse_str(&id).is_err() {
                continue;
            }
            let metadata = metadata(&summary);
            if metadata
                .created_at
                .is_none_or(|created_at| created_at < started_at)
                || metadata
                    .project_path
                    .as_deref()
                    .is_none_or(|cwd| !paths_match(cwd, project))
            {
                continue;
            }
            candidates.push(SessionRef::new(Provider::Grok, id));
        }
        Ok(candidates)
    }

    fn read_session(&self, session: &SessionRef) -> Result<omnis_ir::CanonicalSnapshot> {
        validate_provider(session, Provider::Grok)?;
        let (summary, path) = self.find_summary(&session.id)?;
        let metadata = metadata(&summary);
        let updates_path = path
            .parent()
            .context("Grok summary has no parent directory")?
            .join("updates.jsonl");
        let mut builder = EventBuilder::new(Provider::Grok, &session.id);
        push_grok_session_metadata(&mut builder, &summary);
        let mut updates = GrokUpdateProcessor::default();
        let mut has_readable_update = false;
        let oversized_records = if updates_path.is_file() {
            visit_json_lines(
                &updates_path,
                MAX_COLLECTED_TRANSCRIPT_FILE_SIZE,
                |record| {
                    has_readable_update = true;
                    updates.push_record(&mut builder, record);
                    Ok(())
                },
            )?
        } else {
            0
        };
        if !has_readable_update && declared_message_count(&summary) > 0 {
            return Err(anyhow!(
                "Grok session `{}` declares history but has no readable updates",
                session.id
            ));
        }
        let captured_at = metadata.updated_at.unwrap_or_else(Utc::now);
        updates.finish(&mut builder);
        if let Some(root) = self.sessions_root.as_deref() {
            push_grok_state(&mut builder, root, &path);
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

    fn preview_session(&self, session: &SessionRef) -> Result<omnis_ir::CanonicalSnapshot> {
        const SAMPLE_RECORDS: usize = 1_024;
        validate_provider(session, Provider::Grok)?;
        let (summary, path) = self.find_summary(&session.id)?;
        let metadata = metadata(&summary);
        let updates_path = path
            .parent()
            .context("Grok summary has no parent directory")?
            .join("updates.jsonl");
        let updates = if updates_path.is_file() {
            json_lines_preview(&updates_path, SAMPLE_RECORDS)?
        } else {
            Vec::new()
        };
        if updates.is_empty() && declared_message_count(&summary) > 0 {
            return Err(anyhow!(
                "Grok session `{}` declares history but has no readable updates",
                session.id
            ));
        }
        let captured_at = metadata.updated_at.unwrap_or_else(Utc::now);
        let mut builder = EventBuilder::new(Provider::Grok, &session.id);
        push_grok_session_metadata(&mut builder, &summary);
        push_updates(&mut builder, updates);
        if let Some(root) = self.sessions_root.as_deref() {
            push_grok_state(&mut builder, root, &path);
        }
        Ok(builder.snapshot(
            session.clone(),
            metadata.title,
            metadata.project_path,
            metadata.git_branch,
            captured_at,
        ))
    }

    fn new_session_plan(&self, target: &LaunchTarget) -> Result<LaunchPlan> {
        Ok(LaunchPlan {
            program: "grok".to_owned(),
            args: target.prompt.iter().cloned().collect(),
            cwd: target.cwd.clone(),
            env: Vec::new(),
        })
    }

    fn launch_plan(&self, session: &SessionRef, target: &LaunchTarget) -> Result<LaunchPlan> {
        validate_provider(session, Provider::Grok)?;
        let mut args = vec!["--resume".to_owned(), session.id.clone()];
        if target.fork {
            args.push("--fork-session".to_owned());
        }
        if let Some(prompt) = &target.prompt {
            args.push(prompt.clone());
        }
        Ok(LaunchPlan {
            program: "grok".to_owned(),
            args,
            cwd: target.cwd.clone(),
            env: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SESSION_ID: &str = "77777777-7777-4777-8777-777777777777";

    fn write_session(root: &Path, id: &str, summary: &Value, updates: &[Value]) -> PathBuf {
        let directory = root.join("workspace-hash").join(id);
        fs::create_dir_all(&directory).expect("synthetic session directory");
        fs::write(directory.join("summary.json"), summary.to_string()).expect("synthetic summary");
        fs::write(
            directory.join("updates.jsonl"),
            updates
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .expect("synthetic updates");
        directory
    }

    #[test]
    fn incomplete_search_catalog_does_not_hide_native_summaries() {
        for has_indexed_session in [false, true] {
            let root = tempfile::tempdir().expect("synthetic Grok root");
            let database = root.path().join("session_search.sqlite");
            let connection = rusqlite::Connection::open(&database).expect("synthetic catalog");
            connection
                .execute_batch(
                    "CREATE TABLE session_docs (session_id TEXT PRIMARY KEY, cwd TEXT NOT NULL,
                 updated_at INTEGER NOT NULL, title TEXT);",
                )
                .expect("synthetic catalog schema");
            if has_indexed_session {
                connection
                    .execute(
                        "INSERT INTO session_docs VALUES (?1, ?2, ?3, ?4)",
                        rusqlite::params![
                            SESSION_ID,
                            "/workspace/demo",
                            1_785_000_000_i64,
                            "Indexed"
                        ],
                    )
                    .expect("indexed session");
            }
            write_session(
                root.path(),
                SESSION_ID,
                &json!({"id": SESSION_ID, "cwd": "/workspace/demo", "title": "Summary"}),
                &[],
            );
            let missing = "88888888-8888-4888-8888-888888888888";
            write_session(
                root.path(),
                missing,
                &json!({"id": missing, "cwd": "/workspace/demo", "title": "Missing from catalog"}),
                &[],
            );

            let sessions = GrokAdapter::with_root(root.path())
                .list_sessions(Some(Path::new("/workspace/demo")))
                .expect("combined Grok discovery");
            assert_eq!(sessions.len(), 2, "catalog complete: {has_indexed_session}");
            assert_eq!(
                sessions
                    .iter()
                    .filter(|session| session.session.id == SESSION_ID)
                    .count(),
                1
            );
            assert!(sessions.iter().any(|session| session.session.id == missing));
        }
    }

    #[test]
    fn nontext_messages_and_historical_updates_survive_readback() {
        let root = tempfile::tempdir().expect("synthetic Grok root");
        let records = [
            json!({"sessionUpdate": "user_message_chunk", "messageId": "image-turn",
                "content": {"type": "image", "mimeType": "image/png", "data": "synthetic-image"}}),
            json!({"sessionUpdate": "plan", "entries": [{"content": "Synthetic next action", "status": "pending", "priority": "high"}]}),
            json!({"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": "Synthetic reasoning"}}),
            json!({"sessionUpdate": "future_historical_record", "historical": "Synthetic opaque event"}),
        ];
        write_session(
            root.path(),
            SESSION_ID,
            &json!({"id": SESSION_ID, "cwd": "/workspace/demo"}),
            &records,
        );
        let snapshot = GrokAdapter::with_root(root.path())
            .read_session(&SessionRef::new(Provider::Grok, SESSION_ID))
            .expect("Grok snapshot");
        assert_eq!(
            snapshot
                .events
                .iter()
                .map(|event| &event.kind)
                .collect::<Vec<_>>(),
            [
                &EventKind::MessageUser,
                &EventKind::ProviderEvent,
                &EventKind::PlanUpdated,
                &EventKind::ReasoningSummary,
                &EventKind::ProviderEvent,
                &EventKind::ProviderEvent
            ]
        );
        let image = &snapshot.events[0];
        assert!(
            image.payload["text"]
                .as_str()
                .expect("visible attachment marker")
                .contains("image")
        );
        assert_eq!(image.payload["content"][0]["data"], "synthetic-image");
        assert_eq!(image.replay_policy, ReplayPolicy::Contextual);
        for event in &snapshot.events[1..] {
            assert_eq!(event.replay_policy, ReplayPolicy::HistoricalOnly);
        }
        assert!(omnis_core::import_trajectory(&snapshot).truncated);
        assert_eq!(snapshot.events[1].payload["omitted_events"], 1);
    }

    #[test]
    fn unknown_text_updates_are_preserved_and_report_unsupported_transfer() {
        let root = tempfile::tempdir().expect("synthetic Grok root");
        let records = [
            json!({"sessionUpdate": "user_message_chunk", "content": {"type": "text", "text": "Synthetic known question"}}),
            json!({"sessionUpdate": "future_message_chunk", "data": {"text": "Synthetic future text"}}),
            json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "Synthetic known answer"}}),
        ];
        write_session(
            root.path(),
            SESSION_ID,
            &json!({"id": SESSION_ID, "cwd": "/workspace/demo"}),
            &records,
        );
        let snapshot = GrokAdapter::with_root(root.path())
            .read_session(&SessionRef::new(Provider::Grok, SESSION_ID))
            .expect("unknown text read-back");
        assert_eq!(
            snapshot.events[1].payload["data"]["text"],
            "Synthetic future text"
        );
        assert_eq!(
            snapshot.events[1].replay_policy,
            ReplayPolicy::HistoricalOnly
        );
        assert_eq!(
            snapshot
                .events
                .last()
                .expect("unsupported history notice")
                .payload["unsupported_text_records"],
            1
        );
        assert!(omnis_core::import_trajectory(&snapshot).truncated);
        assert!(omnis_core::import_conversation(&snapshot).truncated);
    }

    #[test]
    fn native_summary_overrides_stale_indexed_workspace() {
        let root = tempfile::tempdir().expect("synthetic Grok root");
        let connection = rusqlite::Connection::open(root.path().join("session_search.sqlite"))
            .expect("synthetic catalog");
        connection.execute_batch("CREATE TABLE session_docs (session_id TEXT PRIMARY KEY, cwd TEXT NOT NULL, updated_at INTEGER NOT NULL, title TEXT);")
            .expect("catalog schema");
        connection
            .execute(
                "INSERT INTO session_docs VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![
                    SESSION_ID,
                    "/workspace/old",
                    1_785_000_000_i64,
                    "Stale title"
                ],
            )
            .expect("stale catalog row");
        write_session(
            root.path(),
            SESSION_ID,
            &json!({"id": SESSION_ID, "info": {"cwd": "/workspace/current"}, "title": "Current title"}),
            &[],
        );
        let adapter = GrokAdapter::with_root(root.path());
        assert!(
            adapter
                .list_sessions(Some(Path::new("/workspace/old")))
                .expect("old workspace filter")
                .is_empty()
        );
        let current = adapter
            .list_sessions(Some(Path::new("/workspace/current")))
            .expect("current workspace filter");
        assert_eq!(current.len(), 1);
        assert_eq!(current[0].title.as_deref(), Some("Current title"));
        assert!(current[0].source_path.is_some());
    }

    #[test]
    fn fork_candidates_require_exact_parent_workspace_and_creation_time() {
        let root = tempfile::tempdir().expect("synthetic Grok root");
        let project = root.path().join("workspace");
        let other_project = root.path().join("other-workspace");
        fs::create_dir_all(&project).expect("workspace");
        fs::create_dir_all(&other_project).expect("other workspace");
        let started_at = DateTime::parse_from_rfc3339("2026-01-01T00:00:01Z")
            .expect("start time")
            .with_timezone(&Utc);
        let children = [
            (
                "11111111-1111-4111-8111-111111111111",
                SESSION_ID,
                &project,
                "2026-01-01T00:00:02Z",
            ),
            (
                "22222222-2222-4222-8222-222222222222",
                SESSION_ID,
                &other_project,
                "2026-01-01T00:00:02Z",
            ),
            (
                "33333333-3333-4333-8333-333333333333",
                SESSION_ID,
                &project,
                "2026-01-01T00:00:00Z",
            ),
            (
                "44444444-4444-4444-8444-444444444444",
                "unrelated-parent",
                &project,
                "2026-01-01T00:00:02Z",
            ),
        ];
        for (id, parent, cwd, created) in children {
            write_session(
                root.path(),
                id,
                &json!({"id": id, "parent_session_id": parent, "info": {"cwd": cwd}, "created_at": created}),
                &[],
            );
        }
        let adapter = GrokAdapter::with_root(root.path());
        let source = SessionRef::new(Provider::Grok, SESSION_ID);
        let candidates = adapter
            .fork_candidates_created_since(&source, &project, started_at)
            .expect("native fork candidates");
        assert_eq!(candidates, [SessionRef::new(Provider::Grok, children[0].0)]);
        let ambiguous = "55555555-5555-4555-8555-555555555555";
        write_session(
            root.path(),
            ambiguous,
            &json!({"id": ambiguous, "parent_session_id": SESSION_ID, "info": {"cwd": project}, "created_at": "2026-01-01T00:00:03Z"}),
            &[],
        );
        assert_eq!(
            adapter
                .fork_candidates_created_since(&source, &project, started_at)
                .expect("ambiguous candidates remain explicit")
                .len(),
            2
        );
    }

    #[test]
    fn known_state_sidecars_remain_historical_and_preview_matches() {
        let root = tempfile::tempdir().expect("synthetic Grok root");
        let directory = write_session(
            root.path(),
            SESSION_ID,
            &json!({"id": SESSION_ID, "cwd": "/workspace/demo"}),
            &[],
        );
        fs::create_dir_all(directory.join("goal")).expect("goal directory");
        for (filename, value) in [
            (
                "plan.json",
                json!([{"content": "Synthetic next action", "status": "pending"}]),
            ),
            (
                "goal/state.json",
                json!({"objective": "Synthetic objective"}),
            ),
            ("usage.json", json!({"total_tokens": 123})),
        ] {
            fs::write(directory.join(filename), value.to_string()).expect("synthetic sidecar");
        }
        let adapter = GrokAdapter::with_root(root.path());
        let session = SessionRef::new(Provider::Grok, SESSION_ID);
        let snapshot = adapter.read_session(&session).expect("state read-back");
        assert_eq!(snapshot.events.len(), 3);
        assert_eq!(snapshot.events[0].kind, EventKind::PlanUpdated);
        assert_eq!(
            snapshot.events[0].payload["plan"][0]["content"],
            "Synthetic next action"
        );
        assert_eq!(
            snapshot.events[1].payload["goal"]["objective"],
            "Synthetic objective"
        );
        assert_eq!(snapshot.events[2].payload["usage"]["total_tokens"], 123);
        assert!(
            snapshot
                .events
                .iter()
                .all(|event| event.replay_policy == ReplayPolicy::HistoricalOnly)
        );
        assert_eq!(
            snapshot.events,
            adapter
                .preview_session(&session)
                .expect("state preview")
                .events
        );
    }

    #[test]
    fn historical_retention_budget_reports_loss_without_limiting_visible_text_or_tools() {
        let mut builder = EventBuilder::new(Provider::Grok, SESSION_ID);
        let mut processor = GrokUpdateProcessor {
            pending: None,
            unsupported_text_records: 0,
            preservation: GrokPreservationBudget {
                bytes_left: 2_048,
                records_left: 2,
                omitted_events: 0,
            },
        };
        for record in [
            json!({"sessionUpdate": "future_record", "text": "Small opaque history"}),
            json!({"sessionUpdate": "user_message_chunk", "content": {"type": "image", "data": "x".repeat(8_192)}}),
            json!({"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": "x".repeat(8_192)}}),
            json!({"sessionUpdate": "future_record", "text": "x".repeat(8_192)}),
            json!({"sessionUpdate": "user_message_chunk", "content": {"type": "text", "text": "Unchanged visible question"}}),
            json!({"sessionUpdate": "tool_call", "toolCallId": "synthetic", "rawInput": "x".repeat(8_192)}),
            json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "Unchanged visible answer"}}),
        ] {
            processor.push_record(&mut builder, record);
        }
        processor.finish(&mut builder);
        let snapshot = builder.snapshot(
            SessionRef::new(Provider::Grok, SESSION_ID),
            None,
            None,
            None,
            Utc::now(),
        );
        assert_eq!(
            snapshot
                .events
                .iter()
                .map(|event| &event.kind)
                .collect::<Vec<_>>(),
            [
                &EventKind::ProviderEvent,
                &EventKind::MessageUser,
                &EventKind::ToolCalled,
                &EventKind::MessageAssistant,
                &EventKind::ProviderEvent,
                &EventKind::ProviderEvent
            ]
        );
        assert_eq!(
            snapshot.events[1].payload["text"],
            "Unchanged visible question"
        );
        assert_eq!(
            snapshot.events[3].payload["text"],
            "Unchanged visible answer"
        );
        assert_eq!(snapshot.events[4].payload["omitted_events"], 3);
        assert_eq!(
            snapshot.events[4].source.raw_record_type.as_deref(),
            Some("omnisession.collection_limit")
        );
        assert!(omnis_core::import_trajectory(&snapshot).truncated);
    }

    #[test]
    fn historical_record_count_budget_emits_one_omission_notice() {
        let mut builder = EventBuilder::new(Provider::Grok, SESSION_ID);
        let mut processor = GrokUpdateProcessor {
            pending: None,
            unsupported_text_records: 0,
            preservation: GrokPreservationBudget {
                bytes_left: 64 * 1024,
                records_left: 1,
                omitted_events: 0,
            },
        };
        for index in 0..3 {
            processor.push_record(
                &mut builder,
                json!({"sessionUpdate": "future_record", "index": index}),
            );
        }
        processor.finish(&mut builder);
        let snapshot = builder.snapshot(
            SessionRef::new(Provider::Grok, SESSION_ID),
            None,
            None,
            None,
            Utc::now(),
        );
        assert_eq!(snapshot.events.len(), 3);
        assert_eq!(snapshot.events[0].payload["index"], 0);
        assert_eq!(snapshot.events[1].payload["omitted_events"], 2);
        assert_eq!(snapshot.events[2].payload["unsupported_text_records"], 1);
    }
}
