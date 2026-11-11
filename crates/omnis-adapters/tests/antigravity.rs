use std::{fs, io::Write, path::Path};

use omnis_adapters::{AntigravityAdapter, LaunchTarget, ProviderAdapter, SearchSourceObservation};
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

fn observed_revision(adapter: &AntigravityAdapter, id: &str) -> SearchSourceObservation {
    let listed = adapter
        .list_sessions(None)
        .unwrap()
        .into_iter()
        .find(|listed| listed.session.id == id)
        .unwrap();
    let observation = adapter
        .observe_search_source(&listed.session, listed.source_path.as_deref())
        .unwrap()
        .unwrap();
    assert_eq!(listed.source_revision, Some(observation.revision));
    assert!(observation.is_current());
    observation
}

#[test]
fn zero_step_sessions_track_their_own_catalog_row() {
    const OTHER: &str = "22222222-2222-4222-8222-222222222222";
    let root = tempdir().unwrap();
    summary_database(root.path(), root.path());
    let catalog = Connection::open(root.path().join("conversation_summaries.db")).unwrap();
    catalog
        .execute("UPDATE conversation_summaries SET step_count=0", [])
        .unwrap();
    catalog
        .execute(
            "INSERT INTO conversation_summaries
             SELECT ?1, title, step_count, last_modified_time, workspace_uris
             FROM conversation_summaries",
            [OTHER],
        )
        .unwrap();
    let adapter = AntigravityAdapter::with_root(root.path());
    let first = observed_revision(&adapter, SESSION_ID);
    let other = observed_revision(&adapter, OTHER);
    assert_ne!(first.revision.file, other.revision.file);

    catalog
        .execute(
            "UPDATE conversation_summaries SET title='Renamed' WHERE conversation_id=?1",
            [OTHER],
        )
        .unwrap();
    assert!(!other.is_current());
    assert_eq!(
        observed_revision(&adapter, SESSION_ID).revision,
        first.revision
    );
    assert_ne!(observed_revision(&adapter, OTHER).revision, other.revision);
}

#[test]
fn restoring_a_wal_then_a_preferred_transcript_changes_the_session_revision() {
    let backup = tempdir().unwrap();
    let database = backup.path().join("steps.db");
    let writer = Connection::open(&database).unwrap();
    writer
        .execute_batch(
            "CREATE TABLE steps(idx INTEGER PRIMARY KEY, step_payload BLOB);
             INSERT INTO steps VALUES(0, x'0801');
             PRAGMA journal_mode=WAL;
             PRAGMA wal_autocheckpoint=0;
             INSERT INTO steps VALUES(1, x'0802');",
        )
        .unwrap();
    let root = tempdir().unwrap();
    summary_database(root.path(), root.path());
    let conversations = root.path().join("conversations");
    fs::create_dir(&conversations).unwrap();
    fs::copy(&database, conversations.join(format!("{SESSION_ID}.db"))).unwrap();
    let adapter = AntigravityAdapter::with_root(root.path());
    let base = observed_revision(&adapter, SESSION_ID);
    let restored_wal = conversations.join(format!("{SESSION_ID}.db-wal"));
    fs::create_dir(&restored_wal).unwrap();
    assert_ne!(
        base.revision,
        observed_revision(&adapter, SESSION_ID).revision
    );
    fs::remove_dir(&restored_wal).unwrap();
    assert_eq!(
        base.revision,
        observed_revision(&adapter, SESSION_ID).revision
    );
    fs::copy(backup.path().join("steps.db-wal"), &restored_wal).unwrap();
    assert!(!base.is_current());
    let with_wal = observed_revision(&adapter, SESSION_ID);
    assert_eq!(base.revision.file, with_wal.revision.file);
    assert_ne!(base.revision.contents, with_wal.revision.contents);

    transcript(root.path());
    assert!(!with_wal.is_current());
    let before_append = observed_revision(&adapter, SESSION_ID);
    assert_ne!(with_wal.revision.file, before_append.revision.file);
    let path = root
        .path()
        .join("brain")
        .join(SESSION_ID)
        .join(".system_generated/logs/transcript_full.jsonl");
    let mut file = fs::OpenOptions::new().append(true).open(path).unwrap();
    writeln!(
        file,
        "\n{}",
        json!({"type":"USER_INPUT","content":"Next complete record"})
    )
    .unwrap();
    assert!(!before_append.is_current());
    let after_append = observed_revision(&adapter, SESSION_ID);
    assert_eq!(before_append.revision.file, after_append.revision.file);
    assert_ne!(
        before_append.revision.contents,
        after_append.revision.contents
    );

    let catalog = Connection::open(root.path().join("conversation_summaries.db")).unwrap();
    catalog
        .execute("UPDATE conversation_summaries SET step_count=5", [])
        .unwrap();
    let after_count = observed_revision(&adapter, SESSION_ID).revision;
    assert_ne!(after_append.revision.contents, after_count.contents);
    catalog
        .execute("UPDATE conversation_summaries SET workspace_uris='[]'", [])
        .unwrap();
    assert_ne!(
        after_count.contents,
        observed_revision(&adapter, SESSION_ID).revision.contents
    );
}

