use std::{fs, io::Write, path::Path};

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
        "payload": {"id": SESSION_ID, "cwd": "/workspace/original"},
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
        let snapshot = read_records(vec![
            message("user", "keep this"),
            first.clone(),
            second.clone(),
            event("thread_rolled_back", json!({"num_turns":1})),
        ])
        .unwrap();
        assert_eq!(visible_text(&snapshot), ["keep this"]);
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
    let call = json!({"type":"response_item","payload":{"type":"function_call","call_id":"cmd","name":"exec_command","arguments":json!({"cmd":"example"}).to_string()}});
    let display = completed(
        &json!({"type":"CommandExecution","id":"cmd","command":["example"],"cwd":"file:///workspace","parsed_cmd":[],"source":"agent","exit_code":1,"aggregated_output":"display text","status":"failed"}),
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
fn gaps_in_the_name_index_warn_without_hiding_rollouts_or_later_names() {
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
        format!(
            "{}\n{}\n \t\r\n{{incomplete private fragment\n{}\n",
            json!({"id": SESSION_ID, "thread_name": "Original synthetic title"}),
            json!({"id": SESSION_ID, "thread_name": "x".repeat(2 * 1024 * 1024)}),
            json!({"id": SESSION_ID, "thread_name": "Renamed synthetic title"}),
        ),
    )
    .expect("synthetic session index");

    let adapter = CodexAdapter::with_root(temporary.path());
    let sessions = adapter.list_sessions(None).expect("Codex listing");

    assert_eq!(sessions.len(), 1);
    assert_eq!(
        sessions[0].title.as_deref(),
        Some("Renamed synthetic title")
    );
    assert!(adapter.discovery_is_complete());
    assert!(!adapter.session_titles_are_complete());
    let warnings = adapter.discovery_notes().join("\n");
    for kind in ["1 malformed", "1 oversized"] {
        assert!(
            warnings
                .lines()
                .any(|line| line.contains("session_index.jsonl") && line.contains(kind)),
            "{warnings}"
        );
    }
    assert!(!warnings.contains("private fragment"));
    assert!(!warnings.contains("synthetic title"));
    let snapshot = adapter
        .read_session(&SessionRef::new(Provider::Codex, SESSION_ID))
        .expect("Codex read");
    assert_eq!(snapshot.title.as_deref(), Some("Renamed synthetic title"));
}

#[test]
fn failed_sqlite_discovery_preserves_independent_name_observations() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let sessions = home.path().join("sessions");
    fs::create_dir(&sessions)?;
    let header = json!({"type":"session_meta","payload":{"id":SESSION_ID,"cwd":"/workspace"}});
    fs::write(
        sessions.join(format!("rollout-{SESSION_ID}.jsonl")),
        header.to_string(),
    )?;
    fs::write(
        home.path().join("state_5.sqlite"),
        b"unreadable sqlite contents",
    )?;
    let adapter = CodexAdapter::with_root(home.path());
    let sidecar = home.path().join("session_index.jsonl");
    let renamed = json!({"id":SESSION_ID,"thread_name":"Latest name"});
    for (contents, complete, name) in [
        (String::new(), true, None),
        (format!("{{\n{renamed}\n"), false, Some("Latest name")),
    ] {
        fs::write(&sidecar, contents)?;
        assert!(adapter.list_sessions(None)?.is_empty());
        assert!(!adapter.discovery_is_complete());
        let observed = adapter
            .observed_session_names()
            .expect("sidecar read before SQLite");
        assert_eq!(observed.complete, complete);
        assert_eq!(observed.names.get(SESSION_ID).map(String::as_str), name);
    }
    Ok(())
}

#[test]
#[cfg(any(unix, windows))]
fn earlier_names_corroborate_only_rollouts_with_one_possible_home() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let flat = root.path().join("flat");
    let outer = root.path().join("outer");
    let inner = outer.join("sessions/team");
    let header = json!({"type":"session_meta", "payload":{"id":SESSION_ID}});
    for home in [&flat, &inner] {
        let sessions = home.join("sessions");
        fs::create_dir_all(&sessions)?;
        fs::write(
            sessions.join(format!("rollout-{SESSION_ID}.jsonl")),
            format!("{header}\n"),
        )?;
    }
    let assignment = |name: &str| json!({"id":SESSION_ID, "thread_name":name});
    let old = assignment("Old name");
    let recent = assignment("  Last recorded name  ");
    let damaged = format!("{old}\n{{torn\n{recent}\n{{torn again\n");
    for home in [&flat, &outer, &inner] {
        fs::write(home.join("session_index.jsonl"), &damaged)?;
    }
    for home in [&flat, &outer, &inner] {
        let adapter = CodexAdapter::with_root(home);
        let session = adapter
            .find_native_session(SESSION_ID)?
            .expect("selected rollout");
        assert!(session.title.is_none());
        assert_eq!(
            adapter.native_session_name(SESSION_ID),
            omnis_adapters::NativeSessionName::Unknown
        );
        let prior = adapter.prior_session_names(&[&session]);
        assert_eq!(
            prior.get(SESSION_ID).map(String::as_str),
            (home == &flat).then_some("  Last recorded name  ")
        );
        let current = assignment("Confirmed name");
        fs::write(
            home.join("session_index.jsonl"),
            format!("{damaged}{current}\n"),
        )?;
        assert!(
            adapter
                .native_session_name(SESSION_ID)
                .confirms(Some("Confirmed name"))
        );
        assert!(adapter.prior_session_names(&[&session]).is_empty());
    }
    Ok(())
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
fn only_a_rollback_of_the_matching_local_opener_withdraws_a_saved_title() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let path = home.path().join(format!("rollout-{SESSION_ID}.jsonl"));
    let source = SessionRef::new(Provider::Codex, SESSION_ID);
    let rolled_back = |turns: u64| event("thread_rolled_back", json!({"num_turns":turns}));
    let saved = || message("user", "Saved   title");
    let header = json!({"type":"session_meta","payload":{"id":SESSION_ID, "history_base":{"session_id":"11111111-1111-4111-8111-111111111111"}}});
    let cases = [
        (
            vec![saved(), message("user", "later"), rolled_back(1)],
            false,
        ),
        (vec![saved(), rolled_back(0)], false),
        (
            vec![saved(), rolled_back(1), message("user", "Saved title")],
            false,
        ),
        (
            vec![
                saved(),
                message("user", "later"),
                rolled_back(1),
                rolled_back(1),
            ],
            true,
        ),
        (
            vec![
                saved(),
                rolled_back(u64::from(u32::MAX)),
                message("user", "replacement"),
            ],
            true,
        ),
        (
            vec![
                saved(),
                rolled_back(u64::MAX),
                message("user", "replacement"),
            ],
            false,
        ),
        (
            vec![message("user", "earlier"), saved(), rolled_back(1)],
            false,
        ),
        (
            vec![message("assistant", "preface"), saved(), rolled_back(1)],
            false,
        ),
        (
            vec![
                message("user", "first"),
                rolled_back(1),
                saved(),
                rolled_back(1),
            ],
            true,
        ),
        (
            vec![
                saved(),
                json!({"type":"compacted","payload":{}}),
                rolled_back(1),
            ],
            false,
        ),
    ];
    for (records, expected) in cases {
        let mut rows = vec![header.to_string()];
        rows.extend(records.iter().map(ToString::to_string));
        fs::write(&path, rows.join("\n"))?;
        let revision = CodexAdapter::index_file_revision(&path).unwrap();
        assert_eq!(
            CodexAdapter::rollout_withdrew_search_title(&path, &source, revision, "Saved title")?,
            expected,
            "{records:?}"
        );
    }
    Ok(())
}

fn rollback_title_reads(
    header: &Value,
    rows: &[Value],
    trailing: &str,
) -> anyhow::Result<(omnis_adapters::IndexRead, omnis_adapters::IndexRead)> {
    let home = tempfile::tempdir()?;
    let folder = home.path().join("sessions");
    fs::create_dir(&folder)?;
    let session = SessionRef::new(Provider::Codex, SESSION_ID);
    let path = folder.join(format!("rollout-{SESSION_ID}.jsonl"));
    let records = std::iter::once(header)
        .chain(rows)
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(&path, format!("{records}\n{trailing}"))?;
    let adapter = CodexAdapter::with_root(home.path());
    let direct = adapter.index_session(&session, Some(&path), u64::MAX)?;
    let sampled = adapter.index_session(&session, Some(&path), 1)?;
    Ok((direct, sampled))
}

fn rollback_title_read(
    header: &Value,
    rows: &[Value],
    trailing: &str,
) -> anyhow::Result<omnis_adapters::IndexRead> {
    let (direct, sampled) = rollback_title_reads(header, rows, trailing)?;
    assert_eq!(direct.rollback_title, sampled.rollback_title);
    Ok(sampled)
}

#[test]
fn bounded_reconstruction_certifies_only_the_surviving_local_search_title() -> anyhow::Result<()> {
    use omnis_adapters::RollbackTitle::{Known, Unknown, UnknownLocal};

    let header = json!({"type":"session_meta","payload":{"id":SESSION_ID,"cwd":"/workspace"}});
    let rollback = |n: u64| event("thread_rolled_back", json!({"num_turns":n}));
    let old = || message("user", "old opener");
    for rows in [vec![old()], vec![old(), rollback(0)]] {
        let (full, sampled) = rollback_title_reads(&header, &rows, "")?;
        assert_eq!(full.rollback_title, Known(Some("old opener".into())));
        assert_eq!(sampled.rollback_title, UnknownLocal);
    }
    for (rows, expected) in [
        (vec![old(), rollback(1)], Known(None)),
        (
            vec![old(), rollback(1), message("assistant", "preface"), old()],
            Known(None),
        ),
        (
            vec![old(), rollback(1), old()],
            Known(Some("old opener".into())),
        ),
        (
            vec![old(), rollback(1), json!({"type":"unrecognized"})],
            Unknown,
        ),
        (
            vec![
                old(),
                rollback(1),
                json!({"type":"compacted","payload":{"message":"checkpoint"}}),
            ],
            Unknown,
        ),
    ] {
        let result = rollback_title_read(&header, &rows, "")?;
        assert!(!result.source_complete);
        assert_eq!(result.rollback_title, expected, "{rows:?}");
    }
    let rows = [old(), rollback(1), message("user", "replacement")];
    let mut legacy = header.clone();
    legacy["payload"]["history_mode"] = json!("legacy");
    assert_eq!(
        rollback_title_read(&legacy, &rows, "")?.rollback_title,
        Known(Some("replacement".to_owned()))
    );
    for (field, value) in [
        ("history_mode", json!("paginated")),
        ("history_mode", json!("default")),
        ("history_mode", Value::Null),
        ("history_base", json!({"thread_id":"unavailable"})),
        (
            "forked_from_id",
            json!("11111111-1111-4111-8111-111111111111"),
        ),
    ] {
        let mut ambiguous = legacy.clone();
        ambiguous["payload"][field] = value;
        assert_eq!(
            rollback_title_read(&ambiguous, &rows, "")?.rollback_title,
            Unknown
        );
    }

    let replacement = format!(
        "Investigate   sk-proj-{} {}",
        "a".repeat(36),
        "details ".repeat(620_000)
    );
    let result = rollback_title_read(
        &header,
        &[
            old(),
            rollback(1),
            message("user", &replacement),
            message("user", "later follow-up"),
        ],
        "",
    )?;
    assert_eq!(visible_text(&result.snapshot), ["later follow-up"]);
    let Known(Some(title)) = result.rollback_title else {
        panic!("missing rolled-back title proof")
    };
    assert_eq!(title.chars().count(), 240);
    assert!(title.starts_with("Investigate [REDACTED: API_KEY] details"));
    Ok(())
}

