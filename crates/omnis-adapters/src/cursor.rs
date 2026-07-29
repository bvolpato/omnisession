use std::{
    collections::{BTreeSet, HashMap},
    env, fs,
    path::Path,
    path::PathBuf,
};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use omnis_ir::{EventKind, Provider, ReplayPolicy, SessionRef};
use prost::Message;
use rusqlite::{Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{
    LaunchPlan, LaunchTarget, NativeSession, ProviderAdapter, ProviderInstallation,
    support::{
        EventBuilder, executable, nested_files, parse_timestamp, paths_match, provider_file,
        provider_root, read_json, selected_metadata, sort_sessions, sqlite_snapshot, string_at,
        validate_provider, value_at,
    },
};

#[derive(Clone, Debug)]
pub struct CursorCliAdapter {
    chats_root: Option<PathBuf>,
}

impl CursorCliAdapter {
    #[must_use]
    pub fn with_root(chats_root: impl Into<PathBuf>) -> Self {
        Self {
            chats_root: Some(chats_root.into()),
        }
    }

    fn metadata_files(&self) -> Vec<PathBuf> {
        self.chats_root
            .as_deref()
            .map(|root| nested_files(root, 2, Some("meta.json")))
            .unwrap_or_default()
    }

    fn find_metadata(&self, id: &str) -> Result<(Value, PathBuf)> {
        self.metadata_files()
            .into_iter()
            .find_map(|path| {
                let value = read_json(&path).ok()?;
                (session_id(&value, &path).as_deref() == Some(id)).then_some((value, path))
            })
            .ok_or_else(|| anyhow!("Cursor CLI session `{id}` was not found"))
    }
}

impl Default for CursorCliAdapter {
    fn default() -> Self {
        Self {
            chats_root: cursor_chats_root(),
        }
    }
}

fn cursor_chats_root() -> Option<PathBuf> {
    env::var_os("CURSOR_AGENT_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            env::var_os("CURSOR_CONFIG_DIR")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .map(|root| root.join("chats"))
        })
        .or_else(|| {
            env::var_os("XDG_CONFIG_HOME")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .map(|root| root.join("cursor").join("chats"))
        })
        .or_else(|| provider_root("CURSOR_AGENT_HOME", &[".cursor", "chats"]))
}

#[derive(Default)]
struct CursorMetadata {
    title: Option<String>,
    project_path: Option<PathBuf>,
    git_branch: Option<String>,
    created_at: Option<DateTime<Utc>>,
    updated_at: Option<DateTime<Utc>>,
    event_count: usize,
}

