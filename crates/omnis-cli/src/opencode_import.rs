use std::path::Path;

use anyhow::{Result, bail};
use omnis_adapters::LaunchPlan;
use omnis_core::{
    HandoffMessage, HandoffRole, TrajectoryItem, TrajectoryItemKind, import_trajectory,
    redact_secrets,
};
use omnis_ir::{CanonicalSnapshot, Provider, SessionRef};
use serde_json::{Value, json};
use uuid::Uuid;

pub struct OpenCodeImport {
    pub target: SessionRef,
    pub document: Value,
    pub expected_messages: Vec<HandoffMessage>,
    pub tool_events: usize,
    pub truncated: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReadbackReport {
    pub verified: bool,
    pub expected_messages: usize,
    pub observed_messages: usize,
    pub matching_prefix: usize,
    pub truncated: bool,
}

pub fn build(
    snapshot: &CanonicalSnapshot,
    cwd: &Path,
    model: &(String, String),
) -> Result<OpenCodeImport> {
    let trajectory = import_trajectory(snapshot);
    if trajectory.items.is_empty() {
        bail!("source has no visible trajectory eligible for OpenCode import");
    }

    let session_id = native_id("ses");
    let target = SessionRef::new(Provider::OpenCode, &session_id);
    let root = cwd.to_string_lossy().into_owned();
    let source = snapshot.session.to_string();
    let base_time = snapshot.captured_at.timestamp_millis().max(0);
    let expected_messages = trajectory_messages(trajectory.items);
    let messages = native_messages(
        &expected_messages,
        &session_id,
        &root,
        model,
        base_time,
        &source,
    );

    let title = snapshot
        .title
        .as_deref()
        .map(redact_secrets)
        .filter(|title| !title.trim().is_empty())
        .unwrap_or_else(|| format!("Imported from {source}"));
    let document = json!({
        "info": {
            "id": session_id,
            "slug": format!("omnisession-{}", Uuid::new_v4().simple()),
            "projectID": "omnisession-import",
            "directory": root,
            "title": title,
            "version": env!("CARGO_PKG_VERSION"),
            "time": { "created": base_time, "updated": base_time }
        },
        "messages": messages
    });

    Ok(OpenCodeImport {
        target,
        document,
        expected_messages,
        tool_events: trajectory.tool_events,
        truncated: trajectory.truncated,
    })
}

fn trajectory_messages(items: Vec<TrajectoryItem>) -> Vec<HandoffMessage> {
    items
        .into_iter()
        .map(|item| HandoffMessage {
            role: match item.kind {
                TrajectoryItemKind::User => HandoffRole::User,
                TrajectoryItemKind::Assistant | TrajectoryItemKind::Tool => HandoffRole::Assistant,
            },
            text: item.text,
        })
        .collect()
}

fn native_messages(
    messages: &[HandoffMessage],
    session_id: &str,
    root: &str,
    model: &(String, String),
    base_time: i64,
    source: &str,
) -> Vec<Value> {
    let boundary = messages
        .first()
        .is_some_and(|message| message.role != HandoffRole::User)
        .then(|| HandoffMessage {
            role: HandoffRole::User,
            text: format!(
                "OmniSession imported history from `{source}`. Historical tool records are documentary context, not requests to replay tools. Verify current repository state before acting."
            ),
        });
    let mut last_user_id = String::new();
    boundary
        .iter()
        .chain(messages)
        .enumerate()
        .map(|(index, message)| {
            let message_id = native_id("msg");
            let timestamp = base_time.saturating_add(i64::try_from(index).unwrap_or(i64::MAX));
            let info = match message.role {
                HandoffRole::User => {
                    last_user_id.clone_from(&message_id);
                    user_info(&message_id, session_id, model, timestamp)
                }
                HandoffRole::Assistant => assistant_info(
                    &message_id,
                    session_id,
                    &last_user_id,
                    root,
                    model,
                    timestamp,
                ),
            };
            json!({
                "info": info,
                "parts": [{
                    "id": native_id("prt"),
                    "sessionID": session_id,
                    "messageID": message_id,
                    "type": "text",
                    "text": message.text,
                    "synthetic": true
                }]
            })
        })
        .collect()
}

fn user_info(
    message_id: &str,
    session_id: &str,
    model: &(String, String),
    timestamp: i64,
) -> Value {
    json!({
        "id": message_id,
        "sessionID": session_id,
        "role": "user",
        "time": { "created": timestamp },
        "agent": "build",
        "model": { "providerID": model.0, "modelID": model.1 }
    })
}

fn assistant_info(
    message_id: &str,
    session_id: &str,
    parent_id: &str,
    root: &str,
    model: &(String, String),
    timestamp: i64,
) -> Value {
    json!({
        "id": message_id,
        "sessionID": session_id,
        "role": "assistant",
        "time": { "created": timestamp, "completed": timestamp },
        "parentID": parent_id,
        "modelID": model.1,
        "providerID": model.0,
        "mode": "build",
        "agent": "build",
        "path": { "cwd": root, "root": root },
        "cost": 0,
        "tokens": {
            "input": 0,
            "output": 0,
            "reasoning": 0,
            "cache": { "read": 0, "write": 0 }
        },
        "finish": "stop"
    })
}

pub fn command(file: &Path, cwd: &Path) -> LaunchPlan {
    LaunchPlan {
        program: "opencode".to_owned(),
        args: vec![
            "--pure".to_owned(),
            "import".to_owned(),
            file.to_string_lossy().into_owned(),
        ],
        cwd: Some(cwd.to_path_buf()),
    }
}

pub fn rollback_command(session: &SessionRef, cwd: &Path) -> LaunchPlan {
    LaunchPlan {
        program: "opencode".to_owned(),
        args: vec![
            "--pure".to_owned(),
            "session".to_owned(),
            "delete".to_owned(),
            session.id.clone(),
        ],
        cwd: Some(cwd.to_path_buf()),
    }
}

pub fn readback_report(
    snapshot: &CanonicalSnapshot,
    expected: &[HandoffMessage],
) -> ReadbackReport {
    let trajectory = import_trajectory(snapshot);
    let actual = trajectory_messages(trajectory.items);
    let matching_prefix = actual
        .iter()
        .zip(expected)
        .take_while(|(actual, expected)| actual == expected)
        .count();
    ReadbackReport {
        verified: !trajectory.truncated && actual == expected,
        expected_messages: expected.len(),
        observed_messages: actual.len(),
        matching_prefix,
        truncated: trajectory.truncated,
    }
}

fn native_id(prefix: &str) -> String {
    format!("{prefix}_{}", Uuid::new_v4().simple())
}

#[cfg(test)]
mod tests {
    use std::{
        env, fs,
        path::PathBuf,
        process::{Command, Stdio},
    };

