use std::{
    borrow::Cow,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow};
use base64::{Engine, engine::general_purpose::STANDARD};
use chrono::{DateTime, Utc};
use omnis_ir::{CanonicalSnapshot, EventKind, Provider, ReplayPolicy, SessionRef};
use prost::Message;
use rusqlite::OptionalExtension;
use serde_json::{Value, json};

use crate::{
    LaunchPlan, LaunchTarget, NativeSession, ProviderAdapter, ProviderInstallation,
    support::{
        EventBuilder, MAX_STREAMED_TRANSCRIPT_FILE_SIZE, estimated_event_bytes, json_lines_preview,
        parse_timestamp, paths_match, provider_executable, provider_file, provider_root,
        sort_sessions, sqlite_snapshot, store_is_missing, validate_provider, visit_json_lines,
    },
};

const SUMMARY_DATABASE: &str = "conversation_summaries.db";
const PREVIEW_RECORDS: usize = 1_024;

#[derive(Clone, Debug)]
pub struct AntigravityAdapter {
    root: Option<PathBuf>,
}

impl AntigravityAdapter {
    #[must_use]
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        Self {
            root: Some(root.into()),
        }
    }

    fn root(&self) -> Result<&Path> {
        self.root
            .as_deref()
            .ok_or_else(|| anyhow!("Antigravity data root was not found"))
    }

    fn summary(&self, id: &str) -> Result<Summary> {
        validate_id(id)?;
        let root = self.root()?;
        let database = root.join(SUMMARY_DATABASE);
        let snapshot = sqlite_snapshot(root, &database)
            .context("failed to snapshot Antigravity summary database")?;
        snapshot
            .connection
            .query_row(
                "SELECT conversation_id, title, step_count, last_modified_time, \
                 workspace_uris FROM conversation_summaries WHERE conversation_id = ?1",
                [id],
                summary_from_row,
            )
            .optional()?
            .ok_or_else(|| anyhow!("Antigravity session `{id}` was not found"))
    }

    fn transcript(&self, id: &str) -> Option<PathBuf> {
        let root = self.root.as_deref()?;
        validate_id(id).ok()?;
        let logs = root
            .join("brain")
            .join(id)
            .join(".system_generated")
            .join("logs");
        ["transcript_full.jsonl", "transcript.jsonl"]
            .into_iter()
            .find_map(|name| provider_file(root, &logs.join(name)))
    }

    fn conversation_database(&self, id: &str) -> Option<PathBuf> {
        let root = self.root.as_deref()?;
        validate_id(id).ok()?;
        provider_file(root, &root.join("conversations").join(format!("{id}.db")))
    }

    fn read(&self, session: &SessionRef, preview: bool) -> Result<CanonicalSnapshot> {
        validate_provider(session, Provider::Antigravity)?;
        let summary = self.summary(&session.id)?;
        let captured_at = summary.updated_at.unwrap_or_else(Utc::now);
        let mut builder = EventBuilder::new(Provider::Antigravity, &session.id);

        if let Some(path) = self.transcript(&session.id) {
            if preview {
                for record in json_lines_preview(&path, PREVIEW_RECORDS)? {
                    push_transcript_record(&mut builder, &record);
                }
            } else {
                let oversized_records =
                    visit_json_lines(&path, MAX_STREAMED_TRANSCRIPT_FILE_SIZE, |record| {
                        push_transcript_record(&mut builder, &record);
                        Ok(())
                    })?;
                builder.push_oversized_record_notice(oversized_records, summary.updated_at);
            }
        } else if let Some(path) = self.conversation_database(&session.id) {
            push_database_steps(self.root()?, &path, &mut builder)?;
        } else if summary.step_count > 0 {
            return Err(anyhow!(
                "Antigravity session `{}` declares history but has no readable transcript",
                session.id
            ));
        }

        Ok(builder.snapshot(
            session.clone(),
            nonempty(summary.title),
            summary.project_path,
            None,
            captured_at,
        ))
    }
}