fn session_id(metadata: &Value, path: &Path) -> Option<String> {
    string_at(
        metadata,
        &[&["id"], &["sessionId"], &["session_id"], &["session", "id"]],
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

fn metadata(value: &Value) -> CursorMetadata {
    CursorMetadata {
        title: string_at(
            value,
            &[
                &["title"],
                &["name"],
                &["session", "title"],
                &["metadata", "title"],
            ],
        )
        .map(str::to_owned),
        project_path: string_at(
            value,
            &[
                &["cwd"],
                &["projectPath"],
                &["project_path"],
                &["workspace", "path"],
            ],
        )
        .map(PathBuf::from),
        git_branch: string_at(
            value,
            &[&["gitBranch"], &["git_branch"], &["git", "branch"]],
        )
        .map(str::to_owned),
        created_at: parse_timestamp(value_at(
            value,
            &[
                &["createdAt"],
                &["created_at"],
                &["created"],
                &["time", "created"],
            ],
        )),
        updated_at: parse_timestamp(value_at(
            value,
            &[
                &["updatedAt"],
                &["updated_at"],
                &["updated"],
                &["time", "updated"],
            ],
        )),
        event_count: value_at(
            value,
            &[
                &["eventCount"],
                &["event_count"],
                &["messageCount"],
                &["message_count"],
            ],
        )
        .and_then(Value::as_u64)
        .and_then(|count| usize::try_from(count).ok())
        .unwrap_or(1),
    }
}

impl ProviderAdapter for CursorCliAdapter {
    fn provider(&self) -> Provider {
        Provider::CursorCli
    }

    fn probe(&self) -> ProviderInstallation {
        ProviderInstallation {
            provider: Provider::CursorCli,
            installed: executable("cursor-agent").is_some()
                || self.chats_root.as_deref().is_some_and(Path::is_dir),
            executable: executable("cursor-agent"),
            data_root: self.chats_root.clone(),
        }
    }

    fn list_sessions(&self, project: Option<&Path>) -> Result<Vec<NativeSession>> {
        let mut sessions = Vec::new();
        for path in self.metadata_files() {
            let value: Value = match read_json(&path) {
                Ok(value) => value,
                Err(_) => continue,
            };
            let Some(id) = session_id(&value, &path) else {
                continue;
            };
            let metadata = metadata(&value);
            if project.is_some_and(|project| {
                metadata
                    .project_path
                    .as_deref()
                    .is_none_or(|recorded| !paths_match(recorded, project))
            }) {
                continue;
            }
            sessions.push(NativeSession {
                session: SessionRef::new(Provider::CursorCli, id),
                title: metadata.title,
                project_path: metadata.project_path,
                git_branch: metadata.git_branch,
                created_at: metadata.created_at,
                updated_at: metadata.updated_at,
                event_count: metadata.event_count,
                source_path: Some(path),
            });
        }
        sort_sessions(&mut sessions);
        Ok(sessions)
    }

    fn read_session(&self, session: &SessionRef) -> Result<omnis_ir::CanonicalSnapshot> {
        validate_provider(session, Provider::CursorCli)?;
        let (value, path) = self.find_metadata(&session.id)?;
        let metadata = metadata(&value);
        let captured_at = metadata.updated_at.unwrap_or_else(Utc::now);
        let mut builder = EventBuilder::new(Provider::CursorCli, &session.id);
        builder.push(
            EventKind::ProviderEvent,
            json!({
                "storage": "opaque",
                "metadata": selected_metadata(&value),
            }),
            metadata.updated_at,
            ReplayPolicy::HistoricalOnly,
            Some("meta".to_owned()),
            None,
        );
        let root = self
            .chats_root
            .as_deref()
            .context("Cursor CLI chats root is unavailable")?;
        let database = path
            .parent()
            .context("Cursor CLI metadata has no session directory")?
            .join("store.db");
        if database.is_file() {
            for event in cursor_events(root, &database)? {
                builder.push(
                    event.kind,
                    event.payload,
                    None,
                    event.replay_policy,
                    Some(event.raw_type.to_owned()),
                    None,
                );
            }
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
            program: "cursor-agent".to_owned(),
            args: target.prompt.iter().cloned().collect(),
            cwd: target.cwd.clone(),
        })
    }

    fn launch_plan(&self, session: &SessionRef, target: &LaunchTarget) -> Result<LaunchPlan> {
        validate_provider(session, Provider::CursorCli)?;
        if target.fork {
            return Err(anyhow!(
                "Cursor CLI has no verified fork flag; pass `--no-fork` to resume in place"
            ));
        }
        let mut args = vec!["--resume".to_owned(), session.id.clone()];
        if let Some(prompt) = &target.prompt {
            args.push(prompt.clone());
        }
        Ok(LaunchPlan {
            program: "cursor-agent".to_owned(),
            args,
            cwd: target.cwd.clone(),
        })
    }
}

const MAX_CURSOR_GRAPH_RECORDS: usize = 100_000;
const MAX_CURSOR_BLOB_SIZE: usize = 8 * 1024 * 1024;
const MAX_CURSOR_PROMPT_BYTES: usize = 64 * 1024 * 1024;

struct CursorEvent {
    kind: EventKind,
    payload: Value,
    replay_policy: ReplayPolicy,
    raw_type: &'static str,
}

fn cursor_events(root: &Path, database: &Path) -> Result<Vec<CursorEvent>> {
    let snapshot = sqlite_snapshot(root, database)?;
    let connection = &snapshot.connection;
    validate_cursor_schema(connection)?;
    let blob_count =
        connection.query_row("SELECT COUNT(*) FROM blobs", [], |row| row.get::<_, i64>(0))?;
    if blob_count < 0 || usize::try_from(blob_count)? > MAX_CURSOR_GRAPH_RECORDS {
        bail!("Cursor CLI session exceeds safe graph record limit");
    }
    let metadata: String = connection
        .query_row("SELECT value FROM meta WHERE key = '0'", [], |row| {
            row.get(0)
        })
        .context("Cursor CLI store omitted root metadata")?;
    let metadata_bytes = hex::decode(metadata).context("Cursor CLI metadata is not hex")?;
    let metadata: Value =
        serde_json::from_slice(&metadata_bytes).context("Cursor CLI metadata is not JSON")?;
    let root_id = metadata
        .get("latestRootBlobId")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .context("Cursor CLI store omitted latest root blob")?;
    let root = load_cursor_blob(connection, root_id)?;
    let state = CursorConversationStateStructure::decode(root.as_slice())
        .context("Cursor CLI root graph is not recognized protobuf")?;
    if state.turns.len() > MAX_CURSOR_GRAPH_RECORDS {
        bail!("Cursor CLI session exceeds safe turn limit");
    }

    let prompt_events = cursor_prompt_events(connection, &state.root_prompt_messages_json)?;
    let structural_events = cursor_structural_events(connection, state.turns)?;
    if prompt_events.iter().any(|event| {
        matches!(
            event.kind,
            EventKind::MessageUser
                | EventKind::MessageAssistant
                | EventKind::ToolCalled
                | EventKind::ToolCompleted
                | EventKind::ToolFailed
        )
    }) {
        let mut events = prompt_events;
        events.extend(
            structural_events
                .into_iter()
                .filter(|event| event.kind == EventKind::ProviderEvent),
        );
        Ok(events)
    } else {
        Ok(structural_events)
    }
}

fn cursor_structural_events(
    connection: &Connection,
    turns: Vec<Vec<u8>>,
) -> Result<Vec<CursorEvent>> {
    let mut structural_events = Vec::new();
    let mut records = 0usize;
    for (turn_index, turn_id) in turns.into_iter().enumerate() {
        records = records.saturating_add(1);
        if records > MAX_CURSOR_GRAPH_RECORDS {
            bail!("Cursor CLI session exceeds safe graph traversal limit");
        }
        let turn = load_cursor_blob(connection, &hex::encode(turn_id))?;
        let turn = CursorConversationTurnStructure::decode(turn.as_slice())
            .context("Cursor CLI turn graph is not recognized protobuf")?;
        let Some(cursor_conversation_turn_structure::Turn::Agent(turn)) = turn.turn else {
            structural_events.push(CursorEvent {
                kind: EventKind::ProviderEvent,
                payload: json!({ "kind": "unsupported_cursor_turn" }),
                replay_policy: ReplayPolicy::HistoricalOnly,
                raw_type: "cursor.turn.unknown",
            });
            continue;
        };

        let user = load_cursor_blob(connection, &hex::encode(turn.user_message))?;
        let user = CursorUserMessage::decode(user.as_slice())
            .context("Cursor CLI user message is not recognized protobuf")?;
        if !user.message_id.is_empty()
            && user
                .thread_id
                .as_deref()
                .is_some_and(|thread_id| thread_id != user.message_id)
        {
            bail!("Cursor CLI user message has inconsistent identity");
        }
        if !user.conversation_state_blob_id.is_empty() {
            let anchor =
                load_cursor_blob(connection, &hex::encode(&user.conversation_state_blob_id))?;
            let anchor = CursorConversationStateStructure::decode(anchor.as_slice())
                .context("Cursor CLI rewind anchor is not recognized protobuf")?;
            if anchor.turns.len() != turn_index {
                bail!("Cursor CLI rewind anchor has inconsistent turn history");
            }
        }
        if !user.text.is_empty() {
            structural_events.push(CursorEvent {
                kind: EventKind::MessageUser,
                payload: json!({ "text": user.text }),
                replay_policy: ReplayPolicy::Contextual,
                raw_type: "cursor.user_message",
            });
        }

        if records.saturating_add(turn.steps.len()) > MAX_CURSOR_GRAPH_RECORDS {
            bail!("Cursor CLI session exceeds safe graph traversal limit");
        }
        records += turn.steps.len();
        for step_id in turn.steps {
            let step = load_cursor_blob(connection, &hex::encode(step_id))?;
            let step = CursorConversationStep::decode(step.as_slice())
                .context("Cursor CLI step is not recognized protobuf")?;
            match step.message {
                Some(cursor_conversation_step::Message::Assistant(message))
                    if !message.text.is_empty() =>
                {
                    structural_events.push(CursorEvent {
                        kind: EventKind::MessageAssistant,
                        payload: json!({ "text": message.text }),
                        replay_policy: ReplayPolicy::Contextual,
                        raw_type: "cursor.assistant_message",
                    });
                }
                Some(cursor_conversation_step::Message::Tool(_)) => {
                    structural_events.push(CursorEvent {
                        kind: EventKind::ProviderEvent,
                        payload: json!({ "kind": "cursor_tool_call", "detail": "opaque" }),
                        replay_policy: ReplayPolicy::HistoricalOnly,
                        raw_type: "cursor.tool_call",
                    });
                }
                Some(cursor_conversation_step::Message::Thinking(_)) => {
                    structural_events.push(CursorEvent {
                        kind: EventKind::ProviderEvent,
                        payload: json!({ "kind": "cursor_reasoning", "detail": "omitted" }),
                        replay_policy: ReplayPolicy::HistoricalOnly,
                        raw_type: "cursor.thinking_message",
                    });
                }
                _ => {}
            }
        }
    }
    Ok(structural_events)
}

fn cursor_prompt_events(
    connection: &Connection,
    references: &[Vec<u8>],
) -> Result<Vec<CursorEvent>> {
    if references.len() > MAX_CURSOR_GRAPH_RECORDS {
        bail!("Cursor CLI session exceeds safe prompt history limit");
    }
    let mut events = Vec::new();
    let mut decoded_bytes = 0usize;
    for reference in references {
        let bytes = load_cursor_blob(connection, &hex::encode(reference))?;
        decoded_bytes = decoded_bytes.saturating_add(bytes.len());
        if decoded_bytes > MAX_CURSOR_PROMPT_BYTES {
            bail!("Cursor CLI prompt history exceeds safe size limit");
        }
        let envelope: Value = serde_json::from_slice(&bytes)
            .context("Cursor CLI prompt history contains invalid JSON")?;
        match envelope.get("role").and_then(Value::as_str) {
            Some("user") => push_cursor_prompt_text(
                &mut events,
                EventKind::MessageUser,
                &envelope,
                "cursor.prompt.user",
            ),
            Some("assistant") => {
                push_cursor_prompt_text(
                    &mut events,
                    EventKind::MessageAssistant,
                    &envelope,
                    "cursor.prompt.assistant",
                );
                push_cursor_tool_calls(&mut events, &envelope);
            }
            Some("tool") => push_cursor_tool_results(&mut events, &envelope),
            Some("system") => events.push(CursorEvent {
                kind: EventKind::ProviderEvent,
                payload: json!({ "kind": "cursor_system_message", "detail": "omitted" }),
                replay_policy: ReplayPolicy::HistoricalOnly,
                raw_type: "cursor.prompt.system",
            }),
            _ => events.push(CursorEvent {
                kind: EventKind::ProviderEvent,
                payload: json!({ "kind": "unsupported_cursor_prompt", "detail": "opaque" }),
                replay_policy: ReplayPolicy::HistoricalOnly,
                raw_type: "cursor.prompt.unknown",
            }),
        }
    }
    Ok(events)
}

fn prompt_content(envelope: &Value) -> impl Iterator<Item = &Value> {
    envelope
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
}

fn push_cursor_prompt_text(
    events: &mut Vec<CursorEvent>,
    kind: EventKind,
    envelope: &Value,
    raw_type: &'static str,
) {
    if let Some(text) = envelope
        .get("content")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
    {
        events.push(CursorEvent {
            kind,
            payload: json!({ "text": text }),
            replay_policy: ReplayPolicy::Contextual,
            raw_type,
        });
        return;
    }
    for part in prompt_content(envelope) {
        if part.get("type").and_then(Value::as_str) != Some("text") {
            continue;
        }
        if let Some(text) = part
            .get("text")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
        {
            events.push(CursorEvent {
                kind: kind.clone(),
                payload: json!({ "text": text }),
                replay_policy: ReplayPolicy::Contextual,
                raw_type,
            });
        }
    }
}

fn push_cursor_tool_calls(events: &mut Vec<CursorEvent>, envelope: &Value) {
    for part in prompt_content(envelope) {
        if part.get("type").and_then(Value::as_str) != Some("tool-call") {
            continue;
        }
        events.push(CursorEvent {
            kind: EventKind::ToolCalled,
            payload: json!({
                "call_id": part.get("toolCallId").cloned().unwrap_or(Value::Null),
                "name": part.get("toolName").cloned().unwrap_or(Value::Null),
                "input": part.get("args").cloned().unwrap_or(Value::Null),
            }),
            replay_policy: ReplayPolicy::HistoricalOnly,
            raw_type: "cursor.prompt.tool_call",
        });
    }
}

fn push_cursor_tool_results(events: &mut Vec<CursorEvent>, envelope: &Value) {
    for part in prompt_content(envelope) {
        if part.get("type").and_then(Value::as_str) != Some("tool-result") {
            continue;
        }
        events.push(CursorEvent {
            kind: EventKind::ToolCompleted,
            payload: json!({
                "call_id": part.get("toolCallId").cloned().unwrap_or(Value::Null),
                "name": part.get("toolName").cloned().unwrap_or(Value::Null),
                "result": part.get("result").cloned().unwrap_or(Value::Null),
            }),
            replay_policy: ReplayPolicy::HistoricalOnly,
            raw_type: "cursor.prompt.tool_result",
        });
    }
}

fn validate_cursor_schema(connection: &Connection) -> Result<()> {
    let version = connection.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))?;
    if version != 1 {
        bail!("Cursor CLI store has unsupported schema version {version}");
    }
    for (table, expected) in [
        ("blobs", [("id", "TEXT", 1_i64), ("data", "BLOB", 0_i64)]),
        ("meta", [("key", "TEXT", 1_i64), ("value", "TEXT", 0_i64)]),
    ] {
        let mut statement = connection.prepare(&format!("PRAGMA table_info({table})"))?;
        let columns = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(5)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let expected = expected
            .into_iter()
            .map(|(name, kind, primary_key)| (name.to_owned(), kind.to_owned(), primary_key))
            .collect::<Vec<_>>();
        if columns != expected {
            bail!("Cursor CLI store has an unsupported `{table}` schema");
        }
    }
    let extra_tables = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name NOT IN ('blobs', 'meta')",
        [],
        |row| row.get::<_, i64>(0),
    )?;
    if extra_tables != 0 {
        bail!("Cursor CLI store contains unsupported tables");
    }
    Ok(())
}