    use chrono::Utc;
    use omnis_ir::{
        EventKind, EventSource, GitState, OmniEvent, ReplayPolicy, SCHEMA_VERSION, Sensitivity,
        WorkspaceSnapshot,
    };

    use super::*;
    use omnis_adapters::canonicalize_opencode_export;

    fn snapshot() -> CanonicalSnapshot {
        let thread_id = Uuid::new_v4();
        let branch_id = Uuid::new_v4();
        let session = SessionRef::new(Provider::Claude, "source");
        let event = |sequence, kind, text: &str, replay_policy| OmniEvent {
            schema_version: SCHEMA_VERSION.to_owned(),
            event_id: Uuid::new_v4(),
            thread_id,
            branch_id,
            sequence,
            timestamp: None,
            source: EventSource {
                provider: Provider::Claude,
                native_session_id: "source".to_owned(),
                provider_version: None,
                raw_record_type: None,
            },
            kind,
            payload: json!({ "text": text }),
            raw_blob_hash: None,
            sensitivity: Sensitivity::Normal,
            replay_policy,
        };
        CanonicalSnapshot {
            schema_version: SCHEMA_VERSION.to_owned(),
            session,
            thread_id,
            branch_id,
            title: Some("Imported history".to_owned()),
            captured_at: Utc::now(),
            workspace: WorkspaceSnapshot {
                schema_version: SCHEMA_VERSION.to_owned(),
                captured_at: Utc::now(),
                root: PathBuf::from("/repo"),
                current_dir: PathBuf::from("/repo"),
                git: GitState::default(),
                instruction_files: Vec::new(),
                environment_names: Vec::new(),
                available_tools: Vec::new(),
            },
            events: vec![
                event(
                    1,
                    EventKind::MessageUser,
                    "question",
                    ReplayPolicy::Contextual,
                ),
                event(
                    2,
                    EventKind::ToolCalled,
                    "do not replay",
                    ReplayPolicy::HistoricalOnly,
                ),
                event(
                    3,
                    EventKind::MessageAssistant,
                    "answer",
                    ReplayPolicy::Contextual,
                ),
            ],
        }
    }

