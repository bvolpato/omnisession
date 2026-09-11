use std::path::Path;

use anyhow::{Result, bail};
use chrono::Utc;
use omnis_adapters::LaunchPlan;
use omnis_core::{
    HandoffRole, NativeTrajectoryItem, import_trajectory, native_trajectory_items,
    native_trajectory_signature, readback_trajectory, redact_secrets,
};
use omnis_ir::{CanonicalSnapshot, Provider, SessionRef};
use serde_json::{Value, json};
use uuid::Uuid;

pub struct OpenCodeImport {
    pub target: SessionRef,
    pub document: Value,
    pub expected_items: Vec<NativeTrajectoryItem>,
    pub native_tool_records: usize,
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

    let base_time = snapshot.captured_at.timestamp_millis().max(0);
    let import_time = Utc::now().timestamp_millis().max(0);
    let session_id = descending_id("ses", import_time, 1);
    let target = SessionRef::new(Provider::OpenCode, &session_id);
    let root = cwd.to_string_lossy().into_owned();
    let source = snapshot.session.to_string();
    let native_items = native_trajectory_items(&trajectory);
    let native_tool_records = native_items
        .iter()
        .filter(|item| matches!(item, NativeTrajectoryItem::Tool { .. }))
        .count();
    let expected_items = native_trajectory_signature(&trajectory);
    let messages = native_messages(
        &native_items,
        &session_id,
        &root,
        model,
        base_time,
        import_time,
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
        expected_items,
        native_tool_records,
        tool_events: trajectory.tool_events,
        truncated: trajectory.truncated,
    })
}

fn native_messages(
    items: &[NativeTrajectoryItem],
    session_id: &str,
    root: &str,
    model: &(String, String),
    base_time: i64,
    id_anchor: i64,
    source: &str,
) -> Vec<Value> {
    let boundary = items
        .first()
        .is_some_and(|item| {
            !matches!(
                item,
                NativeTrajectoryItem::Message {
                    role: HandoffRole::User,
                    ..
                }
            )
        })
        .then(|| NativeTrajectoryItem::Message {
            role: HandoffRole::User,
            text: format!(
                "OmniSession imported history from `{source}`. Historical tool records are documentary context, not requests to replay tools. Verify current repository state before acting."
            ),
        });
    let total = usize::from(boundary.is_some()) + items.len();
    let mut last_user_id = String::new();
    boundary
        .iter()
        .chain(items)
        .enumerate()
        .map(|(index, item)| {
            let timestamp = base_time.saturating_add(i64::try_from(index).unwrap_or(i64::MAX));
            // IDs count back from import time, so every imported record sorts before OpenCode's next one.
            let id_time =
                id_anchor.saturating_sub(i64::try_from(total - index).unwrap_or(i64::MAX));
            let message_id = ascending_id("msg", id_time, 1);
            let info = match item {
                NativeTrajectoryItem::Message {
                    role: HandoffRole::User,
                    ..
                } => {
                    last_user_id.clone_from(&message_id);
                    user_info(&message_id, session_id, model, timestamp)
                }
                _ => assistant_info(
                    &message_id,
                    session_id,
                    &last_user_id,
                    root,
                    model,
                    timestamp,
                ),
            };
            let part_id = ascending_id("prt", id_time, 2);
            let part = match item {
                NativeTrajectoryItem::Message { text, .. } => json!({
                    "id": part_id,
                    "sessionID": session_id,
                    "messageID": message_id,
                    "type": "text",
                    "text": text,
                    "synthetic": true
                }),
                NativeTrajectoryItem::Tool {
                    call_id,
                    name,
                    input,
                    output,
                    is_error,
                } => {
                    let state = if *is_error {
                        json!({
                            "status": "error",
                            "input": input,
                            "error": output,
                            "time": { "start": timestamp, "end": timestamp }
                        })
                    } else {
                        json!({
                            "status": "completed",
                            "input": input,
                            "output": output,
                            "title": name,
                            "metadata": {},
                            "time": { "start": timestamp, "end": timestamp }
                        })
                    };
                    json!({
                        "id": part_id,
                        "sessionID": session_id,
                        "messageID": message_id,
                        "type": "tool",
                        "callID": call_id,
                        "tool": name,
                        "state": state
                    })
                }
            };
            json!({ "info": info, "parts": [part] })
        })
        .collect()
}

