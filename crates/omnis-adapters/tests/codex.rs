use std::{fs, path::Path};

use omnis_adapters::{CodexAdapter, ProviderAdapter};
use omnis_ir::{CanonicalSnapshot, EventKind, Provider, SessionRef};
use serde_json::{Value, json};

#[path = "codex/active_order.rs"]
mod active_order;
#[path = "codex/async_questions.rs"]
mod async_questions;
#[path = "codex/display_text.rs"]
mod display_text;
#[path = "codex/early_assistants.rs"]
mod early_assistants;
#[path = "codex/hooks.rs"]
mod hooks;
#[path = "codex/inherited_prefix.rs"]
mod inherited_prefix;
#[path = "codex/media.rs"]
mod media;
#[path = "codex/message_identity.rs"]
mod message_identity;
#[path = "codex/mirror_turns.rs"]
mod mirror_turns;
#[path = "codex/paginated_answers.rs"]
mod paginated_answers;
#[path = "codex/paginated_instructions.rs"]
mod paginated_instructions;
#[path = "codex/paginated_visibility.rs"]
mod paginated_visibility;
#[path = "codex/plan_occurrences.rs"]
mod plan_occurrences;
#[path = "codex/rollout_lines.rs"]
mod rollout_lines;
#[path = "codex/steered_inputs.rs"]
mod steered_inputs;
#[path = "codex/tool_outcomes.rs"]
mod tool_outcomes;
#[path = "codex/turn_ownership.rs"]
mod turn_ownership;

const SESSION_ID: &str = "44444444-4444-4444-8444-444444444444";

fn read_records(records: Vec<Value>) -> anyhow::Result<CanonicalSnapshot> {
    read_records_with_preview(records, false)
}

fn read_records_with_preview(
    records: Vec<impl std::fmt::Display>,
    preview: bool,
) -> anyhow::Result<CanonicalSnapshot> {
    read_with_history_mode(records, preview, None)
}

