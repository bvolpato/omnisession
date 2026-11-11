use std::{fs, path::Path};

use omnis_adapters::{AntigravityAdapter, LaunchTarget, ProviderAdapter};
use omnis_ir::{EventKind, Provider, ReplayPolicy, SessionRef};
use rusqlite::Connection;
use serde_json::json;
use tempfile::tempdir;

const SESSION_ID: &str = "11111111-1111-4111-8111-111111111111";

fn summary_database(root: &Path, project: &Path) {
    let connection = Connection::open(root.join("conversation_summaries.db")).expect("summary DB");
    connection
        .execute_batch(
            "CREATE TABLE conversation_summaries (
                conversation_id TEXT PRIMARY KEY,
                title TEXT NOT NULL DEFAULT '',
                step_count INTEGER NOT NULL DEFAULT 0,
                last_modified_time DATETIME NOT NULL,
                workspace_uris TEXT NOT NULL
            );",
        )
        .expect("summary schema");
    let mut uri_path = project.to_string_lossy().replace('\\', "/");
    if cfg!(windows) {
        uri_path.insert(0, '/');
    }
    let uri = format!("file://{uri_path}").replace(' ', "%20");
    connection
        .execute(
            "INSERT INTO conversation_summaries VALUES (?1, ?2, ?3, ?4, ?5)",
            (
                SESSION_ID,
                "Synthetic Antigravity session",
                4_i64,
                "2026-07-28T12:00:00Z",
                serde_json::to_string(&[uri]).expect("workspace JSON"),
            ),
        )
        .expect("summary row");
}

fn transcript(root: &Path) -> Vec<u8> {
    let logs = root
        .join("brain")
        .join(SESSION_ID)
        .join(".system_generated")
        .join("logs");
    fs::create_dir_all(&logs).expect("transcript directory");
    let records = [
        json!({
            "step_index": 0,
            "type": "USER_INPUT",
            "status": "DONE",
            "created_at": "2026-07-28T11:59:00Z",
            "content": "synthetic question"
        }),
        json!({
            "step_index": 1,
            "type": "PLANNER_RESPONSE",
            "status": "DONE",
            "created_at": "2026-07-28T11:59:01Z",
            "content": "synthetic answer",
            "thinking": "must remain omitted",
            "tool_calls": [{"name": "view_file", "args": {"path": "fixture.rs"}}]
        }),
        json!({
            "step_index": 2,
            "type": "RUN_COMMAND",
            "status": "DONE",
            "created_at": "2026-07-28T11:59:02Z",
            "content": "must remain omitted",
            "command": "must remain omitted"
        }),
    ];
    let bytes = records
        .iter()
        .map(serde_json::to_string)
        .collect::<Result<Vec<_>, _>>()
        .expect("serialize records")
        .join("\n")
        .into_bytes();
    fs::write(logs.join("transcript_full.jsonl"), &bytes).expect("transcript fixture");
    bytes
}

#[test]
fn lists_and_reads_synthetic_antigravity_session_without_mutation() {
    let temporary = tempdir().expect("temporary directory");
    let project = temporary.path().join("project space");
    fs::create_dir(&project).expect("project directory");
    summary_database(temporary.path(), &project);
    let transcript_before = transcript(temporary.path());
    let adapter = AntigravityAdapter::with_root(temporary.path());

    let sessions = adapter.list_sessions(Some(&project)).expect("session list");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].session.id, SESSION_ID);
    assert_eq!(sessions[0].project_path.as_deref(), Some(project.as_path()));
    assert_eq!(sessions[0].event_count, 4);

    let snapshot = adapter
        .read_session(&SessionRef::new(Provider::Antigravity, SESSION_ID))
        .expect("session read");
    assert_eq!(
        snapshot
            .events
            .iter()
            .map(|event| &event.kind)
            .collect::<Vec<_>>(),
        [
            &EventKind::MessageUser,
            &EventKind::MessageAssistant,
            &EventKind::ToolCalled,
            &EventKind::CommandExecuted,
        ]
    );
    let indexed = adapter
        .index_search_document(
            &sessions[0].session,
            sessions[0].source_path.as_deref(),
            u64::MAX,
            None,
        )
        .unwrap();
    let mut expected_document = omnis_core::trajectory_search_document(&snapshot);
    expected_document
        .projection
        .as_mut()
        .unwrap()
        .fully_scanned_bytes = Some(transcript_before.len() as u64);
    assert_eq!(indexed.document, expected_document);
    assert_eq!(
        omnis_core::session_search_title(&indexed.read.snapshot),
        omnis_core::session_search_title(&snapshot)
    );
    assert_eq!(
        omnis_core::session_preview(&indexed.read.snapshot).first,
        omnis_core::session_preview(&snapshot).first
    );
    assert_eq!(snapshot.events[0].replay_policy, ReplayPolicy::Contextual);
    assert_eq!(
        snapshot.events[2].replay_policy,
        ReplayPolicy::HistoricalOnly
    );
    let serialized = serde_json::to_string(&snapshot).expect("snapshot JSON");
    assert!(!serialized.contains("must remain omitted"));
    assert_eq!(
        fs::read(
            temporary
                .path()
                .join("brain")
                .join(SESSION_ID)
                .join(".system_generated/logs/transcript_full.jsonl")
        )
        .expect("source transcript"),
        transcript_before
    );
}