impl Default for AntigravityAdapter {
    fn default() -> Self {
        Self {
            root: provider_root("ANTIGRAVITY_CLI_HOME", &[".gemini", "antigravity-cli"]),
        }
    }
}

impl ProviderAdapter for AntigravityAdapter {
    fn provider(&self) -> Provider {
        Provider::Antigravity
    }

    fn probe(&self) -> ProviderInstallation {
        let executable = provider_executable(Provider::Antigravity);
        let data_root = self.root.clone().filter(|root| root.is_dir());
        ProviderInstallation {
            provider: Provider::Antigravity,
            installed: executable.is_some() || data_root.is_some(),
            executable,
            data_root,
        }
    }

    fn list_sessions(&self, project: Option<&Path>) -> Result<Vec<NativeSession>> {
        let Some(root) = self.root.as_deref() else {
            return Ok(Vec::new());
        };
        let database = root.join(SUMMARY_DATABASE);
        if store_is_missing(&database) {
            return Ok(Vec::new());
        }
        let snapshot = sqlite_snapshot(root, &database)
            .context("failed to snapshot Antigravity summary database")?;
        let mut statement = snapshot.connection.prepare(
            "SELECT conversation_id, title, step_count, last_modified_time, workspace_uris \
             FROM conversation_summaries",
        )?;
        let rows = statement.query_map([], summary_from_row)?;
        let mut sessions = Vec::new();
        for row in rows {
            let summary = row?;
            if validate_id(&summary.id).is_err()
                || project.is_some_and(|requested| {
                    summary
                        .project_path
                        .as_deref()
                        .is_none_or(|recorded| !paths_match(recorded, requested))
                })
            {
                continue;
            }
            sessions.push(NativeSession {
                session: SessionRef::new(Provider::Antigravity, summary.id.clone()),
                title: nonempty(summary.title),
                project_path: summary.project_path,
                git_branch: None,
                created_at: None,
                updated_at: summary.updated_at,
                updated_at_approximate: false,
                event_count: summary.step_count,
                source_path: self.conversation_database(&summary.id),
            });
        }
        sort_sessions(&mut sessions);
        Ok(sessions)
    }

    fn read_session(&self, session: &SessionRef) -> Result<CanonicalSnapshot> {
        self.read(session, false)
    }

    fn preview_session(&self, session: &SessionRef) -> Result<CanonicalSnapshot> {
        self.read(session, true)
    }

    fn new_session_plan(&self, target: &LaunchTarget) -> Result<LaunchPlan> {
        let mut args = Vec::new();
        if let Some(prompt) = &target.prompt {
            args.push("--prompt-interactive".to_owned());
            args.push(prompt.clone());
        }
        Ok(LaunchPlan {
            program: "agy".to_owned(),
            args,
            cwd: target.cwd.clone(),
            env: Vec::new(),
        })
    }

    fn launch_plan(&self, session: &SessionRef, target: &LaunchTarget) -> Result<LaunchPlan> {
        validate_provider(session, Provider::Antigravity)?;
        validate_id(&session.id)?;
        if target.fork {
            return Err(anyhow!(
                "agy 1.1.8 has no documented native conversation fork command"
            ));
        }
        let mut args = vec!["--conversation".to_owned(), session.id.clone()];
        if let Some(prompt) = &target.prompt {
            args.push("--prompt-interactive".to_owned());
            args.push(prompt.clone());
        }
        Ok(LaunchPlan {
            program: "agy".to_owned(),
            args,
            cwd: target.cwd.clone(),
            env: Vec::new(),
        })
    }
}

#[derive(Debug)]
struct Summary {
    id: String,
    title: String,
    step_count: usize,
    updated_at: Option<DateTime<Utc>>,
    project_path: Option<PathBuf>,
}

