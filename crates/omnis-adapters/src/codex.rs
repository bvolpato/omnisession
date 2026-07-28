use std::{
    collections::HashMap,
    fs,
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
        EventBuilder, MAX_TRANSCRIPT_FILE_SIZE, executable, json_lines, json_lines_prefix,
        json_lines_tail, parse_timestamp, paths_match, provider_file, provider_root, sort_sessions,
        string_at, validate_provider,
    },
};

const SCAN_LIMIT: usize = 10_000;

#[derive(Clone, Debug)]
pub struct CodexAdapter {
    codex_home: Option<PathBuf>,
    session_files: Arc<OnceLock<Vec<PathBuf>>>,
    titles: Arc<OnceLock<HashMap<String, String>>>,
}

impl CodexAdapter {
    #[must_use]
    pub fn with_root(codex_home: impl Into<PathBuf>) -> Self {
        Self {
            codex_home: Some(codex_home.into()),
            session_files: Arc::default(),
            titles: Arc::default(),
        }
    }

    fn session_files(&self) -> &[PathBuf] {
        self.session_files.get_or_init(|| {
            let Some(home) = self.codex_home.as_deref() else {
                return Vec::new();
            };
            let mut files = Vec::new();
            collect_jsonl(&home.join("sessions"), 5, &mut files);
            if files.len() < SCAN_LIMIT {
                collect_jsonl(&home.join("archived_sessions"), 5, &mut files);
            }
            files.truncate(SCAN_LIMIT);
            files
        })
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

    fn sessions(&self) -> Vec<CodexSession> {
        let titles = self.title_index();
        let candidates = self
            .session_files()
            .iter()
            .cloned()
            .filter_map(CodexSession::parse_metadata_path);
        let mut sessions: HashMap<String, CodexSession> = HashMap::new();
        for mut session in candidates {
            if session.is_subagent {
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
        sessions.into_values().collect()
    }

    fn find_session(&self, id: &str) -> Result<CodexSession> {
        let path = self.find_session_path(id)?;
        let mut session = CodexSession::parse_path(path)
            .ok_or_else(|| anyhow!("Codex session `{id}` could not be parsed"))?;
        if let Some(title) = self.title_index().get(id) {
            session.title = Some(title.clone());
        }
        Ok(session)
    }

    fn find_session_metadata(&self, id: &str) -> Result<CodexSession> {
        let path = self.find_session_path(id)?;
        let mut session = CodexSession::parse_metadata_path(path)
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
            .ok_or_else(|| anyhow!("Codex session `{id}` was not found"))
    }
}

impl Default for CodexAdapter {
    fn default() -> Self {
        Self {
            codex_home: provider_root("CODEX_HOME", &[".codex"]),
            session_files: Arc::default(),
            titles: Arc::default(),
        }
    }
}

fn collect_jsonl(root: &Path, depth: usize, output: &mut Vec<PathBuf>) {
    if output.len() >= SCAN_LIMIT {
        return;
    }
    let Ok(entries) = fs::read_dir(root) else {
        return;
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
        if output.len() >= SCAN_LIMIT {
            break;
        }
        if file_type.is_dir() && depth > 0 {
            collect_jsonl(&path, depth - 1, output);
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
    fn parse_metadata_path(path: PathBuf) -> Option<Self> {
        let records = json_lines_prefix(&path, 1).ok()?;
        let record = records.first()?;
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

    fn parse_path(path: PathBuf) -> Option<Self> {
        let records = json_lines(&path).ok()?;
        let mut id = None;
        let mut project_path = None;
        let mut git_branch = None;
        let mut cli_version = None;
        let mut is_subagent = false;
        let mut created_at = None;
        let mut updated_at = None;
        for record in records {
            let timestamp = parse_timestamp(record.get("timestamp"));
            if let Some(timestamp) = timestamp {
                updated_at = Some(
                    updated_at.map_or(timestamp, |current: DateTime<Utc>| current.max(timestamp)),
                );
            }
            match record.get("type").and_then(Value::as_str) {
                Some("session_meta") if id.is_none() => {
                    let payload = record.get("payload")?;
                    let native_id = string_at(payload, &[&["id"], &["session_id"]])?;
                    Uuid::parse_str(native_id).ok()?;
                    id = Some(native_id.to_owned());
                    project_path = string_at(payload, &[&["cwd"]]).map(PathBuf::from);
                    git_branch = string_at(payload, &[&["git", "branch"], &["git", "branch_name"]])
                        .map(str::to_owned);
                    cli_version = string_at(payload, &[&["cli_version"]]).map(str::to_owned);
                    is_subagent = string_at(
                        payload,
                        &[
                            &["agent_role"],
                            &["agent_path"],
                            &["thread_source", "agent_role"],
                        ],
                    )
                    .is_some();
                    created_at = timestamp;
                }
                Some("turn_context") => {
                    if let Some(cwd) = string_at(&record, &[&["payload", "cwd"]]) {
                        project_path = Some(PathBuf::from(cwd));
                    }
                }
                _ => {}
            }
        }
        let id = id?;
        (path_uuid(&path).as_deref() == Some(id.as_str())).then_some(Self {
            id,
            title: None,
            path,
            project_path,
            git_branch,
            cli_version,
            is_subagent,
            created_at,
            updated_at,
        })
    }

    fn canonical_events(&self) -> Result<EventBuilder> {
        Ok(self.events_from_records(json_lines(&self.path)?))
    }

    fn preview_events(&self) -> Result<EventBuilder> {
        const SAMPLE_RECORDS: usize = 1_024;
        if fs::metadata(&self.path)?.len() <= MAX_TRANSCRIPT_FILE_SIZE {
            return self.canonical_events();
        }
        let mut records = json_lines_prefix(&self.path, SAMPLE_RECORDS)?;
        records.extend(json_lines_tail(&self.path, SAMPLE_RECORDS)?);
        Ok(self.events_from_records(records))
    }

    fn events_from_records(&self, records: Vec<Value>) -> EventBuilder {
        let mut builder = EventBuilder::new(Provider::Codex, &self.id);
        builder.set_provider_version(self.cli_version.clone());
        let mut last_visible: Option<(EventKind, String)> = None;
        for record in records {
            let timestamp = parse_timestamp(record.get("timestamp"));
            match record.get("type").and_then(Value::as_str) {
                Some("response_item") => {
                    if let Some(payload) = record.get("payload") {
                        push_response_item(&mut builder, payload, timestamp, &mut last_visible);
                    }
                }
                Some("event_msg") => {
                    if let Some(payload) = record.get("payload") {
                        push_event_message(&mut builder, payload, timestamp, &mut last_visible);
                    }
                }
                _ => {}
            }
        }
        builder
    }
}

fn push_message_text(
    builder: &mut EventBuilder,
    kind: EventKind,
    text: &str,
    timestamp: Option<DateTime<Utc>>,
    raw_type: &str,
    last_visible: &mut Option<(EventKind, String)>,
) {
    if text.is_empty()
        || last_visible
            .as_ref()
            .is_some_and(|(last_kind, last_text)| last_kind == &kind && last_text == text)
    {
        return;
    }
    *last_visible = Some((kind.clone(), text.to_owned()));
    builder.push(
        kind,
        json!({ "text": text }),
        timestamp,
        ReplayPolicy::Contextual,
        Some(raw_type.to_owned()),
        None,
    );
}

fn push_response_item(
    builder: &mut EventBuilder,
    payload: &Value,
    timestamp: Option<DateTime<Utc>>,
    last_visible: &mut Option<(EventKind, String)>,
) {
    let item_type = payload.get("type").and_then(Value::as_str).unwrap_or("");
    if item_type == "message" {
        let kind = match payload.get("role").and_then(Value::as_str) {
            Some("user") => Some(EventKind::MessageUser),
            Some("assistant") => Some(EventKind::MessageAssistant),
            _ => None,
        };
        let Some(kind) = kind else {
            return;
        };
        if let Some(text) = payload.get("content").and_then(Value::as_str) {
            push_message_text(
                builder,
                kind,
                text,
                timestamp,
                "response_item.message",
                last_visible,
            );
            return;
        }
        if let Some(parts) = payload.get("content").and_then(Value::as_array) {
            for part in parts {
                if matches!(
                    part.get("type").and_then(Value::as_str),
                    Some("input_text" | "output_text" | "text")
                ) {
                    if let Some(text) = part.get("text").and_then(Value::as_str) {
                        push_message_text(
                            builder,
                            kind.clone(),
                            text,
                            timestamp,
                            "response_item.message",
                            last_visible,
                        );
                    }
                }
            }
        }
        return;
    }

    let (kind, output) = match item_type {
        "function_call" | "custom_tool_call" | "web_search_call" | "computer_call" => {
            (Some(EventKind::ToolCalled), false)
        }
        "local_shell_call" => (Some(EventKind::CommandExecuted), false),
        "function_call_output" | "custom_tool_call_output" | "local_shell_call_output" => {
            (Some(EventKind::ToolCompleted), true)
        }
        _ => (None, false),
    };
    let Some(kind) = kind else {
        return;
    };
    let mut metadata = Map::new();
    metadata.insert("type".to_owned(), Value::String(item_type.to_owned()));
    for field in ["name", "call_id", "status"] {
        if let Some(value) = payload.get(field) {
            metadata.insert(field.to_owned(), value.clone());
        }
    }
    if output {
        metadata.insert("content_omitted".to_owned(), Value::Bool(true));
    }
    builder.push(
        kind,
        Value::Object(metadata),
        timestamp,
        ReplayPolicy::HistoricalOnly,
        Some(format!("response_item.{item_type}")),
        None,
    );
    *last_visible = None;
}

fn push_event_message(
    builder: &mut EventBuilder,
    payload: &Value,
    timestamp: Option<DateTime<Utc>>,
    last_visible: &mut Option<(EventKind, String)>,
) {
    let event_type = payload.get("type").and_then(Value::as_str).unwrap_or("");
    let kind = match event_type {
        "user_message" => Some(EventKind::MessageUser),
        "agent_message" => Some(EventKind::MessageAssistant),
        _ => None,
    };
    let Some(kind) = kind else {
        return;
    };
    if let Some(text) = string_at(payload, &[&["message"], &["text"]]) {
        push_message_text(builder, kind, text, timestamp, "event_msg", last_visible);
    }
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
                session: SessionRef::new(Provider::Codex, session.id),
                title: session.title,
                project_path: session.project_path,
                git_branch: session.git_branch,
                created_at: session.created_at,
                updated_at: session.updated_at,
                event_count: 0,
                source_path: Some(session.path),
            });
        }
        sort_sessions(&mut sessions);
        Ok(sessions)
    }

    fn read_session(&self, session: &SessionRef) -> Result<omnis_ir::CanonicalSnapshot> {
        validate_provider(session, Provider::Codex)?;
        let native = self.find_session(&session.id)?;
        let captured_at = native.updated_at.unwrap_or_else(Utc::now);
        Ok(native.canonical_events()?.snapshot(
            session.clone(),
            native.title,
            native.project_path,
            native.git_branch,
            captured_at,
        ))
    }

    fn preview_session(&self, session: &SessionRef) -> Result<omnis_ir::CanonicalSnapshot> {
        validate_provider(session, Provider::Codex)?;
        let native = self.find_session_metadata(&session.id)?;
        let captured_at = native.updated_at.unwrap_or_else(Utc::now);
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