fn load_cursor_blob(connection: &Connection, id: &str) -> Result<Vec<u8>> {
    if id.len() != 64 || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("Cursor CLI graph contains an invalid blob ID");
    }
    let data = connection
        .query_row("SELECT data FROM blobs WHERE id = ?1", [id], |row| {
            row.get::<_, Vec<u8>>(0)
        })
        .with_context(|| format!("Cursor CLI graph references missing blob `{}`", &id[..12]))?;
    if data.len() > MAX_CURSOR_BLOB_SIZE {
        bail!("Cursor CLI blob exceeds safe size limit");
    }
    if hex::encode(Sha256::digest(&data)) != id.to_ascii_lowercase() {
        bail!("Cursor CLI blob failed content-address verification");
    }
    Ok(data)
}

#[derive(Clone, PartialEq, Message)]
struct CursorConversationStateStructure {
    #[prost(bytes = "vec", repeated, tag = "1")]
    root_prompt_messages_json: Vec<Vec<u8>>,
    #[prost(bytes = "vec", repeated, tag = "8")]
    turns: Vec<Vec<u8>>,
}

#[derive(Clone, PartialEq, Message)]
struct CursorConversationTurnStructure {
    #[prost(oneof = "cursor_conversation_turn_structure::Turn", tags = "1, 2")]
    turn: Option<cursor_conversation_turn_structure::Turn>,
}

