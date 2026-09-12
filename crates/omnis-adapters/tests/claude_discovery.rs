use std::{collections::HashMap, fmt::Write, fs, path::Path};

use omnis_adapters::{ClaudeAdapter, ProviderAdapter};
use omnis_ir::{EventKind, Provider, SessionRef};
use serde_json::{Value, json};
use tempfile::TempDir;

const SESSION_ID: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
const OTHER_ID: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
const CUSTOM_ID: &str = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";

fn write_jsonl(path: &Path, records: &[Value]) {
    let mut document = String::new();
    for record in records {
        writeln!(document, "{record}").expect("JSON line");
    }
    fs::write(path, document).expect("synthetic JSONL");
}

fn user_request(cwd: &str) -> Value {
    json!({
        "type": "user",
        "cwd": cwd,
        "timestamp": "2026-01-01T00:00:00Z",
        "message": {"role": "user", "content": "synthetic request"}
    })
}

fn padded_assistant(bytes: usize) -> Value {
    json!({
        "type": "assistant",
        "message": {"role": "assistant", "content": [{"type": "text", "text": "p".repeat(bytes)}]}
    })
}

fn ai_title(title: &str) -> Value {
    json!({"type": "ai-title", "aiTitle": title, "sessionId": SESSION_ID})
}

fn history_record(id: &str, project: &str, display: &str, timestamp: i64) -> Value {
    json!({
        "display": display,
        "pastedContents": {},
        "project": project,
        "sessionId": id,
        "timestamp": timestamp
    })
}

fn discovered_titles(adapter: &ClaudeAdapter) -> HashMap<String, Option<String>> {
    adapter
        .list_sessions(None)
        .expect("Claude discovery")
        .into_iter()
        .map(|session| (session.session.id, session.title))
        .collect()
}

#[test]
fn history_with_an_oversized_record_keeps_workspace_mapping_and_notes_it() {
    let temporary = TempDir::new().expect("temporary directory");
    let projects = temporary.path().join("projects");
    fs::create_dir_all(&projects).expect("project directory");
    write_jsonl(
        &projects.join(format!("{SESSION_ID}.jsonl")),
        &[json!({"type": "user", "message": {"role": "user", "content": "synthetic request"}})],
    );
    write_jsonl(
        &temporary.path().join("history.jsonl"),
        &[
            history_record(
                SESSION_ID,
                "/workspace/indexed",
                "synthetic request",
                1_767_225_600_000,
            ),
            history_record(
                OTHER_ID,
                "/workspace/other",
                &"x".repeat(2 * 1024 * 1024),
                1_767_225_601_000,
            ),
        ],
    );

    let adapter = ClaudeAdapter::with_root(&projects);
    let sessions = adapter
        .list_sessions(Some(Path::new("/workspace/indexed")))
        .expect("indexed discovery");

    assert_eq!(sessions.len(), 1);
    let notes = adapter.discovery_notes();
    assert!(
        notes
            .iter()
            .any(|note| note.contains("history.jsonl") && note.contains("1 oversized")),
        "{notes:?}"
    );
}

#[test]
fn history_beyond_the_strict_record_limit_keeps_the_latest_workspace() {
    let temporary = TempDir::new().expect("temporary directory");
    let projects = temporary.path().join("projects");
    fs::create_dir_all(&projects).expect("project directory");
    write_jsonl(
        &projects.join(format!("{SESSION_ID}.jsonl")),
        &[json!({"type": "user", "message": {"role": "user", "content": "synthetic request"}})],
    );
    let mut history = String::new();
    for index in 0..100_000 {
        writeln!(
            history,
            "{{\"display\":\"\",\"project\":\"/workspace/stale\",\"sessionId\":\"{SESSION_ID}\",\"timestamp\":{}}}",
            1_767_225_600_000_i64 + index
        )
        .expect("history line");
    }
    writeln!(
        history,
        "{}",
        history_record(SESSION_ID, "/workspace/latest", "", 1_767_225_700_000)
    )
    .expect("latest history line");
    fs::write(temporary.path().join("history.jsonl"), history).expect("synthetic history");

    let adapter = ClaudeAdapter::with_root(&projects);
    let sessions = adapter.list_sessions(None).expect("Claude discovery");

    assert_eq!(sessions.len(), 1);
    assert_eq!(
        sessions[0].project_path.as_deref(),
        Some(Path::new("/workspace/latest"))
    );
    assert!(
        adapter.discovery_notes().is_empty(),
        "{:?}",
        adapter.discovery_notes()
    );
}

