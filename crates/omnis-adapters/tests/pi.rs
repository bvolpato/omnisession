use std::{fs, path::Path};

use omnis_adapters::{LaunchTarget, PiAdapter, ProviderAdapter};
use omnis_ir::{EventKind, Provider, ReplayPolicy, SessionRef};
use serde_json::json;

const SESSION_ID: &str = "pi-session-42";

#[test]
#[allow(clippy::too_many_lines)]
fn pi_v3_reads_active_compacted_branch_without_retaining_reasoning() {
    let temporary = tempfile::tempdir().expect("temporary Pi root");
    let root = temporary.path().join("sessions");
    let directory = root.join("--workspace-demo--");
    fs::create_dir_all(&directory).expect("Pi session directory");
    let records = vec![
        json!({
            "type": "session", "version": 3, "id": SESSION_ID,
            "timestamp": "2026-01-01T00:00:00.000Z", "cwd": "/workspace/demo"
        }),
        json!({
            "type": "message", "id": "11111111", "parentId": null,
            "timestamp": "2026-01-01T00:00:01.000Z",
            "message": {"role": "user", "content": "compacted question", "timestamp": 1}
        }),
        json!({
            "type": "message", "id": "22222222", "parentId": "11111111",
            "timestamp": "2026-01-01T00:00:02.000Z",
            "message": {
                "role": "assistant", "timestamp": 2, "api": "test", "provider": "test",
                "model": "test", "usage": {}, "stopReason": "toolUse",
                "content": [
                    {"type": "text", "text": "retained answer"},
                    {"type": "thinking", "thinking": "private reasoning must not persist"},
                    {"type": "toolCall", "id": "call-1", "name": "read", "arguments": {"path": "x"}}
                ]
            }
        }),
        json!({
            "type": "message", "id": "33333333", "parentId": "22222222",
            "timestamp": "2026-01-01T00:00:03.000Z",
            "message": {
                "role": "toolResult", "timestamp": 3, "toolCallId": "call-1", "toolName": "read",
                "content": [{"type": "text", "text": "documentary output"}], "isError": false
            }
        }),
        json!({
            "type": "message", "id": "44444444", "parentId": "11111111",
            "timestamp": "2026-01-01T00:00:04.000Z",
            "message": {"role": "user", "content": "abandoned branch", "timestamp": 4}
        }),
        json!({
            "type": "branch_summary", "id": "55555555", "parentId": "33333333",
            "timestamp": "2026-01-01T00:00:05.000Z", "fromId": "44444444", "summary": "branch summary"
        }),
        json!({
            "type": "compaction", "id": "66666666", "parentId": "55555555",
            "timestamp": "2026-01-01T00:00:06.000Z", "summary": "compaction summary",
            "firstKeptEntryId": "22222222", "tokensBefore": 100
        }),
        json!({
            "type": "message", "id": "77777777", "parentId": "66666666",
            "timestamp": "2026-01-01T00:00:07.000Z",
            "message": {"role": "user", "content": "visible question", "timestamp": 7}
        }),
        json!({
            "type": "message", "id": "88888888", "parentId": "77777777",
            "timestamp": "2026-01-01T00:00:08.000Z",
            "message": {
                "role": "assistant", "timestamp": 8, "api": "test", "provider": "test",
                "model": "test", "usage": {}, "stopReason": "stop",
                "content": [{"type": "text", "text": "visible answer"}]
            }
        }),
        json!({
            "type": "session_info", "id": "99999999", "parentId": "88888888",
            "timestamp": "2026-01-01T00:00:09.000Z", "name": "Pi fixture"
        }),
    ];
    let document = records
        .iter()
        .map(serde_json::Value::to_string)
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    let source = directory.join("2026-01-01T00-00-00-000Z_pi-session-42.jsonl");
    fs::write(&source, document).expect("Pi fixture");

    let adapter = PiAdapter::with_root(&root);
    let discovered = adapter
        .list_sessions(Some(Path::new("/workspace/demo")))
        .expect("Pi discovery");
    assert_eq!(discovered.len(), 1);
    assert_eq!(discovered[0].session.id, SESSION_ID);
    assert_eq!(discovered[0].event_count, 0);

    let before = fs::read(&source).expect("fixture before read");
    let snapshot = adapter
        .read_session(&SessionRef::new(Provider::Pi, SESSION_ID))
        .expect("Pi read");
    assert_eq!(before, fs::read(&source).expect("fixture after read"));
    assert_eq!(snapshot.title.as_deref(), Some("Pi fixture"));
    assert!(
        snapshot
            .events
            .iter()
            .any(|event| event.kind == EventKind::CompactionCreated)
    );
    assert!(
        snapshot
            .events
            .iter()
            .any(|event| event.kind == EventKind::HandoffCreated)
    );
    assert!(snapshot.events.iter().any(|event| {
        event.kind == EventKind::ToolCalled && event.replay_policy == ReplayPolicy::HistoricalOnly
    }));
    assert!(snapshot.events.iter().any(|event| {
        event.kind == EventKind::ToolCompleted
            && event.replay_policy == ReplayPolicy::HistoricalOnly
    }));
    let rendered = serde_json::to_string(&snapshot).expect("serialize Pi snapshot");
    assert!(rendered.contains("visible question"));
    assert!(rendered.contains("retained answer"));
    assert!(!rendered.contains("compacted question"));
    assert!(!rendered.contains("abandoned branch"));
    assert!(!rendered.contains("private reasoning must not persist"));
}

#[test]
fn pi_launch_plans_use_documented_session_and_fork_flags() {
    let adapter = PiAdapter::default();
    let session = SessionRef::new(Provider::Pi, "abc123");
    let target = LaunchTarget {
        cwd: Some("/workspace/demo".into()),
        fork: false,
        prompt: Some("continue".to_owned()),
    };
    assert_eq!(
        adapter
            .launch_plan(&session, &target)
            .expect("Pi session plan")
            .args,
        ["--session", "abc123", "continue"]
    );
    assert_eq!(
        adapter
            .launch_plan(
                &session,
                &LaunchTarget {
                    fork: true,
                    ..target
                },
            )
            .expect("Pi fork plan")
            .args,
        ["--fork", "abc123", "continue"]
    );
}

#[test]
fn pi_discovery_reads_only_bounded_header() {
    let temporary = tempfile::tempdir().expect("temporary Pi root");
    let root = temporary.path().join("sessions");
    let directory = root.join("--workspace-demo--");
    fs::create_dir_all(&directory).expect("Pi session directory");
    let header = json!({
        "type": "session", "version": 3, "id": SESSION_ID,
        "timestamp": "2026-01-01T00:00:00.000Z", "cwd": "/workspace/demo"
    })
    .to_string();
    let source = directory.join("large-tail.jsonl");
    let mut fixture = format!("{header}\n").into_bytes();
    fixture.extend(std::iter::repeat_n(b'x', 2 * 1024 * 1024 + 1));
    fs::write(source, fixture).expect("Pi large-tail fixture");

    let sessions = PiAdapter::with_root(&root)
        .list_sessions(None)
        .expect("bounded Pi discovery");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].session.id, SESSION_ID);
}