mod cursor_conversation_turn_structure {
    use prost::Oneof;

    #[derive(Clone, PartialEq, Oneof)]
    pub(super) enum Turn {
        #[prost(message, tag = "1")]
        Agent(super::CursorAgentConversationTurnStructure),
        #[prost(message, tag = "2")]
        Shell(super::CursorOpaqueMessage),
    }
}

#[derive(Clone, PartialEq, Message)]
struct CursorAgentConversationTurnStructure {
    #[prost(bytes = "vec", tag = "1")]
    user_message: Vec<u8>,
    #[prost(bytes = "vec", repeated, tag = "2")]
    steps: Vec<Vec<u8>>,
}

#[derive(Clone, PartialEq, Message)]
struct CursorUserMessage {
    #[prost(string, tag = "1")]
    text: String,
    #[prost(string, tag = "2")]
    message_id: String,
    #[prost(bytes = "vec", tag = "10")]
    conversation_state_blob_id: Vec<u8>,
    #[prost(string, optional, tag = "17")]
    thread_id: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
struct CursorConversationStep {
    #[prost(oneof = "cursor_conversation_step::Message", tags = "1, 2, 3")]
    message: Option<cursor_conversation_step::Message>,
}

mod cursor_conversation_step {
    use prost::Oneof;

    #[derive(Clone, PartialEq, Oneof)]
    pub(super) enum Message {
        #[prost(message, tag = "1")]
        Assistant(super::CursorAssistantMessage),
        #[prost(message, tag = "2")]
        Tool(super::CursorOpaqueMessage),
        #[prost(message, tag = "3")]
        Thinking(super::CursorOpaqueMessage),
    }
}

#[derive(Clone, PartialEq, Message)]
struct CursorAssistantMessage {
    #[prost(string, tag = "1")]
    text: String,
}

#[derive(Clone, PartialEq, Message)]
struct CursorOpaqueMessage {}

#[derive(Clone, Debug)]
pub struct CursorIdeAdapter {
    metadata_root: Option<PathBuf>,
}

impl CursorIdeAdapter {
    #[must_use]
    pub fn with_root(metadata_root: impl Into<PathBuf>) -> Self {
        Self {
            metadata_root: Some(metadata_root.into()),
        }
    }

    fn database_path(&self) -> Option<PathBuf> {
        let root = self.metadata_root.as_deref()?;
        provider_file(root, &root.join("globalStorage").join("state.vscdb"))
    }

    fn workspace_paths(&self) -> HashMap<String, PathBuf> {
        let Some(root) = self.metadata_root.as_deref() else {
            return HashMap::new();
        };
        let workspace_root = root.join("workspaceStorage");
        if workspace_root
            .symlink_metadata()
            .is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            return HashMap::new();
        }
        let Ok(entries) = fs::read_dir(workspace_root) else {
            return HashMap::new();
        };
        entries
            .flatten()
            .filter_map(|entry| {
                let file_type = entry.file_type().ok()?;
                if file_type.is_symlink() || !file_type.is_dir() {
                    return None;
                }
                let id = entry.file_name().to_str()?.to_owned();
                let candidate = entry.path().join("workspace.json");
                let path = provider_file(root, &candidate)?;
                let value = read_json(&path).ok()?;
                let uri = string_at(&value, &[&["folder"], &["workspace"]])?;
                Some((id, file_uri_path(uri).unwrap_or_else(|| PathBuf::from(uri))))
            })
            .collect()
    }