fn summary_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Summary> {
    let workspace_uris: String = row.get(4)?;
    let step_count = row.get::<_, i64>(2)?;
    Ok(Summary {
        id: row.get(0)?,
        title: row.get(1)?,
        step_count: usize::try_from(step_count.max(0)).unwrap_or(usize::MAX),
        updated_at: row
            .get::<_, String>(3)
            .ok()
            .and_then(|value| DateTime::parse_from_rfc3339(&value).ok())
            .map(|value| value.with_timezone(&Utc)),
        project_path: workspace_path(&workspace_uris),
    })
}

fn workspace_path(serialized: &str) -> Option<PathBuf> {
    let uri = serde_json::from_str::<Vec<String>>(serialized)
        .ok()?
        .into_iter()
        .next()?;
    workspace_uri_path(&uri)
}

/// Decodes a local `file://` workspace URI.
pub(crate) fn workspace_uri_path(uri: &str) -> Option<PathBuf> {
    let path = uri.strip_prefix("file://")?;
    if !path.starts_with('/') {
        return None;
    }
    let bytes = path.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let high = hex_value(bytes[index + 1])?;
            let low = hex_value(bytes[index + 2])?;
            decoded.push((high << 4) | low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    let decoded = String::from_utf8(decoded).ok()?;
    #[cfg(windows)]
    let decoded = decoded
        .strip_prefix('/')
        .filter(|path| path.as_bytes().get(1) == Some(&b':'))
        .unwrap_or(&decoded);
    Some(PathBuf::from(decoded))
}

fn hex_value(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

pub(crate) fn validate_id(id: &str) -> Result<()> {
    uuid::Uuid::parse_str(id)
        .map(|_| ())
        .map_err(|_| anyhow!("invalid Antigravity conversation ID `{id}`"))
}

pub(crate) fn nonempty(value: String) -> Option<String> {
    (!value.is_empty()).then_some(value)
}

fn push_transcript_record(builder: &mut EventBuilder, record: &Value) {
    let record_type = record.get("type").and_then(Value::as_str).unwrap_or("");
    let timestamp = parse_timestamp(record.get("created_at"));
    let status = record.get("status").and_then(Value::as_str).unwrap_or("");
    match record_type {
        "USER_INPUT" => {
            if let Some(text) = record.get("content").and_then(Value::as_str) {
                let text = antigravity_user_request(text);
                if text.is_empty() {
                    return;
                }
                builder.push(
                    EventKind::MessageUser,
                    json!({"text": text}),
                    timestamp,
                    ReplayPolicy::Contextual,
                    Some(record_type.to_owned()),
                    None,
                );
            }
        }
        "PLANNER_RESPONSE" => {
            if let Some(text) = record.get("content").and_then(Value::as_str) {
                builder.push(
                    EventKind::MessageAssistant,
                    json!({"text": text}),
                    timestamp,
                    ReplayPolicy::Contextual,
                    Some(record_type.to_owned()),
                    None,
                );
            }
            if let Some(calls) = record.get("tool_calls").and_then(Value::as_array) {
                for call in calls {
                    let Some(name) = call.get("name").and_then(Value::as_str) else {
                        continue;
                    };
                    builder.push(
                        EventKind::ToolCalled,
                        json!({"name": name, "arguments": call.get("args").cloned().unwrap_or(Value::Null)}),
                        timestamp,
                        ReplayPolicy::HistoricalOnly,
                        Some(record_type.to_owned()),
                        None,
                    );
                }
            }
        }
        "RUN_COMMAND" => push_historical(
            builder,
            record_type,
            status,
            timestamp,
            EventKind::CommandExecuted,
        ),
        "CHECKPOINT" => push_historical(
            builder,
            record_type,
            status,
            timestamp,
            EventKind::CheckpointCreated,
        ),
        "CODE_ACTION" | "WRITE_TO_FILE" | "FILE_CHANGE" => {
            push_historical(
                builder,
                record_type,
                status,
                timestamp,
                EventKind::FilePatch,
            );
        }
        "VIEW_FILE" | "READ_URL_CONTENT" | "READ_RESOURCE" => {
            push_historical(builder, record_type, status, timestamp, EventKind::FileRead);
        }
        "" => {}
        _ => push_historical(
            builder,
            record_type,
            status,
            timestamp,
            EventKind::ProviderEvent,
        ),
    }
}

pub(crate) fn antigravity_user_request(text: &str) -> &str {
    let Some(request) = text.strip_prefix("<USER_REQUEST>") else {
        return text;
    };
    request
        .split_once("</USER_REQUEST>")
        .map_or(text, |(request, _)| request.trim())
}

fn push_historical(
    builder: &mut EventBuilder,
    record_type: &str,
    status: &str,
    timestamp: Option<DateTime<Utc>>,
    kind: EventKind,
) {
    builder.push(
        kind,
        json!({"status": status}),
        timestamp,
        ReplayPolicy::HistoricalOnly,
        Some(record_type.to_owned()),
        None,
    );
}

/// Pushes decoded conversation steps and returns how many stored steps were decoded.
pub(crate) fn push_database_steps(
    root: &Path,
    database: &Path,
    builder: &mut EventBuilder,
) -> Result<usize> {
    push_database_steps_with_limits(root, database, builder, DatabaseReadLimits::default())
}

#[derive(Clone, Copy)]
struct DatabaseReadLimits {
    row_bytes: usize,
    wire_bytes: usize,
    history_bytes: usize,
    records: usize,
}

impl Default for DatabaseReadLimits {
    fn default() -> Self {
        Self {
            row_bytes: 16 * 1024 * 1024,
            wire_bytes: 64 * 1024 * 1024,
            history_bytes: 64 * 1024 * 1024,
            records: 100_000,
        }
    }
}

fn push_database_steps_with_limits(
    root: &Path,
    database: &Path,
    builder: &mut EventBuilder,
    limits: DatabaseReadLimits,
) -> Result<usize> {
    let snapshot = sqlite_snapshot(root, database)
        .context("failed to snapshot Antigravity conversation database")?;
    let stored = usize::try_from(snapshot.connection.query_row(
        "SELECT count(*) FROM steps",
        [],
        |row| row.get::<_, i64>(0),
    )?)?;
    let mut statement = snapshot
        .connection
        .prepare("SELECT length(step_payload), CASE WHEN length(step_payload) <= ?1 THEN step_payload ELSE NULL END FROM steps ORDER BY idx LIMIT ?2")?;
    let mut rows = statement.query(rusqlite::params![
        i64::try_from(limits.row_bytes)?,
        i64::try_from(limits.records)?,
    ])?;
    let mut decoded = 0;
    let mut visited = 0;
    let mut remaining_wire = limits.wire_bytes;
    let mut remaining_history = limits.history_bytes;
    let mut omitted = stored.saturating_sub(limits.records);
    let mut unsupported = 0;
    while let Some(row) = rows.next()? {
        let bytes = usize::try_from(row.get::<_, i64>(0)?)?;
        visited += 1;
        if bytes > limits.row_bytes {
            omitted += 1;
            continue;
        }
        if bytes > remaining_wire {
            omitted = omitted.saturating_add(stored.min(limits.records) - visited + 1);
            break;
        }
        remaining_wire -= bytes;
        let payload = row.get::<_, Vec<u8>>(1)?;
        let step = ProtoStep::decode(payload.as_slice())?;
        decoded += 1;
        let event = database_step_event(step, &payload);
        unsupported += usize::from(event.replay_policy == ReplayPolicy::HistoricalOnly);
        let bytes = estimated_event_bytes(&event.payload);
        if bytes > remaining_history {
            omitted += 1;
        } else {
            remaining_history -= bytes;
            builder.push(
                event.kind,
                event.payload,
                event.timestamp,
                event.replay_policy,
                Some(event.record_type),
                None,
            );
        }
    }
    if omitted > 0 || unsupported > 0 {
        builder.push(
            EventKind::ProviderEvent,
            json!({"omitted_events": omitted, "unsupported_text_records": unsupported}),
            None,
            ReplayPolicy::HistoricalOnly,
            Some("omnisession.antigravity_step_limit".to_owned()),
            None,
        );
    }
    Ok(decoded)
}

struct DatabaseStepEvent {
    kind: EventKind,
    payload: Value,
    timestamp: Option<DateTime<Utc>>,
    replay_policy: ReplayPolicy,
    record_type: String,
}

fn database_step_event(step: ProtoStep, wire: &[u8]) -> DatabaseStepEvent {
    let timestamp = step
        .metadata
        .and_then(|metadata| metadata.created_at)
        .as_ref()
        .and_then(proto_timestamp);
    let opaque = || {
        (
            EventKind::ProviderEvent,
            json!({"status": step.status.to_string(), "step_payload_base64": STANDARD.encode(wire)}),
            ReplayPolicy::HistoricalOnly,
            format!("STEP_TYPE_{}", step.r#type),
        )
    };
    let (kind, payload, replay_policy, record_type) = match step.step {
        Some(proto_step::Step::UserInput(input)) => {
            let text = user_input_text(&input);
            let query = antigravity_user_request(&text);
            if query.is_empty() {
                opaque()
            } else {
                (
                    EventKind::MessageUser,
                    json!({"text": query}),
                    ReplayPolicy::Contextual,
                    "USER_INPUT".to_owned(),
                )
            }
        }
        Some(proto_step::Step::PlannerResponse(response)) => {
            let text = if response.modified_response.is_empty() {
                response.response
            } else {
                response.modified_response
            };
            if text.is_empty() {
                opaque()
            } else {
                (
                    EventKind::MessageAssistant,
                    json!({"text": text}),
                    ReplayPolicy::Contextual,
                    "PLANNER_RESPONSE".to_owned(),
                )
            }
        }
        _ => opaque(),
    };
    DatabaseStepEvent {
        kind,
        payload,
        timestamp,
        replay_policy,
        record_type,
    }
}

/// Antigravity CLI writes `query`. The desktop app leaves it empty and stores typed text in
/// `user_response` and text `items`.
fn user_input_text(input: &ProtoUserInput) -> Cow<'_, str> {
    [&input.query, &input.user_response]
        .into_iter()
        .find(|text| !text.is_empty())
        .map_or_else(
            || Cow::Owned(input.items.iter().map(|item| item.text.as_str()).collect()),
            |text| Cow::Borrowed(text.as_str()),
        )
}

pub(crate) fn proto_timestamp(value: &ProtoTimestamp) -> Option<DateTime<Utc>> {
    DateTime::from_timestamp(value.seconds, u32::try_from(value.nanos).ok()?)
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct ProtoStep {
    #[prost(int32, tag = "1")]
    pub(crate) r#type: i32,
    #[prost(int32, tag = "4")]
    pub(crate) status: i32,
    #[prost(message, optional, tag = "5")]
    pub(crate) metadata: Option<ProtoStepMetadata>,
    #[prost(oneof = "proto_step::Step", tags = "19, 20")]
    pub(crate) step: Option<proto_step::Step>,
}

pub(crate) mod proto_step {
    use prost::Oneof;

    use super::{ProtoPlannerResponse, ProtoUserInput};

    #[derive(Clone, PartialEq, Oneof)]
    pub(crate) enum Step {
        #[prost(message, tag = "19")]
        UserInput(ProtoUserInput),
        #[prost(message, tag = "20")]
        PlannerResponse(ProtoPlannerResponse),
    }
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct ProtoStepMetadata {
    #[prost(message, optional, tag = "1")]
    pub(crate) created_at: Option<ProtoTimestamp>,
    #[prost(int32, tag = "3")]
    pub(crate) source: i32,
    #[prost(string, tag = "12")]
    pub(crate) execution_id: String,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct ProtoTimestamp {
    #[prost(int64, tag = "1")]
    pub(crate) seconds: i64,
    #[prost(int32, tag = "2")]
    pub(crate) nanos: i32,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct ProtoUserInput {
    #[prost(string, tag = "1")]
    pub(crate) query: String,
    #[prost(string, tag = "2")]
    pub(crate) user_response: String,
    #[prost(message, repeated, tag = "3")]
    pub(crate) items: Vec<ProtoTextOrScopeItem>,
}

/// Text chunk of `TextOrScopeItem`. Scope items (field 2) are skipped.
#[derive(Clone, PartialEq, Message)]
pub(crate) struct ProtoTextOrScopeItem {
    #[prost(string, tag = "1")]
    pub(crate) text: String,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct ProtoPlannerResponse {
    #[prost(string, tag = "1")]
    pub(crate) response: String,
    #[prost(string, tag = "8")]
    pub(crate) modified_response: String,
}

#[cfg(test)]
pub(crate) fn synthetic_step(step_type: i32, step: Option<proto_step::Step>) -> ProtoStep {
    ProtoStep {
        r#type: step_type,
        status: 3,
        metadata: Some(ProtoStepMetadata {
            created_at: Some(ProtoTimestamp {
                seconds: 1_785_240_000,
                nanos: 0,
            }),
            source: 0,
            execution_id: String::new(),
        }),
        step,
    }
}

/// Writes the `steps` table shared by Antigravity CLI and desktop app conversation databases.
#[cfg(test)]
pub(crate) fn write_steps_database(database: &Path, steps: &[ProtoStep]) {
    let connection = rusqlite::Connection::open(database).expect("conversation database");
    connection
        .execute_batch(
            "CREATE TABLE steps (
                idx INTEGER PRIMARY KEY,
                step_type INTEGER NOT NULL DEFAULT 0,
                step_payload BLOB
            );",
        )
        .expect("steps schema");
    for (index, step) in steps.iter().enumerate() {
        connection
            .execute(
                "INSERT INTO steps (idx, step_type, step_payload) VALUES (?1, ?2, ?3)",
                rusqlite::params![
                    i64::try_from(index).expect("index"),
                    step.r#type,
                    step.encode_to_vec()
                ],
            )
            .expect("step row");
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use omnis_ir::{EventKind, Provider, SessionRef};

    use super::{
        EventBuilder, ProtoPlannerResponse, ProtoTextOrScopeItem, ProtoUserInput,
        antigravity_user_request, proto_step, push_database_steps, synthetic_step, user_input_text,
        workspace_path, write_steps_database,
    };

    #[test]
    fn decodes_local_workspace_uri() {
        assert_eq!(
            workspace_path(r#"["file:///tmp/project%20space"]"#).as_deref(),
            Some(std::path::Path::new("/tmp/project space"))
        );
        assert!(workspace_path(r#"["file://remote/path"]"#).is_none());
    }

    #[test]
    fn unwraps_user_request_without_copying_runtime_metadata() {
        let wrapped = "<USER_REQUEST>\nquestion\n</USER_REQUEST>\n<ADDITIONAL_METADATA>synthetic metadata</ADDITIONAL_METADATA>\n<USER_SETTINGS_CHANGE>synthetic settings</USER_SETTINGS_CHANGE>";

        assert_eq!(antigravity_user_request(wrapped), "question");
        assert_eq!(
            antigravity_user_request("<div>keep markup</div>"),
            "<div>keep markup</div>"
        );
    }

    #[test]
    fn reads_message_steps_from_native_database() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let database = temporary.path().join("conversation.db");
        write_steps_database(
            &database,
            &[
                synthetic_step(
                    14,
                    Some(proto_step::Step::UserInput(ProtoUserInput {
                        query: "question".to_owned(),
                        ..ProtoUserInput::default()
                    })),
                ),
                synthetic_step(
                    15,
                    Some(proto_step::Step::PlannerResponse(ProtoPlannerResponse {
                        response: "answer".to_owned(),
                        modified_response: String::new(),
                    })),
                ),
            ],
        );

        let mut builder = EventBuilder::new(
            Provider::Antigravity,
            "11111111-1111-4111-8111-111111111111",
        );
        let decoded =
            push_database_steps(temporary.path(), &database, &mut builder).expect("native steps");
        assert_eq!(decoded, 2);
        let snapshot = builder.snapshot(
            SessionRef::new(
                Provider::Antigravity,
                "11111111-1111-4111-8111-111111111111",
            ),
            None,
            None,
            None,
            Utc::now(),
        );
        assert_eq!(snapshot.events.len(), 2);
        assert_eq!(snapshot.events[0].kind, EventKind::MessageUser);
        assert_eq!(snapshot.events[0].payload["text"], "question");
        assert_eq!(snapshot.events[1].kind, EventKind::MessageAssistant);
        assert_eq!(snapshot.events[1].payload["text"], "answer");
    }

    #[test]
    fn database_budgets_skip_large_blobs_and_report_history_omissions_without_mutation() {
        use prost::Message;

        let temporary = tempfile::tempdir().unwrap();
        let database = temporary.path().join("conversation.db");
        let step = synthetic_step(
            14,
            Some(proto_step::Step::UserInput(ProtoUserInput {
                query: "Synthetic visible history".to_owned(),
                ..ProtoUserInput::default()
            })),
        );
        let wire_bytes = step.encoded_len();
        write_steps_database(&database, &[step.clone(), step]);
        let connection = rusqlite::Connection::open(&database).unwrap();
        connection
            .execute(
                "INSERT INTO steps (idx, step_payload) VALUES (2, zeroblob(4096))",
                [],
            )
            .unwrap();
        drop(connection);
        let before = std::fs::read(&database).unwrap();
        let visible_bytes = crate::support::estimated_event_bytes(
            &serde_json::json!({"text":"Synthetic visible history"}),
        );
        for (limits, omitted) in [
            (
                super::DatabaseReadLimits {
                    row_bytes: wire_bytes,
                    ..super::DatabaseReadLimits::default()
                },
                1,
            ),
            (
                super::DatabaseReadLimits {
                    row_bytes: 1024,
                    history_bytes: visible_bytes,
                    ..super::DatabaseReadLimits::default()
                },
                2,
            ),
            (
                super::DatabaseReadLimits {
                    records: 1,
                    ..super::DatabaseReadLimits::default()
                },
                2,
            ),
            (
                super::DatabaseReadLimits {
                    wire_bytes,
                    ..super::DatabaseReadLimits::default()
                },
                2,
            ),
        ] {
            let mut builder = EventBuilder::new(
                Provider::Antigravity,
                "11111111-1111-4111-8111-111111111111",
            );
            let decoded = super::push_database_steps_with_limits(
                temporary.path(),
                &database,
                &mut builder,
                limits,
            )
            .unwrap();
            assert!(decoded > 0);
            let snapshot = builder.snapshot(
                SessionRef::new(
                    Provider::Antigravity,
                    "11111111-1111-4111-8111-111111111111",
                ),
                None,
                None,
                None,
                Utc::now(),
            );
            assert_eq!(snapshot.events[0].kind, EventKind::MessageUser);
            assert_eq!(
                snapshot.events[0].payload["text"],
                "Synthetic visible history"
            );
            let notice = snapshot.events.last().unwrap();
            assert_eq!(notice.payload["omitted_events"], omitted);
            assert_eq!(notice.replay_policy, omnis_ir::ReplayPolicy::HistoricalOnly);
        }
        assert_eq!(std::fs::read(&database).unwrap(), before);
    }

    #[test]
    fn unknown_database_steps_keep_opaque_wire_history_and_report_unsupported_text() {
        use prost::Message;

        #[derive(Clone, PartialEq, Message)]
        struct FutureStep {
            #[prost(int32, tag = "1")]
            kind: i32,
            #[prost(string, tag = "99")]
            future_text: String,
        }

        let wire = FutureStep {
            kind: 999,
            future_text: "Synthetic future native history".to_owned(),
        }
        .encode_to_vec();
        assert_opaque_database_step(&wire);
    }

    #[test]
    fn scope_only_user_steps_and_empty_responses_preserve_wire_history() {
        use prost::Message;

        #[derive(Clone, PartialEq, Message)]
        struct ScopeItem {
            #[prost(bytes = "vec", tag = "2")]
            scope: Vec<u8>,
        }
        #[derive(Clone, PartialEq, Message)]
        struct ScopeInput {
            #[prost(message, repeated, tag = "3")]
            items: Vec<ScopeItem>,
        }
        #[derive(Clone, PartialEq, Message)]
        struct ScopeStep {
            #[prost(int32, tag = "1")]
            kind: i32,
            #[prost(message, optional, tag = "19")]
            input: Option<ScopeInput>,
        }
        let scope_only = ScopeStep {
            kind: 14,
            input: Some(ScopeInput {
                items: vec![ScopeItem { scope: vec![8, 1] }],
            }),
        }
        .encode_to_vec();
        assert_opaque_database_step(&scope_only);
        assert_opaque_database_step(
            &synthetic_step(
                15,
                Some(proto_step::Step::PlannerResponse(
                    ProtoPlannerResponse::default(),
                )),
            )
            .encode_to_vec(),
        );
    }

    fn assert_opaque_database_step(wire: &[u8]) {
        use base64::Engine;

        let temporary = tempfile::tempdir().unwrap();
        let database = temporary.path().join("conversation.db");
        write_steps_database(&database, &[]);
        let connection = rusqlite::Connection::open(&database).unwrap();
        connection
            .execute(
                "INSERT INTO steps (idx, step_payload) VALUES (0, ?1)",
                [wire],
            )
            .unwrap();
        drop(connection);
        let before = std::fs::read(&database).unwrap();
        let mut builder = EventBuilder::new(
            Provider::Antigravity,
            "11111111-1111-4111-8111-111111111111",
        );
        assert_eq!(
            push_database_steps(temporary.path(), &database, &mut builder).unwrap(),
            1
        );
        let snapshot = builder.snapshot(
            SessionRef::new(
                Provider::Antigravity,
                "11111111-1111-4111-8111-111111111111",
            ),
            None,
            None,
            None,
            Utc::now(),
        );
        assert_eq!(snapshot.events.len(), 2);
        assert_eq!(snapshot.events[0].kind, EventKind::ProviderEvent);
        assert_eq!(
            snapshot.events[0].replay_policy,
            omnis_ir::ReplayPolicy::HistoricalOnly
        );
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(
                    snapshot.events[0].payload["step_payload_base64"]
                        .as_str()
                        .unwrap()
                )
                .unwrap(),
            wire
        );
        assert_eq!(snapshot.events[1].payload["unsupported_text_records"], 1);
        assert!(omnis_core::import_conversation(&snapshot).truncated);
        assert_eq!(std::fs::read(&database).unwrap(), before);
    }

    #[test]
    fn reads_user_text_from_query_then_response_then_items() {
        let items = vec![
            ProtoTextOrScopeItem {
                text: "first ".to_owned(),
            },
            ProtoTextOrScopeItem {
                text: "second".to_owned(),
            },
        ];
        let input = ProtoUserInput {
            query: "query".to_owned(),
            user_response: "response".to_owned(),
            items: items.clone(),
        };
        assert_eq!(user_input_text(&input), "query");
        let input = ProtoUserInput {
            query: String::new(),
            ..input
        };
        assert_eq!(user_input_text(&input), "response");
        let input = ProtoUserInput {
            items,
            ..ProtoUserInput::default()
        };
        assert_eq!(user_input_text(&input), "first second");
    }
}
