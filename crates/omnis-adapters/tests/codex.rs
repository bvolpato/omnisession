use std::fs;

use omnis_adapters::{CodexAdapter, ProviderAdapter};
use omnis_ir::{CanonicalSnapshot, EventKind, Provider, SessionRef};
use serde_json::{Value, json};

const SESSION_ID: &str = "44444444-4444-4444-8444-444444444444";

fn read_records(records: Vec<Value>) -> anyhow::Result<CanonicalSnapshot> {
    read_records_with_preview(records, false)
}

fn read_records_with_preview(
    records: Vec<impl std::fmt::Display>,
    preview: bool,
) -> anyhow::Result<CanonicalSnapshot> {
    let temporary = tempfile::tempdir()?;
    fs::create_dir(temporary.path().join("sessions"))?;
    let source = temporary
        .path()
        .join(format!("sessions/rollout-{SESSION_ID}.jsonl"));
    let mut document = json!({
        "type": "session_meta",
        "payload": {"id": SESSION_ID, "cwd": "/workspace/original"},
    })
    .to_string();
    for record in records {
        document.push('\n');
        document.push_str(&record.to_string());
    }
    document.push('\n');
    fs::write(&source, &document)?;
    let adapter = CodexAdapter::with_root(temporary.path());
    let session = SessionRef::new(Provider::Codex, SESSION_ID);
    let snapshot = if preview {
        adapter.preview_session(&session)
    } else {
        adapter.read_session(&session)
    };
    assert_eq!(fs::read_to_string(source)?, document);
    snapshot
}

fn message(role: &str, text: &str) -> Value {
    json!({"type": "response_item", "payload": {
        "type": "message", "role": role,
        "content": [{"type": "input_text", "text": text}],
    }})
}

fn event(kind: &str, payload: Value) -> Value {
    let mut payload = payload;
    payload["type"] = kind.into();
    json!({"type": "event_msg", "payload": payload})
}

fn visible_text(snapshot: &CanonicalSnapshot) -> Vec<&str> {
    snapshot
        .events
        .iter()
        .filter_map(|event| event.payload["text"].as_str())
        .collect()
}

#[test]
fn codex_session_index_with_an_oversized_row_keeps_titles_and_notes_it() {
    let temporary = tempfile::tempdir().expect("temporary Codex home");
    fs::create_dir(temporary.path().join("sessions")).expect("sessions directory");
    fs::write(
        temporary
            .path()
            .join(format!("sessions/rollout-{SESSION_ID}.jsonl")),
        format!(
            "{}\n{}\n",
            json!({"type": "session_meta", "payload": {"id": SESSION_ID, "cwd": "/workspace/original"}}),
            message("user", "synthetic request")
        ),
    )
    .expect("synthetic rollout");
    fs::write(
        temporary.path().join("session_index.jsonl"),
        [
            json!({"id": SESSION_ID, "thread_name": "Original synthetic title", "updated_at": "2026-01-01T00:00:00Z"}),
            json!({"id": SESSION_ID, "thread_name": "x".repeat(2 * 1024 * 1024), "updated_at": "2026-01-02T00:00:00Z"}),
            json!({"id": SESSION_ID, "thread_name": "Renamed synthetic title", "updated_at": "2026-01-03T00:00:00Z"}),
        ]
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n")
            + "\n",
    )
    .expect("synthetic session index");

    let adapter = CodexAdapter::with_root(temporary.path());
    let sessions = adapter.list_sessions(None).expect("Codex listing");

    assert_eq!(sessions.len(), 1);
    assert_eq!(
        sessions[0].title.as_deref(),
        Some("Renamed synthetic title")
    );
    let notes = adapter.discovery_notes();
    assert!(
        notes
            .iter()
            .any(|note| note.contains("session_index.jsonl") && note.contains("1 oversized")),
        "{notes:?}"
    );
    let snapshot = adapter
        .read_session(&SessionRef::new(Provider::Codex, SESSION_ID))
        .expect("Codex read");
    assert_eq!(snapshot.title.as_deref(), Some("Renamed synthetic title"));
}

#[test]
fn codex_listing_skips_guardian_subagent_threads() {
    let temporary = tempfile::tempdir().expect("temporary Codex home");
    fs::create_dir(temporary.path().join("sessions")).expect("sessions directory");
    let user_session = "11111111-1111-4111-8111-111111111111";
    let guardian_session = "22222222-2222-4222-8222-222222222222";
    for (id, source) in [
        (user_session, json!("cli")),
        (guardian_session, json!({"subagent": {"other": "guardian"}})),
    ] {
        let document = [
            json!({"type": "session_meta", "payload": {"id": id, "cwd": "/workspace/original", "source": source}}),
            message("user", "synthetic request"),
        ]
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
        fs::write(
            temporary
                .path()
                .join(format!("sessions/rollout-{id}.jsonl")),
            document + "\n",
        )
        .expect("synthetic rollout");
    }

    let sessions = CodexAdapter::with_root(temporary.path())
        .list_sessions(None)
        .expect("Codex listing");

    assert_eq!(
        sessions
            .iter()
            .map(|session| session.session.id.as_str())
            .collect::<Vec<_>>(),
        [user_session]
    );
}

