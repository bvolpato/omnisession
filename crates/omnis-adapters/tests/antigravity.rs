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