    fn open_database(&self) -> Result<(Connection, PathBuf)> {
        let database = self
            .database_path()
            .filter(|path| path.is_file())
            .ok_or_else(|| anyhow!("Cursor IDE metadata database was not found"))?;
        let connection = Connection::open_with_flags(
            &database,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .context("opening Cursor IDE metadata read-only")?;
        connection.pragma_update(None, "query_only", "ON")?;
        connection.pragma_update(None, "trusted_schema", "OFF")?;
        connection.busy_timeout(std::time::Duration::ZERO)?;
        Ok((connection, database))
    }

    fn headers(&self) -> Result<Vec<CursorIdeHeader>> {
        let (mut connection, database) = self.open_database()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Deferred)?;
        let headers = cursor_ide_headers(&transaction, &database, &self.workspace_paths())?;
        transaction.commit()?;
        Ok(headers)
    }

    fn session_data(&self, id: &str) -> Result<(CursorIdeHeader, Vec<CursorIdeEvent>)> {
        let (mut connection, database) = self.open_database()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Deferred)?;
        let header = cursor_ide_headers(&transaction, &database, &self.workspace_paths())?
            .into_iter()
            .find(|header| header.id == id)
            .ok_or_else(|| anyhow!("Cursor IDE session `{id}` was not found"))?;
        let events = cursor_ide_events(&transaction, id)?;
        transaction.commit()?;
        Ok((header, events))
    }
}

const MAX_CURSOR_IDE_HEADERS: usize = 100_000;
const MAX_CURSOR_IDE_RECORD_SIZE: usize = 16 * 1024 * 1024;
const MAX_CURSOR_IDE_TRAJECTORY_BYTES: usize = 128 * 1024 * 1024;

fn cursor_ide_headers(
    transaction: &Transaction<'_>,
    database: &Path,
    workspace_paths: &HashMap<String, PathBuf>,
) -> Result<Vec<CursorIdeHeader>> {
    if !table_exists(transaction, "composerHeaders")? {
        return Ok(Vec::new());
    }
    let mut statement = transaction.prepare(
        "SELECT composerId, workspaceId, createdAt, lastUpdatedAt, \
         isArchived, isSubagent, value FROM composerHeaders ORDER BY composerId",
    )?;
    let mut rows = statement.query([])?;
    let mut headers = Vec::new();
    while let Some(row) = rows.next()? {
        if headers.len() >= MAX_CURSOR_IDE_HEADERS {
            bail!("Cursor IDE metadata exceeds safe composer-header limit");
        }
        let value = row.get_ref(6)?;
        let bytes = match value {
            rusqlite::types::ValueRef::Text(value) | rusqlite::types::ValueRef::Blob(value) => {
                value
            }
            _ => &[],
        };
        if bytes.len() > MAX_CURSOR_IDE_RECORD_SIZE {
            bail!("Cursor IDE composer header exceeds safe record limit");
        }
        let mut value = serde_json::from_slice::<Value>(bytes).unwrap_or_else(|_| json!({}));
        let Some(object) = value.as_object_mut() else {
            continue;
        };
        let id = row.get::<_, String>(0)?;
        object
            .entry("composerId")
            .or_insert_with(|| Value::String(id));
        if let Some(workspace_id) = row.get::<_, Option<String>>(1)? {
            object
                .entry("workspaceId")
                .or_insert_with(|| Value::String(workspace_id.clone()));
            if let Some(path) = workspace_paths.get(&workspace_id) {
                object
                    .entry("workspacePath")
                    .or_insert_with(|| Value::String(path.to_string_lossy().into_owned()));
            }
        }
        insert_integer(object, "createdAt", row.get(2)?);
        insert_integer(object, "lastUpdatedAt", row.get(3)?);
        insert_boolean(object, "isArchived", row.get(4)?);
        insert_boolean(object, "isSubagent", row.get(5)?);
        if let Some(header) = CursorIdeHeader::parse(&value, database) {
            headers.push(header);
        }
    }
    Ok(headers)
}

fn table_exists(connection: &Connection, table: &str) -> rusqlite::Result<bool> {
    connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
        [table],
        |row| row.get(0),
    )
}

fn insert_integer(object: &mut serde_json::Map<String, Value>, key: &str, value: Option<i64>) {
    if let Some(value) = value {
        object
            .entry(key)
            .or_insert_with(|| Value::Number(value.into()));
    }
}

fn insert_boolean(object: &mut serde_json::Map<String, Value>, key: &str, value: Option<i64>) {
    if let Some(value) = value {
        object.entry(key).or_insert_with(|| Value::Bool(value != 0));
    }
}

struct CursorIdeHeader {
    id: String,
    title: Option<String>,
    project_path: Option<PathBuf>,
    created_at: Option<DateTime<Utc>>,
    updated_at: Option<DateTime<Utc>>,
    source_path: PathBuf,
    metadata: Value,
}

impl CursorIdeHeader {
    fn parse(value: &Value, database: &Path) -> Option<Self> {
        let id = string_at(value, &[&["composerId"], &["id"]])?;
        if value.get("isSubagent").and_then(Value::as_bool) == Some(true)
            || value.get("isArchived").and_then(Value::as_bool) == Some(true)
            || value.get("isRoot").and_then(Value::as_bool) == Some(false)
            || string_at(value, &[&["type"]]) == Some("subagent")
            || string_at(value, &[&["parentComposerId"]]).is_some()
            || string_at(value, &[&["rootComposerId"]]).is_some_and(|root| root != id)
        {
            return None;
        }
        let project_path = workspace_path(value);
        Some(Self {
            id: id.to_owned(),
            title: string_at(value, &[&["title"], &["name"]]).map(str::to_owned),
            project_path,
            created_at: parse_timestamp(value_at(
                value,
                &[&["createdAt"], &["created_at"], &["created"]],
            )),
            updated_at: parse_timestamp(value_at(
                value,
                &[
                    &["updatedAt"],
                    &["lastUpdatedAt"],
                    &["updated_at"],
                    &["lastUpdated"],
                ],
            )),
            source_path: database.to_path_buf(),
            metadata: json!({
                "composerId": id,
                "title": string_at(value, &[&["title"], &["name"]]),
                "workspace": workspace_path(value),
            }),
        })
    }