fn read_with_history_mode(
    records: Vec<impl std::fmt::Display>,
    preview: bool,
    mode: Option<&str>,
) -> anyhow::Result<CanonicalSnapshot> {
    let temporary = tempfile::tempdir()?;
    fs::create_dir(temporary.path().join("sessions"))?;
    let source = temporary
        .path()
        .join(format!("sessions/rollout-{SESSION_ID}.jsonl"));
    let mut header = json!({
        "type": "session_meta",
        "payload": {"id": SESSION_ID, "cwd": "/workspace/original", "git":{"branch":"original"}},
    });
    if let Some(mode) = mode {
        header["payload"]["history_mode"] = mode.into();
    }
    let paginated = mode == Some("paginated");
    let timestamp = "2026-01-05T09:00:00.000Z";
    if paginated {
        header["timestamp"] = timestamp.into();
        header["ordinal"] = 0.into();
    }
    let mut document = header.to_string();
    for (index, record) in records.into_iter().enumerate() {
        document.push('\n');
        let line = record.to_string();
        if paginated
            && let Ok(Value::Object(mut row)) = serde_json::from_str(&line)
            && !row.contains_key("timestamp")
            && !row.contains_key("ordinal")
        {
            row.insert("timestamp".into(), timestamp.into());
            row.insert("ordinal".into(), (index + 1).into());
            document.push_str(&Value::Object(row).to_string());
        } else {
            document.push_str(&line);
        }
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

fn published_text(snapshot: &omnis_ir::CanonicalSnapshot) -> Vec<&str> {
    snapshot
        .events
        .iter()
        .filter(|e| e.source.raw_record_type.as_deref() == Some("event_msg.item_completed"))
        .filter_map(|e| e.payload["text"].as_str())
        .collect()
}

fn visible_text(snapshot: &CanonicalSnapshot) -> Vec<&str> {
    snapshot
        .events
        .iter()
        .filter_map(|event| event.payload["text"].as_str())
        .collect()
}

fn completed(item: &Value) -> Value {
    event(
        "item_completed",
        json!({"thread_id":SESSION_ID, "turn_id":"turn-1", "item":item}),
    )
}

#[test]
fn migrated_codex_messages_survive_full_reads_and_previews() {
    let request = completed(&json!({"type":"UserMessage", "id":"item-1", "content":[
        {"type":"text","text":"Inspect "}, {"type":"text","text":"the cache."}
    ]}));
    let answer = completed(&json!({"type":"AgentMessage", "id":"item-2", "content":[
        {"type":"Text","text":"The entry "}, {"type":"Text","text":"expired."}
    ]}));
    for preview in [false, true] {
        let snapshot = read_records_with_preview(
            vec![
                request.clone(),
                completed(&json!({"type":"Reasoning","id":"private","raw_content":["private reasoning"]})),
                completed(&json!({"type":"HookPrompt","id":"hook","fragments":[{"text":"private hook context"}]})),
                answer.clone(),
                message("assistant", "The entry expired."),
                completed(&json!({"type":"AgentMessage","id":"item-3","content":[{"type":"Text","text":"The entry expired."}]})),
                completed(&json!({"type":"UserMessage","id":"item-4","content":[{"type":"local_image","path":"private-image.png"}]})),
            ],
            preview,
        ).unwrap();
        assert_eq!(
            visible_text(&snapshot),
            [
                "Inspect the cache.",
                "The entry expired.",
                "The entry expired.",
                "[1 image omitted]"
            ]
        );
        let encoded = serde_json::to_string(&snapshot).unwrap();
        assert!(!encoded.contains("private"));
    }
}

#[test]
fn rollback_counts_a_paginated_multipart_request_and_its_mirror_once() {
    let mut model_request = message("user", "first");
    model_request["payload"]["content"]
        .as_array_mut()
        .unwrap()
        .push(json!({"type":"input_text","text":" second"}));
    let display_request = completed(&json!({"type":"UserMessage","id":"request","content":[
        {"type":"text","text":"first"},{"type":"text","text":" second"}
    ]}));
    for (first, second) in [
        (&model_request, &display_request),
        (&display_request, &model_request),
    ] {
        for compacted in [false, true] {
            let mut records = vec![message("user", "keep this"), first.clone(), second.clone()];
            if compacted {
                records.push(json!({"type":"compacted","payload":{"message":"",
                    "replacement_history":[model_request["payload"]]}}));
            }
            records.push(event("thread_rolled_back", json!({"num_turns":1})));
            assert_eq!(visible_text(&read_records(records).unwrap()), ["keep this"]);
        }
    }
}

#[test]
fn paginated_model_records_can_follow_all_display_items_in_the_turn() {
    let display_user = completed(
        &json!({"type":"UserMessage","id":"request","content":[{"type":"text","text":"repeat"}]}),
    );
    let display_reply = completed(
        &json!({"type":"AgentMessage","id":"reply","content":[{"type":"Text","text":"repeat"}]}),
    );
    let records = vec![
        event("task_started", json!({"turn_id":"turn-1"})),
        display_user,
        display_reply,
        message("user", "repeat"),
        message("assistant", "repeat"),
        event("task_complete", json!({"turn_id":"turn-1"})),
    ];
    for preview in [false, true] {
        let snapshot = read_records_with_preview(records.clone(), preview).unwrap();
        let roles: Vec<_> = snapshot.events.iter().map(|e| &e.kind).collect();
        assert_eq!(
            roles,
            [&EventKind::MessageUser, &EventKind::MessageAssistant]
        );
        let mut rolled_back = records.clone();
        rolled_back.push(event("thread_rolled_back", json!({"num_turns":1})));
        assert!(
            read_records_with_preview(rolled_back, preview)
                .unwrap()
                .events
                .is_empty()
        );
    }
}

#[test]
fn paginated_model_messages_can_precede_their_synthesized_turns() {
    let mut rows = vec![
        message("user", "first question"),
        message("assistant", "first answer"),
        message("user", "second question"),
        message("assistant", "second answer"),
    ];
    for (turn, question, answer) in [
        ("rollout-5", "first question", "first answer"),
        ("rollout-7", "second question", "second answer"),
    ] {
        rows.push(event("task_started", json!({"turn_id":turn})));
        for (item_type, tag, id, text) in [
            ("UserMessage", "text", "question", question),
            ("AgentMessage", "Text", "answer", answer),
        ] {
            rows.push(event(
                "item_completed",
                json!({"thread_id":SESSION_ID,"turn_id":turn,"item":{
                    "type":item_type,"id":id,"content":[{"type":tag,"text":text}]
                }}),
            ));
        }
        rows.push(event("task_complete", json!({"turn_id":turn})));
    }
    for preview in [false, true] {
        let mut history = rows.clone();
        for wanted in [
            vec![
                "first question",
                "first answer",
                "second question",
                "second answer",
            ],
            vec!["first question", "first answer"],
            vec![],
        ] {
            assert_eq!(
                visible_text(&read_records_with_preview(history.clone(), preview).unwrap()),
                wanted
            );
            history.push(event("thread_rolled_back", json!({"num_turns":1})));
        }
    }
}

#[test]
fn paginated_command_and_model_output_preserve_one_result_in_either_order() {
    let call = json!({"type":"response_item","payload":{"type":"function_call","call_id":"cmd","name":"exec","arguments":"{}"}});
    let display = completed(
        &json!({"type":"CommandExecution","id":"cmd","command":["example"],"exit_code":1,"aggregated_output":"display text","status":"failed"}),
    );
    let result = json!({"type":"response_item","payload":{"type":"function_call_output","call_id":"cmd","output":"model-visible result"}});
    for (first, second) in [(&display, &result), (&result, &display)] {
        let snapshot = read_records(vec![call.clone(), first.clone(), second.clone()]).unwrap();
        assert_eq!(snapshot.events.len(), 2);
        assert_eq!(snapshot.events[0].kind, EventKind::ToolCalled);
        let outcome = &snapshot.events[1];
        assert_eq!(outcome.kind, EventKind::ToolFailed);
        assert_eq!(outcome.payload["output"], "model-visible result");
        assert_eq!(
            outcome.replay_policy,
            omnis_ir::ReplayPolicy::HistoricalOnly
        );
    }
}

#[test]
fn native_startup_and_source_directory_respect_settings_ownership() {
    use std::io::Write as _;

    let id = "019ab000-0000-7000-8000-000000000020";
    let home = tempfile::tempdir().unwrap();
    let sessions = home.path().join("sessions");
    fs::create_dir(&sessions).unwrap();
    let mut file = fs::File::create(sessions.join(format!("rollout-{id}.jsonl"))).unwrap();
    let adapter = CodexAdapter::with_root(home.path());
    let session = SessionRef::new(Provider::Codex, id);
    let check = |effective: Option<&str>, startup: Option<&str>| {
        let recorded = adapter.native_workspace(&session).unwrap().unwrap();
        assert_eq!(
            recorded.startup_directory.as_deref(),
            startup.map(Path::new)
        );
        assert_eq!(recorded.directory.as_deref(), effective.map(Path::new));
        for snapshot in [
            adapter.read_session(&session),
            adapter.preview_session(&session),
        ] {
            assert_eq!(
                snapshot.unwrap().workspace.current_dir,
                Path::new(effective.unwrap_or_default())
            );
        }
    };
    let creation = home.path().join("creation");
    let saved = home.path().join("saved");
    let a = creation.to_str().unwrap();
    let b = saved.to_str().unwrap();
    let turn = json!({"type":"turn_context","payload":{"cwd":a}});
    writeln!(
        file,
        "{}",
        json!({"type":"session_meta","payload":{"id":id,"cwd":a}})
    )
    .unwrap();
    check(Some(a), Some(a));
    writeln!(
        file,
        "{}",
        settings(Some("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"), b)
    )
    .unwrap();
    writeln!(file, "{}", settings(None, b)).unwrap();
    check(Some(a), None);
    writeln!(
        file,
        "{}",
        event(
            "thread_settings_applied",
            json!({"thread_id":id,"thread_settings":{"cwd":b}})
        )
    )
    .unwrap();
    let mut malformed = settings(Some(id), b);
    malformed["payload"]["thread_settings"]["approval_policy"] = json!("unknown-policy");
    writeln!(file, "{malformed}").unwrap();
    check(None, None);
    let mut managed = settings(Some(id), b);
    managed["payload"]["thread_settings"]["permission_profile"] = json!({
        "type":"managed","file_system":{"type":"restricted","entries":[
            {"path":{"type":"path","path":b},"access":"write"}]},"network":"restricted"
    });
    let mut malformed = managed.clone();
    malformed["payload"]["thread_settings"]["runtime_workspace_roots"] = json!([{"path":b}]);
    writeln!(file, "{malformed}").unwrap();
    check(None, None);
    writeln!(file, "{managed}").unwrap();
    writeln!(file, "{turn}").unwrap();
    check(Some(b), None);

    writeln!(
        file,
        "{}",
        message("assistant", &"x".repeat(5 * 1024 * 1024))
    )
    .unwrap();
    writeln!(file, "{turn}").unwrap();
    check(Some(b), None);
    writeln!(file, "{}", settings(Some(id), a)).unwrap();
    check(Some(a), None);
    writeln!(file, " \t").unwrap();
    check(Some(a), None);
    writeln!(file, r#"{{"type":"#).unwrap();
    check(None, None);
    let equivalent_id = id.replace('-', "").to_ascii_uppercase();
    writeln!(file, "{}", settings(Some(&equivalent_id), b)).unwrap();
    check(Some(b), None);
}

#[test]
fn marked_workspace_settings_follow_the_cold_reader() {
    let home = tempfile::tempdir().unwrap();
    let sessions = home.path().join("sessions");
    fs::create_dir(&sessions).unwrap();
    let file = sessions.join(format!("rollout-{SESSION_ID}.jsonl"));
    let adapter = CodexAdapter::with_root(home.path());
    let session = SessionRef::new(Provider::Codex, SESSION_ID);
    let creation_path = home.path().join("creation");
    let saved_path = home.path().join("saved");
    let creation = creation_path.to_str().unwrap();
    let moved = saved_path.to_str().unwrap();
    let header = json!({"type":"session_meta","timestamp":"2026-01-01T00:00:00Z","ordinal":0,
        "payload":{"id":SESSION_ID,"cwd":creation,"history_mode":"paginated"}});
    let mut original = settings(Some(SESSION_ID), creation);
    original["timestamp"] = header["timestamp"].clone();
    original["ordinal"] = 7.into();
    for (timestamp, ordinal, newline, expected) in [
        (json!(null), json!(8), "\n", creation),
        (json!("opaque"), json!("8"), "\n", creation),
        (json!("opaque"), json!(2), "\n", moved),
        (header["timestamp"].clone(), json!(8), "", moved),
    ] {
        let mut next = settings(Some(SESSION_ID), moved);
        next["timestamp"] = timestamp;
        next["ordinal"] = ordinal;
        fs::write(&file, format!("{header}\n{original}\n{next}{newline}")).unwrap();
        assert_eq!(
            adapter
                .native_workspace(&session)
                .unwrap()
                .unwrap()
                .directory
                .as_deref(),
            Some(Path::new(expected))
        );
        for snapshot in [
            adapter.read_session(&session),
            adapter.preview_session(&session),
        ] {
            assert_eq!(snapshot.unwrap().workspace.current_dir, Path::new(expected));
        }
    }
}

fn settings(owner: Option<&str>, cwd: &str) -> Value {
    event(
        "thread_settings_applied",
        json!({"thread_id":owner,"thread_settings":{
            "cwd":cwd,"model":"fixture","model_provider_id":"synthetic","approval_policy":"never",
            "approvals_reviewer":"user","permission_profile":{"type":"disabled"},
            "collaboration_mode":{"mode":"default","settings":{"model":"fixture"}}
        }}),
    )
}

#[test]
fn damaged_workspace_records_clear_routes_until_later_evidence() {
    use std::io::Write as _;

    let home = tempfile::tempdir().unwrap();
    let sessions = home.path().join("sessions");
    fs::create_dir(&sessions).unwrap();
    let mut file = fs::File::create(sessions.join(format!("rollout-{SESSION_ID}.jsonl"))).unwrap();
    let adapter = CodexAdapter::with_root(home.path());
    let session = SessionRef::new(Provider::Codex, SESSION_ID);
    let check = |directory: Option<&str>| {
        assert_eq!(
            adapter
                .native_workspace(&session)
                .unwrap()
                .unwrap()
                .directory
                .as_deref(),
            directory.map(Path::new)
        );
        let full = adapter.read_session(&session).unwrap();
        let preview = adapter.preview_session(&session).unwrap();
        for snapshot in [&full, &preview] {
            assert_eq!(
                snapshot.workspace.current_dir,
                Path::new(directory.unwrap_or_default())
            );
            assert_eq!(visible_text(snapshot), ["surviving answer"]);
        }
        assert_eq!(
            full.events
                .iter()
                .any(|event| event.payload["workspace_unknown"] == true),
            directory.is_none()
        );
    };
    writeln!(
        file,
        "{}",
        json!({"type":"session_meta","payload":{"id":SESSION_ID,"cwd":"/workspace/initial"}})
    )
    .unwrap();
    writeln!(file, "{}", message("assistant", "surviving answer")).unwrap();
    writeln!(
        file,
        r#"{{"type":"event_msg","payload":{{"type":"agent_message","message":"turn_context"#
    )
    .unwrap();
    check(Some("/workspace/initial"));
    writeln!(file, r#"{{"type":"turn_context","payload":"#).unwrap();
    check(None);
    writeln!(
        file,
        "{}",
        json!({"type":"turn_context","payload":{"cwd":"/workspace/later"}})
    )
    .unwrap();
    check(Some("/workspace/later"));
    writeln!(
        file,
        r#"{{"type":"event_msg","payload":{{"type":"thread_settings_applied","thread_settings":"#
    )
    .unwrap();
    writeln!(
        file,
        "{}",
        json!({"type":"turn_context","payload":{"cwd":"/workspace/unverified"}})
    )
    .unwrap();
    check(None);
}

#[test]
fn legacy_rollback_only_restores_directory_evidence_after_a_gap() {
    let turn = |id: &str, cwd: &str| {
        [
            event("task_started", json!({"turn_id": id})),
            json!({"type":"turn_context","payload":{"turn_id":id,"cwd":cwd}}),
            message("user", id),
            event("task_complete", json!({"turn_id":id})),
        ]
        .map(|r| r.to_string())
    };
    let rollback = |count| event("thread_rolled_back", json!({"num_turns":count})).to_string();
    let mut records = Vec::from(turn("old", "/workspace/original"));
    records.push(r#"{"type":"turn_context","payload":"#.to_owned());
    records.extend(turn("first", "/workspace/survivor"));
    records.extend(turn("second", "/workspace/withdrawn"));
    records.push(rollback(1));
    for preview in [false, true] {
        assert_eq!(
            read_records_with_preview(records.clone(), preview)
                .unwrap()
                .workspace
                .current_dir,
            Path::new("/workspace/survivor")
        );
    }
    records.extend([
        rollback(2),
        event("task_started", json!({"turn_id":"fresh"})).to_string(),
        message("user", "request without new directory").to_string(),
        event("task_complete", json!({"turn_id":"fresh"})).to_string(),
        rollback(1),
    ]);
    for preview in [false, true] {
        assert!(
            read_records_with_preview(records.clone(), preview)
                .unwrap()
                .workspace
                .current_dir
                .as_os_str()
                .is_empty()
        );
    }
}

#[test]
fn changed_turn_directory_does_not_inherit_creation_git() {
    let padding = json!({"type":"event_msg","payload":{"note":"x".repeat(2_048)}}).to_string();
    let context = r#"{"type":"turn_\u0063ontext","payload":{"cwd":"/workspace/elsewhere"}}"#;
    let records: Vec<_> = std::iter::repeat_n(padding.as_str(), 1_025)
        .chain([context])
        .chain(std::iter::repeat_n(padding.as_str(), 1_025))
        .collect();
    for preview in [false, true] {
        let snapshot = read_records_with_preview(records.clone(), preview).unwrap();
        assert_eq!(snapshot.workspace.root, Path::new("/workspace/elsewhere"));
        assert_eq!(snapshot.workspace.git, omnis_ir::GitState::default());
    }
}

#[test]
fn unreadable_workspace_changes_require_a_later_complete_turn_directory() {
    use std::io::Write as _;

    let home = tempfile::tempdir().unwrap();
    let sessions = home.path().join("sessions");
    fs::create_dir(&sessions).unwrap();
    let mut file = fs::File::create(sessions.join(format!("rollout-{SESSION_ID}.jsonl"))).unwrap();
    writeln!(
        file,
        "{}",
        json!({"type":"session_meta","payload":{"id":SESSION_ID,"cwd":"/workspace/initial"}})
    )
    .unwrap();
    writeln!(file, "{}", json!({"type":"turn_context","payload":{"cwd":"/workspace/unreadable","padding":"x".repeat(16*1024*1024)}})).unwrap();
    writeln!(file, "{}", message("assistant", "A recent reply")).unwrap();

    let adapter = CodexAdapter::with_root(home.path());
    let session = SessionRef::new(Provider::Codex, SESSION_ID);
    let preview = adapter.preview_session(&session).unwrap();
    assert_eq!(visible_text(&preview), ["A recent reply"]);
    assert!(preview.workspace.root.as_os_str().is_empty());
    assert!(
        adapter
            .read_session(&session)
            .unwrap()
            .workspace
            .root
            .as_os_str()
            .is_empty()
    );
    assert!(
        adapter
            .native_workspace(&session)
            .unwrap()
            .unwrap()
            .directory
            .is_none()
    );

    writeln!(
        file,
        "{}",
        json!({"type":"turn_context","payload":{"cwd":"/workspace/recovered"}})
    )
    .unwrap();
    writeln!(
        file,
        "{}",
        event("thread_rolled_back", json!({"num_turns": 0}))
    )
    .unwrap();
    let directory = Path::new("/workspace/recovered");
    assert_eq!(
        adapter
            .native_workspace(&session)
            .unwrap()
            .unwrap()
            .directory
            .as_deref(),
        Some(directory)
    );
    assert_eq!(
        adapter.preview_session(&session).unwrap().workspace.root,
        directory
    );
    writeln!(
        file,
        "{}",
        event("thread_rolled_back", json!({"num_turns": "unknown"}))
    )
    .unwrap();
    assert!(
        adapter
            .native_workspace(&session)
            .unwrap()
            .unwrap()
            .directory
            .is_none()
    );
}

#[test]
fn external_import_marker_keeps_the_authored_reply() {
    let marker = omnis_core::CODEX_IMPORT_MARKER;
    let quoted = format!(" {marker} ");
    let snapshot = read_records(vec![
        message("user", "Repeat the import marker"),
        event("agent_message", json!({"message": marker})),
        message("assistant", marker),
        event("agent_message", json!({"message": marker})),
        event("agent_message", json!({"message": quoted})),
        message("assistant", &quoted),
    ])
    .unwrap();

    let conversation = omnis_core::import_conversation(&snapshot);
    assert_eq!(conversation.messages.len(), 3);
    assert_eq!(
        conversation.messages[1].role,
        omnis_core::HandoffRole::Assistant
    );
    assert_eq!(conversation.messages[1].text, marker);
    assert_eq!(conversation.messages[2].text, quoted);
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
fn missing_codex_home_does_not_claim_complete_discovery() {
    let temporary = tempfile::tempdir().expect("temporary parent directory");
    let missing_home = temporary.path().join("missing-codex-home");
    let adapter = CodexAdapter::with_root(missing_home);

    assert!(
        adapter
            .list_sessions(None)
            .expect("list missing home")
            .is_empty()
    );
    assert!(!adapter.discovery_is_complete());

    let empty_home = temporary.path().join("empty-codex-home");
    fs::create_dir(&empty_home).expect("empty Codex home");
    let adapter = CodexAdapter::with_root(empty_home);
    assert!(
        adapter
            .list_sessions(None)
            .expect("list empty home")
            .is_empty()
    );
    assert!(adapter.discovery_is_complete());
}

#[test]
fn fork_candidates_cover_rollouts_filed_under_a_local_date() {
    const SOURCE: &str = "11111111-1111-4111-8111-111111111111";
    const CHILD: &str = "22222222-2222-4222-8222-222222222222";
    // West of UTC, a fork at this instant is filed under the previous day. East, under the next.
    for folder in ["2026/10/31", "2026/11/01", "2026/11/02"] {
        let temporary = tempfile::tempdir().expect("temporary Codex home");
        let day = temporary.path().join("sessions").join(folder);
        fs::create_dir_all(&day).expect("session day");
        fs::write(
            day.join(format!("rollout-{CHILD}.jsonl")),
            json!({
                "timestamp": "2026-11-01T02:24:47Z",
                "type": "session_meta",
                "payload": {"id": CHILD, "cwd": "/workspace/original", "source": "cli", "forked_from_id": SOURCE}
            })
            .to_string(),
        )
        .expect("synthetic rollout");

        let candidates = CodexAdapter::with_root(temporary.path())
            .fork_candidates_created_since(
                &SessionRef::new(Provider::Codex, SOURCE),
                Path::new("/workspace/original"),
                "2026-11-01T02:24:00Z".parse().expect("start time"),
            )
            .expect("fork candidates");

        assert_eq!(
            candidates,
            [SessionRef::new(Provider::Codex, CHILD)],
            "{folder}"
        );
    }
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
fn codex_replacement_history_keeps_the_summary_once_in_context() {
    let mut summary = message("user", "Documented decision")["payload"].clone();
    summary["internal_chat_message_metadata_passthrough"] =
        json!({"content_item_kinds": ["compaction.summary"]});
    let records = vec![
        message("user", "Original request"),
        message("assistant", "Detail for the recovery file"),
        json!({"type":"compacted","payload":{
            "message": "Documented decision",
            "replacement_history": [
                message("developer", "Hidden developer setup")["payload"],
                message("user", "Retained request")["payload"],
                summary,
                message("assistant", "Retained answer")["payload"],
            ],
        }}),
        message("user", "New request"),
    ];
    for preview in [false, true] {
        let snapshot = read_records_with_preview(records.clone(), preview).unwrap();
        let conversation = omnis_core::import_conversation(&snapshot);
        assert!(!conversation.truncated);
        let messages = &conversation.messages;
        let retained = messages
            .iter()
            .position(|m| m.text == "Retained request")
            .unwrap();
        assert_eq!(
            messages[retained + 1].role,
            omnis_core::HandoffRole::Assistant
        );
        assert!(messages[retained + 1].text.contains("Documented decision"));
        assert_eq!(messages[retained + 2].text, "Retained answer");
        assert_eq!(messages[retained + 3].text, "New request");
        assert_eq!(
            messages
                .iter()
                .filter(|m| m.text.contains("Documented decision"))
                .count(),
            1
        );
        assert!(
            !messages
                .iter()
                .any(|m| m.text.contains("Detail for the recovery file"))
        );
        assert!(omnis_core::needs_recovery_handoff(&snapshot, false));
        assert!(
            !serde_json::to_string(&snapshot)
                .unwrap()
                .contains("Hidden developer setup")
        );
    }
}

#[test]
fn codex_opaque_compaction_reports_missing_context() {
    let snapshot = read_records(vec![
        message("user", "Old request"),
        json!({"type":"compacted","payload":{
            "message": "Documentary note", "replacement_history": [
                {"type":"compaction","encrypted_content":"unreadable-ciphertext"},
                message("user", "Retained request")["payload"],
            ],
        }}),
    ])
    .unwrap();
    let conversation = omnis_core::import_conversation(&snapshot);
    assert!(conversation.truncated);
    assert!(
        !conversation
            .messages
            .iter()
            .any(|message| message.text.contains("Documentary note"))
    );
    assert_eq!(
        conversation.messages.last().unwrap().text,
        "Retained request"
    );
    assert!(
        !conversation
            .messages
            .iter()
            .any(|m| m.text == "Old request")
    );
    assert!(
        !serde_json::to_string(&snapshot)
            .unwrap()
            .contains("unreadable-ciphertext")
    );
}

#[test]
fn explicit_replacements_exclude_documentary_summaries_from_the_active_handoff() {
    for (items, expected) in [
        (vec![], vec!["Fresh request"]),
        (
            vec![message("user", "Retained request")["payload"].clone()],
            vec!["Retained request", "Fresh request"],
        ),
    ] {
        let snapshot=read_records(vec![
            json!({"type":"compacted","payload":{"message":"Documentary note","replacement_history":items}}),
            message("user","Fresh request"),
        ]).unwrap();
        let conversation = omnis_core::import_conversation(&snapshot);
        let trajectory = omnis_core::import_trajectory(&snapshot);
        assert!(!conversation.truncated && !trajectory.truncated);
        assert_eq!(
            conversation
                .messages
                .iter()
                .map(|m| m.text.as_str())
                .collect::<Vec<_>>(),
            expected
        );
        assert_eq!(
            trajectory
                .items
                .iter()
                .map(|i| i.text.as_str())
                .collect::<Vec<_>>(),
            expected
        );
        assert!(!omnis_core::render_semantic_handoff(&snapshot).contains("Documentary note"));
        let report = omnis_core::build_native_materialization_report_for_snapshot(
            &snapshot,
            Provider::Claude,
            true,
            false,
            0,
            0,
        );
        assert_eq!(
            report.entries[0].status,
            omnis_ir::FidelityStatus::Preserved
        );
        assert!(omnis_core::needs_recovery_handoff(&snapshot, false));
        let mut recovery = Vec::new();
        omnis_core::write_recovery_handoff(&snapshot, &mut recovery).unwrap();
        assert!(
            String::from_utf8(recovery)
                .unwrap()
                .contains("Documentary note")
        );
        assert!(
            omnis_core::trajectory_search_document(&snapshot)
                .text
                .contains("Documentary note")
        );
    }
}

#[test]
fn codex_checkpoint_only_rollback_preserves_completed_turns() {
    let snapshot = read_records(vec![
        json!({"type":"compacted","payload":{"message":"Previous context","replacement_history":[
            message("user", "Retained request")["payload"],
            message("assistant", "Retained reply")["payload"],
            message("user", "Discarded request")["payload"],
            message("assistant", "Discarded reply")["payload"],
        ]}}),
        event("thread_rolled_back", json!({"num_turns": 1})),
        message("user", "Replacement request"),
    ])
    .unwrap();
    assert_eq!(
        visible_text(&snapshot),
        ["Retained request", "Retained reply", "Replacement request"]
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
fn codex_image_only_turns_keep_one_placeholder() {
    const IMAGE: &str = "data:image/png;base64,iVBORw0KGgo=";
    let user_content = |content: Value| json!({"type": "response_item", "payload": {"type": "message", "role": "user", "content": content}});
    let snapshot = read_records(vec![
        user_content(json!([{"type": "input_image", "image_url": IMAGE}])),
        event("user_message", json!({"message": "", "images": [IMAGE]})),
        message("assistant", "pasted image seen"),
        // Local images arrive wrapped in Codex's `<image name=...>` tag texts.
        user_content(json!([
            {"type": "input_text", "text": "<image name=[Image #1] path=\"/tmp/screen.png\">"},
            {"type": "input_image", "image_url": IMAGE},
            {"type": "input_text", "text": "</image>"}
        ])),
        event(
            "user_message",
            json!({"message": "", "local_images": ["/tmp/screen.png"]}),
        ),
        message("assistant", "local image seen"),
        // A tag typed without an image is the user's own text.
        message("user", "</image>"),
    ])
    .expect("image-only turns");
    assert_eq!(
        visible_text(&snapshot),
        [
            "[1 image omitted]",
            "pasted image seen",
            "[1 image omitted]",
            "local image seen",
            "</image>"
        ]
    );
    assert!(
        !serde_json::to_string(&snapshot)
            .expect("serialize Codex snapshot")
            .contains("iVBORw0KGgo")
    );
}

#[test]
fn codex_reads_rollouts_above_the_record_budget_keeping_the_newest_events() {
    let mut records = Vec::with_capacity(100_002);
    for index in 0..50_001 {
        records.push(message("user", &format!("request {index}")).to_string());
        records.push(message("assistant", &format!("answer {index}")).to_string());
    }
    let snapshot = read_records_with_preview(records, false).expect("large rollout");
    let text = visible_text(&snapshot);
    assert_eq!(text.len(), 100_000);
    assert_eq!(text.first(), Some(&"request 1"));
    assert_eq!(text.last(), Some(&"answer 50000"));
    assert_eq!(snapshot.events.last().unwrap().payload["omitted_events"], 2);
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

#[test]
fn partial_checkpoint_rollback_removes_only_discarded_requests() {
    let checkpoint = json!({"type":"compacted","payload":{"message":"","replacement_history":[
        message("user", "Kept request")["payload"],
        message("assistant", "Kept reply")["payload"],
        message("user", "Discarded request")["payload"],
        message("assistant", "Discarded reply")["payload"],
    ]}});
    for original in [
        vec![event("task_started", json!({"turn_id": "pending"}))],
        vec![json!({"type":"turn_context","payload":{"cwd":"/workspace/pending"}})],
        vec![message("user", "Kept request")],
        vec![message("user", "Discarded request")],
    ] {
        let mut records = original;
        records.extend([
            checkpoint.clone(),
            event("thread_rolled_back", json!({"num_turns":1})),
        ]);
        let snapshot = read_records(records.clone()).unwrap();
        let active = omnis_core::import_conversation(&snapshot);
        assert!(active.messages.iter().any(|m| m.text == "Kept request"));
        assert!(active.messages.iter().any(|m| m.text == "Kept reply"));
        assert!(
            !serde_json::to_string(&snapshot)
                .unwrap()
                .contains("Discarded")
        );
        records.extend([
            event("thread_rolled_back", json!({"num_turns":1})),
            message("user", "Fresh request"),
        ]);
        assert_eq!(
            visible_text(&read_records(records).unwrap()),
            ["Fresh request"]
        );
    }
}

#[test]
fn legacy_compaction_keeps_real_requests_and_plain_prior_summaries_in_native_order() {
    let snapshot = read_records(vec![
        message("user", "Initial request"),
        message("assistant", "Original reply"),
        message(
            "user",
            "<environment_context>Local setup</environment_context>",
        ),
        json!({"type":"compacted","payload":{"message":"First summary"}}),
        message("user", "Follow-up request"),
        event("user_message", json!({"message":"Follow-up request"})),
        json!({"type":"compacted","payload":{"message":"Latest summary"}}),
        message("assistant", "Final reply"),
    ])
    .unwrap();
    let active = omnis_core::import_conversation(&snapshot);
    let trajectory = omnis_core::import_trajectory(&snapshot);
    for texts in [
        active
            .messages
            .iter()
            .map(|m| m.text.as_str())
            .collect::<Vec<_>>(),
        trajectory.items.iter().map(|i| i.text.as_str()).collect(),
    ] {
        let visible: Vec<_> = texts
            .into_iter()
            .filter(|t| !t.contains("Codex replaced"))
            .map(|t| t.lines().last().unwrap())
            .collect();
        assert_eq!(
            visible,
            [
                "Initial request",
                "First summary",
                "Follow-up request",
                "Latest summary",
                "Final reply"
            ]
        );
    }
    assert!(!active.truncated);
    assert!(!trajectory.truncated);
}

#[test]
fn repeated_legacy_compactions_apply_the_native_user_budget_only_to_the_active_window() {
    let long = format!("start{}end", "界".repeat(30_000));
    let checkpoint = json!({"type":"compacted","payload":{"message":"Current summary"}});
    let mut records = vec![
        message("user", "Older request"),
        message("user", &long),
        message("user", "Last request"),
    ];
    records.extend(std::iter::repeat_n(checkpoint, 8));
    let snapshot = read_records(records).unwrap();
    let replayed = snapshot
        .events
        .iter()
        .filter(|e| e.source.raw_record_type.as_deref() == Some("compacted.user"))
        .map(|e| e.payload["text"].as_str().unwrap())
        .collect::<Vec<_>>();
    let [trimmed, latest] = replayed.as_slice() else {
        panic!("expected only the active projected requests")
    };
    assert!(trimmed.starts_with("start") && trimmed.ends_with("end"));
    assert!(trimmed.contains("tokens truncated") && trimmed.len() <= 80_032);
    assert_eq!(*latest, "Last request");
    assert!(
        snapshot
            .events
            .windows(2)
            .all(|pair| pair[0].sequence < pair[1].sequence)
    );
}

#[test]
fn a_request_with_the_summary_text_keeps_its_role() {
    let ordinary = message("user", "Identical text")["payload"].clone();
    let mut tagged = ordinary.clone();
    tagged["internal_chat_message_metadata_passthrough"] =
        json!({"content_item_kinds":["compaction.summary"]});
    for (replacement, roles) in [
        (
            vec![tagged, ordinary.clone()],
            [EventKind::MessageAssistant, EventKind::MessageUser],
        ),
        (
            vec![ordinary.clone(), ordinary],
            [EventKind::MessageUser, EventKind::MessageUser],
        ),
    ] {
        let snapshot = read_records(vec![json!({"type":"compacted","payload":{"message":"Identical text", "replacement_history":replacement}})]).unwrap();
        let observed: Vec<_> = snapshot
            .events
            .iter()
            .filter(|e| e.payload["text"] == "Identical text")
            .map(|e| e.kind.clone())
            .collect();
        assert_eq!(observed, roles);
    }
}

#[test]
fn rollback_of_a_natively_shortened_replacement_removes_its_original_turn() {
    let quoted_notice = "Earlier output said …2 tokens truncated…";
    let prefix = format!(
        "{quoted_notice}{}",
        "a".repeat(40_000 - quoted_notice.len())
    );
    let suffix = "z".repeat(40_000);
    let original = format!("{prefix}lost{suffix}");
    let checkpoint = json!({"type":"compacted","payload":{"message":"A summary","replacement_history":[{
        "type":"message","role":"user","content":[
            {"type":"input_text","text":prefix},
            {"type":"input_text","text":"…1 tokens truncated…"},
            {"type":"input_text","text":suffix}
        ]
    }]}});
    let snapshot = read_records(vec![
        message("user", &original),
        checkpoint,
        event("thread_rolled_back", json!({"num_turns":1})),
    ])
    .unwrap();
    assert!(visible_text(&snapshot).is_empty());
}

fn turn_item(turn: &str, item: &Value) -> Value {
    event(
        "item_completed",
        json!({"thread_id": SESSION_ID, "turn_id": turn, "item": item}),
    )
}

fn page_user(turn: &str, id: &str, text: &str) -> Value {
    turn_item(
        turn,
        &json!({"type":"UserMessage","id":id,"content":[{"type":"text","text":text}]}),
    )
}

#[test]
fn paginated_implicit_turns_match_model_requests_across_synthetic_boundaries() {
    let records = vec![
        message("user", "repeat"),
        event("task_started", json!({"turn_id":"rollout-2"})),
        page_user("rollout-2", "item-1", "repeat"),
        message("user", "repeat"),
        event("task_complete", json!({"turn_id":"rollout-2"})),
        event("task_started", json!({"turn_id":"rollout-4"})),
        page_user("rollout-4", "item-2", "repeat"),
        event("task_complete", json!({"turn_id":"rollout-4"})),
    ];
    for preview in [false, true] {
        let mut rows = records.clone();
        for expected in [vec!["repeat", "repeat"], vec!["repeat"], vec![]] {
            let snapshot = read_records_with_preview(rows.clone(), preview).unwrap();
            assert_eq!(visible_text(&snapshot), expected);
            rows.push(event("thread_rolled_back", json!({"num_turns":1})));
        }
    }
}

#[test]
fn paginated_rollback_preserves_late_command_results_from_surviving_turns() {
    let command = |turn, id, output| {
        turn_item(
            turn,
            &json!({
                "type":"CommandExecution","id":id,"command":["example"],"cwd":"file:///workspace",
                "parsed_cmd":[],"source":"agent","aggregated_output":output,"status":"completed","exit_code":0
            }),
        )
    };
    let records = vec![
        event("task_started", json!({"turn_id":"first"})),
        page_user("first", "item-1", "keep"),
        event("task_started", json!({"turn_id":"second"})),
        page_user("second", "item-2", "discard"),
        event("task_complete", json!({"turn_id":"first"})),
        command("first", "command-1", "late result"),
        command("second", "command-2", "discarded result"),
        event("task_complete", json!({"turn_id":"second"})),
        event("thread_rolled_back", json!({"num_turns":1})),
    ];
    for preview in [false, true] {
        let snapshot = read_records_with_preview(records.clone(), preview).unwrap();
        assert_eq!(visible_text(&snapshot), ["keep"]);
        let results: Vec<_> = snapshot
            .events
            .iter()
            .filter(|event| event.kind == EventKind::CommandExecuted)
            .map(|event| event.payload["output"].as_str().unwrap())
            .collect();
        assert_eq!(results, ["late result"]);
        let mut rows = records.clone();
        rows.push(event("thread_rolled_back", json!({"num_turns":1})));
        let empty = read_records_with_preview(rows, preview).unwrap();
        assert!(
            empty
                .events
                .iter()
                .all(|e| e.source.raw_record_type.as_deref()
                    == Some("omnisession.codex_legacy_rollback"))
        );
        assert!(
            empty
                .events
                .iter()
                .any(|e| e.payload["native_history_unverified"] == true)
        );
    }
}

#[test]
fn paginated_item_updates_keep_the_original_position_and_turn() {
    let assistant = |turn, id, text| {
        turn_item(
            turn,
            &json!({
                "type":"AgentMessage","id":id,"content":[{"type":"Text","text":text}]
            }),
        )
    };
    let records = vec![
        event("task_started", json!({"turn_id":"first"})),
        page_user("first", "request", "first request"),
        assistant("first", "answer", "superseded"),
        assistant("first", "other", "separate answer"),
        assistant("first", "answer", "updated answer"),
        event("task_complete", json!({"turn_id":"first"})),
        event("task_started", json!({"turn_id":"second"})),
        page_user("second", "request", "second request"),
        assistant("second", "answer", "second answer"),
    ];
    for preview in [false, true] {
        let mut rows = records.clone();
        let snapshot = read_records_with_preview(rows.clone(), preview).unwrap();
        assert_eq!(
            visible_text(&snapshot),
            [
                "first request",
                "updated answer",
                "separate answer",
                "second request",
                "second answer"
            ]
        );
        rows.push(event("thread_rolled_back", json!({"num_turns":1})));
        let snapshot = read_records_with_preview(rows, preview).unwrap();
        assert_eq!(
            visible_text(&snapshot),
            ["first request", "updated answer", "separate answer"]
        );
    }
}

#[test]
fn partial_compaction_keeps_a_late_completion_until_its_named_turn_is_rolled_back() {
    let command = |turn, output| {
        turn_item(
            turn,
            &json!({
                "type":"CommandExecution","id":"command","command":["example"],"cwd":"file:///workspace",
                "parsed_cmd":[],"source":"agent","aggregated_output":output,"status":"completed","exit_code":0,
            }),
        )
    };
    let mut records = vec![
        event("task_started", json!({"turn_id":"first"})),
        page_user("first", "item-1", "Keep first"),
        message("user", "Keep first"),
        event("task_complete", json!({"turn_id":"first"})),
        json!({"type":"compacted","payload":{"message":"","replacement_history":[
            message("user","Keep first")["payload"],
            message("user","Discard second")["payload"],
        ]}}),
        event("task_started", json!({"turn_id":"third"})),
        page_user("third", "item-3", "Discard third"),
        command("first", "Late result for first"),
        command("third", "Result for third"),
        event("thread_rolled_back", json!({"num_turns":2})),
    ];
    for preview in [false, true] {
        let snapshot = read_records_with_preview(records.clone(), preview).unwrap();
        let active = omnis_core::import_conversation(&snapshot);
        assert!(active.messages.iter().any(|m| m.text == "Keep first"));
        assert!(
            !visible_text(&snapshot)
                .iter()
                .any(|m| m.starts_with("Discard"))
        );
        let results: Vec<_> = snapshot
            .events
            .iter()
            .filter_map(|e| e.payload["output"].as_str())
            .collect();
        assert_eq!(results, ["Late result for first"]);
    }
    records.push(event("thread_rolled_back", json!({"num_turns":1})));
    let snapshot = read_records(records).unwrap();
    assert!(visible_text(&snapshot).is_empty());
    assert!(
        !snapshot
            .events
            .iter()
            .any(|e| e.kind == EventKind::CommandExecuted)
    );
}