#[test]
fn removing_a_conflicting_source_changes_an_empty_transcripts_revision() {
    let root = tempdir().unwrap();
    summary_database(root.path(), root.path());
    let transcript_bytes = transcript(root.path());
    let catalog = Connection::open(root.path().join("conversation_summaries.db")).unwrap();
    catalog
        .execute("UPDATE conversation_summaries SET step_count = 0", [])
        .unwrap();
    let adapter = AntigravityAdapter::with_root(root.path());
    let listed = adapter.list_sessions(None).unwrap().pop().unwrap();
    let preferred = listed.source_path.unwrap();
    fs::write(&preferred, []).unwrap();
    let clear = observed_revision(&adapter, SESSION_ID).revision;
    for conflict in [
        preferred.with_file_name("transcript.jsonl"),
        root.path().join("conversation_summaries.db-journal"),
    ] {
        fs::write(&conflict, &transcript_bytes).unwrap();
        let blocked = observed_revision(&adapter, SESSION_ID);
        assert!(adapter.read_session(&listed.session).is_err());
        assert_ne!(clear, blocked.revision);
        fs::remove_file(conflict).unwrap();
        assert!(!blocked.is_current());
        assert_eq!(clear, observed_revision(&adapter, SESSION_ID).revision);
        assert!(
            adapter
                .read_session(&listed.session)
                .unwrap()
                .events
                .is_empty()
        );
    }
}

#[test]
fn only_a_verified_zero_can_stand_in_for_missing_history() {
    let root = tempdir().unwrap();
    summary_database(root.path(), root.path());
    let path = root
        .path()
        .join("conversation_summaries.db")
        .canonicalize()
        .unwrap();
    let catalog = Connection::open(&path).unwrap();
    let adapter = AntigravityAdapter::with_root(root.path());
    let session = SessionRef::new(Provider::Antigravity, SESSION_ID);
    let set_count = |count| {
        catalog
            .execute("UPDATE conversation_summaries SET step_count = ?1", [count])
            .unwrap();
    };
    for count in [1, -1] {
        set_count(count);
        assert!(
            adapter.list_sessions(None).unwrap()[0]
                .source_path
                .is_none()
        );
        assert!(adapter.read_session(&session).is_err());
    }
    set_count(0);
    assert_eq!(
        adapter.list_sessions(None).unwrap()[0]
            .source_path
            .as_deref(),
        Some(path.as_path())
    );
    let empty = adapter
        .index_for_search(&session, Some(&path), 1)
        .unwrap()
        .0;
    assert!(empty.source_complete && empty.snapshot.events.is_empty());
    assert!(
        adapter
            .search_source_is_summary(&session, Some(&path))
            .unwrap()
    );

    transcript(root.path());
    assert!(!adapter.search_source_is_summary(&session, None).unwrap());
    assert!(adapter.search_source_files(&session, Some(&path)).is_err());
    assert!(!adapter.read_session(&session).unwrap().events.is_empty());
}