    fn into_native(self) -> NativeSession {
        NativeSession {
            session: SessionRef::new(Provider::CursorIde, self.id),
            title: self.title,
            project_path: self.project_path,
            git_branch: None,
            created_at: self.created_at,
            updated_at: self.updated_at,
            event_count: 1,
            source_path: Some(self.source_path),
        }
    }
}

struct CursorIdeEvent {
    kind: EventKind,
    payload: Value,
    timestamp: Option<DateTime<Utc>>,
    replay_policy: ReplayPolicy,
    raw_type: &'static str,
}

fn cursor_ide_events(
    transaction: &Transaction<'_>,
    composer_id: &str,
) -> Result<Vec<CursorIdeEvent>> {
    if !table_exists(transaction, "cursorDiskKV")? {
        return Ok(vec![unsupported_cursor_ide_event(
            "missing_cursor_disk_kv_table",
            None,
            None,
        )]);
    }
    let root_key = format!("composerData:{composer_id}");
    let Some(root_bytes) = cursor_disk_value(transaction, &root_key)? else {
        return Ok(vec![unsupported_cursor_ide_event(
            "missing_composer_data",
            None,
            None,
        )]);
    };
    let root: Value = match serde_json::from_slice(&root_bytes) {
        Ok(root) => root,
        Err(_) => {
            return Ok(vec![unsupported_cursor_ide_event(
                "invalid_composer_data",
                Some(&root_bytes),
                None,
            )]);
        }
    };
    let mut events = vec![CursorIdeEvent {
        kind: EventKind::ProviderEvent,
        payload: cursor_ide_root_metadata(&root),
        timestamp: parse_timestamp(value_at(&root, &[&["lastUpdatedAt"], &["createdAt"]])),
        replay_policy: ReplayPolicy::HistoricalOnly,
        raw_type: "cursor_ide.composer_data",
    }];
    let Some(headers) = root
        .get("fullConversationHeadersOnly")
        .and_then(Value::as_array)
    else {
        events.push(unsupported_cursor_ide_event(
            "missing_full_conversation_headers",
            Some(&root_bytes),
            None,
        ));
        return Ok(events);
    };
    if headers.len() > MAX_CURSOR_IDE_HEADERS {
        bail!("Cursor IDE session exceeds safe conversation-header limit");
    }

    let mut seen = BTreeSet::new();
    let mut checkpoint_ids = BTreeSet::new();
    if let Some(checkpoint_id) = string_at(&root, &[&["latestCheckpointId"]]) {
        checkpoint_ids.insert(checkpoint_id.to_owned());
    }
    let mut trajectory_bytes = root_bytes.len();
    for header in headers {
        let Some(bubble_id) = header.get("bubbleId").and_then(Value::as_str) else {
            events.push(unsupported_cursor_ide_event(
                "conversation_header_without_bubble_id",
                None,
                None,
            ));
            continue;
        };
        if !seen.insert(bubble_id.to_owned()) {
            bail!("Cursor IDE session contains duplicate bubble ID");
        }
        let key = format!("bubbleId:{composer_id}:{bubble_id}");
        let Some(bytes) = cursor_disk_value(transaction, &key)? else {
            events.push(unsupported_cursor_ide_event(
                "missing_bubble",
                None,
                Some(bubble_id),
            ));
            continue;
        };
        trajectory_bytes = trajectory_bytes.saturating_add(bytes.len());
        if trajectory_bytes > MAX_CURSOR_IDE_TRAJECTORY_BYTES {
            bail!("Cursor IDE session exceeds safe trajectory size limit");
        }
        let bubble: Value = if let Ok(value) = serde_json::from_slice(&bytes) {
            value
        } else {
            events.push(unsupported_cursor_ide_event(
                "invalid_bubble_json",
                Some(&bytes),
                Some(bubble_id),
            ));
            continue;
        };
        if let Some(checkpoint_id) =
            string_at(&bubble, &[&["checkpointId"], &["conversationCheckpointId"]])
        {
            checkpoint_ids.insert(checkpoint_id.to_owned());
        }
        push_cursor_ide_bubble(&mut events, header, &bubble, bubble_id, &bytes);
    }
    push_cursor_ide_checkpoints(transaction, composer_id, &checkpoint_ids, &mut events)?;
    push_cursor_ide_diffs(transaction, composer_id, &root, &mut events)?;
    Ok(events)
}

fn cursor_disk_value(transaction: &Transaction<'_>, key: &str) -> Result<Option<Vec<u8>>> {
    let value = transaction
        .query_row(
            "SELECT value FROM cursorDiskKV WHERE key = ?1",
            [key],
            |row| match row.get_ref(0)? {
                rusqlite::types::ValueRef::Text(value) | rusqlite::types::ValueRef::Blob(value) => {
                    Ok(value.to_vec())
                }
                _ => Ok(Vec::new()),
            },
        )
        .optional()?;
    if value
        .as_ref()
        .is_some_and(|value| value.len() > MAX_CURSOR_IDE_RECORD_SIZE)
    {
        bail!("Cursor IDE trajectory record exceeds safe size limit");
    }
    Ok(value)
}

fn cursor_disk_value_length(transaction: &Transaction<'_>, key: &str) -> Result<Option<i64>> {
    transaction
        .query_row(
            "SELECT length(value) FROM cursorDiskKV WHERE key = ?1",
            [key],
            |row| row.get(0),
        )
        .optional()
        .map_err(Into::into)
}