#[test]
fn discovery_titles_prefer_custom_titles_then_the_latest_generated_title() {
    let temporary = TempDir::new().expect("temporary directory");
    let projects = temporary.path().join("projects");
    let project = projects.join("encoded-project");
    fs::create_dir_all(&project).expect("project directory");
    fs::write(
        project.join(format!("{SESSION_ID}.jsonl")),
        include_bytes!("fixtures/claude-session.jsonl"),
    )
    .expect("legacy summary transcript");
    write_jsonl(
        &project.join(format!("{OTHER_ID}.jsonl")),
        &[
            user_request("/workspace/demo"),
            ai_title("Early synthetic title"),
            padded_assistant(512 * 1024),
            ai_title("Latest synthetic title"),
            padded_assistant(1024),
        ],
    );
    write_jsonl(
        &project.join(format!("{CUSTOM_ID}.jsonl")),
        &[
            json!({"type": "summary", "summary": "Legacy synthetic summary", "leafUuid": "11111111-1111-4111-8111-111111111111"}),
            user_request("/workspace/demo"),
            padded_assistant(512 * 1024),
            json!({"type": "custom-title", "customTitle": "Custom synthetic title", "sessionId": CUSTOM_ID}),
            ai_title("Generated after rename"),
        ],
    );

    let adapter = ClaudeAdapter::with_root(&projects);
    let titles = discovered_titles(&adapter);

    for (id, expected) in [
        (SESSION_ID, "Synthetic fixture"),
        (OTHER_ID, "Latest synthetic title"),
        (CUSTOM_ID, "Custom synthetic title"),
    ] {
        assert_eq!(
            titles[id].as_deref(),
            Some(expected),
            "listing title of {id}"
        );
        let snapshot = adapter
            .read_session(&SessionRef::new(Provider::Claude, id))
            .expect("Claude read");
        assert_eq!(
            snapshot.title.as_deref(),
            Some(expected),
            "read title of {id}"
        );
    }
}

#[test]
fn discovery_keeps_titles_read_while_searching_for_the_workspace() {
    let temporary = TempDir::new().expect("temporary directory");
    let projects = temporary.path().join("projects");
    fs::create_dir_all(&projects).expect("project directory");
    // Without history, discovery scans past this title to find `cwd`. The tail sample starts later.
    write_jsonl(
        &projects.join(format!("{SESSION_ID}.jsonl")),
        &[
            json!({"type": "file-history-snapshot", "snapshot": {"padding": "s".repeat(20 * 1024)}}),
            ai_title("Title before the workspace"),
            user_request("/workspace/demo"),
            padded_assistant(1024),
        ],
    );

    let adapter = ClaudeAdapter::with_root(&projects);
    let titles = discovered_titles(&adapter);

    assert_eq!(
        titles[SESSION_ID].as_deref(),
        Some("Title before the workspace")
    );
    let snapshot = adapter
        .read_session(&SessionRef::new(Provider::Claude, SESSION_ID))
        .expect("Claude read");
    assert_eq!(
        snapshot.title.as_deref(),
        Some("Title before the workspace")
    );
}

#[test]
fn discovery_title_falls_back_to_the_first_history_prompt() {
    let temporary = TempDir::new().expect("temporary directory");
    let projects = temporary.path().join("projects");
    fs::create_dir_all(&projects).expect("project directory");
    // Discovery samples the head and tail only, so a title record in the middle stays unread.
    write_jsonl(
        &projects.join(format!("{SESSION_ID}.jsonl")),
        &[
            user_request("/workspace/demo"),
            padded_assistant(512 * 1024),
            ai_title("Buried synthetic title"),
            padded_assistant(512 * 1024),
        ],
    );
    write_jsonl(
        &projects.join(format!("{OTHER_ID}.jsonl")),
        &[user_request("/workspace/demo")],
    );
    write_jsonl(
        &temporary.path().join("history.jsonl"),
        &[
            history_record(
                SESSION_ID,
                "/workspace/demo",
                "  First synthetic\n  prompt  ",
                1_767_225_600_000,
            ),
            history_record(
                SESSION_ID,
                "/workspace/demo",
                "Later synthetic prompt",
                1_767_225_601_000,
            ),
            history_record(OTHER_ID, "/workspace/demo", "   ", 1_767_225_602_000),
        ],
    );

    let titles = discovered_titles(&ClaudeAdapter::with_root(&projects));

    assert_eq!(
        titles[SESSION_ID].as_deref(),
        Some("First synthetic prompt")
    );
    assert_eq!(titles[OTHER_ID], None);
}