#[test]
fn a_damaged_wal_commit_cannot_restore_an_older_zero_step_summary() {
    let source = tempdir().unwrap();
    summary_database(source.path(), source.path());
    let database = source.path().join("conversation_summaries.db");
    let wal_path = source.path().join("conversation_summaries.db-wal");
    let writer = Connection::open(&database).unwrap();
    writer
        .execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;
             UPDATE conversation_summaries SET step_count=0;",
        )
        .unwrap();
    let zero = fs::read(&wal_path).unwrap();
    writer
        .execute("UPDATE conversation_summaries SET step_count=1", [])
        .unwrap();
    let mut damaged = fs::read(&wal_path).unwrap();
    assert!(damaged.len() > zero.len());
    *damaged.last_mut().unwrap() ^= 1;
    let short = &damaged[..damaged.len() - 1];

    for (wal, verified) in [(zero.as_slice(), true), (short, false), (&damaged, false)] {
        assert_catalog_only_source(&database, wal, verified);
    }
}

#[test]
fn an_older_wal_frame_cannot_hide_a_later_current_frame() {
    let source = tempdir().unwrap();
    summary_database(source.path(), source.path());
    let database = source.path().join("conversation_summaries.db");
    let wal = source.path().join("conversation_summaries.db-wal");
    let writer = Connection::open(&database).unwrap();
    writer
        .execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;
         UPDATE conversation_summaries SET step_count=0;
         UPDATE conversation_summaries SET title='old a';
         UPDATE conversation_summaries SET title='old b';",
        )
        .unwrap();
    let old = fs::read(&wal).unwrap();
    let page = u32::from_be_bytes(old[8..12].try_into().unwrap()) as usize;
    writer
        .execute_batch(
            "PRAGMA wal_checkpoint(RESTART);
         UPDATE conversation_summaries SET title='current';",
        )
        .unwrap();
    let reset = fs::read(&wal).unwrap();
    assert_eq!(reset.len(), old.len());
    assert_ne!(&reset[16..24], &old[16..24]);
    assert_catalog_only_source(&database, &reset, true);

    writer
        .execute("UPDATE conversation_summaries SET step_count=1", [])
        .unwrap();
    let mut restored = fs::read(&wal).unwrap();
    let first = 32..32 + 24 + page;
    restored[first.clone()].copy_from_slice(&old[first]);
    assert_catalog_only_source(&database, &restored, false);
}

fn assert_catalog_only_source(database: &Path, wal: &[u8], verified: bool) {
    let root = tempdir().unwrap();
    fs::copy(database, root.path().join("conversation_summaries.db")).unwrap();
    fs::write(root.path().join("conversation_summaries.db-wal"), wal).unwrap();
    let adapter = AntigravityAdapter::with_root(root.path());
    let listed = adapter.list_sessions(None).unwrap();
    assert_eq!(listed[0].source_path.is_some(), verified);
    assert_eq!(listed[0].source_revision.is_some(), verified);
    let session = SessionRef::new(Provider::Antigravity, SESSION_ID);
    assert_eq!(
        adapter.observe_search_source(&session, None).is_ok(),
        verified
    );
    let read = adapter.read_session(&session);
    if verified {
        assert!(read.unwrap().events.is_empty());
    } else {
        assert!(read.is_err());
    }
}

#[test]
fn empty_or_malformed_preferred_logs_cannot_hide_another_history_file() {
    let root = tempdir().unwrap();
    summary_database(root.path(), root.path());
    let bytes = transcript(root.path());
    let logs = root
        .path()
        .join("brain")
        .join(SESSION_ID)
        .join(".system_generated/logs");
    let preferred = logs.join("transcript_full.jsonl");
    fs::write(logs.join("transcript.jsonl"), bytes).unwrap();
    let catalog = Connection::open(root.path().join("conversation_summaries.db")).unwrap();
    catalog
        .execute("UPDATE conversation_summaries SET step_count = 0", [])
        .unwrap();
    let session = SessionRef::new(Provider::Antigravity, SESSION_ID);
    let adapter = AntigravityAdapter::with_root(root.path());
    for unverified in ["", "broken JSON"] {
        fs::write(&preferred, unverified).unwrap();
        assert!(adapter.read_session(&session).is_err());
        assert!(adapter.preview_session(&session).is_err());
    }
    fs::remove_file(preferred).unwrap();
    assert!(!adapter.read_session(&session).unwrap().events.is_empty());
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