fn cursor_ide_root_metadata(root: &Value) -> Value {
    let known = [
        "_v",
        "composerId",
        "name",
        "status",
        "unifiedMode",
        "forceMode",
        "createdAt",
        "lastUpdatedAt",
        "latestCheckpointId",
        "conversationCheckpointLastUpdatedAt",
        "fullConversationHeadersOnly",
        "codeBlockData",
    ];
    let unknown_fields = root.as_object().map_or_else(Vec::new, |object| {
        object
            .keys()
            .filter(|key| !known.contains(&key.as_str()))
            .cloned()
            .collect::<Vec<_>>()
    });
    json!({
        "storage": "cursor_disk_kv",
        "schema_version": root.get("_v").cloned().unwrap_or(Value::Null),
        "status": root.get("status").cloned().unwrap_or(Value::Null),
        "mode": root.get("unifiedMode").cloned().unwrap_or(Value::Null),
        "message_headers": root
            .get("fullConversationHeadersOnly")
            .and_then(Value::as_array)
            .map_or(0, Vec::len),
        "latest_checkpoint_id": root
            .get("latestCheckpointId")
            .cloned()
            .unwrap_or(Value::Null),
        "unknown_fields": unknown_fields,
        "unknown_fidelity": "opaque_metadata_only",
    })
}

fn push_cursor_ide_bubble(
    events: &mut Vec<CursorIdeEvent>,
    header: &Value,
    bubble: &Value,
    bubble_id: &str,
    raw: &[u8],
) {
    let timestamp = parse_timestamp(value_at(bubble, &[&["createdAt"], &["timestamp"]]))
        .or_else(|| parse_timestamp(value_at(header, &[&["createdAt"], &["timestamp"]])));
    let role = value_at(header, &[&["type"]]).or_else(|| value_at(bubble, &[&["type"], &["role"]]));
    let kind = match role {
        Some(Value::Number(value)) if value.as_i64() == Some(1) => Some(EventKind::MessageUser),
        Some(Value::Number(value)) if value.as_i64() == Some(2) => {
            Some(EventKind::MessageAssistant)
        }
        Some(Value::String(value))
            if matches!(value.to_ascii_lowercase().as_str(), "human" | "user") =>
        {
            Some(EventKind::MessageUser)
        }
        Some(Value::String(value))
            if matches!(value.to_ascii_lowercase().as_str(), "ai" | "assistant") =>
        {
            Some(EventKind::MessageAssistant)
        }
        _ => None,
    };
    let text = bubble
        .get("text")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty());
    if let (Some(kind), Some(text)) = (kind, text) {
        events.push(CursorIdeEvent {
            kind,
            payload: json!({ "text": text, "bubble_id": bubble_id }),
            timestamp,
            replay_policy: ReplayPolicy::Contextual,
            raw_type: "cursor_ide.bubble",
        });
    }
    if let Some(tool) = bubble.get("toolFormerData") {
        let status = string_at(tool, &[&["status"], &["additionalData", "status"]]);
        let kind = if tool.get("error").is_some()
            || status.is_some_and(|status| matches!(status, "error" | "failed" | "cancelled"))
        {
            EventKind::ToolFailed
        } else if tool.get("result").is_some()
            || status.is_some_and(|status| {
                matches!(
                    status,
                    "completed" | "complete" | "success" | "succeeded" | "done"
                )
            })
        {
            EventKind::ToolCompleted
        } else {
            EventKind::ToolCalled
        };
        events.push(CursorIdeEvent {
            kind,
            payload: json!({
                "bubble_id": bubble_id,
                "tool_former_data": tool,
                "documentary": true,
            }),
            timestamp,
            replay_policy: ReplayPolicy::HistoricalOnly,
            raw_type: "cursor_ide.tool_former_data",
        });
    }
    if text.is_none() && bubble.get("toolFormerData").is_none() {
        events.push(unsupported_cursor_ide_event(
            "unsupported_bubble",
            Some(raw),
            Some(bubble_id),
        ));
    }
}

fn push_cursor_ide_checkpoints(
    transaction: &Transaction<'_>,
    composer_id: &str,
    checkpoint_ids: &BTreeSet<String>,
    events: &mut Vec<CursorIdeEvent>,
) -> Result<()> {
    for checkpoint_id in checkpoint_ids {
        let key = format!("checkpointId:{composer_id}:{checkpoint_id}");
        events.push(CursorIdeEvent {
            kind: EventKind::CheckpointCreated,
            payload: json!({
                "checkpoint_id": checkpoint_id,
                "stored_bytes": cursor_disk_value_length(transaction, &key)?,
                "content": "omitted",
            }),
            timestamp: None,
            replay_policy: ReplayPolicy::HistoricalOnly,
            raw_type: "cursor_ide.checkpoint_metadata",
        });
    }
    Ok(())
}

fn push_cursor_ide_diffs(
    transaction: &Transaction<'_>,
    composer_id: &str,
    root: &Value,
    events: &mut Vec<CursorIdeEvent>,
) -> Result<()> {
    let Some(files) = root.get("codeBlockData").and_then(Value::as_object) else {
        return Ok(());
    };
    let mut count = 0usize;
    for (path, versions) in files {
        let values: Vec<&Value> = match versions {
            Value::Array(values) => values.iter().collect(),
            Value::Object(values) => values.values().collect(),
            _ => Vec::new(),
        };
        for value in values {
            let Some(diff_id) = value.get("diffId").and_then(Value::as_str) else {
                continue;
            };
            count += 1;
            if count > MAX_CURSOR_IDE_HEADERS {
                bail!("Cursor IDE session exceeds safe diff-metadata limit");
            }
            let key = format!("codeBlockDiff:{composer_id}:{diff_id}");
            events.push(CursorIdeEvent {
                kind: EventKind::FilePatch,
                payload: json!({
                    "path": path,
                    "diff_id": diff_id,
                    "bubble_id": value.get("bubbleId").cloned().unwrap_or(Value::Null),
                    "version": value.get("version").cloned().unwrap_or(Value::Null),
                    "status": value.get("status").cloned().unwrap_or(Value::Null),
                    "stored_bytes": cursor_disk_value_length(transaction, &key)?,
                    "content": "omitted",
                }),
                timestamp: parse_timestamp(value.get("createdAt")),
                replay_policy: ReplayPolicy::HistoricalOnly,
                raw_type: "cursor_ide.diff_metadata",
            });
        }
    }
    Ok(())
}

