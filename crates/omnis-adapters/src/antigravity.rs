use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use omnis_ir::{CanonicalSnapshot, EventKind, Provider, ReplayPolicy, SessionRef};
use prost::Message;
use rusqlite::OptionalExtension;
use serde_json::{Value, json};

use crate::{
    LaunchPlan, LaunchTarget, NativeSession, ProviderAdapter, ProviderInstallation,
    support::{
        EventBuilder, executable, json_lines_preview, parse_timestamp, paths_match, provider_file,
        provider_root, sort_sessions, sqlite_snapshot, validate_provider, visit_json_lines,
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
                visit_json_lines(&path, |record| {
                    push_transcript_record(&mut builder, &record);
                    Ok(())
                })?;
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
        let executable = executable("agy");
        let data_root = self.root.clone().filter(|root| root.is_dir());
        ProviderInstallation {
            provider: Provider::Antigravity,
            installed: executable.is_some() || data_root.is_some(),
            executable,
            data_root,
        }
    }

    fn list_sessions(&self, project: Option<&Path>) -> Result<Vec<NativeSession>> {
        let root = self.root()?;
        let database = root.join(SUMMARY_DATABASE);
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

fn validate_id(id: &str) -> Result<()> {
    uuid::Uuid::parse_str(id)
        .map(|_| ())
        .map_err(|_| anyhow!("invalid Antigravity conversation ID `{id}`"))
}

fn nonempty(value: String) -> Option<String> {
    (!value.is_empty()).then_some(value)
}

fn push_transcript_record(builder: &mut EventBuilder, record: &Value) {
    let record_type = record.get("type").and_then(Value::as_str).unwrap_or("");
    let timestamp = parse_timestamp(record.get("created_at"));
    let status = record.get("status").and_then(Value::as_str).unwrap_or("");
    match record_type {
        "USER_INPUT" => {
            if let Some(text) = record.get("content").and_then(Value::as_str) {
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

fn push_database_steps(root: &Path, database: &Path, builder: &mut EventBuilder) -> Result<()> {
    let snapshot = sqlite_snapshot(root, database)
        .context("failed to snapshot Antigravity conversation database")?;
    let mut statement = snapshot
        .connection
        .prepare("SELECT step_payload FROM steps ORDER BY idx")?;
    let rows = statement.query_map([], |row| row.get::<_, Vec<u8>>(0))?;
    for payload in rows {
        let payload = payload?;
        let step = ProtoStep::decode(payload.as_slice())?;
        let timestamp = step
            .metadata
            .and_then(|metadata| metadata.created_at)
            .and_then(|value| {
                DateTime::from_timestamp(value.seconds, u32::try_from(value.nanos).ok()?)
            });
        match step.step {
            Some(proto_step::Step::UserInput(input)) if !input.query.is_empty() => builder.push(
                EventKind::MessageUser,
                json!({"text": input.query}),
                timestamp,
                ReplayPolicy::Contextual,
                Some("USER_INPUT".to_owned()),
                None,
            ),
            Some(proto_step::Step::PlannerResponse(response)) => {
                let text = if response.modified_response.is_empty() {
                    response.response
                } else {
                    response.modified_response
                };
                if !text.is_empty() {
                    builder.push(
                        EventKind::MessageAssistant,
                        json!({"text": text}),
                        timestamp,
                        ReplayPolicy::Contextual,
                        Some("PLANNER_RESPONSE".to_owned()),
                        None,
                    );
                }
            }
            _ => push_historical(
                builder,
                &format!("STEP_TYPE_{}", step.r#type),
                &step.status.to_string(),
                timestamp,
                EventKind::ProviderEvent,
            ),
        }
    }
    Ok(())
}

#[derive(Clone, PartialEq, Message)]
struct ProtoStep {
    #[prost(int32, tag = "1")]
    r#type: i32,
    #[prost(int32, tag = "4")]
    status: i32,
    #[prost(message, optional, tag = "5")]
    metadata: Option<ProtoStepMetadata>,
    #[prost(oneof = "proto_step::Step", tags = "19, 20")]
    step: Option<proto_step::Step>,
}

mod proto_step {
    use prost::Oneof;

    use super::{ProtoPlannerResponse, ProtoUserInput};

    #[derive(Clone, PartialEq, Oneof)]
    pub(super) enum Step {
        #[prost(message, tag = "19")]
        UserInput(ProtoUserInput),
        #[prost(message, tag = "20")]
        PlannerResponse(ProtoPlannerResponse),
    }
}

#[derive(Clone, PartialEq, Message)]
struct ProtoStepMetadata {
    #[prost(message, optional, tag = "1")]
    created_at: Option<ProtoTimestamp>,
    #[prost(int32, tag = "3")]
    source: i32,
    #[prost(string, tag = "12")]
    execution_id: String,
}

#[derive(Clone, PartialEq, Message)]
struct ProtoTimestamp {
    #[prost(int64, tag = "1")]
    seconds: i64,
    #[prost(int32, tag = "2")]
    nanos: i32,
}

#[derive(Clone, PartialEq, Message)]
struct ProtoUserInput {
    #[prost(string, tag = "1")]
    query: String,
}

#[derive(Clone, PartialEq, Message)]
struct ProtoPlannerResponse {
    #[prost(string, tag = "1")]
    response: String,
    #[prost(string, tag = "8")]
    modified_response: String,
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use omnis_ir::{EventKind, Provider, SessionRef};
    use prost::Message;
    use rusqlite::{Connection, params};

    use super::{
        EventBuilder, ProtoPlannerResponse, ProtoStep, ProtoStepMetadata, ProtoUserInput,
        proto_step, push_database_steps, workspace_path,
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
    fn reads_message_steps_from_native_database() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let database = temporary.path().join("conversation.db");
        let connection = Connection::open(&database).expect("conversation database");
        connection
            .execute_batch(
                "CREATE TABLE steps (
                    idx INTEGER PRIMARY KEY,
                    step_payload BLOB NOT NULL
                );",
            )
            .expect("steps schema");
        for (index, step) in [
            proto_step::Step::UserInput(ProtoUserInput {
                query: "question".to_owned(),
            }),
            proto_step::Step::PlannerResponse(ProtoPlannerResponse {
                response: "answer".to_owned(),
                modified_response: String::new(),
            }),
        ]
        .into_iter()
        .enumerate()
        {
            let step_type = if index == 0 { 14 } else { 15 };
            let payload = ProtoStep {
                r#type: step_type,
                status: 3,
                metadata: Some(ProtoStepMetadata {
                    created_at: None,
                    source: if index == 0 { 4 } else { 2 },
                    execution_id: format!("synthetic-{index}"),
                }),
                step: Some(step),
            }
            .encode_to_vec();
            connection
                .execute(
                    "INSERT INTO steps (idx, step_payload) VALUES (?1, ?2)",
                    params![i64::try_from(index).expect("index"), payload],
                )
                .expect("step row");
        }
        drop(connection);

        let mut builder = EventBuilder::new(
            Provider::Antigravity,
            "11111111-1111-4111-8111-111111111111",
        );
        push_database_steps(temporary.path(), &database, &mut builder).expect("native steps");
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
}