// OpenCode orders messages and parts by ascending ID and sessions by descending ID, so imported
// records must use its time-encoded format to sort before anything created later.
fn ascending_id(prefix: &str, timestamp: i64, counter: u64) -> String {
    opencode_id(prefix, time_value(timestamp, counter))
}

fn descending_id(prefix: &str, timestamp: i64, counter: u64) -> String {
    opencode_id(prefix, !time_value(timestamp, counter))
}

fn time_value(timestamp: i64, counter: u64) -> u64 {
    u64::try_from(timestamp)
        .unwrap_or(0)
        .saturating_mul(0x1000)
        .saturating_add(counter)
}

fn opencode_id(prefix: &str, value: u64) -> String {
    const BASE62: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    let suffix = Uuid::new_v4()
        .as_bytes()
        .iter()
        .take(14)
        .map(|byte| char::from(BASE62[usize::from(*byte % 62)]))
        .collect::<String>();
    format!(
        "{prefix}_{}{suffix}",
        hex::encode(&value.to_be_bytes()[2..])
    )
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
    expected: &[NativeTrajectoryItem],
) -> ReadbackReport {
    let trajectory = readback_trajectory(snapshot);
    let truncated = trajectory.is_none();
    let actual = trajectory
        .as_ref()
        .map(native_trajectory_signature)
        .unwrap_or_default();
    let matching_prefix = actual
        .iter()
        .zip(expected)
        .take_while(|(actual, expected)| actual == expected)
        .count();
    ReadbackReport {
        verified: !truncated && actual == expected,
        expected_messages: expected.len(),
        observed_messages: actual.len(),
        matching_prefix,
        truncated,
    }
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
                let kind = if sequence % 2 == 0 {
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
        assert_eq!(import.expected_items.len(), 3);
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
    fn complete_tool_pairs_become_native_tool_parts() {
        let mut source = snapshot();
        let template = source.events[0].clone();
        source.events.splice(
            1..2,
            [
                OmniEvent {
                    event_id: Uuid::new_v4(),
                    sequence: 2,
                    kind: EventKind::ToolCalled,
                    payload: json!({"type": "function_call", "name": "shell", "call_id": "call_a", "arguments": "{\"command\":\"cargo test\"}"}),
                    replay_policy: ReplayPolicy::HistoricalOnly,
                    ..template.clone()
                },
                OmniEvent {
                    event_id: Uuid::new_v4(),
                    sequence: 3,
                    kind: EventKind::ToolCompleted,
                    payload: json!({"type": "function_call_output", "call_id": "call_a", "output": "x".repeat(7_900)}),
                    replay_policy: ReplayPolicy::HistoricalOnly,
                    ..template.clone()
                },
            ],
        );
        source.events[3].sequence = 4;
        let import = build(
            &source,
            Path::new("/repo"),
            &("opencode".to_owned(), "big-pickle".to_owned()),
        )
        .expect("valid import");

        assert_eq!(import.native_tool_records, 1);
        let part = &import.document["messages"][1]["parts"][0];
        assert_eq!(part["type"], "tool");
        assert_eq!(part["tool"], "hist_claude_shell");
        assert_eq!(part["state"]["status"], "completed");
        assert_eq!(part["state"]["input"]["command"], "cargo test");
        assert!(
            part["callID"]
                .as_str()
                .is_some_and(|id| id.starts_with("omni_"))
        );
        let readback = canonicalize_opencode_export(&import.target, &import.document)
            .expect("canonical OpenCode export");
        assert!(readback_report(&readback, &import.expected_items).verified);
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

        assert!(readback_report(&readback, &import.expected_items).verified);
    }

    #[test]
    fn generated_ids_sort_like_native_opencode_records() {
        let mut before_wrap = bounded_large_snapshot();
        before_wrap.captured_at =
            chrono::TimeZone::with_ymd_and_hms(&chrono::Utc, 2026, 7, 1, 0, 0, 0)
                .single()
                .expect("capture time");
        let imports = [bounded_large_snapshot(), before_wrap].map(|snapshot| {
            build(
                &snapshot,
                Path::new("/repo"),
                &("opencode".to_owned(), "big-pickle".to_owned()),
            )
            .expect("valid bounded import")
        });
        let now = chrono::Utc::now().timestamp_millis();
        let later_message = ascending_id("msg", now, 1);
        let later_session = descending_id("ses", now + 1, 1);

        for import in &imports {
            let ids = import.document["messages"]
                .as_array()
                .expect("messages")
                .iter()
                .flat_map(|message| {
                    std::iter::once(&message["info"]["id"])
                        .chain(
                            message["parts"]
                                .as_array()
                                .expect("parts")
                                .iter()
                                .map(|part| &part["id"]),
                        )
                        .map(|id| id.as_str().expect("record ID").to_owned())
                })
                .collect::<Vec<_>>();
            let bodies = ids
                .iter()
                .map(|id| id.split_once('_').expect("prefixed ID").1)
                .collect::<Vec<_>>();

            assert!(bodies.iter().all(|body| body.len() == 26));
            assert!(bodies.windows(2).all(|pair| pair[0] < pair[1]));
            assert!(
                ids.iter()
                    .filter(|id| id.starts_with("msg_"))
                    .all(|id| *id < later_message)
            );
            assert!(later_session < import.target.id);
        }
    }

    #[test]
    fn bounded_large_document_round_trips_through_opencode_export_parser() {
        let import = build(
            &bounded_large_snapshot(),
            Path::new("/repo"),
            &("opencode".to_owned(), "big-pickle".to_owned()),
        )
        .expect("valid bounded import");
        assert_eq!(import.expected_items.len(), 305);
        assert_eq!(import.tool_events, 256);
        assert!(import.truncated);
        assert!(matches!(
            import.expected_items.first(),
            Some(NativeTrajectoryItem::Message { text, .. }) if text.contains("newest source context")
        ));
        assert!(!import.document.to_string().contains("synthetic-value"));

        let readback = canonicalize_opencode_export(&import.target, &import.document)
            .expect("canonical OpenCode export");
        assert!(readback_report(&readback, &import.expected_items).verified);
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

        assert_eq!(import.expected_items.len(), 1);
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
        assert!(readback_report(&readback, &import.expected_items).verified);
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
        let mut source = bounded_large_snapshot();
        source.events[255].kind = EventKind::ToolCalled;
        source.events[255].payload = json!({"type": "function_call", "name": "shell", "call_id": "synthetic-pair", "arguments": "{\"command\":\"cargo test\"}"});
        source.events[256].kind = EventKind::ToolFailed;
        source.events[256].payload = json!({"type": "function_call_output", "call_id": "synthetic-pair", "output": "x".repeat(7_900)});
        let import = build(
            &source,
            &workspace,
            &("opencode".to_owned(), "big-pickle".to_owned()),
        )
        .expect("valid bounded import");
        assert_eq!(import.native_tool_records, 1);
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
        let tool_parts = document["messages"]
            .as_array()
            .expect("exported OpenCode messages")
            .iter()
            .flat_map(|message| message["parts"].as_array().into_iter().flatten())
            .filter(|part| part["type"] == "tool")
            .count();
        assert_eq!(tool_parts, 1, "exported native tool parts");
        let readback = canonicalize_opencode_export(&import.target, &document)
            .expect("canonical OpenCode export");
        let report = readback_report(&readback, &import.expected_items);
        assert!(
            report.verified,
            "OpenCode read-back matched {} of {} items",
            report.matching_prefix, report.expected_messages
        );
    }
}