fn unsupported_cursor_ide_event(
    reason: &str,
    raw: Option<&[u8]>,
    bubble_id: Option<&str>,
) -> CursorIdeEvent {
    let opaque_sha256 = raw.map(|raw| hex::encode(Sha256::digest(raw)));
    CursorIdeEvent {
        kind: EventKind::ProviderEvent,
        payload: json!({
            "kind": "unsupported_cursor_ide_record",
            "reason": reason,
            "bubble_id": bubble_id,
            "opaque_bytes": raw.map(<[u8]>::len),
            "opaque_sha256": opaque_sha256,
            "fidelity": "unsupported",
        }),
        timestamp: None,
        replay_policy: ReplayPolicy::HistoricalOnly,
        raw_type: "cursor_ide.unknown",
    }
}

fn workspace_path(value: &Value) -> Option<PathBuf> {
    if let Some(path) = string_at(
        value,
        &[
            &["workspacePath"],
            &["projectPath"],
            &["cwd"],
            &["workspace", "path"],
            &["workspace", "folder"],
            &["workspace", "rootPath"],
            &["workspaceUri", "fsPath"],
            &["workspaceUri", "path"],
            &["workspace", "uri", "fsPath"],
            &["workspace", "uri", "path"],
        ],
    ) {
        return Some(PathBuf::from(path));
    }
    let uri = string_at(
        value,
        &[
            &["workspaceUri"],
            &["workspaceURI"],
            &["workspace", "uri"],
            &["workspaceUris", "0"],
        ],
    )
    .or_else(|| {
        let first = value
            .get("workspaceUris")
            .and_then(Value::as_array)
            .and_then(|uris| uris.first())?;
        first
            .as_str()
            .or_else(|| string_at(first, &[&["fsPath"], &["path"]]))
    })?;
    file_uri_path(uri)
}

fn file_uri_path(uri: &str) -> Option<PathBuf> {
    let encoded = uri.strip_prefix("file://")?;
    let decoded = percent_decode(encoded)?;
    #[cfg(target_os = "windows")]
    let decoded = decoded
        .strip_prefix('/')
        .filter(|path| path.as_bytes().get(1) == Some(&b':'))
        .unwrap_or(&decoded)
        .to_owned();
    Some(PathBuf::from(decoded))
}

fn percent_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let high = *bytes.get(index + 1)?;
            let low = *bytes.get(index + 2)?;
            decoded.push((hex_value(high)? << 4) | hex_value(low)?);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

const fn hex_value(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

impl Default for CursorIdeAdapter {
    fn default() -> Self {
        let metadata_root = env::var_os("CURSOR_IDE_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(default_cursor_ide_root);
        Self { metadata_root }
    }
}

#[cfg(target_os = "windows")]
fn default_cursor_ide_root() -> Option<PathBuf> {
    env::var_os("APPDATA")
        .map(PathBuf::from)
        .map(|root| root.join("Cursor").join("User"))
}

#[cfg(target_os = "macos")]
fn default_cursor_ide_root() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|directories| {
        directories
            .home_dir()
            .join("Library")
            .join("Application Support")
            .join("Cursor")
            .join("User")
    })
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn default_cursor_ide_root() -> Option<PathBuf> {
    directories::BaseDirs::new()
        .map(|directories| directories.config_dir().join("Cursor").join("User"))
}

impl ProviderAdapter for CursorIdeAdapter {
    fn provider(&self) -> Provider {
        Provider::CursorIde
    }

    fn probe(&self) -> ProviderInstallation {
        let database = self.database_path();
        ProviderInstallation {
            provider: Provider::CursorIde,
            installed: database.as_deref().is_some_and(Path::is_file),
            executable: None,
            data_root: self.metadata_root.clone(),
        }
    }

    fn list_sessions(&self, project: Option<&Path>) -> Result<Vec<NativeSession>> {
        let mut sessions = self
            .headers()?
            .into_iter()
            .filter(|header| {
                project.is_none_or(|project| {
                    header
                        .project_path
                        .as_deref()
                        .is_some_and(|recorded| paths_match(recorded, project))
                })
            })
            .map(CursorIdeHeader::into_native)
            .collect::<Vec<_>>();
        sort_sessions(&mut sessions);
        Ok(sessions)
    }

    fn read_session(&self, session: &SessionRef) -> Result<omnis_ir::CanonicalSnapshot> {
        validate_provider(session, Provider::CursorIde)?;
        let (header, events) = self.session_data(&session.id)?;
        let captured_at = header.updated_at.unwrap_or_else(Utc::now);
        let mut builder = EventBuilder::new(Provider::CursorIde, &session.id);
        builder.push(
            EventKind::ProviderEvent,
            json!({ "storage": "opaque", "metadata": header.metadata }),
            header.updated_at,
            ReplayPolicy::HistoricalOnly,
            Some("composerHeader".to_owned()),
            None,
        );
        for event in events {
            builder.push(
                event.kind,
                event.payload,
                event.timestamp,
                event.replay_policy,
                Some(event.raw_type.to_owned()),
                None,
            );
        }
        Ok(builder.snapshot(
            session.clone(),
            header.title,
            header.project_path,
            None,
            captured_at,
        ))
    }

    fn new_session_plan(&self, _target: &LaunchTarget) -> Result<LaunchPlan> {
        Err(anyhow!("Cursor IDE has no supported native launcher"))
    }

    fn launch_plan(&self, session: &SessionRef, _target: &LaunchTarget) -> Result<LaunchPlan> {
        validate_provider(session, Provider::CursorIde)?;
        Err(anyhow!(
            "Cursor IDE sessions have no supported native launcher"
        ))
    }
}
