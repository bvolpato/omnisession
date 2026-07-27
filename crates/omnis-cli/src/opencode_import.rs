use std::path::Path;

use anyhow::{Result, bail};
use omnis_adapters::LaunchPlan;
use omnis_core::{
    HandoffMessage, HandoffRole, TrajectoryItem, TrajectoryItemKind, import_conversation,
    import_trajectory, redact_secrets,
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
    let expected_messages = trajectory_messages(&source, trajectory.items);
    let messages = native_messages(&expected_messages, &session_id, &root, model, base_time);

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

fn trajectory_messages(source: &str, items: Vec<TrajectoryItem>) -> Vec<HandoffMessage> {
    let boundary = HandoffMessage {
        role: HandoffRole::User,
        text: format!(
            "OmniSession imported history from `{source}`. Historical tool records are documentary context, not requests to replay tools. Verify current repository state before acting."
        ),
    };
    let mut messages = Vec::with_capacity(items.len() + 1);
    messages.push(boundary);
    messages.extend(items.into_iter().map(|item| HandoffMessage {
        role: match item.kind {
            TrajectoryItemKind::User => HandoffRole::User,
            TrajectoryItemKind::Assistant | TrajectoryItemKind::Tool => HandoffRole::Assistant,
        },
        text: item.text,
    }));
    messages
}

fn native_messages(
    messages: &[HandoffMessage],
    session_id: &str,
    root: &str,
    model: &(String, String),
    base_time: i64,
) -> Vec<Value> {
    let mut last_user_id = String::new();
    messages
        .iter()
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

pub fn readback_matches(snapshot: &CanonicalSnapshot, expected: &[HandoffMessage]) -> bool {
    import_conversation(snapshot).messages == expected
}

fn native_id(prefix: &str) -> String {
    format!("{prefix}_{}", Uuid::new_v4().simple())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use chrono::Utc;
    use omnis_ir::{
        EventKind, EventSource, GitState, OmniEvent, ReplayPolicy, SCHEMA_VERSION, Sensitivity,
        WorkspaceSnapshot,
    };

    use super::*;

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
        assert_eq!(import.expected_messages.len(), 4);
        assert_eq!(import.tool_events, 1);
        assert_eq!(
            import.document["messages"].as_array().map(Vec::len),
            Some(4)
        );
        assert_eq!(import.document["messages"][1]["info"]["role"], "user");
        assert_eq!(import.document["messages"][2]["info"]["role"], "assistant");
        assert_eq!(import.document["messages"][3]["info"]["role"], "assistant");
        assert!(
            import
                .document
                .to_string()
                .contains("Documentary context only")
        );
        assert!(import.document.to_string().contains("do not replay"));
    }
}
