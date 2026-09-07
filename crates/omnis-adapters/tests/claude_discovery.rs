use std::{fs, path::Path};

use omnis_adapters::{ClaudeAdapter, ProviderAdapter};
use omnis_ir::{Provider, SessionRef};
use serde_json::json;
use tempfile::TempDir;

const SESSION_ID: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";

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
    assert!(sessions[0].title.is_none());
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