#[test]
fn dynamic_tool_catalog_preserves_an_ordinary_rollback_title() -> anyhow::Result<()> {
    use omnis_adapters::RollbackTitle::{Known, Unknown};

    let mut header =
        json!({"type":"session_meta","payload":{"id":SESSION_ID,"history_mode":"legacy"}});
    let function = json!({"type":"function","name":"weather","description":"Catalog documentation only","inputSchema":{"type":"object"}});
    let catalog = |tools| json!({"type":"dynamic_tools_updated","payload":{"thread_id":SESSION_ID,"tools":tools}});
    let rows = |update| {
        [
            message("user", "retired opener"),
            update,
            event("thread_rolled_back", json!({"num_turns":1})),
            message("user", "replacement"),
        ]
    };
    let namespace =
        json!({"type":"namespace","name":"local","description":"Local tools","tools":[function]});
    for tools in [json!([]), json!([function, namespace])] {
        let read = rollback_title_read(&header, &rows(catalog(tools)), "")?;
        assert_eq!(read.rollback_title, Known(Some("replacement".into())));
        assert_eq!(visible_text(&read.snapshot), ["replacement"]);
    }
    for unrecognized in [
        json!({"type":"dynamic_tool_call","payload":{"thread_id":SESSION_ID,"tools":[]}}),
        catalog(json!([{"type":"function_call","arguments":"{}"}])),
        json!({"type":"dynamic_tools_updated","payload":{"tools":[]}}),
    ] {
        assert_eq!(
            rollback_title_read(&header, &rows(unrecognized), "")?.rollback_title,
            Unknown
        );
    }
    header["payload"]["history_mode"] = json!("paginated");
    assert_eq!(
        rollback_title_read(&header, &rows(catalog(json!([]))), "")?.rollback_title,
        Unknown
    );
    Ok(())
}

#[test]
fn bounded_reconstruction_distinguishes_gaps_before_and_after_withdrawal() -> anyhow::Result<()> {
    use omnis_adapters::RollbackTitle::UnknownLocal;

    let header = json!({"type":"session_meta","payload":{"id":SESSION_ID,"cwd":"/workspace"}});
    let rollback = |n: u64| event("thread_rolled_back", json!({"num_turns":n}));
    let old = || message("user", "old opener");
    let rows = [old(), rollback(1), message("user", "replacement")];
    let after = rollback_title_read(&header, &rows, "{malformed\n")?.rollback_title;
    assert!(after.withdraws("old opener"));
    assert!(!after.withdraws("other opener"));
    let oversized_successor = format!(
        "{}\n{}\n",
        message("user", &"x".repeat(17 * 1024 * 1024)),
        message("assistant", "retained reply"),
    );
    let after = rollback_title_read(&header, &[old(), rollback(1)], &oversized_successor)?;
    assert!(after.rollback_title.withdraws("old opener"));
    assert_eq!(visible_text(&after.snapshot), ["retained reply"]);
    let early_gap = format!("{{malformed\n{}\n", rollback(1));
    assert_eq!(
        rollback_title_read(&header, &[old()], &early_gap)?.rollback_title,
        UnknownLocal
    );
    assert_eq!(
        rollback_title_read(&header, &[old(), rollback(1), old()], "{malformed\n")?.rollback_title,
        UnknownLocal
    );
    assert!(
        rollback_title_read(
            &header,
            &[old(), rollback(1), old(), rollback(1)],
            "{malformed\n"
        )?
        .rollback_title
        .withdraws("old opener")
    );
    Ok(())
}

#[test]
fn bounded_reconstruction_checks_distinct_withdrawn_openers_after_overflow() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let folder = home.path().join("sessions");
    fs::create_dir(&folder)?;
    let path = folder.join(format!("rollout-{SESSION_ID}.jsonl"));
    let session = SessionRef::new(Provider::Codex, SESSION_ID);
    let header = json!({"type":"session_meta","payload":{"id":SESSION_ID,"cwd":"/workspace"}});
    let mut rows = vec![header.to_string()];
    for id in 0..12 {
        rows.push(message("user", &format!("opening {id}")).to_string());
        rows.push(event("thread_rolled_back", json!({"num_turns":1})).to_string());
    }
    for reissued in [None, Some("opening 0"), Some("opening 10")] {
        let mut records = rows.clone();
        if let Some(title) = reissued {
            records.push(message("user", title).to_string());
        }
        records.push("{unread".into());
        fs::write(&path, records.join("\n"))?;
        let item = CodexAdapter::with_root(home.path()).index_session(&session, Some(&path), 1)?;
        let receipt = item.rollback_title;
        assert!(receipt.withdraws("opening 1"));
        assert!(!receipt.needs_withdrawal_check("opening 1"));
        assert_eq!(
            receipt.withdraws("opening 0"),
            reissued != Some("opening 0")
        );
        for candidate in ["opening 0", "opening 10", "never present"] {
            if receipt.withdraws(candidate) {
                continue;
            }
            assert!(receipt.needs_withdrawal_check(candidate));
            assert_eq!(
                CodexAdapter::verify_earlier_opener_withdrawal(
                    &path,
                    &session,
                    item.source_revision,
                    candidate,
                )?,
                candidate != "never present" && reissued != Some(candidate)
            );
        }
    }
    Ok(())
}

#[test]
fn compressed_rollback_title_evidence_requires_a_verified_complete_stream() -> anyhow::Result<()> {
    use std::io::Write as _;
    let home = tempfile::tempdir()?;
    let path = home.path().join(format!("rollout-{SESSION_ID}.jsonl.gz"));
    let source = SessionRef::new(Provider::Codex, SESSION_ID);
    let write = |extra: &str| -> anyhow::Result<()> {
        let mut file =
            flate2::write::GzEncoder::new(fs::File::create(&path)?, flate2::Compression::fast());
        writeln!(
            file,
            "{}",
            json!({"type":"session_meta","payload":{"id":SESSION_ID}})
        )?;
        writeln!(file, "{}", message("user", "saved"))?;
        writeln!(
            file,
            "{}",
            event("thread_rolled_back", json!({"num_turns":1}))
        )?;
        file.write_all(extra.as_bytes())?;
        file.finish()?;
        Ok(())
    };
    write("")?;
    let first = CodexAdapter::index_file_revision(&path).unwrap();
    assert!(CodexAdapter::rollout_withdrew_search_title(
        &path, &source, first, "saved"
    )?);
    let unverified = omnis_ir::SourceRevision {
        contents: None,
        ..first
    };
    assert!(!CodexAdapter::rollout_withdrew_search_title(
        &path, &source, unverified, "saved"
    )?);
    let other = SessionRef::new(Provider::Codex, "11111111-1111-4111-8111-111111111111");
    assert!(CodexAdapter::rollout_withdrew_search_title(&path, &other, first, "saved").is_err());
    write("{broken record\n")?;
    let current = CodexAdapter::index_file_revision(&path).unwrap();
    assert!(CodexAdapter::rollout_withdrew_search_title(&path, &source, first, "saved").is_err());
    assert!(!CodexAdapter::rollout_withdrew_search_title(
        &path, &source, current, "saved"
    )?);
    Ok(())
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
        event("thread_rolled_back", json!({"num_turns": u32::MAX})),
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
    let middle = (0..1_050).map(|_| event("token_count", json!({})).to_string());
    let prefix: Vec<_> = std::iter::once(message("user", "discarded request").to_string())
        .chain(middle)
        .collect();
    let tail = json!({"type": "response_item", "payload": {
        "type": "reasoning", "encrypted_content": "x".repeat(5 * 1024 * 1024),
    }})
    .to_string();
    for rollback in [
        r#"{"type":"event_msg","payload":{"type":"thread_rolled_\u0062ack","num_turns":1}}"#,
        r#"{"type":"event_msg","payload":[ 14 ,1]}"#,
        r#"{"type":"event_msg","payload":{"num_turns":1,"type":14}}"#,
    ] {
        let mut records = prefix.clone();
        records.extend([
            rollback.to_owned(),
            tail.clone(),
            message("user", "replacement request").to_string(),
        ]);
        let snapshot = read_records_with_preview(records, true).expect("rollback-aware preview");
        assert_eq!(visible_text(&snapshot), ["replacement request"]);
    }
}