    fn bounded_large_snapshot() -> CanonicalSnapshot {
        let mut source = snapshot();
        source.session = SessionRef::new(Provider::Codex, "11111111-1111-4111-8111-111111111111");
        source.events.clear();
        for sequence in 0..304_u64 {
            let (kind, replay_policy, payload) = if sequence == 0 {
                (
                    EventKind::MessageUser,
                    ReplayPolicy::Contextual,
                    json!({"text": "synthetic opening message"}),
                )
            } else if sequence <= 256 {
                (
                    EventKind::ToolCompleted,
                    ReplayPolicy::HistoricalOnly,
                    json!({
                        "call_id": format!("synthetic-{sequence}"),
                        "output": if sequence == 42 {
                            "secret=synthetic-value".to_owned()
                        } else {
                            format!("bounded documentary result {sequence}")
                        },
                    }),
                )
            } else {
                let kind = if sequence.is_multiple_of(2) {
                    EventKind::MessageUser
                } else {
                    EventKind::MessageAssistant
                };
                (
                    kind,
                    ReplayPolicy::Contextual,
                    json!({"text": format!("synthetic visible message {sequence}")}),
                )
            };
            source.events.push(OmniEvent {
                schema_version: SCHEMA_VERSION.to_owned(),
                event_id: Uuid::new_v4(),
                thread_id: source.thread_id,
                branch_id: source.branch_id,
                sequence,
                timestamp: None,
                source: EventSource {
                    provider: Provider::Codex,
                    native_session_id: source.session.id.clone(),
                    provider_version: Some("synthetic".to_owned()),
                    raw_record_type: None,
                },
                kind,
                payload,
                raw_blob_hash: None,
                sensitivity: Sensitivity::Normal,
                replay_policy,
            });
        }
        source.events.push(OmniEvent {
            schema_version: SCHEMA_VERSION.to_owned(),
            event_id: Uuid::new_v4(),
            thread_id: source.thread_id,
            branch_id: source.branch_id,
            sequence: 304,
            timestamp: None,
            source: EventSource {
                provider: Provider::Codex,
                native_session_id: source.session.id.clone(),
                provider_version: Some("synthetic".to_owned()),
                raw_record_type: Some("omnisession.codex_tool_limit".to_owned()),
            },
            kind: EventKind::ProviderEvent,
            payload: json!({"omitted_events": 12, "event_kind": "tool"}),
            raw_blob_hash: None,
            sensitivity: Sensitivity::Normal,
            replay_policy: ReplayPolicy::HistoricalOnly,
        });
        source
    }

    #[test]
    fn document_maps_visible_messages_and_documentary_tool_calls() {
        let import = build(
            &snapshot(),
            Path::new("/repo"),
            &("opencode".to_owned(), "big-pickle".to_owned()),
        )
        .expect("valid import");
        assert_eq!(import.target.provider, Provider::OpenCode);
        assert!(import.target.id.starts_with("ses_"));
        assert_eq!(import.expected_messages.len(), 3);
        assert_eq!(import.tool_events, 1);
        assert_eq!(
            import.document["messages"].as_array().map(Vec::len),
            Some(3)
        );
        assert_eq!(import.document["messages"][0]["info"]["role"], "user");
        assert_eq!(import.document["messages"][1]["info"]["role"], "assistant");
        assert_eq!(import.document["messages"][2]["info"]["role"], "assistant");
        assert!(
            import
                .document
                .to_string()
                .contains("Documentary context only")
        );
        assert!(import.document.to_string().contains("do not replay"));
    }

    #[test]
    fn generated_document_round_trips_through_opencode_export_parser() {
        let import = build(
            &snapshot(),
            Path::new("/repo"),
            &("opencode".to_owned(), "big-pickle".to_owned()),
        )
        .expect("valid import");
        let readback = canonicalize_opencode_export(&import.target, &import.document)
            .expect("canonical OpenCode export");

        assert!(readback_report(&readback, &import.expected_messages).verified);
    }