#[test]
fn codex_rollback_removes_mirrors_tools_and_workspace_before_followup() {
    let snapshot = read_records(vec![
        event("user_message", json!({"message": "keep this request"})),
        json!({"type": "turn_context", "payload": {"cwd": "/workspace/original"}}),
        message("user", "keep this request"),
        message("assistant", "keep this answer"),
        event("task_complete", json!({"turn_id": "kept"})),
        event("task_started", json!({"turn_id": "discarded"})),
        json!({"type": "turn_context", "payload": {"cwd": "/workspace/discarded", "model": "discarded-model"}}),
        event("user_message", json!({"message": "discard this request"})),
        message("user", "discard this request"),
        json!({"type": "response_item", "payload": {"type": "function_call", "name": "discarded-tool", "arguments": "{}"}}),
        json!({"type": "response_item", "payload": {"type": "function_call_output", "output": "discarded-result"}}),
        message("assistant", "discard this answer"),
        event("thread_rolled_back", json!({"num_turns": 1})),
        message("user", "replacement request"),
        message("assistant", "replacement answer"),
    ]).expect("rollback-aware Codex read");
    assert_eq!(
        visible_text(&snapshot),
        [
            "keep this request",
            "keep this answer",
            "replacement request",
            "replacement answer"
        ]
    );
    assert_eq!(
        snapshot.workspace.current_dir.to_str(),
        Some("/workspace/original")
    );
    assert!(
        !serde_json::to_string(&snapshot)
            .unwrap()
            .contains("discarded")
    );
}

#[test]
fn codex_rollback_counts_multipart_and_repeated_requests_once_per_message() {
    let mut multipart = message("user", "first part");
    multipart["payload"]["content"]
        .as_array_mut()
        .unwrap()
        .push(json!({"type": "input_text", "text": "second part"}));
    let snapshot = read_records(vec![
        message("user", "same request"),
        event("task_complete", json!({"turn_id": "first"})),
        event("task_started", json!({"turn_id": "second"})),
        event("user_message", json!({"message": "same request"})),
        message("user", "same request"),
        multipart,
        event(
            "user_message",
            json!({"message": "first part\nsecond part"}),
        ),
        event("thread_rolled_back", json!({"num_turns": 2})),
    ])
    .expect("rollback count");
    assert_eq!(visible_text(&snapshot), ["same request"]);
}

#[test]
fn codex_rollback_crossing_compaction_does_not_resurrect_discarded_output() {
    let snapshot = read_records(vec![
        message("user", "surviving request"),
        message("assistant", "surviving answer"),
        message("user", "discarded request"),
        json!({"type": "compacted", "payload": {"message": "discarded summary", "replacement_history": [
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "discarded compacted context"}]}
        ]}}),
        message("assistant", "discarded post-compaction answer"),
        event("thread_rolled_back", json!({"num_turns": 1})),
    ]).expect("rollback across compaction");
    assert_eq!(
        visible_text(&snapshot),
        ["surviving request", "surviving answer"]
    );
}

#[test]
fn codex_rollback_handles_zero_excess_and_consecutive_rollbacks() {
    let snapshot = read_records(vec![
        message("user", "# AGENTS.md instructions for /workspace/original\n\n<INSTRUCTIONS>Project context</INSTRUCTIONS>"),
        message("user", "first request"),
        event("thread_rolled_back", json!({"num_turns": 0})),
        message("assistant", "first answer"),
        message("user", "second request"),
        event("thread_rolled_back", json!({"num_turns": 1})),
        event("thread_rolled_back", json!({"num_turns": u64::MAX})),
        message("user", "fresh request"),
        message("assistant", "fresh answer"),
    ]).expect("saturating rollback");
    let texts = visible_text(&snapshot);
    assert_eq!(texts.len(), 3);
    assert!(texts[0].contains("Project context"));
    assert_eq!(&texts[1..], ["fresh request", "fresh answer"]);
}

#[test]
fn codex_invalid_rollback_is_reported_instead_of_exporting_discarded_history() {
    let error = read_records(vec![
        message("user", "possibly discarded request"),
        event("thread_rolled_back", json!({"num_turns": "one"})),
    ])
    .expect_err("malformed rollback must not silently preserve stale turns");
    assert!(error.to_string().contains("rollback"));
}