#[test]
fn transcripts_over_the_strict_file_limit_still_read() {
    let temporary = TempDir::new().expect("temporary directory");
    let projects = temporary.path().join("projects");
    let project = projects.join("encoded-project");
    fs::create_dir_all(&project).expect("project directory");
    let padding = "x".repeat(11 * 1024 * 1024);
    let mut transcript = json!({
        "type": "user",
        "uuid": "11111111-1111-4111-8111-111111111111",
        "sessionId": SESSION_ID,
        "timestamp": "2026-01-01T00:00:00Z",
        "cwd": "/workspace/demo",
        "message": {"role": "user", "content": "Synthetic question"}
    })
    .to_string();
    transcript.push('\n');
    for index in 0..3 {
        transcript.push_str(
            &json!({
                "type": "assistant",
                "uuid": format!("2222222{index}-2222-4222-8222-222222222222"),
                "sessionId": SESSION_ID,
                "timestamp": "2026-01-01T00:00:01Z",
                "cwd": "/workspace/demo",
                "message": {"role": "assistant", "content": [{"type": "text", "text": &padding}]}
            })
            .to_string(),
        );
        transcript.push('\n');
    }
    fs::write(project.join(format!("{SESSION_ID}.jsonl")), transcript)
        .expect("large synthetic transcript");

    let snapshot = ClaudeAdapter::with_root(&projects)
        .read_session(&SessionRef::new(Provider::Claude, SESSION_ID))
        .expect("Claude transcript over 32 MiB reads");

    assert!(
        snapshot
            .events
            .iter()
            .any(|event| event.kind == EventKind::MessageUser)
    );
}

#[test]
fn transcript_without_history_is_discovered_with_its_workspace() {
    let temporary = TempDir::new().expect("temporary directory");
    let projects = temporary.path().join("projects");
    let project = projects.join("encoded-project");
    fs::create_dir_all(&project).expect("project directory");
    let source = project.join(format!("{SESSION_ID}.jsonl"));
    let transcript = include_bytes!("fixtures/claude-session.jsonl");
    fs::write(&source, transcript).expect("synthetic transcript");

    let adapter = ClaudeAdapter::with_root(&projects);
    let sessions = adapter
        .list_sessions(Some(Path::new("/workspace/demo")))
        .expect("Claude discovery without history");

    assert_eq!(sessions.len(), 1);
    assert_eq!(
        sessions[0].session,
        SessionRef::new(Provider::Claude, SESSION_ID)
    );
    assert_eq!(
        sessions[0].project_path.as_deref(),
        Some(Path::new("/workspace/demo"))
    );
    assert_eq!(sessions[0].git_branch.as_deref(), Some("fixture"));
    assert_eq!(
        sessions[0].created_at,
        Some("2026-01-01T00:00:00Z".parse().unwrap())
    );
    assert!(sessions[0].updated_at.is_some());
    assert_eq!(sessions[0].title.as_deref(), Some("Synthetic fixture"));
    assert_eq!(sessions[0].event_count, 0);
    let snapshot = adapter
        .read_session(&sessions[0].session)
        .expect("read discovered session");
    assert_eq!(snapshot.workspace.current_dir, Path::new("/workspace/demo"));
    assert!(!snapshot.events.is_empty());
    assert_eq!(fs::read(source).expect("unchanged transcript"), transcript);
    assert!(!temporary.path().join("history.jsonl").exists());
}