#[test]
fn discovery_selects_the_history_that_search_reads() {
    let temporary = tempdir().unwrap();
    let root = temporary.path();
    summary_database(root, root);
    let adapter = AntigravityAdapter::with_root(root);
    let check = |expected: Option<&Path>, text: &str| {
        let session = adapter.list_sessions(None).unwrap().pop().unwrap();
        assert_eq!(
            session.source_path,
            expected.map(|path| fs::canonicalize(path).unwrap())
        );
        let read =
            adapter.index_session(&session.session, session.source_path.as_deref(), u64::MAX);
        if expected.is_none() {
            assert!(read.is_err());
        } else {
            let snapshot = read.unwrap().snapshot;
            let message = snapshot
                .events
                .iter()
                .find(|event| event.kind == EventKind::MessageUser)
                .unwrap();
            assert_eq!(message.payload["text"], text);
        }
    };

    check(None, "");
    let database = root.join("conversations").join(format!("{SESSION_ID}.db"));
    fs::create_dir(database.parent().unwrap()).unwrap();
    Connection::open(&database)
        .unwrap()
        .execute_batch(
            "CREATE TABLE steps (idx INTEGER PRIMARY KEY, step_payload BLOB);
             INSERT INTO steps VALUES (0, x'080e9a01080a066462776f7264');",
        )
        .unwrap();
    check(Some(&database), "dbword");

    let logs = root
        .join("brain")
        .join(SESSION_ID)
        .join(".system_generated/logs");
    fs::create_dir_all(&logs).unwrap();
    let fallback = logs.join("transcript.jsonl");
    let full = logs.join("transcript_full.jsonl");
    fs::write(
        &fallback,
        concat!(r#"{"type":"USER_INPUT","content":"fallback"}"#, "\n"),
    )
    .unwrap();
    check(Some(&fallback), "fallback");
    fs::write(
        &full,
        concat!(r#"{"type":"USER_INPUT","content":"full"}"#, "\n"),
    )
    .unwrap();
    check(Some(&full), "full");
    fs::remove_file(database).unwrap();
    check(Some(&full), "full");
}

#[test]
fn observed_preview_reports_damage_and_keeps_the_messages_on_both_sides() {
    let temporary = tempdir().unwrap();
    let root = temporary.path();
    summary_database(root, root);
    let logs = root
        .join("brain")
        .join(SESSION_ID)
        .join(".system_generated/logs");
    fs::create_dir_all(&logs).unwrap();
    let path = logs.join("transcript_full.jsonl");
    fs::write(
        &path,
        concat!(
            r#"{"type":"USER_INPUT","content":"question"}"#,
            "\n{broken}\n",
            r#"{"type":"PLANNER_RESPONSE","content":"answer"}"#,
        ),
    )
    .unwrap();

    let adapter = AntigravityAdapter::with_root(root);
    let session = SessionRef::new(Provider::Antigravity, SESSION_ID);
    let read = adapter
        .index_session(&session, Some(&path.canonicalize().unwrap()), 0)
        .unwrap();
    assert!(!read.source_complete);
    let events = read.snapshot.events;
    assert_eq!(events[0].payload["text"], "question");
    assert_eq!(events[1].payload["text"], "answer");
    assert_eq!(events[2].payload["omitted_events"], 1);

    let full = adapter
        .read_full_index_at(&session, Some(&path.canonicalize().unwrap()))
        .unwrap();
    assert!(!full.source_complete);
    assert_eq!(
        full.verified_prefix_end,
        Some(full.snapshot.events[1].sequence)
    );
    assert!(
        full.snapshot
            .events
            .iter()
            .map(|event| &event.payload)
            .eq(events.iter().map(|event| &event.payload))
    );
}

#[test]
fn full_search_retries_empty_jsonl_and_streams_past_the_snapshot_record_limit() {
    let temporary = tempdir().unwrap();
    let root = temporary.path();
    summary_database(root, root);
    let logs = root
        .join("brain")
        .join(SESSION_ID)
        .join(".system_generated/logs");
    fs::create_dir_all(&logs).unwrap();
    let path = logs.join("transcript_full.jsonl");
    fs::write(&path, "").unwrap();
    let adapter = AntigravityAdapter::with_root(root);
    let listed = adapter.list_sessions(None).unwrap().pop().unwrap();
    let search = || {
        adapter.index_search_document(
            &listed.session,
            listed.source_path.as_deref(),
            u64::MAX,
            None,
        )
    };
    assert!(
        adapter
            .preview_session(&listed.session)
            .unwrap_err()
            .is::<omnis_adapters::IncompleteTranscript>()
    );
    assert!(
        search()
            .err()
            .unwrap()
            .is::<omnis_adapters::IncompleteTranscript>()
    );

    let mut data = "{}\n".repeat(50_000);
    data.push_str(concat!(
        r#"{"type":"USER_INPUT","content":"middleword"}"#,
        "\n"
    ));
    data.push_str(&"{}\n".repeat(50_000));
    data.push_str(concat!(
        r#"{"type":"USER_INPUT","content":"newestword"}"#,
        "\n"
    ));
    fs::write(path, data).unwrap();
    let indexed = search().unwrap();
    assert_eq!(indexed.document.text, "middleword\n\nnewestword");
    assert!(indexed.read.source_complete);
}

#[test]
fn plans_documented_resume_and_rejects_unsupported_fork() {
    let adapter = AntigravityAdapter::with_root("/unused");
    let session = SessionRef::new(Provider::Antigravity, SESSION_ID);
    let resume = adapter
        .launch_plan(
            &session,
            &LaunchTarget {
                cwd: Some("/synthetic/project".into()),
                fork: false,
                prompt: Some("continue here".to_owned()),
            },
        )
        .expect("resume plan");
    assert_eq!(resume.program, "agy");
    assert_eq!(
        resume.args,
        [
            "--conversation",
            SESSION_ID,
            "--prompt-interactive",
            "continue here"
        ]
    );

    let error = adapter
        .launch_plan(
            &session,
            &LaunchTarget {
                fork: true,
                ..LaunchTarget::default()
            },
        )
        .expect_err("fork must fail closed");
    assert!(
        error
            .to_string()
            .contains("no documented native conversation fork")
    );
}

#[test]
fn a_torn_tail_preserves_confirmed_turns_for_handoff_and_rejects_search_replacement() {
    let temporary = tempdir().unwrap();
    let root = temporary.path();
    summary_database(root, root);
    let mut bytes = transcript(root);
    let adapter = AntigravityAdapter::with_root(root);
    let native = adapter.list_sessions(None).unwrap().pop().unwrap();
    let path = native.source_path.as_deref().unwrap();
    let intact = adapter.read_session(&native.session).unwrap();
    bytes.extend_from_slice(b"\n{\"type\":\"PLANNER_RESPONSE\",\"content\":\"unconfirmed");
    for padding in ["", "\n", "\n\n", "\r\n \t\r\n"] {
        fs::write(path, [bytes.as_slice(), padding.as_bytes()].concat()).unwrap();
        for budget in [0, u64::MAX] {
            let failed = adapter.index_search_document(
                &native.session,
                Some(path),
                budget,
                Some(omnis_adapters::PriorSearchDocument::RequireCompleteTail(0)),
            );
            assert!(
                failed
                    .err()
                    .expect("torn search read")
                    .is::<omnis_adapters::IncompleteTranscript>()
            );
        }
        let snapshot = adapter.read_session(&native.session).unwrap();
        let (notice, confirmed) = snapshot.events.split_last().unwrap();
        assert_eq!(confirmed, intact.events);
        assert!(omnis_core::reports_omitted_events(notice));
        assert_eq!(notice.payload["omitted_events"], 1);
    }
}

#[test]
fn skipped_final_record_contributes_one_omission_and_search_checks_its_contents() {
    let temporary = tempdir().unwrap();
    let root = temporary.path();
    summary_database(root, root);
    let mut bytes = transcript(root);
    bytes.extend_from_slice(b"\n{\"type\":\"USER_INPUT\",\"content\":\"");
    bytes.resize(bytes.len() + 16 * 1024 * 1024, b'x');
    let adapter = AntigravityAdapter::with_root(root);
    let session = SessionRef::new(Provider::Antigravity, SESSION_ID);
    let native = adapter.list_sessions(None).unwrap().pop().unwrap();
    let path = native.source_path.as_deref().unwrap();
    for (suffix, valid) in [
        (b"\"}".as_slice(), true),
        (b"\"}\n\n", true),
        (b"\n\n", false),
    ] {
        fs::write(path, [&bytes[..], suffix].concat()).unwrap();
        let snapshot = adapter.read_session(&session).unwrap();
        assert_eq!(
            snapshot
                .events
                .iter()
                .filter_map(|event| event.payload["omitted_events"].as_u64())
                .sum::<u64>(),
            1
        );
        assert!(
            snapshot
                .events
                .iter()
                .any(|e| e.kind == EventKind::MessageAssistant)
        );
        for budget in [0, u64::MAX] {
            let result = adapter.index_search_document(
                &session,
                Some(path),
                budget,
                Some(omnis_adapters::PriorSearchDocument::RequireCompleteTail(0)),
            );
            if valid {
                assert!(result.unwrap().document.text.contains("synthetic answer"));
            } else {
                assert!(
                    result
                        .err()
                        .expect("torn search read")
                        .is::<omnis_adapters::IncompleteTranscript>()
                );
            }
        }
    }
}
