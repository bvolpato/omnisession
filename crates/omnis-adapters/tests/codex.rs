use std::{fs, path::Path};

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
        "payload": {"id": SESSION_ID, "cwd": "/workspace/original", "git":{"branch":"original"}},
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
fn native_startup_and_source_directory_respect_settings_ownership() {
    use std::io::Write as _;

    let home = tempfile::tempdir().unwrap();
    let sessions = home.path().join("sessions");
    fs::create_dir(&sessions).unwrap();
    let mut file = fs::File::create(sessions.join(format!("rollout-{SESSION_ID}.jsonl"))).unwrap();
    let adapter = CodexAdapter::with_root(home.path());
    let session = SessionRef::new(Provider::Codex, SESSION_ID);
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
    let settings = |owner: Option<&str>, cwd: &str| {
        event(
            "thread_settings_applied",
            json!({
        "thread_id":owner,"thread_settings":{"cwd":cwd,"model":"fixture","model_provider_id":"synthetic",
        "approval_policy":"never","approvals_reviewer":"user","permission_profile":{"type":"disabled"},
        "collaboration_mode":{"mode":"default","settings":{"model":"fixture"}}}}),
        )
    };
    let creation = home.path().join("creation");
    let saved = home.path().join("saved");
    let a = creation.to_str().unwrap();
    let b = saved.to_str().unwrap();
    let turn = json!({"type":"turn_context","payload":{"cwd":a}});
    writeln!(
        file,
        "{}",
        json!({"type":"session_meta","payload":{"id":SESSION_ID,"cwd":a}})
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
            json!({"thread_id":SESSION_ID,"thread_settings":{"cwd":b}})
        )
    )
    .unwrap();
    let mut malformed = settings(Some(SESSION_ID), b);
    malformed["payload"]["thread_settings"]["approval_policy"] = json!("unknown-policy");
    writeln!(file, "{malformed}").unwrap();
    check(None, None);
    let mut managed = settings(Some(SESSION_ID), b);
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
    writeln!(file, "{}", settings(Some(SESSION_ID), a)).unwrap();
    check(Some(a), None);
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
            "message": "", "replacement_history": [
                {"type":"compaction","encrypted_content":"unreadable-ciphertext"},
                message("user", "Retained request")["payload"],
            ],
        }}),
    ])
    .unwrap();
    let conversation = omnis_core::import_conversation(&snapshot);
    assert!(conversation.truncated);
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
fn legacy_compaction_keeps_real_requests_then_the_latest_summary() {
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
            [EventKind::MessageUser, EventKind::MessageAssistant],
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