#[test]
fn legacy_preview_applies_a_display_revision_between_sampled_edges() -> anyhow::Result<()> {
    let assistant = |text| {
        turn_item(
            "turn",
            &json!({
                "type":"AgentMessage","id":"answer","content":[{"type":"Text","text":text}]
            }),
        )
    };
    let header = json!({"type":"session_meta","payload":{"id":SESSION_ID,"cwd":"/synthetic"}});
    let padding = format!(
        "{}\n",
        json!({"type":"progress","padding":"p".repeat(8 * 1024)})
    )
    .repeat(1050);
    let document = format!(
        "{header}\n{}\n{}\n{padding}{}\n{padding}",
        event("task_started", json!({"turn_id":"turn"})),
        assistant("earlier draft"),
        assistant("corrected answer")
    );
    for suffix in ["jsonl", "gz", "zst"] {
        let home = tempfile::tempdir()?;
        let folder = home.path().join("sessions");
        fs::create_dir(&folder)?;
        let bytes = if suffix == "jsonl" {
            document.as_bytes().to_vec()
        } else {
            encode_rollout(document.as_bytes(), suffix)
        };
        let extension = if suffix == "jsonl" {
            suffix.to_owned()
        } else {
            format!("jsonl.{suffix}")
        };
        fs::write(
            folder.join(format!("rollout-{SESSION_ID}.{extension}")),
            bytes,
        )?;
        let preview = CodexAdapter::with_root(home.path())
            .preview_session(&SessionRef::new(Provider::Codex, SESSION_ID))?;
        assert_eq!(visible_text(&preview), ["corrected answer"], "{suffix}");
    }
    Ok(())
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
    let prefix_end = snapshot.events.last().unwrap().payload["prefix_end_sequence"]
        .as_u64()
        .expect("position of the first skipped request");
    let in_prefix = snapshot
        .events
        .iter()
        .take_while(|event| event.sequence < prefix_end)
        .count();
    assert_eq!(in_prefix, 512);
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
fn codex_reports_malformed_records_without_losing_verified_openers() {
    for opener_before_gap in [true, false] {
        let mut records = Vec::new();
        if opener_before_gap {
            records.push(message("user", "opening request").to_string());
        }
        records.extend([
            "\t ".to_owned(),
            r#"{"type":"response_item""#.to_owned(),
            message("user", "later request").to_string(),
        ]);
        let snapshot = read_records_with_preview(records, false).unwrap();
        let notice = snapshot.events.last().unwrap();
        assert_eq!(
            notice.source.raw_record_type.as_deref(),
            Some("omnisession.codex_malformed_records")
        );
        assert_eq!(notice.payload["malformed_records"], 1);
        assert!(omnis_core::import_conversation(&snapshot).truncated);
        assert!(!omnis_core::trajectory_search_document(&snapshot).source_complete);
        assert_eq!(
            omnis_core::session_message_prefix_is_known(&snapshot, true),
            opener_before_gap
        );
    }
}

#[test]
fn codex_prefix_ends_at_the_first_skipped_source_record() {
    let oversized = message("user", &"x".repeat(16 * 1024 * 1024)).to_string();
    for preview in [true, false] {
        for original_precedes_gap in [true, false] {
            let mut records = Vec::new();
            if original_precedes_gap {
                records.push(message("user", "original request").to_string());
            }
            records.extend([
                oversized.clone(),
                message("user", "later request").to_string(),
            ]);
            let snapshot = read_records_with_preview(records, preview).expect("bounded rollout");
            let raw_type = if preview {
                "omnisession.codex_preview_prefix"
            } else {
                "omnisession.record_size_limit"
            };
            let bound = snapshot
                .events
                .iter()
                .find(|event| event.source.raw_record_type.as_deref() == Some(raw_type))
                .and_then(|event| event.payload["prefix_end_sequence"].as_u64())
                .expect("known contiguous source prefix");
            let proven = snapshot
                .events
                .iter()
                .filter(|event| event.kind == EventKind::MessageUser && event.sequence < bound)
                .map(|event| event.payload["text"].as_str().unwrap())
                .collect::<Vec<_>>();
            assert_eq!(
                proven,
                original_precedes_gap
                    .then_some("original request")
                    .as_slice()
            );
        }
    }
}

#[test]
fn plain_edge_sample_preserves_the_verified_opening_and_discloses_the_unread_middle() {
    let padding = event("token_count", json!({"padding":"x".repeat(64 * 1024)})).to_string();
    for known_opener in [true, false] {
        let mut records = Vec::new();
        if known_opener {
            records.push(message("user", "recorded opening").to_string());
        }
        records.extend(std::iter::repeat_n("{}".to_owned(), 1_024));
        records.push(message("user", "request in unread middle").to_string());
        records.extend(std::iter::repeat_n(padding.clone(), 80));
        records.push(message("user", "later request").to_string());
        let preview = read_records_with_preview(records, true).unwrap();
        assert!(!visible_text(&preview).contains(&"request in unread middle"));
        assert_eq!(
            omnis_core::session_message_prefix_is_known(&preview, false),
            known_opener
        );
        assert!(!omnis_core::trajectory_search_document(&preview).source_complete);
        assert!(omnis_core::import_conversation(&preview).truncated);
    }
}

#[test]
fn codex_preview_certifies_absence_only_after_reading_through_eof() {
    const MIB: usize = 1024 * 1024;
    let metadata = event("token_count", json!({"padding": "x".repeat(MIB)})).to_string();
    let hidden_user = message("user", &"h".repeat(MIB)).to_string();
    for (middle, tail_bytes, visible_bytes, known) in [
        (Some(&metadata), 0, None, true),
        (None, 2 * MIB, Some(2 * MIB), true),
        (Some(&metadata), 2 * MIB, Some(2 * MIB), false),
        (Some(&hidden_user), 2 * MIB, Some(2 * MIB), false),
        (Some(&metadata), 5 * MIB, None, false),
    ] {
        let mut records = vec![metadata.clone(); 3];
        records.extend(middle.cloned());
        records.push(metadata.clone());
        if tail_bytes > 0 {
            records.push(message("user", &"x".repeat(tail_bytes)).to_string());
        }
        let snapshot = read_records_with_preview(records, true).expect("bounded preview");
        let lengths: Vec<_> = visible_text(&snapshot).into_iter().map(str::len).collect();
        assert_eq!(lengths, visible_bytes.as_slice());
        assert_eq!(
            omnis_core::session_message_prefix_is_known(&snapshot, false),
            known
        );
    }
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
        let snapshot = read_records_with_preview(rows, preview).unwrap();
        let [notice] = snapshot.events.as_slice() else {
            panic!("expected only the notice for the unconfirmed legacy input");
        };
        assert!(omnis_core::reports_omitted_events(notice));
        assert_eq!(
            notice.source.raw_record_type.as_deref(),
            Some("omnisession.codex_legacy_rollback")
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

fn encode_rollout(contents: &[u8], extension: &str) -> Vec<u8> {
    use std::io::Write;
    if extension == "gz" {
        let mut writer = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        writer.write_all(contents).unwrap();
        writer.finish().unwrap()
    } else {
        zstd::stream::encode_all(contents, 1).unwrap()
    }
}

#[test]
fn restored_native_resume_uses_the_selected_header_history_mode() -> anyhow::Result<()> {
    use omnis_adapters::NativeResumeSupport::{Direct, ImportOnly, Since};

    let home = tempfile::tempdir()?;
    let folder = home.path().join("sessions/2026/05/15");
    fs::create_dir_all(&folder)?;
    let path = folder.join(format!("rollout-2026-05-15T01-02-03-{SESSION_ID}.jsonl"));
    let base = json!({"thread_id":"55555555-5555-4555-8555-555555555555", "end_ordinal_exclusive":1, "end_byte_offset":100});
    let tuple = json!(["55555555-5555-4555-8555-555555555555", 1, 100]);
    let malformed_base =
        json!({"thread_id":SESSION_ID, "end_ordinal_exclusive":0, "end_byte_offset":100});
    let source = SessionRef::new(Provider::Codex, SESSION_ID);
    let adapter = CodexAdapter::with_root(home.path());
    for (mode, prefix, expected) in [
        (None, None, Direct),
        (Some(json!("legacy")), None, Direct),
        (Some(json!("paginated")), Some(&base), Since("0.167.0")),
        (
            Some(json!({"paginated":null})),
            Some(&tuple),
            Since("0.167.0"),
        ),
        (Some(json!("paginated")), None, Since("0.167.0")),
        (Some(json!(null)), None, ImportOnly),
        (Some(json!("future")), None, ImportOnly),
        (Some(json!(42)), None, ImportOnly),
        (None, Some(&base), ImportOnly),
        (Some(json!("legacy")), Some(&base), ImportOnly),
        (Some(json!({"legacy":null})), Some(&tuple), ImportOnly),
        (Some(json!("paginated")), Some(&malformed_base), ImportOnly),
    ] {
        let mut header = json!({"type":"session_meta","payload":{"id":SESSION_ID}});
        if let Some(mode) = mode {
            header["payload"]["history_mode"] = mode;
        }
        if let Some(prefix) = prefix {
            header["payload"]["history_base"] = prefix.clone();
        }
        let paginated = header["payload"]["history_mode"] == "paginated";
        let offset = prefix
            .and_then(|base| base["end_ordinal_exclusive"].as_u64())
            .unwrap_or(0);
        let mut records = vec![header.clone(), message("user", "retained request")];
        if paginated {
            records.push(page_user("turn-one", "request", "retained request"));
            for (index, record) in records.iter_mut().enumerate() {
                record["ordinal"] = (offset + index as u64).into();
                record["timestamp"] = "2026-01-01T00:00:00.000Z".into();
            }
        }
        let rows = records
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        fs::write(&path, &rows)?;
        assert_eq!(
            adapter.native_resume_support(&source)?,
            expected,
            "{header}"
        );
        assert_eq!(
            visible_text(&adapter.read_session(&source)?),
            ["retained request"]
        );
        assert_eq!(fs::read_to_string(&path)?, rows);
    }
    let header =
        json!({"type":"session_meta","payload":{"id":SESSION_ID,"history_mode":"paginated"}});
    fs::write(&path, header.to_string())?;
    let restored_name = folder.join(format!("rollout-{SESSION_ID}.jsonl"));
    fs::rename(&path, &restored_name)?;
    assert_eq!(adapter.native_resume_support(&source)?, ImportOnly);
    fs::rename(restored_name, &path)?;
    let db = rusqlite::Connection::open(home.path().join("state_5.sqlite"))?;
    db.execute_batch(
        "CREATE TABLE threads(id TEXT PRIMARY KEY, rollout_path TEXT, history_mode TEXT)",
    )?;
    db.execute(
        "INSERT INTO threads VALUES (?1, ?2, 'paginated')",
        [SESSION_ID, path.to_str().unwrap()],
    )?;
    let mut damaged = header;
    damaged["payload"]["history_base"] = malformed_base;
    damaged["timestamp"] = "2026-01-01T00:00:00.000Z".into();
    damaged["ordinal"] = 0.into();
    let mut retained = page_user("turn-one", "request", "readable leaf");
    retained["timestamp"] = damaged["timestamp"].clone();
    retained["ordinal"] = 1.into();
    fs::write(&path, format!("{damaged}\n{retained}\n"))?;
    assert_eq!(adapter.native_resume_support(&source)?, ImportOnly);
    let snapshot = adapter.read_session(&source)?;
    assert_eq!(visible_text(&snapshot), ["readable leaf"]);
    assert!(omnis_core::import_conversation(&snapshot).truncated);
    Ok(())
}

#[test]
fn codex_marks_the_workspace_unknown_until_a_later_turn_establishes_it() {
    use std::fmt::Write as _;

    for codec in ["jsonl", "jsonl.gz", "jsonl.zst"] {
        let home = tempfile::tempdir().unwrap();
        let folder = home.path().join("archived_sessions");
        fs::create_dir(&folder).unwrap();
        let path = folder.join(format!("rollout-{SESSION_ID}.{codec}"));
        let session = SessionRef::new(Provider::Codex, SESSION_ID);
        let header = json!({"type":"session_meta","payload":{"id":SESSION_ID,"cwd":"/creation","git":{"branch":"creation"}}});
        let turn = |cwd| json!({"type":"turn_context","payload":{"cwd":cwd}});
        for (damaged, ambiguous) in [
            (r#"{"type":"turn_context","payload":"#, true),
            (r#"{"type":"turn_co"#, true),
            (r#"{"type":"t"#, true),
            (r#"{"t"#, true),
            ("{", true),
            (
                r#"{"type":"response_item","payload":{"text":"turn_context"#,
                false,
            ),
            (
                r#"{"timestamp":"2026-01-02","type":"event_msg","payload":{"type":"thread_rolled_ba"#,
                true,
            ),
            (
                r#"{"timestamp":"2026-01-02","type":"event_msg","payload":{"type":"token_count","text":"turn_context"#,
                false,
            ),
        ] {
            for (moved, recovered) in [(false, false), (true, false), (true, true)] {
                let mut body = format!("{header}\n{}\n", message("user", "preserved request"));
                if moved {
                    writeln!(body, "{}", turn("/moved")).unwrap();
                }
                body.push_str(damaged);
                if recovered {
                    writeln!(body, "\n{}", turn("/recovered")).unwrap();
                }
                let bytes = codec.strip_prefix("jsonl.").map_or_else(
                    || body.as_bytes().to_vec(),
                    |ext| encode_rollout(body.as_bytes(), ext),
                );
                fs::write(&path, &bytes).unwrap();
                let adapter = CodexAdapter::with_root(home.path());
                let unknown = ambiguous && !recovered;
                let expected = if unknown {
                    ""
                } else if recovered {
                    "/recovered"
                } else if moved {
                    "/moved"
                } else {
                    "/creation"
                };
                for snapshot in [
                    adapter.read_session(&session).unwrap(),
                    adapter.preview_session(&session).unwrap(),
                    adapter
                        .index_session(&session, Some(&path), 1)
                        .unwrap()
                        .snapshot,
                ] {
                    assert_eq!(
                        snapshot.workspace.current_dir,
                        Path::new(expected),
                        "{codec}, {damaged}"
                    );
                    assert_eq!(snapshot.workspace.root, Path::new(expected));
                    assert_eq!(
                        snapshot.workspace.git.branch.as_deref(),
                        (expected == "/creation").then_some("creation")
                    );
                    assert_eq!(
                        snapshot
                            .events
                            .iter()
                            .any(|event| event.payload["workspace_unknown"] == true),
                        unknown
                    );
                    assert_eq!(visible_text(&snapshot), ["preserved request"]);
                    assert!(omnis_core::import_conversation(&snapshot).truncated);
                }
                assert_eq!(fs::read(&path).unwrap(), bytes);
            }
        }
    }
}

#[test]
fn codex_reports_malformed_rows_without_losing_readable_history() {
    for extension in ["jsonl", "jsonl.zst", "jsonl.gz"] {
        let home = tempfile::tempdir().unwrap();
        let folder = home.path().join("archived_sessions");
        fs::create_dir(&folder).unwrap();
        let path = folder.join(format!("rollout-{SESSION_ID}.{extension}"));
        let session = SessionRef::new(Provider::Codex, SESSION_ID);
        let header = json!({"type":"session_meta","payload":{"id":SESSION_ID,"cwd":"/fixture"}});
        for (damaged, opener_before_gap) in [(false, true), (true, true), (true, false)] {
            let unreadable = if damaged {
                "{invalid private fragment}\n"
            } else {
                ""
            };
            let ending = if damaged {
                "{\"type\":\"response_item\""
            } else {
                ""
            };
            let contents = format!(
                "{header}\n{}{}\n \t\r\n{}{}\n{ending}",
                if opener_before_gap { "" } else { unreadable },
                message("user", "opening"),
                if opener_before_gap { unreadable } else { "" },
                message("assistant", "readable reply")
            );
            let bytes = match extension {
                "jsonl" => contents.into_bytes(),
                extension => encode_rollout(
                    contents.as_bytes(),
                    extension.strip_prefix("jsonl.").unwrap(),
                ),
            };
            fs::write(&path, &bytes).unwrap();
            let adapter = CodexAdapter::with_root(home.path());
            let full = adapter
                .index_session(&session, Some(&path), u64::MAX)
                .unwrap();
            assert!(full.source_complete, "all physical bytes were inspected");
            if damaged {
                let notice = full
                    .snapshot
                    .events
                    .iter()
                    .find(|event| event.payload["malformed_records"] == 2)
                    .unwrap();
                assert_eq!(
                    notice.payload["prefix_end_sequence"],
                    u64::from(opener_before_gap)
                );
            }
            for snapshot in [
                full.snapshot,
                adapter.read_session_at(&session, Some(&path)).unwrap(),
                adapter.preview_session(&session).unwrap(),
                adapter
                    .index_session(&session, Some(&path), 1)
                    .unwrap()
                    .snapshot,
            ] {
                assert_eq!(visible_text(&snapshot), ["opening", "readable reply"]);
                assert_eq!(
                    omnis_core::session_message_prefix_is_known(&snapshot, true),
                    opener_before_gap
                );
                let missing = snapshot
                    .events
                    .iter()
                    .find_map(|event| event.payload["malformed_records"].as_u64());
                assert_eq!(missing, damaged.then_some(2));
                assert_eq!(
                    omnis_core::trajectory_search_document(&snapshot).source_complete,
                    !damaged
                );
                assert_eq!(omnis_core::import_trajectory(&snapshot).truncated, damaged);
                let mut recovery = Vec::new();
                omnis_core::write_recovery_handoff(&snapshot, &mut recovery).unwrap();
                let recovery = std::str::from_utf8(&recovery).unwrap();
                assert_eq!(recovery.contains("contained invalid JSON"), damaged);
                assert!(!recovery.contains("invalid private fragment"));
            }
            assert_eq!(fs::read(&path).unwrap(), bytes);
        }
    }
}

#[test]
fn a_copied_parent_header_preserves_the_child_history_mode() {
    let parent = "11111111-1111-4111-8111-111111111111";
    let rows = [
        json!({"type":"session_meta","payload":{"id":SESSION_ID,"forked_from_id":parent,"history_mode":"legacy"}}),
        json!({"type":"session_meta","payload":{"id":parent,"history_mode":"paginated"}}),
        message("user", "child request"),
        message("assistant", "child reply"),
    ];
    let source = rows.map(|row| row.to_string()).join("\n") + "\n";
    for extension in ["", "gz", "zst"] {
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        fs::create_dir(&sessions).unwrap();
        let mut path = sessions.join(format!("rollout-{SESSION_ID}.jsonl"));
        let bytes = if extension.is_empty() {
            source.as_bytes().to_vec()
        } else {
            path.set_extension(format!("jsonl.{extension}"));
            encode_rollout(source.as_bytes(), extension)
        };
        fs::write(&path, bytes).unwrap();
        let adapter = CodexAdapter::with_root(root.path());
        let session = SessionRef::new(Provider::Codex, SESSION_ID);
        for snapshot in [
            adapter.read_session(&session).unwrap(),
            adapter.preview_session(&session).unwrap(),
            adapter
                .index_session(&session, Some(&path), u64::MAX)
                .unwrap()
                .snapshot,
        ] {
            assert_eq!(visible_text(&snapshot), ["child request", "child reply"]);
            let search = omnis_core::trajectory_search_document(&snapshot);
            assert!(search.source_complete);
            for text in [search.text, omnis_core::render_semantic_handoff(&snapshot)] {
                assert!(text.contains("child request") && text.contains("child reply"));
            }
        }
    }
}

#[test]
fn codex_discovers_and_reads_compressed_active_and_archived_rollouts() {
    use omnis_adapters::NativeResumeSupport;
    for extension in ["gz", "zst"] {
        for directory in ["sessions/2026/05/15", "archived_sessions"] {
            let home = tempfile::tempdir().unwrap();
            let folder = home.path().join(directory);
            fs::create_dir_all(&folder).unwrap();
            let path = folder.join(format!(
                "rollout-2026-05-15T01-02-03-{SESSION_ID}.jsonl.{extension}"
            ));
            let rows = [
                json!({"type":"session_meta","payload":{"id":SESSION_ID,"cwd":"/sample/work","model_provider":"synthetic-provider"}}),
                message("user", "a discarded request"),
                event("thread_rolled_back", json!({"num_turns":1})),
                message("user", "the active request"),
                message("assistant", "the complete reply"),
            ];
            let bytes = encode_rollout(
                rows.map(|row| row.to_string()).join("\n").as_bytes(),
                extension,
            );
            fs::write(&path, &bytes).unwrap();
            let adapter = CodexAdapter::with_root(home.path());
            let listed = adapter.list_sessions(None).unwrap();
            assert_eq!(listed.len(), 1);
            assert_eq!(listed[0].source_path.as_ref(), Some(&path));
            let session = &listed[0].session;
            assert_eq!(
                adapter
                    .recorded_model_provider(SESSION_ID)
                    .unwrap()
                    .as_deref(),
                Some("synthetic-provider")
            );
            assert_eq!(
                adapter.native_resume_support(session).unwrap(),
                if directory.starts_with("sessions/") && extension == "zst" {
                    NativeResumeSupport::Since("0.137.0")
                } else {
                    NativeResumeSupport::ImportOnly
                }
            );
            for snapshot in [
                adapter.read_session(session).unwrap(),
                adapter.preview_session(session).unwrap(),
                adapter
                    .index_session(session, Some(&path), 100)
                    .unwrap()
                    .snapshot,
            ] {
                assert_eq!(
                    visible_text(&snapshot),
                    ["the active request", "the complete reply"]
                );
            }
            assert_eq!(fs::read(&path).unwrap(), bytes);
            assert_eq!(fs::read_dir(&folder).unwrap().count(), 1);
        }
    }
}

#[test]
fn codex_reports_unavailable_inheritance_and_compaction_before_restore() {
    for inherited in [true, false] {
        let home = tempfile::tempdir().unwrap();
        let folder = home.path().join("archived_sessions");
        fs::create_dir(&folder).unwrap();
        let source = folder.join(format!("rollout-{SESSION_ID}.jsonl.zst"));
        let mut header =
            json!({"type":"session_meta","payload":{"id":SESSION_ID,"cwd":"/example"}});
        let mut rows = Vec::new();
        if inherited {
            header["payload"]["history_mode"] = json!("paginated");
            header["payload"]["history_base"] = json!({
                "thread_id":"55555555-5555-4555-8555-555555555555",
                "end_byte_offset":512, "end_ordinal_exclusive":4
            });
        } else {
            rows.push(json!({"type":"compacted","payload":{
                "message":"Checkpoint for the remaining work",
                "replacement_history":[{"type":"message","role":"user","content":[{"type":"input_text","text":"retained request"}]}]
            }}));
        }
        rows.insert(0, header);
        rows.push(message("user", "request in the retained rollout"));
        if inherited {
            rows.push(page_user(
                "turn-one",
                "request",
                "request in the retained rollout",
            ));
            for (index, row) in rows.iter_mut().enumerate() {
                row["ordinal"] = (4 + index).into();
                row["timestamp"] = "2026-01-01T00:00:00.000Z".into();
            }
        }
        let text = rows
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        let encoded = encode_rollout(text.as_bytes(), "zst");
        fs::write(&source, &encoded).unwrap();
        let adapter = CodexAdapter::with_root(home.path());
        let session = SessionRef::new(Provider::Codex, SESSION_ID);
        let indexed = adapter
            .index_session(&session, Some(&source), 4 * 1024 * 1024)
            .unwrap();
        assert!(
            indexed.source_complete,
            "the complete leaf rollout was read"
        );
        for snapshot in [
            indexed.snapshot,
            adapter.read_session(&session).unwrap(),
            adapter.preview_session(&session).unwrap(),
        ] {
            assert!(visible_text(&snapshot).contains(&"request in the retained rollout"));
            let trajectory = omnis_core::import_trajectory(&snapshot);
            let restored_checkpoint = !inherited
                && trajectory
                    .items
                    .iter()
                    .any(|item| item.text.contains("retained request"));
            if !restored_checkpoint {
                assert!(!omnis_core::trajectory_search_document(&snapshot).source_complete);
                assert!(omnis_core::import_conversation(&snapshot).truncated);
                assert!(trajectory.truncated);
            }
        }
        assert_eq!(fs::read(&source).unwrap(), encoded);
    }
}

#[test]
fn codex_uses_plain_sibling_and_reports_truncated_compressed_history() {
    let home = tempfile::tempdir().unwrap();
    let folder = home.path().join("sessions");
    fs::create_dir(&folder).unwrap();
    let path = folder.join(format!("rollout-{SESSION_ID}.jsonl"));
    let header = json!({"type":"session_meta","payload":{"id":SESSION_ID,"cwd":"/example"}});
    let source = SessionRef::new(Provider::Codex, SESSION_ID);
    for extension in ["zst", "gz"] {
        let compressed = path.with_extension(format!("jsonl.{extension}"));
        let mut bytes = encode_rollout(
            format!("{header}\n{}\n", message("user", "stale")).as_bytes(),
            extension,
        );
        bytes.truncate(bytes.len() - 2);
        fs::write(&compressed, bytes).unwrap();
        fs::write(&path, format!("{header}\n{}\n", message("user", "newest"))).unwrap();
        let adapter = CodexAdapter::with_root(home.path());
        assert_eq!(
            visible_text(&adapter.read_session(&source).unwrap()),
            ["newest"]
        );
        assert_eq!(adapter.list_sessions(None).unwrap().len(), 1);
        fs::remove_file(&path).unwrap();
        assert!(adapter.read_session(&source).is_err());
        fs::remove_file(&compressed).unwrap();
    }
}

#[test]
fn compressed_rollback_preview_bounds_message_bytes_and_recovers_surviving_edges() {
    use std::io::Write;

    let temporary = tempfile::tempdir().unwrap();
    let folder = temporary.path().join("archived_sessions");
    fs::create_dir(&folder).unwrap();
    let path = folder.join(format!("rollout-{SESSION_ID}.jsonl.zst"));
    let mut writer =
        zstd::stream::write::Encoder::new(fs::File::create(&path).unwrap(), 1).unwrap();
    let header = json!({"type":"session_meta","payload":{"id":SESSION_ID,"cwd":"/synthetic","cli_version":"v".repeat(32 * 1024)}});
    writeln!(writer, "{header}").unwrap();
    writeln!(
        writer,
        "{}",
        json!({"type":"turn_context","payload":{"cwd":"/synthetic/surviving"}})
    )
    .unwrap();
    writeln!(writer, "{}", message("user", "surviving head")).unwrap();
    let surviving = message("assistant", &"s".repeat(128 * 1024)).to_string();
    for index in 0..96 {
        if index == 48 {
            writeln!(
                writer,
                "{}",
                message("assistant", "middle outside both edges")
            )
            .unwrap();
        }
        writeln!(writer, "{surviving}").unwrap();
    }
    writeln!(writer, "{}", message("assistant", "reply before rollback")).unwrap();
    let discarded = message("user", &format!("discarded {}", "d".repeat(128 * 1024))).to_string();
    let path_component = "a/".repeat(16 * 1024);
    for index in 0..160 {
        let cwd = format!("/synthetic/{index}/{path_component}");
        writeln!(
            writer,
            "{}",
            json!({"type":"turn_context","payload":{"cwd":cwd}})
        )
        .unwrap();
        writeln!(writer, "{discarded}").unwrap();
    }
    writeln!(
        writer,
        "{}",
        event("thread_rolled_back", json!({"num_turns":160}))
    )
    .unwrap();
    writeln!(writer, "{}", message("user", "follow-up")).unwrap();
    writer.finish().unwrap();

    let adapter = CodexAdapter::with_root(temporary.path());
    let session = SessionRef::new(Provider::Codex, SESSION_ID);
    let read = adapter
        .index_session(&session, Some(&path), 64 * 1024)
        .unwrap();
    let text = visible_text(&read.snapshot);
    assert_eq!(text.first(), Some(&"surviving head"));
    assert!(text.ends_with(&["reply before rollback", "follow-up"]));
    assert!(!text.contains(&"middle outside both edges"));
    assert_eq!(
        read.workspace_projection,
        omnis_adapters::CodexWorkspaceProjection::Observed(Some("/synthetic/surviving".into()))
    );
    assert!(text.iter().all(|message| !message.starts_with("discarded")));
    assert!(omnis_core::session_message_prefix_is_known(
        &read.snapshot,
        false
    ));
    let text_bytes = text.iter().map(|text| text.len()).sum::<usize>();
    let metadata_bytes: usize = read
        .snapshot
        .events
        .iter()
        .filter_map(|e| e.source.provider_version.as_ref())
        .map(String::len)
        .sum();
    assert!(text_bytes + metadata_bytes <= 8 * 1024 * 1024);
    assert_eq!(
        read.snapshot.workspace.root,
        Path::new("/synthetic/surviving")
    );
    assert!(!read.source_complete);
    assert!(omnis_core::import_conversation(&read.snapshot).truncated);
}

#[test]
fn compressed_rollout_titles_depend_on_the_first_gap() -> anyhow::Result<()> {
    let header = json!({"type":"session_meta", "payload":{"id":SESSION_ID}});
    let oversized = json!({"type":"response_item", "payload":{
        "type":"message","role":"user","content":[{"type":"input_image","image_url":"a".repeat(16 * 1024 * 1024)}]
    }}).to_string();
    let follow_up = message("user", "visible follow-up");
    for extension in ["gz", "zst"] {
        let home = tempfile::tempdir()?;
        let archive = home.path().join("archived_sessions");
        fs::create_dir(&archive)?;
        let path = archive.join(format!("rollout-{SESSION_ID}.jsonl.{extension}"));
        let session = SessionRef::new(Provider::Codex, SESSION_ID);
        for opener_before_gap in [true, false] {
            let opener = if opener_before_gap {
                format!("{}\n", message("user", "verified opener"))
            } else {
                String::new()
            };
            let raw = format!("{header}\n{opener}{oversized}\n{follow_up}\n");
            fs::write(&path, encode_rollout(raw.as_bytes(), extension))?;
            let adapter = CodexAdapter::with_root(home.path());
            let indexed = adapter.index_session(&session, Some(&path), 16 * 1024 * 1024)?;
            assert!(!indexed.source_complete);
            for (snapshot, full_read) in [
                (indexed.snapshot, false),
                (adapter.read_session(&session)?, true),
            ] {
                assert_eq!(visible_text(&snapshot).last(), Some(&"visible follow-up"));
                assert_eq!(
                    omnis_core::session_message_prefix_is_known(&snapshot, full_read),
                    opener_before_gap,
                    "{extension}, full read {full_read}, opener before gap {opener_before_gap}"
                );
            }
        }
        let raw = format!(
            "{header}\n{}\n{}\n{follow_up}\n",
            message("user", &"x".repeat(4 * 1024 * 1024)),
            event("thread_rolled_back", json!({"num_turns":0})),
        );
        fs::write(&path, encode_rollout(raw.as_bytes(), extension))?;
        let adapter = CodexAdapter::with_root(home.path());
        let preview = adapter.preview_session(&session)?;
        assert_eq!(visible_text(&preview), ["visible follow-up"]);
        assert!(!omnis_core::session_message_prefix_is_known(
            &preview, false
        ));
    }
    Ok(())
}

#[test]
fn compressed_previews_confirm_absence_when_no_records_are_missing() -> anyhow::Result<()> {
    let metadata = json!({"type":"turn_context","payload":{"model":"synthetic","padding":"x".repeat(1024 * 1024)}});
    let edge = format!("{metadata}\n").repeat(3);
    let header = json!({"type":"session_meta","payload":{"id":SESSION_ID}});
    let rollback = event("thread_rolled_back", json!({"num_turns":0}));
    for extension in ["gz", "zst"] {
        let home = tempfile::tempdir()?;
        let folder = home.path().join("sessions");
        fs::create_dir(&folder)?;
        let path = folder.join(format!("rollout-{SESSION_ID}.jsonl.{extension}"));
        let session = SessionRef::new(Provider::Codex, SESSION_ID);
        for (body, known) in [
            (edge.repeat(2), true),
            (
                format!(
                    "{edge}{edge}{}\n{edge}{edge}",
                    message("user", "unsampled request")
                ),
                false,
            ),
            (format!("{edge}{edge}broken JSON\n{rollback}\n"), false),
            (format!("{edge}{edge}{rollback}\n"), true),
        ] {
            fs::write(
                &path,
                encode_rollout(format!("{header}\n{body}").as_bytes(), extension),
            )?;
            let adapter = CodexAdapter::with_root(home.path());
            let preview = adapter.preview_session(&session)?;
            assert!(visible_text(&preview).is_empty());
            assert_eq!(
                omnis_core::session_message_prefix_is_known(&preview, false),
                known
            );
        }
    }
    Ok(())
}

#[test]
fn codex_index_budgets_the_decoded_size_of_compressed_rollouts() {
    let temporary = tempfile::tempdir().unwrap();
    let folder = temporary.path().join("archived_sessions");
    fs::create_dir(&folder).unwrap();
    let path = folder.join(format!("rollout-{SESSION_ID}.jsonl.zst"));
    let header = json!({"type":"session_meta", "payload":{"id":SESSION_ID, "cwd":"/synthetic"}});
    let padding = format!(
        "{}\n",
        json!({"type":"progress", "data":"x".repeat(64 * 1024)})
    )
    .repeat(80);
    let raw = format!(
        "{header}\n{}\n{padding}{}\n{padding}{}\n",
        message("user", "head"),
        message("assistant", "middle"),
        message("user", "tail")
    );
    let encoded = encode_rollout(raw.as_bytes(), "zst");
    let budget = 64 * 1024;
    assert!((encoded.len() as u64) < budget);
    fs::write(&path, encoded).unwrap();
    let adapter = CodexAdapter::with_root(temporary.path());
    let session = SessionRef::new(Provider::Codex, SESSION_ID);
    let sampled = adapter
        .index_session(&session, Some(&path), budget)
        .unwrap();
    assert!(!sampled.source_complete);
    assert_eq!(visible_text(&sampled.snapshot), ["head", "tail"]);
    assert_eq!(
        visible_text(&adapter.read_session(&session).unwrap()),
        ["head", "middle", "tail"]
    );
}

#[test]
fn codex_list_read_index_and_resume_choose_the_same_rollout() -> anyhow::Result<()> {
    use std::time::{Duration, SystemTime};

    use omnis_adapters::NativeResumeSupport;

    let home = tempfile::tempdir()?;
    let archive_dir = home.path().join("archived_sessions");
    let active_dir = home.path().join("sessions/2026/03/10");
    fs::create_dir_all(&active_dir)?;
    fs::create_dir(&archive_dir)?;
    let name = format!("rollout-2026-03-10T09-00-00-{SESSION_ID}.jsonl");
    let archive = archive_dir.join(format!("{name}.gz"));
    let active = active_dir.join(name);
    let source = |text| {
        format!(
            "{}\n{}\n",
            json!({"type":"session_meta","payload":{"id":SESSION_ID}}),
            message("user", text)
        )
    };
    fs::write(
        &archive,
        encode_rollout(source("archived request").as_bytes(), "gz"),
    )?;
    let picker = CodexAdapter::with_root(home.path());
    let cached = picker.list_sessions(None)?.pop().unwrap();
    assert_eq!(cached.source_path.as_ref(), Some(&archive));
    fs::write(&active, source("active request"))?;
    fs::File::options()
        .write(true)
        .open(&active)?
        .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(100))?;
    let adapter = CodexAdapter::with_root(home.path());
    let listed = adapter.list_sessions(None)?;
    assert_eq!(listed.len(), 1);
    let session = &listed[0].session;
    assert_eq!(listed[0].source_path.as_ref(), Some(&active));
    assert_eq!(
        adapter.native_resume_support(session)?,
        NativeResumeSupport::Direct
    );
    assert_eq!(
        visible_text(&adapter.read_session(session)?),
        ["active request"]
    );
    assert_eq!(
        visible_text(
            &picker
                .index_session(session, cached.source_path.as_deref(), u64::MAX)?
                .snapshot
        ),
        ["active request"]
    );
    fs::remove_file(&active)?;
    assert_eq!(
        visible_text(
            &adapter
                .index_session(session, Some(&active), u64::MAX)?
                .snapshot
        ),
        ["archived request"]
    );
    assert_eq!(
        adapter.native_resume_support(session)?,
        NativeResumeSupport::ImportOnly
    );
    Ok(())
}

#[test]
fn unreadable_active_headers_fall_back_to_verified_archives_and_siblings() -> anyhow::Result<()> {
    use omnis_adapters::NativeResumeSupport;

    let home = tempfile::tempdir()?;
    let active = home.path().join("sessions/2026/03/10");
    let archive = home.path().join("archived_sessions");
    fs::create_dir_all(&active)?;
    fs::create_dir(&archive)?;
    let name = format!("rollout-2026-03-10T09-00-00-{SESSION_ID}.jsonl");
    let wrong_id = "11111111-1111-4111-8111-111111111111";
    let contents = |id, text| {
        format!(
            "{}\n{}\n",
            json!({"type":"session_meta","payload":{"id":id,"model_provider":text}}),
            message("user", text)
        )
    };
    let plain = active.join(&name);
    let compressed = plain.with_extension("jsonl.zst");
    fs::write(&plain, contents(wrong_id, "unrelated"))?;
    fs::write(&compressed, [0x28, 0xb5, 0x2f])?;
    let fallback = archive.join(name);
    fs::write(&fallback, contents(SESSION_ID, "archived"))?;
    let source = SessionRef::new(Provider::Codex, SESSION_ID);
    for (path, expected) in [(&fallback, "archived"), (&compressed, "compressed")] {
        if expected == "compressed" {
            fs::write(
                &compressed,
                encode_rollout(contents(SESSION_ID, expected).as_bytes(), "zst"),
            )?;
        }
        let adapter = CodexAdapter::with_root(home.path());
        let listed = adapter.list_sessions(None)?;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].source_path.as_ref(), Some(path));
        assert_eq!(
            adapter
                .find_native_session(SESSION_ID)?
                .unwrap()
                .source_path
                .as_ref(),
            Some(path)
        );
        assert_eq!(
            adapter.recorded_model_provider(SESSION_ID)?.as_deref(),
            Some(expected)
        );
        assert_eq!(visible_text(&adapter.read_session(&source)?), [expected]);
        assert_eq!(
            visible_text(
                &adapter
                    .index_session(&source, Some(&plain), u64::MAX)?
                    .snapshot
            ),
            [expected]
        );
        assert_eq!(
            adapter.native_resume_support(&source)?,
            NativeResumeSupport::ImportOnly
        );
    }
    Ok(())
}

#[test]
fn cached_lookup_finds_a_new_archive_after_all_old_headers_fail() -> anyhow::Result<()> {
    use omnis_adapters::NativeResumeSupport;

    let home = tempfile::tempdir()?;
    let day = home.path().join("sessions/2026/03/10");
    fs::create_dir_all(&day)?;
    let name = format!("rollout-2026-03-10T09-00-00-{SESSION_ID}.jsonl");
    let active = day.join(&name);
    let folder = home.path().join("archived_sessions");
    fs::create_dir_all(folder.join("older"))?;
    let rows = format!(
        "{}\n{}\n",
        json!({"type":"session_meta","payload":{"id":SESSION_ID,"cwd":"/example"}}),
        message("user", "preserved request")
    );
    fs::write(active.with_extension("jsonl.zst"), [0x28, 0xb5, 0x2f])?;
    for archive in [folder.join(&name), folder.join("older").join(&name)] {
        fs::write(&active, &rows)?;
        let picker = CodexAdapter::with_root(home.path());
        let direct = CodexAdapter::with_root(home.path());
        let session = picker.list_sessions(None)?.pop().unwrap().session;
        assert_eq!(
            direct.native_resume_support(&session)?,
            NativeResumeSupport::Direct
        );
        fs::rename(&active, &archive)?;
        let read = picker.index_session(&session, Some(&archive), u64::MAX)?;
        assert_eq!(visible_text(&read.snapshot), ["preserved request"]);
        assert_eq!(
            visible_text(&direct.read_session(&session)?),
            ["preserved request"]
        );
        assert_eq!(
            direct.native_resume_support(&session)?,
            NativeResumeSupport::ImportOnly
        );
        assert_eq!(
            direct
                .find_native_session(SESSION_ID)?
                .unwrap()
                .source_path
                .as_deref(),
            Some(archive.as_path())
        );
        fs::remove_file(&archive)?;
        assert!(format!("{:#}", direct.read_session(&session).unwrap_err()).contains("frame"));
        #[cfg(unix)]
        {
            let outside = tempfile::tempdir()?;
            let outside_rollout = outside.path().join(&name);
            fs::write(&outside_rollout, &rows)?;
            std::os::unix::fs::symlink(&outside_rollout, &archive)?;
            assert!(
                picker
                    .index_session(&session, Some(&archive), u64::MAX)
                    .is_err()
            );
            fs::remove_file(&archive)?;
        }
    }
    Ok(())
}

#[test]
fn picker_learns_a_preferred_new_source_while_its_archive_stays_readable() -> anyhow::Result<()> {
    use omnis_adapters::NativeResumeSupport;

    let home = tempfile::tempdir()?;
    let archive = home
        .path()
        .join("archived_sessions")
        .join(format!("rollout-2026-03-10T09-00-00-{SESSION_ID}.jsonl"));
    let active_dir = home.path().join("sessions/2026/03/11");
    let name = format!("rollout-2026-03-11T11-00-00-{SESSION_ID}.jsonl");
    let active = active_dir.join(&name);
    let rows = |id, text| {
        format!(
            "{}\n{}\n",
            json!({"type":"session_meta","payload":{"id":id}}),
            message("user", text)
        )
    };
    fs::create_dir_all(archive.parent().unwrap())?;
    fs::create_dir_all(&active_dir)?;
    fs::write(&archive, rows(SESSION_ID, "archive"))?;
    let worker = CodexAdapter::with_root(home.path());
    let session = worker.list_sessions(None)?.pop().unwrap().session;
    let index = |path: &Path| worker.index_session(&session, Some(path), u64::MAX);
    let outside = home.path().join("other");
    fs::create_dir(&outside)?;
    fs::write(outside.join(&name), rows(SESSION_ID, "unrelated"))?;
    assert_eq!(
        visible_text(&index(&outside.join(&name))?.snapshot),
        ["archive"]
    );
    #[cfg(unix)]
    {
        let link = active_dir.join("alias");
        std::os::unix::fs::symlink(&outside, &link)?;
        assert_eq!(
            visible_text(&index(&link.join(&name))?.snapshot),
            ["archive"]
        );
        fs::remove_file(link)?;
    }
    let other_id = "11111111-1111-4111-8111-111111111111";
    let other = active.with_file_name(name.replace(SESSION_ID, other_id));
    fs::write(&other, rows(other_id, "another session"))?;
    assert_eq!(visible_text(&index(&other)?.snapshot), ["archive"]);
    fs::write(&active, rows(other_id, "mismatched header"))?;
    assert_eq!(visible_text(&index(&active)?.snapshot), ["archive"]);
    fs::remove_file(&active)?;

    // The other picker discovers the new rollout after the worker cached its archive.
    let worker = CodexAdapter::with_root(home.path());
    worker.find_native_session(SESSION_ID)?;
    fs::write(&active, rows(SESSION_ID, "active"))?;
    let fresh = CodexAdapter::with_root(home.path())
        .find_native_session(SESSION_ID)?
        .unwrap();
    assert_eq!(fresh.source_path.as_deref(), Some(active.as_path()));
    let read = worker.index_session(&session, fresh.source_path.as_deref(), u64::MAX)?;
    assert_eq!(visible_text(&read.snapshot), ["active"]);
    assert_eq!(visible_text(&worker.read_session(&session)?), ["active"]);
    assert_eq!(
        worker.native_resume_support(&session)?,
        NativeResumeSupport::Direct
    );
    assert_eq!(
        visible_text(
            &worker
                .index_session(&session, Some(&archive), u64::MAX)?
                .snapshot
        ),
        ["active"]
    );
    Ok(())
}

#[test]
fn gui_name_follows_the_selected_rollout_and_a_clear_overrides_the_index() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let sessions = home.path().join("sessions");
    fs::create_dir(&sessions)?;
    let path = sessions.join(format!("rollout-2026-10-01T00-00-00-{SESSION_ID}.jsonl"));
    fs::write(
        &path,
        format!(
            "{}\n",
            json!({
                "type": "session_meta", "payload": {"id": SESSION_ID, "cwd": "/synthetic/project", "history_mode":"paginated"}
            })
        ),
    )?;
    let path = path.canonicalize()?;
    let index = home.path().join("session_index.jsonl");
    let db_path = home.path().join("state_5.sqlite");
    let db = rusqlite::Connection::open(&db_path)?;
    db.execute_batch("PRAGMA journal_mode=WAL;
        CREATE TABLE threads(id TEXT PRIMARY KEY, rollout_path TEXT, history_mode TEXT, name TEXT, cwd TEXT);
    ")?;
    db.execute(
        "INSERT INTO threads VALUES(?1, ?2, 'paginated', ' 🛰️ Orbital notes ', '/synthetic/project')",
        rusqlite::params![SESSION_ID, path.to_str()],
    )?;
    let check = |expected: Option<&str>| check_gui_title(home.path(), &path, expected);
    check(Some("🛰️ Orbital notes"))?;
    let adapter = CodexAdapter::with_root(home.path());
    let listed = adapter.list_sessions(None)?;
    assert_eq!(
        adapter.gui_session_names(&listed).get(SESSION_ID),
        Some(&Some("🛰️ Orbital notes".to_owned()))
    );
    fs::write(
        &index,
        json!({"id": SESSION_ID, "thread_name": "Earlier sidebar name"}).to_string(),
    )?;
    db.execute(
        "UPDATE threads SET name = 'Renamed', cwd = '/changed/workspace'",
        [],
    )?;
    check(Some("Renamed"))?;
    db.execute("UPDATE threads SET name = NULL", [])?;
    assert!(adapter.gui_session_names(&listed).is_empty());
    let cleared = adapter.list_sessions(None)?;
    assert_eq!(
        adapter.gui_session_names(&cleared).get(SESSION_ID),
        Some(&None)
    );
    check(None)?;
    drop(db);
    check(None)?;
    assert!(!db_path.with_extension("sqlite-wal").exists());
    assert!(!db_path.with_extension("sqlite-shm").exists());

    let db = rusqlite::Connection::open(&db_path)?;
    check_foreign_gui_rollout(home.path(), &path, &db)?;
    db.execute_batch(
        "ALTER TABLE threads ADD COLUMN title TEXT;
        ALTER TABLE threads ADD COLUMN first_user_message TEXT;
        ALTER TABLE threads ADD COLUMN source TEXT;
        UPDATE threads SET history_mode='legacy', title='Assigned legacy name',
            first_user_message='Synthetic private opening', source='\"cli\"';",
    )?;
    fs::write(&path, json!({"type":"session_meta","payload":{"id":SESSION_ID,"cwd":"/synthetic/project","history_mode":"legacy"}}).to_string())?;
    check(Some("Assigned legacy name"))?;
    db.execute("UPDATE threads SET title='Guardian review'", [])?;
    check(Some("Guardian review"))?;
    db.execute(
        "UPDATE threads SET source=?1",
        [r#"{"subagent":{"other":"guardian"}}"#],
    )?;
    check(Some("Earlier sidebar name"))?;
    db.execute(
        "UPDATE threads SET title=char(160)||first_user_message||char(9)",
        [],
    )?;
    check(Some("Earlier sidebar name"))?;
    fs::remove_file(index)?;
    check(None)?;
    drop(db);
    fs::create_dir(home.path().join("alternate"))?;
    fs::rename(&db_path, home.path().join("alternate/state_5.sqlite"))?;
    fs::write(home.path().join("config.toml"), "sqlite_home = 'alternate'")?;
    let db = rusqlite::Connection::open(home.path().join("alternate/state_5.sqlite"))?;
    db.execute("UPDATE threads SET title='Configured name'", [])?;
    check(Some("Configured name"))?;
    #[cfg(unix)]
    check_linked_gui_config(home.path(), &path)?;
    check_archived_gui_name(home.path(), &path, &db)?;
    Ok(())
}

fn check_foreign_gui_rollout(
    home: &Path,
    path: &Path,
    db: &rusqlite::Connection,
) -> anyhow::Result<()> {
    let other = tempfile::tempdir()?;
    let other_path = other.path().join(path.file_name().unwrap());
    fs::copy(path, &other_path)?;
    db.execute(
        "UPDATE threads SET rollout_path = ?1, name = 'Foreign title'",
        [other_path.to_str()],
    )?;
    assert!(
        CodexAdapter::with_root(home)
            .list_sessions(None)?
            .is_empty()
    );
    db.execute("UPDATE threads SET rollout_path=?1", [path.to_str()])?;
    Ok(())
}

fn check_gui_title(home: &Path, path: &Path, expected: Option<&str>) -> anyhow::Result<()> {
    check_gui_title_for(home, path, expected, SESSION_ID)
}

fn check_gui_title_for(
    home: &Path,
    path: &Path,
    expected: Option<&str>,
    id: &str,
) -> anyhow::Result<()> {
    let session = SessionRef::new(Provider::Codex, id);
    let adapter = CodexAdapter::with_root(home);
    let mut rows = adapter.list_sessions(None)?;
    assert_eq!(rows[0].title.as_deref(), expected);
    rows[0].source_path = None;
    assert!(
        rows[0]
            .source_revision
            .and_then(|revision| revision.file)
            .is_some()
    );
    rows[0].title = Some("Old assigned name".into());
    adapter.refresh_session_titles(&mut rows);
    assert_eq!(rows[0].title.as_deref(), expected);
    rows[0].source_revision = None;
    rows[0].title = Some("Cached old opener".into());
    adapter.refresh_session_titles(&mut rows);
    assert_eq!(rows[0].title.as_deref(), expected);
    assert_eq!(rows[0].source_path.as_ref().unwrap().canonicalize()?, path);
    assert_eq!(
        adapter.find_native_session(id)?.unwrap().title.as_deref(),
        expected
    );
    assert_eq!(adapter.read_session(&session)?.title.as_deref(), expected);
    assert_eq!(
        adapter.preview_session(&session)?.title.as_deref(),
        expected
    );
    Ok(())
}

#[cfg(unix)]
fn check_linked_gui_config(home: &Path, path: &Path) -> anyhow::Result<()> {
    let config = home.join("config.toml");
    fs::rename(&config, home.join("profile.toml"))?;
    std::os::unix::fs::symlink("profile.toml", &config)?;
    check_gui_title(home, path, Some("Configured name"))?;
    fs::remove_file(&config)?;
    let dotfiles = tempfile::tempdir()?;
    let profile = dotfiles.path().join("profile.toml");
    fs::rename(home.join("profile.toml"), &profile)?;
    std::os::unix::fs::symlink(&profile, &config)?;
    check_gui_title(home, path, Some("Configured name"))?;
    fs::remove_file(&config)?;
    fs::rename(&profile, home.join("auth.json"))?;
    std::os::unix::fs::symlink("auth.json", &config)?;
    let adapter = CodexAdapter::with_root(home);
    assert!(adapter.list_sessions(None)?.is_empty());
    assert!(
        adapter
            .discovery_notes()
            .iter()
            .any(|note| note.contains("could not verify the current rollouts"))
    );
    fs::remove_file(&config)?;
    fs::rename(home.join("auth.json"), config)?;
    Ok(())
}

fn check_archived_gui_name(
    home: &Path,
    path: &Path,
    db: &rusqlite::Connection,
) -> anyhow::Result<()> {
    let mut rows = CodexAdapter::with_root(home).list_sessions(None)?;
    rows[0].source_path = None;
    let archive = home.join("archived_sessions");
    fs::create_dir(&archive)?;
    let archived = archive.join(path.file_name().unwrap());
    fs::rename(path, &archived)?;
    db.execute("UPDATE threads SET rollout_path = ?1", [archived.to_str()])?;
    CodexAdapter::with_root(home).refresh_session_titles(&mut rows);
    assert_eq!(rows[0].title.as_deref(), Some("Configured name"));
    assert_eq!(rows[0].source_path.as_deref(), Some(archived.as_path()));
    Ok(())
}

#[test]
fn index_only_names_require_a_unique_rollout_even_after_caching() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let sessions = home.path().join("sessions");
    fs::create_dir(&sessions)?;
    let rollout = |name: &str, project: &str| -> anyhow::Result<_> {
        let path = sessions.join(format!("rollout-{name}-{SESSION_ID}.jsonl"));
        let header = json!({"type":"session_meta", "payload":{"id":SESSION_ID, "cwd":project}});
        fs::write(&path, format!("{header}\n"))?;
        Ok(path.canonicalize()?)
    };
    let original = rollout("2026-10-02T00-00-00", "/one")?;
    fs::write(
        home.path().join("session_index.jsonl"),
        json!({"id":SESSION_ID,"thread_name":"Original name"}).to_string(),
    )?;
    fs::OpenOptions::new()
        .write(true)
        .open(&original)?
        .set_modified(std::time::SystemTime::UNIX_EPOCH)?;
    let mut warm = CodexAdapter::with_root(home.path()).list_sessions(None)?;
    assert_eq!(warm[0].title.as_deref(), Some("Original name"));
    warm[0].source_path = None;
    let duplicate = rollout("2026-10-01T00-00-00", "/two")?;
    let check = |expected_path: &Path, project: &str, title: Option<&str>| -> anyhow::Result<()> {
        let adapter = CodexAdapter::with_root(home.path());
        assert_eq!(adapter.list_sessions(None)?[0].title.as_deref(), title);
        let mut cached = warm.clone();
        adapter.refresh_session_titles(&mut cached);
        assert_eq!(cached[0].title.as_deref(), title);
        let preview = adapter.preview_session(&cached[0].session)?;
        assert_eq!(preview.title, cached[0].title);
        assert_eq!(preview.workspace.current_dir, Path::new(project));
        assert_eq!(
            cached[0].project_path.as_ref(),
            Some(&preview.workspace.current_dir)
        );
        assert_eq!(
            cached[0].source_path.as_ref().unwrap().canonicalize()?,
            expected_path
        );
        Ok(())
    };
    fs::write(
        home.path().join("session_index.jsonl"),
        format!(
            "{{\n{}\n",
            json!({"id":SESSION_ID,"thread_name":"Original name"})
        ),
    )?;
    let adapter = CodexAdapter::with_root(home.path());
    let listed = adapter.list_sessions(None)?;
    assert!(!adapter.session_titles_are_complete());
    assert_eq!(
        adapter.observed_listed_names(&listed).get(SESSION_ID),
        Some(&omnis_adapters::NativeSessionName::Absent)
    );
    check(&original, "/one", None)?;
    let state = home.path().join("alternate");
    fs::create_dir(&state)?;
    fs::write(home.path().join("config.toml"), "sqlite_home = 'alternate'")?;
    let db = rusqlite::Connection::open(state.join("state_5.sqlite"))?;
    db.execute_batch("CREATE TABLE threads(id TEXT PRIMARY KEY, rollout_path TEXT, history_mode TEXT, name TEXT, title TEXT, first_user_message TEXT)")?;
    check(&original, "/one", None)?;
    db.execute(
        "INSERT INTO threads VALUES(?1,?2,'legacy',NULL,'Private opener','Private opener')",
        rusqlite::params![SESSION_ID, original.to_str()],
    )?;
    check(&original, "/one", None)?;
    db.execute(
        "UPDATE threads SET history_mode='paginated', name='SQLite original name'",
        [],
    )?;
    for (path, project) in [(&original, "/one"), (&duplicate, "/two")] {
        fs::write(path, json!({"type":"session_meta","payload":{"id":SESSION_ID,"cwd":project,"history_mode":"paginated"}}).to_string())?;
    }
    check(&original, "/one", Some("SQLite original name"))?;
    db.execute(
        "UPDATE threads SET rollout_path=?1, name='SQLite second name'",
        [duplicate.to_str()],
    )?;
    check(&duplicate, "/two", Some("SQLite second name"))?;
    drop(db);
    fs::remove_file(state.join("state_5.sqlite"))?;
    fs::remove_file(duplicate)?;
    check_gui_title(home.path(), &original, Some("Original name"))?;
    let archive = home.path().join("archived_sessions");
    fs::create_dir(&archive)?;
    let archive_path = archive.join(original.file_name().unwrap());
    fs::rename(&original, &archive_path)?;
    let adapter = CodexAdapter::with_root(home.path());
    adapter.refresh_session_titles(&mut warm);
    assert_eq!(warm[0].source_path.as_ref(), Some(&archive_path));
    assert_eq!(warm[0].title.as_deref(), Some("Original name"));
    let preview = adapter.preview_session(&warm[0].session)?;
    assert_eq!(preview.title, warm[0].title);
    assert_eq!(preview.workspace.current_dir, Path::new("/one"));
    Ok(())
}

#[test]
fn cached_picker_reads_an_appended_reply_from_the_same_rollout() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let sessions = home.path().join("sessions");
    fs::create_dir(&sessions)?;
    let path = sessions.join(format!("rollout-2026-10-01T00-00-00-{SESSION_ID}.jsonl"));
    let header = json!({"type": "session_meta", "payload": {"id": SESSION_ID, "cwd": "/project"}});
    fs::write(&path, format!("{header}\n"))?;
    let mut cached = CodexAdapter::with_root(home.path()).list_sessions(None)?;
    cached[0].source_path = None;
    let old = cached[0].source_revision.unwrap();
    writeln!(
        fs::OpenOptions::new().append(true).open(&path)?,
        "{}",
        message("assistant", "Additional reply")
    )?;

    let adapter = CodexAdapter::with_root(home.path());
    adapter.refresh_session_titles(&mut cached);
    let read = adapter.index_session(&cached[0].session, None, 0)?;
    assert_eq!(old.file, read.source_revision.file);
    assert_ne!(old.contents, read.source_revision.contents);
    assert_eq!(cached[0].source_revision, Some(read.source_revision));
    assert!(
        visible_text(&read.snapshot)
            .iter()
            .any(|text| text.contains("Additional reply"))
    );
    Ok(())
}

#[test]
fn a_newer_archive_copy_preserves_the_selected_active_reply() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let file = format!("rollout-2026-10-01T00-00-00-{SESSION_ID}.jsonl");
    let write = |directory: &str, project: &str, replies: &[&str]| -> anyhow::Result<_> {
        let dir = home.path().join(directory);
        fs::create_dir(&dir)?;
        let path = dir.join(&file);
        let mut text =
            json!({"type":"session_meta","payload":{"id":SESSION_ID,"cwd":project}}).to_string();
        for reply in replies {
            text.push('\n');
            text.push_str(&json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":reply}]}}).to_string());
        }
        fs::write(&path, text)?;
        Ok(path)
    };
    let active = write("sessions", "/active", &["First reply", "Final reply"])?;
    let mut cached = CodexAdapter::with_root(home.path()).list_sessions(None)?;
    cached[0].source_path = None;
    fs::OpenOptions::new()
        .write(true)
        .open(&active)?
        .set_modified(std::time::SystemTime::UNIX_EPOCH)?;
    write("archived_sessions", "/archive", &["First reply"])?;
    let adapter = CodexAdapter::with_root(home.path());
    let listed = adapter.list_sessions(None)?;
    adapter.refresh_session_titles(&mut cached);
    for row in [&listed[0], &cached[0]] {
        assert_eq!(row.source_path.as_ref(), Some(&active));
        assert_eq!(row.project_path.as_deref(), Some(Path::new("/active")));
    }
    for snapshot in [
        adapter.read_session(&cached[0].session)?,
        adapter.preview_session(&cached[0].session)?,
    ] {
        assert_eq!(snapshot.workspace.current_dir, Path::new("/active"));
        assert!(serde_json::to_string(&snapshot.events)?.contains("Final reply"));
    }
    Ok(())
}

#[test]
fn index_reads_bind_revision_and_budget_to_the_selected_rollout() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let create = |directory: &str, project: &str, reply: &str| -> anyhow::Result<_> {
        let dir = home.path().join(directory);
        fs::create_dir(&dir)?;
        let path = dir.join(format!("rollout-2026-10-01T00-00-00-{SESSION_ID}.jsonl"));
        let meta = json!({"type": "session_meta", "payload": {"id":SESSION_ID, "cwd":project}});
        let msg = json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":reply}]}});
        fs::write(&path, format!("{meta}\n{msg}\n"))?;
        Ok(path)
    };
    let archived = create("archived_sessions", "/archive", "Obsolete response")?;
    let adapter = CodexAdapter::with_root(home.path());
    let session = SessionRef::new(Provider::Codex, SESSION_ID);
    let first = adapter.list_sessions(None)?;
    assert_eq!(first[0].source_path.as_ref(), Some(&archived));
    let original = adapter.read_full_index_at(&session, None)?;
    assert_eq!(Some(original.source_revision), first[0].source_revision);
    let active = create("sessions", "/active", "Current response")?;
    let revision = omnis_adapters::source_file_revision(&active).unwrap();
    assert_ne!(revision.file, original.source_revision.file);
    for hint in [None, Some(archived.as_path())] {
        for (read, complete) in [
            (adapter.index_session(&session, hint, 0)?, false),
            (adapter.read_full_index_at(&session, hint)?, true),
        ] {
            let meta = read.native_session.unwrap();
            assert_eq!(read.source_revision, revision);
            assert_eq!(meta.source_revision, Some(revision));
            assert_eq!(meta.source_path.as_ref(), Some(&active));
            assert_eq!(meta.project_path.as_deref(), Some(Path::new("/active")));
            assert_eq!(meta.updated_at, Some(read.snapshot.captured_at));
            assert_eq!(read.source_complete, complete);
            assert!(!read.provider_limited);
            let events = serde_json::to_string(&read.snapshot.events)?;
            assert!(events.contains("Current response"));
            assert!(!events.contains("Obsolete response"));
        }
    }
    Ok(())
}

#[test]
fn compressed_archive_uses_the_name_and_reply_of_the_registered_generation() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let active = home.path().join("sessions");
    let archive = home.path().join("archived_sessions");
    fs::create_dir(&active)?;
    fs::create_dir(&archive)?;
    let write = |dir: &Path,
                 generation: &str,
                 reply: &str,
                 compressed: bool|
     -> anyhow::Result<_> {
        let path = dir.join(format!(
            "rollout-2026-10-01T00-00-00-{SESSION_ID}_{generation}.jsonl"
        ));
        let header = json!({"type":"session_meta","payload":{"id":SESSION_ID,"cwd":"/project","history_mode":"paginated"}});
        let started = event("task_started", json!({"turn_id":"archive-turn"}));
        let published = turn_item(
            "archive-turn",
            &json!({"type":"AgentMessage","id":"reply","content":[{"type":"Text","text":reply}]}),
        );
        let mut text = String::new();
        for (ordinal, mut row) in [header, started, published].into_iter().enumerate() {
            row["ordinal"] = ordinal.into();
            row["timestamp"] = "2026-10-01T00:00:00Z".into();
            text.push_str(&row.to_string());
            text.push('\n');
        }
        if compressed {
            fs::write(
                path.with_extension("jsonl.zst"),
                encode_rollout(text.as_bytes(), "zst"),
            )?;
        } else {
            fs::write(&path, text)?;
        }
        Ok(path)
    };
    write(
        &active,
        "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
        "Superseded reply",
        false,
    )?;
    let recorded = write(
        &archive,
        "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
        "Archived reply",
        true,
    )?;
    let compressed = recorded.with_extension("jsonl.zst");
    let db = rusqlite::Connection::open(home.path().join("state_5.sqlite"))?;
    db.execute_batch("CREATE TABLE threads(id TEXT PRIMARY KEY, rollout_path TEXT, history_mode TEXT, name TEXT, archived INTEGER)")?;
    db.execute(
        "INSERT INTO threads VALUES(?1, ?2, 'paginated', 'Archived GUI name', 1)",
        rusqlite::params![SESSION_ID, recorded.to_str()],
    )?;
    let adapter = CodexAdapter::with_root(home.path());
    let mut rows = adapter.list_sessions(None)?;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].source_path.as_deref(), Some(compressed.as_path()));
    assert_eq!(rows[0].title.as_deref(), Some("Archived GUI name"));
    db.execute("UPDATE threads SET name='Renamed archive'", [])?;
    let adapter = CodexAdapter::with_root(home.path());
    adapter.refresh_session_titles(&mut rows);
    assert_eq!(rows[0].title.as_deref(), Some("Renamed archive"));
    let read = adapter.index_session(&rows[0].session, None, 0)?;
    assert_eq!(read.snapshot.title, rows[0].title);
    assert_eq!(
        read.native_session.unwrap().source_revision,
        rows[0].source_revision
    );
    assert_eq!(visible_text(&read.snapshot), ["Archived reply"]);
    Ok(())
}

