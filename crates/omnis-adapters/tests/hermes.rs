use std::path::Path;

use omnis_adapters::{HermesAdapter, LaunchTarget, ProviderAdapter};
use omnis_ir::{EventKind, Provider, ReplayPolicy, SessionRef};
use rusqlite::{Connection, params};

const SESSION_ID: &str = "20260731_120000_fixture";

#[allow(clippy::too_many_lines)]
fn fixture(root: &Path) {
    std::fs::create_dir_all(root).expect("Hermes root");
    let connection = Connection::open(root.join("state.db")).expect("Hermes fixture database");
    connection
        .execute_batch(
            "CREATE TABLE schema_version (version INTEGER NOT NULL);
             INSERT INTO schema_version VALUES (23);
             CREATE TABLE sessions (
               id TEXT PRIMARY KEY, source TEXT NOT NULL, title TEXT, cwd TEXT,
               git_branch TEXT, started_at REAL NOT NULL, ended_at REAL,
               message_count INTEGER DEFAULT 0, model TEXT, model_config TEXT,
               parent_session_id TEXT,
               input_tokens INTEGER DEFAULT 0, output_tokens INTEGER DEFAULT 0,
               cache_read_tokens INTEGER DEFAULT 0, cache_write_tokens INTEGER DEFAULT 0,
               reasoning_tokens INTEGER DEFAULT 0, archived INTEGER DEFAULT 0
             );
             CREATE TABLE messages (
               id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL,
               role TEXT NOT NULL, content TEXT, tool_call_id TEXT, tool_calls TEXT,
               tool_name TEXT, effect_disposition TEXT, timestamp REAL NOT NULL,
               finish_reason TEXT, reasoning TEXT, reasoning_content TEXT,
               active INTEGER NOT NULL DEFAULT 1
             );",
        )
        .expect("Hermes fixture schema");
    connection
        .execute(
            "INSERT INTO sessions (
               id, source, title, cwd, git_branch, started_at, message_count, model,
               input_tokens, output_tokens, cache_read_tokens, reasoning_tokens
             ) VALUES (?1, 'cli', NULL, '/workspace/demo', 'main', 100.0, 4,
                       'nous/hermes-4', 120, 40, 10, 5)",
            [SESSION_ID],
        )
        .expect("Hermes fixture session");
    let messages = [
        (
            "user",
            Some(r#"[{"type":"text","text":"fix the auth race"}]"#),
            None,
            None,
            None,
            None,
            101.0,
            None,
            None,
        ),
        (
            "assistant",
            Some("I will inspect the coordinator."),
            None,
            Some(
                r#"[{"id":"call-1","function":{"name":"read","arguments":"{\"path\":\"src/auth.rs\"}"}}]"#,
            ),
            None,
            None,
            102.0,
            Some("stop"),
            Some("private reasoning must not leave Hermes"),
        ),
        (
            "tool",
            Some("auth tests passed"),
            Some("call-1"),
            None,
            Some("read"),
            Some("success"),
            103.0,
            None,
            None,
        ),
        (
            "assistant",
            Some("Race fixed and tests pass."),
            None,
            None,
            None,
            None,
            104.0,
            Some("stop"),
            None,
        ),
    ];
    for (role, content, call_id, calls, tool_name, disposition, timestamp, finish, reasoning) in
        messages
    {
        connection
            .execute(
                "INSERT INTO messages (
                   session_id, role, content, tool_call_id, tool_calls, tool_name,
                   effect_disposition, timestamp, finish_reason, reasoning, active
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 1)",
                params![
                    SESSION_ID,
                    role,
                    content,
                    call_id,
                    calls,
                    tool_name,
                    disposition,
                    timestamp,
                    finish,
                    reasoning
                ],
            )
            .expect("Hermes fixture message");
    }
}