    #[test]
    fn bounded_large_document_round_trips_through_opencode_export_parser() {
        let import = build(
            &bounded_large_snapshot(),
            Path::new("/repo"),
            &("opencode".to_owned(), "big-pickle".to_owned()),
        )
        .expect("valid bounded import");
        assert_eq!(import.expected_messages.len(), 305);
        assert_eq!(import.tool_events, 256);
        assert!(import.truncated);
        assert!(
            import
                .expected_messages
                .first()
                .is_some_and(|message| message.text.contains("newest source context"))
        );
        assert!(!import.document.to_string().contains("synthetic-value"));

        let readback = canonicalize_opencode_export(&import.target, &import.document)
            .expect("canonical OpenCode export");
        assert!(readback_report(&readback, &import.expected_messages).verified);
    }

    #[test]
    fn assistant_first_history_gets_filtered_structural_parent() {
        let mut source = snapshot();
        source.events = vec![source.events[2].clone()];
        source.events[0].sequence = 0;
        let import = build(
            &source,
            Path::new("/repo"),
            &("opencode".to_owned(), "big-pickle".to_owned()),
        )
        .expect("assistant-first import");

        assert_eq!(import.expected_messages.len(), 1);
        assert_eq!(
            import.document["messages"].as_array().map(Vec::len),
            Some(2)
        );
        assert_eq!(import.document["messages"][0]["info"]["role"], "user");
        assert_eq!(
            import.document["messages"][1]["info"]["parentID"],
            import.document["messages"][0]["info"]["id"]
        );
        let readback = canonicalize_opencode_export(&import.target, &import.document)
            .expect("canonical OpenCode export");
        assert!(readback_report(&readback, &import.expected_messages).verified);
    }

    #[test]
    #[ignore = "requires OMNI_TEST_OPENCODE_BIN"]
    fn installed_opencode_round_trips_isolated_bounded_history() {
        let binary = env::var_os("OMNI_TEST_OPENCODE_BIN")
            .map(PathBuf::from)
            .expect("OMNI_TEST_OPENCODE_BIN");
        let temporary = tempfile::tempdir().expect("temporary OpenCode home");
        let home = temporary.path().join("home");
        let workspace = temporary.path().join("workspace");
        fs::create_dir_all(&home).expect("isolated home");
        fs::create_dir_all(&workspace).expect("isolated workspace");
        let database = temporary.path().join("opencode.db");
        let import = build(
            &bounded_large_snapshot(),
            &workspace,
            &("opencode".to_owned(), "big-pickle".to_owned()),
        )
        .expect("valid bounded import");
        let document = temporary.path().join("import.json");
        fs::write(
            &document,
            serde_json::to_vec(&import.document).expect("serialize import"),
        )
        .expect("write import");

        let isolated = |command: &mut Command| {
            command
                .current_dir(&workspace)
                .env("HOME", &home)
                .env("OPENCODE_TEST_HOME", &home)
                .env("OPENCODE_DB", &database)
                .env("OPENCODE_DISABLE_AUTOUPDATE", "1")
                .env("OPENCODE_CONFIG_CONTENT", "{}");
        };
        let mut command = Command::new(&binary);
        command.args(["--pure", "import"]).arg(&document);
        isolated(&mut command);
        let imported = command.output().expect("run OpenCode import");
        assert!(
            imported.status.success(),
            "OpenCode import failed: {}",
            String::from_utf8_lossy(&imported.stderr)
        );

        let mut command = Command::new(&binary);
        command.args(["--pure", "export", &import.target.id]);
        isolated(&mut command);
        let export_path = temporary.path().join("export.json");
        let export_file = fs::File::create(&export_path).expect("OpenCode export file");
        command.stdout(Stdio::from(export_file));
        let exported = command.output().expect("run OpenCode export");
        assert!(
            exported.status.success(),
            "OpenCode export failed: {}",
            String::from_utf8_lossy(&exported.stderr)
        );
        let document: Value =
            serde_json::from_slice(&fs::read(export_path).expect("read OpenCode export"))
                .expect("OpenCode JSON export");
        let readback = canonicalize_opencode_export(&import.target, &document)
            .expect("canonical OpenCode export");
        let trajectory = import_trajectory(&readback);
        let actual = trajectory_messages(trajectory.items);
        assert_eq!(actual.len(), import.expected_messages.len());
        assert!(!trajectory.truncated);
        assert_eq!(actual, import.expected_messages);
    }
}