#[test]
fn assigned_legacy_opener_survives_only_agreeing_archive_copies() -> anyhow::Result<()> {
    let id = "abcdefab-1234-4234-8234-123456789abc";
    let canonical = format!("rollout-2026-10-02T00-00-00-{id}.jsonl");
    for file in [&canonical[..], "orchard-history.jsonl"] {
        for compressed in [false, true] {
            check_assigned_legacy_opener(id, file, compressed)?;
        }
    }
    Ok(())
}

fn check_assigned_legacy_opener(id: &str, file: &str, compressed: bool) -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let home = tmp.path().canonicalize()?;
    let active_dir = home.join("sessions");
    let archived_dir = home.join("archived_sessions");
    fs::create_dir(&active_dir)?;
    fs::create_dir(&archived_dir)?;
    let recorded = active_dir.join(file);
    let active = if compressed {
        recorded.with_extension("jsonl.zst")
    } else {
        recorded.clone()
    };
    let archived = archived_dir.join(file.replace(id, &id.to_ascii_uppercase()));
    let opener = "Synthetic first question";
    let header =
        json!({"type":"session_meta","payload":{"id":id,"cwd":"/example","history_mode":"legacy"}});
    let text = format!(
        "{header}\n{}\n{}\n",
        message("user", opener),
        message("assistant", "Reply A")
    );
    if compressed {
        fs::write(&active, encode_rollout(text.as_bytes(), "zst"))?;
    } else {
        fs::write(&active, &text)?;
    }
    let db = rusqlite::Connection::open(home.join("state_5.sqlite"))?;
    db.execute_batch("CREATE TABLE threads(id TEXT PRIMARY KEY, rollout_path TEXT, history_mode TEXT, name TEXT, title TEXT, first_user_message TEXT)")?;
    db.execute(
        "INSERT INTO threads VALUES(?1,?2,'legacy',NULL,?3,?3)",
        rusqlite::params![id, recorded.to_str(), opener],
    )?;
    let index = home.join("session_index.jsonl");
    fs::write(&index, json!({"id":id,"thread_name":opener}).to_string())?;
    check_gui_title_for(&home, &active, Some(opener), id)?;
    fs::write(&archived, &text)?;
    check_gui_title_for(&home, &active, Some(opener), id)?;
    fs::write(&archived, text.replace("Reply A", "Reply B"))?;
    check_gui_title_for(&home, &active, None, id)?;
    let renamed = archived_dir.join("independent-history.jsonl");
    fs::rename(&archived, &renamed)?;
    check_gui_title_for(&home, &active, None, id)?;
    fs::remove_file(renamed)?;
    fs::write(&archived, &text)?;
    fs::write(
        &index,
        json!({"id":id,"thread_name":"Unrelated stale name"}).to_string(),
    )?;
    check_gui_title_for(&home, &active, None, id)?;
    fs::remove_file(&index)?;
    check_gui_title_for(&home, &active, None, id)?;
    Ok(())
}