#[test]
fn hermes_reads_documented_sqlite_without_reasoning_or_mutation() {
    let temporary = tempfile::tempdir().expect("temporary Hermes root");
    fixture(temporary.path());
    let database = temporary.path().join("state.db");
    let before = std::fs::read(&database).expect("fixture before read");
    let adapter = HermesAdapter::with_root(temporary.path());

    let sessions = adapter
        .list_sessions(Some(Path::new("/workspace/demo")))
        .expect("Hermes discovery");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].session.id, SESSION_ID);
    assert_eq!(sessions[0].title.as_deref(), Some("fix the auth race"));
    assert_eq!(sessions[0].git_branch.as_deref(), Some("main"));
    assert_eq!(sessions[0].event_count, 4);

    let snapshot = adapter
        .read_session(&SessionRef::new(Provider::Hermes, SESSION_ID))
        .expect("Hermes read");
    assert_eq!(before, std::fs::read(database).expect("fixture after read"));
    assert!(snapshot.events.iter().any(|event| {
        event.kind == EventKind::ToolCalled && event.replay_policy == ReplayPolicy::HistoricalOnly
    }));
    assert!(snapshot.events.iter().any(|event| {
        event.kind == EventKind::ToolCompleted
            && event.replay_policy == ReplayPolicy::HistoricalOnly
    }));
    let rendered = serde_json::to_string(&snapshot).expect("serialize Hermes snapshot");
    assert!(rendered.contains("fix the auth race"));
    assert!(rendered.contains("auth tests passed"));
    assert!(rendered.contains("nous/hermes-4"));
    assert!(!rendered.contains("private reasoning must not leave Hermes"));
}

#[test]
fn hermes_decodes_structured_content_without_leaking_its_marker_or_images() {
    const ENCODED_ID: &str = "20260801_120000_encoded";
    // Hermes stores list content as `\x00json:` plus JSON (`SessionDB._encode_content`).
    const IMAGE: &str =
        r#"{"type":"image_url","image_url":{"url":"data:image/png;base64,iVBORw0KGgo="}}"#;
    let temporary = tempfile::tempdir().expect("temporary Hermes root");
    fixture(temporary.path());
    let connection =
        Connection::open(temporary.path().join("state.db")).expect("Hermes fixture database");
    connection
        .execute(
            "INSERT INTO sessions (id, source, cwd, started_at, message_count)
             VALUES (?1, 'cli', '/workspace/demo', 200.0, 4)",
            [ENCODED_ID],
        )
        .expect("encoded Hermes session");
    for (role, content, tool_call_id, timestamp) in [
        (
            "user",
            format!("\u{0}json:[{{\"type\":\"text\",\"text\":\"describe this\"}},{IMAGE}]"),
            None,
            201.0,
        ),
        ("user", format!("\u{0}json:[{IMAGE}]"), None, 202.0),
        (
            "tool",
            "\u{0}json:[{\"type\":\"text\",\"text\":\"vision result\"}]".to_owned(),
            Some("call-2"),
            203.0,
        ),
        ("assistant", "A screenshot".to_owned(), None, 204.0),
    ] {
        connection
            .execute(
                "INSERT INTO messages (session_id, role, content, tool_call_id, timestamp, active)
                 VALUES (?1, ?2, ?3, ?4, ?5, 1)",
                params![ENCODED_ID, role, content, tool_call_id, timestamp],
            )
            .expect("encoded Hermes message");
    }
    let adapter = HermesAdapter::with_root(temporary.path());

    let sessions = adapter.list_sessions(None).expect("Hermes discovery");
    let listed = sessions
        .iter()
        .find(|session| session.session.id == ENCODED_ID)
        .expect("encoded session listed");
    assert_eq!(listed.title.as_deref(), Some("describe this"));
    let snapshot = adapter
        .read_session(&SessionRef::new(Provider::Hermes, ENCODED_ID))
        .expect("encoded Hermes read");
    assert_eq!(
        omnis_core::import_conversation(&snapshot)
            .messages
            .iter()
            .map(|message| message.text.as_str())
            .collect::<Vec<_>>(),
        ["describe this", "[1 image omitted]", "A screenshot"]
    );
    let tool = snapshot
        .events
        .iter()
        .find(|event| event.kind == EventKind::ToolCompleted)
        .expect("encoded tool row");
    assert_eq!(tool.payload["output"][0]["text"], "vision result");
    let rendered = serde_json::to_string(&snapshot).expect("serialize Hermes snapshot");
    assert!(!rendered.contains("\\u0000json"), "{rendered}");
    assert!(!rendered.contains("iVBORw0KGgo"), "{rendered}");
}

#[test]
fn hermes_launches_exact_native_resume_and_materializes_forks_elsewhere() {
    let adapter = HermesAdapter::with_root("/unused");
    let session = SessionRef::new(Provider::Hermes, SESSION_ID);
    let resume = adapter
        .launch_plan(
            &session,
            &LaunchTarget {
                cwd: Some("/workspace/demo".into()),
                fork: false,
                prompt: None,
            },
        )
        .expect("Hermes resume plan");
    assert_eq!(resume.program, "hermes");
    assert_eq!(resume.args, ["--resume", SESSION_ID]);

    assert!(
        adapter
            .launch_plan(
                &session,
                &LaunchTarget {
                    fork: true,
                    ..LaunchTarget::default()
                },
            )
            .is_err()
    );
}
