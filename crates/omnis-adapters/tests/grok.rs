use std::{fs, path::Path};

use omnis_adapters::{GrokAdapter, ProviderAdapter};
use omnis_ir::{CanonicalSnapshot, EventKind, Provider, ReplayPolicy, SessionRef};
use serde_json::json;

const SESSION_ID: &str = "77777777-7777-4777-8777-777777777777";

fn text_update(kind: &str, text: &str) -> String {
    json!({"params": {"update": {"sessionUpdate": kind, "content": {"type": "text", "text": text}}}})
        .to_string()
}

fn read_updates(updates: &str) -> anyhow::Result<CanonicalSnapshot> {
    let temporary = tempfile::tempdir()?;
    write_session(temporary.path(), updates)?;
    GrokAdapter::with_root(temporary.path())
        .read_session(&SessionRef::new(Provider::Grok, SESSION_ID))
}

fn write_session(root: &Path, updates: &str) -> anyhow::Result<()> {
    let session = root.join("workspace-hash").join(SESSION_ID);
    fs::create_dir_all(&session)?;
    fs::write(
        session.join("summary.json"),
        json!({"id": SESSION_ID, "cwd": "/workspace/demo", "num_messages": 2}).to_string(),
    )?;
    fs::write(session.join("updates.jsonl"), updates)?;
    Ok(())
}

fn kinds(snapshot: &CanonicalSnapshot) -> Vec<&EventKind> {
    snapshot.events.iter().map(|event| &event.kind).collect()
}

#[test]
fn grok_updates_over_the_strict_file_limit_still_read() {
    let padding = "x".repeat(11 * 1024 * 1024);
    let mut updates = format!(
        "{}\n",
        text_update("user_message_chunk", "synthetic question")
    );
    for _ in 0..3 {
        updates.push_str(
            &json!({"params": {"update": {"sessionUpdate": "available_commands_update", "padding": &padding}}})
                .to_string(),
        );
        updates.push('\n');
    }
    updates.push_str(&text_update("agent_message_chunk", "synthetic answer"));
    updates.push('\n');

    let snapshot = read_updates(&updates).expect("Grok updates over 32 MiB read");

    assert_eq!(
        kinds(&snapshot),
        [&EventKind::MessageUser, &EventKind::MessageAssistant]
    );
}

#[test]
fn grok_skips_oversized_updates_and_reports_omission() {
    const STREAMED_LINE_LIMIT: usize = 16 * 1024 * 1024;
    let updates = format!(
        "{}\n{}\n{}\n",
        text_update("user_message_chunk", "synthetic question"),
        "x".repeat(STREAMED_LINE_LIMIT + 1),
        text_update("agent_message_chunk", "synthetic answer")
    );

    let snapshot = read_updates(&updates).expect("oversized Grok update must not abort read");

    assert_eq!(
        kinds(&snapshot),
        [
            &EventKind::MessageUser,
            &EventKind::MessageAssistant,
            &EventKind::ProviderEvent
        ]
    );
    let notice = snapshot.events.last().expect("oversized record notice");
    assert_eq!(
        notice.source.raw_record_type.as_deref(),
        Some("omnisession.record_size_limit")
    );
    assert_eq!(notice.payload["omitted_records"], 1);
    assert!(omnis_core::import_conversation(&snapshot).truncated);
}

#[test]
fn grok_streaming_coalesces_chunks_and_flushes_at_record_boundaries() {
    let records = [
        json!({
            "timestamp": "2026-01-02T00:00:00Z",
            "params": {"update": {
                "sessionUpdate": "user_message_chunk",
                "messageId": "user-1",
                "content": {"text": "Hello "}
            }}
        }),
        json!({
            "timestamp": "2026-01-02T00:00:01Z",
            "params": {"update": {
                "sessionUpdate": "user_message_chunk",
                "messageId": "user-1",
                "content": {"text": "world"}
            }}
        }),
        json!({
            "timestamp": "2026-01-02T00:00:02Z",
            "params": {"update": {"sessionUpdate": "available_commands_update"}}
        }),
        json!({
            "timestamp": "2026-01-02T00:00:03Z",
            "params": {"update": {
                "sessionUpdate": "user_message_chunk",
                "messageId": "user-1",
                "content": {"text": "after gap"}
            }}
        }),
        json!({
            "timestamp": "2026-01-02T00:00:04Z",
            "params": {"update": {
                "sessionUpdate": "agent_message_chunk",
                "messageId": "assistant-1",
                "content": {"text": "before tool"}
            }}
        }),
        json!({
            "timestamp": "2026-01-02T00:00:05Z",
            "params": {"update": {
                "sessionUpdate": "tool_call",
                "toolName": "synthetic_tool",
                "status": "running"
            }}
        }),
        json!({
            "timestamp": "2026-01-02T00:00:06Z",
            "params": {"update": {
                "sessionUpdate": "agent_message_chunk",
                "messageId": "assistant-1",
                "content": {"text": "after tool"}
            }}
        }),
    ];
    let updates = records
        .iter()
        .map(serde_json::Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");

    let snapshot = read_updates(&updates).expect("Grok updates stream into snapshot");

    assert_eq!(
        kinds(&snapshot),
        [
            &EventKind::MessageUser,
            &EventKind::MessageUser,
            &EventKind::MessageAssistant,
            &EventKind::ToolCalled,
            &EventKind::MessageAssistant,
        ]
    );
    assert_eq!(snapshot.events[0].payload["text"], "Hello world");
    assert_eq!(
        snapshot.events[0].timestamp.unwrap().to_rfc3339(),
        "2026-01-02T00:00:00+00:00"
    );
    assert_eq!(
        snapshot.events[0].source.raw_record_type.as_deref(),
        Some("user_message_chunk")
    );
    assert_eq!(snapshot.events[1].payload["text"], "after gap");
    assert_eq!(snapshot.events[2].payload["text"], "before tool");
    assert_eq!(snapshot.events[3].payload, records[5]["params"]["update"]);
    assert_eq!(
        snapshot.events[3].replay_policy,
        ReplayPolicy::HistoricalOnly
    );
    assert_eq!(snapshot.events[4].payload["text"], "after tool");
}

#[test]
fn grok_rejects_declared_history_without_any_readable_json_records() {
    for updates in ["", "not-json\n"] {
        let error = read_updates(updates).expect_err("declared history has no JSON records");
        assert!(
            error
                .to_string()
                .contains("declares history but has no readable updates")
        );
    }
}

#[test]
fn grok_accepts_valid_records_that_do_not_emit_events() {
    let updates = json!({
        "params": {"update": {"sessionUpdate": "available_commands_update"}}
    })
    .to_string();

    let snapshot = read_updates(&updates).expect("unknown update is still readable history");

    assert!(snapshot.events.is_empty());
}