#[test]
fn legacy_rollouts_with_a_shared_basename_keep_their_own_titles() -> anyhow::Result<()> {
    for file in [
        "rollout-imported.jsonl",
        "rollout-imported-aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee.jsonl",
        "rollout-imported-12345678-1234-4234-8234-123456789abc.jsonl",
    ] {
        check_legacy_owners(file)?;
    }
    Ok(())
}

fn check_legacy_owners(file: &str) -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let home = tmp.path().canonicalize()?;
    let db = rusqlite::Connection::open(home.join("state_5.sqlite"))?;
    db.execute_batch("CREATE TABLE threads(id TEXT PRIMARY KEY, rollout_path TEXT, history_mode TEXT, name TEXT, title TEXT, first_user_message TEXT)")?;
    let mut index = fs::File::create(home.join("session_index.jsonl"))?;
    let threads = [
        (
            "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee",
            "01",
            "First assigned opener",
        ),
        (
            "BBBBBBBB-CCCC-4DDD-8EEE-FFFFFFFFFFFF",
            "02",
            "Second assigned opener",
        ),
    ];
    for (id, day, title) in threads {
        let dir = home.join("sessions/2026/10").join(day);
        fs::create_dir_all(&dir)?;
        let path = dir.join(file);
        let header =
            json!({"type":"session_meta","payload":{"id":id,"cwd":dir,"history_mode":"legacy"}});
        fs::write(&path, format!("{header}\n{}\n", message("user", title)))?;
        db.execute(
            "INSERT INTO threads VALUES(?1,?2,'legacy',NULL,NULL,?3)",
            rusqlite::params![id, path.to_str(), title],
        )?;
        writeln!(index, "{}", json!({"id":id, "thread_name":title}))?;
    }
    let persistent = CodexAdapter::with_root(&home);
    let warm = persistent.list_sessions(None)?;
    let check = |expected: [Option<&str>; 2]| -> anyhow::Result<()> {
        let adapter = CodexAdapter::with_root(&home);
        let listed = adapter.list_sessions(None)?;
        let reused = persistent.list_sessions(None)?;
        let mut cached = warm.clone();
        for row in &mut cached {
            row.source_path = None;
        }
        adapter.refresh_session_titles(&mut cached);
        for ((id, _, _), name) in threads.into_iter().zip(expected) {
            for rows in [&listed, &cached, &reused] {
                assert_eq!(rows.len(), threads.len());
                assert_eq!(
                    rows.iter()
                        .find(|row| row.session.id == id)
                        .unwrap()
                        .title
                        .as_deref(),
                    name
                );
            }
            let snapshot = adapter.read_session(&SessionRef::new(Provider::Codex, id))?;
            assert_eq!(snapshot.title.as_deref(), name);
        }
        Ok(())
    };
    check([Some(threads[0].2), Some(threads[1].2)])?;
    let archive = home.join("archived_sessions");
    fs::create_dir(&archive)?;
    let shadow = archive.join(file);
    fs::write(&shadow, "{")?;
    check([None, None])?;
    let header = json!({"type":"session_meta","payload":{"id":threads[0].0.to_uppercase(),"cwd":"/another","history_mode":"legacy"}});
    fs::write(&shadow, format!("{header}\n"))?;
    check([None, Some(threads[1].2)])?;
    fs::write(
        &shadow,
        format!(
            "{}\n",
            json!({"type":"session_meta","payload":{"id":"c1234567-1234-4234-8234-123456789abc","cwd":"/other"}})
        ),
    )?;
    check([Some(threads[0].2), Some(threads[1].2)])?;
    fs::write(&shadow, "{")?;
    check([None, None])?;
    Ok(())
}