#[test]
fn history_workspace_stays_authoritative_for_discovery() {
    let temporary = TempDir::new().expect("temporary directory");
    let projects = temporary.path().join("projects");
    fs::create_dir_all(&projects).expect("project directory");
    fs::write(
        projects.join(format!("{SESSION_ID}.jsonl")),
        include_bytes!("fixtures/claude-session.jsonl"),
    )
    .expect("synthetic transcript");
    fs::write(
        temporary.path().join("history.jsonl"),
        json!({"sessionId": SESSION_ID, "project": "/workspace/indexed", "timestamp": 1_767_225_600_000_i64}).to_string(),
    )
    .expect("synthetic history");

    let adapter = ClaudeAdapter::with_root(&projects);
    let sessions = adapter
        .list_sessions(Some(Path::new("/workspace/indexed")))
        .expect("indexed discovery");
    assert_eq!(sessions.len(), 1);
    assert_eq!(
        sessions[0].project_path.as_deref(),
        Some(Path::new("/workspace/indexed"))
    );
    assert!(
        adapter
            .list_sessions(Some(Path::new("/workspace/demo")))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn transcript_discovery_only_reads_a_bounded_metadata_prefix() {
    let temporary = TempDir::new().expect("temporary directory");
    let projects = temporary.path().join("projects");
    let workspace = temporary.path().join("workspace");
    fs::create_dir_all(&projects).expect("project directory");
    fs::create_dir_all(workspace.join("nested")).expect("workspace directory");
    let first = json!({"type": "user", "cwd": workspace, "timestamp": "2026-01-01T00:00:00Z", "message": {"role": "user", "content": "synthetic request"}});
    let transcript = format!("{first}\n{}\n", "x".repeat(2 * 1024 * 1024 + 1));
    fs::write(projects.join(format!("{SESSION_ID}.jsonl")), transcript)
        .expect("synthetic transcript");

    let sessions = ClaudeAdapter::with_root(projects)
        .list_sessions(Some(&workspace.join("nested").join("..")))
        .expect("bounded discovery");
    assert_eq!(sessions.len(), 1);
    assert_eq!(
        sessions[0].project_path.as_deref(),
        Some(workspace.as_path())
    );
}

#[test]
fn undiscovered_workspace_metadata_does_not_trigger_a_full_transcript_scan() {
    for prefix in ["{}\n".repeat(32), " ".repeat(2 * 1024 * 1024)] {
        let temporary = TempDir::new().expect("temporary directory");
        let projects = temporary.path().join("projects");
        fs::create_dir_all(&projects).expect("project directory");
        let transcript = format!("{prefix}\n{{\"cwd\":\"/workspace/too-late\"}}\n");
        fs::write(projects.join(format!("{SESSION_ID}.jsonl")), transcript)
            .expect("synthetic transcript");

        let sessions = ClaudeAdapter::with_root(projects)
            .list_sessions(None)
            .expect("bounded discovery without metadata");
        assert_eq!(sessions.len(), 1);
        assert!(sessions[0].project_path.is_none());
    }
}

#[cfg(unix)]
#[test]
fn cached_discovery_rejects_a_transcript_replaced_by_an_external_symlink() {
    use std::os::unix::fs::symlink;

    let temporary = TempDir::new().expect("temporary directory");
    let projects = temporary.path().join("projects");
    fs::create_dir_all(&projects).expect("project directory");
    let source = projects.join(format!("{SESSION_ID}.jsonl"));
    fs::write(&source, include_bytes!("fixtures/claude-session.jsonl"))
        .expect("synthetic transcript");
    let adapter = ClaudeAdapter::with_root(&projects);
    assert_eq!(
        adapter
            .list_sessions(None)
            .expect("initial discovery")
            .len(),
        1
    );

    let external = temporary.path().join("outside.jsonl");
    fs::write(&external, b"{\"cwd\":\"/workspace/outside\"}\n")
        .expect("external synthetic transcript");
    fs::remove_file(&source).expect("remove synthetic transcript");
    symlink(external, source).expect("replace with symlink");

    assert!(
        adapter
            .list_sessions(None)
            .expect("revalidated discovery")
            .is_empty()
    );
    assert!(
        adapter
            .read_session(&SessionRef::new(Provider::Claude, SESSION_ID))
            .is_err()
    );
}