#[test]
fn codex_rollback_still_truncates_after_tool_event_pruning() {
    let mut records = vec![
        message("user", "surviving request"),
        message("user", "discarded request"),
    ];
    records.extend((0..1_050).map(|index| json!({"type": "response_item", "payload": {
        "type": "function_call_output", "call_id": index.to_string(), "output": "discarded output",
    }})));
    records.push(event("thread_rolled_back", json!({"num_turns": 1})));
    let snapshot = read_records(records).expect("rollback after tool pruning");
    assert_eq!(visible_text(&snapshot), ["surviving request"]);
    assert!(
        !snapshot
            .events
            .iter()
            .any(|event| event.kind == EventKind::ToolCompleted)
    );
    assert!(!omnis_core::import_conversation(&snapshot).truncated);
}

#[test]
fn codex_rollback_reports_surviving_tools_evicted_during_discarded_turn() {
    let mut records = vec![
        message("user", "surviving request"),
        json!({"type": "response_item", "payload": {
            "type": "function_call_output", "call_id": "surviving", "output": "surviving output",
        }}),
        message("user", "discarded request"),
    ];
    records.extend((0..1_050).map(|index| json!({"type": "response_item", "payload": {
        "type": "function_call_output", "call_id": index.to_string(), "output": "discarded output",
    }})));
    records.push(event("thread_rolled_back", json!({"num_turns": 1})));
    let snapshot = read_records(records).expect("rollback after tool pruning");
    assert_eq!(visible_text(&snapshot), ["surviving request"]);
    assert_eq!(
        snapshot.events.last().unwrap().payload["omitted_tool_events"],
        1
    );
    assert!(omnis_core::import_conversation(&snapshot).truncated);
}

#[test]
fn codex_preview_finds_rollback_outside_prefix_and_tail_samples() {
    let mut records = vec![message("user", "discarded request")];
    records.extend((0..1_050).map(|_| event("token_count", json!({}))));
    records.push(event("thread_rolled_back", json!({"num_turns": 1})));
    records.push(json!({"type": "response_item", "payload": {
        "type": "reasoning", "encrypted_content": "x".repeat(5 * 1024 * 1024),
    }}));
    records.push(message("user", "replacement request"));
    let encoded = records
        .into_iter()
        .map(|record| {
            record
                .to_string()
                .replace("thread_rolled_back", r"thread_rolled_\u0062ack")
        })
        .collect();
    let snapshot = read_records_with_preview(encoded, true).expect("rollback-aware preview");
    assert_eq!(visible_text(&snapshot), ["replacement request"]);
}

#[test]
fn codex_preview_reports_sampling_after_rollback_reconstruction() {
    let mut records = (0..1_100)
        .map(|index| message("user", &format!("request {index}")))
        .collect::<Vec<_>>();
    records.push(event("thread_rolled_back", json!({"num_turns": 1})));
    let snapshot = read_records_with_preview(records, true).expect("bounded active preview");
    assert_eq!(snapshot.events.len(), 1_025);
    assert_eq!(
        snapshot.events.last().unwrap().payload["omitted_events"],
        75
    );
    assert!(omnis_core::import_conversation(&snapshot).truncated);
    assert!(!visible_text(&snapshot).contains(&"request 1099"));
}

#[test]
fn codex_skips_oversized_records_and_reports_omission() {
    const STREAMED_LINE_LIMIT: usize = 16 * 1024 * 1024;
    let prefix = r#"{"type":"event_msg","payload":{"type":"item_completed","item":{"changes":""#;
    let suffix = r#""}}}"#;
    let boundary = format!(
        "{prefix}{}{suffix}",
        "x".repeat(STREAMED_LINE_LIMIT - prefix.len() - suffix.len())
    );
    let oversized = format!(
        "{}{}",
        "x".repeat(STREAMED_LINE_LIMIT + 1),
        message("user", "tail of oversized record")
    );
    let snapshot = read_records_with_preview(
        vec![
            message("user", "request before large change").to_string(),
            boundary,
            oversized,
            message("assistant", "answer after large change").to_string(),
        ],
        false,
    )
    .expect("oversized records must not abort streaming read");
    assert_eq!(
        visible_text(&snapshot),
        ["request before large change", "answer after large change"]
    );
    assert_eq!(
        snapshot.events.last().unwrap().payload["omitted_records"],
        2
    );
    assert!(omnis_core::import_conversation(&snapshot).truncated);
}

#[test]
fn codex_contextual_fragments_in_any_block_do_not_add_rollback_turns() {
    let mut context = message("user", "contextual preface");
    context["payload"]["content"].as_array_mut().unwrap().push(json!({"type": "input_text", "text": "<environment_context>project settings</environment_context>"}));
    let snapshot = read_records(vec![
        message("user", "request to remove"),
        context,
        event("thread_rolled_back", json!({"num_turns": 1})),
    ])
    .expect("context fragment classification");
    assert!(visible_text(&snapshot).is_empty());
}
