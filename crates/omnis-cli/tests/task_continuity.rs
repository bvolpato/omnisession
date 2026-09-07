use std::{fs, path::Path, process::Command};

use omnis_ir::PortableBundle;
use serde_json::{Value, json};

const SESSION_ID: &str = "11111111-1111-4111-8111-111111111111";

fn command(root: &Path, workspace: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_omni"));
    command
        .current_dir(workspace)
        .env("HOME", root.join("home"))
        .env("USERPROFILE", root.join("home"))
        .env("OMNISESSION_HOME", root.join("state"))
        .env("CLAUDE_CONFIG_DIR", root.join("claude"))
        .env("CODEX_HOME", root.join("codex"))
        .env("OMNI_CLAUDE_BIN", root.join("missing-claude"))
        .env("OMNI_CODEX_BIN", root.join("missing-codex"))
        .env("OMNI_NO_UPDATE_CHECK", "1");
    command
}

fn successful_json(command: &mut Command) -> Value {
    let output = command.output().expect("run synthetic CLI");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("CLI JSON")
}

#[test]
fn same_provider_switch_resumes_bound_session_without_forking() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let workspace = root.join("workspace");
    let projects = root.join("claude/projects/synthetic");
    fs::create_dir_all(&workspace).unwrap();
    fs::create_dir_all(&projects).unwrap();
    fs::write(
        projects.join(format!("{SESSION_ID}.jsonl")),
        json!({"type":"user", "sessionId":SESSION_ID, "uuid":"synthetic-user", "cwd":workspace, "timestamp":"2026-01-01T00:00:00Z", "message":{"role":"user","content":"Synthetic continuity request"}}).to_string(),
    ).unwrap();
    // Keep this regression independent of transcript-only Claude discovery.
    fs::write(
        root.join("claude/history.jsonl"),
        json!({"sessionId":SESSION_ID,"project":workspace,"timestamp":1_767_225_600_000_i64})
            .to_string(),
    )
    .unwrap();
    successful_json(command(root, &workspace).args([
        "--json",
        "task",
        "start",
        "continuity",
        "--from",
        &format!("claude:{SESSION_ID}"),
    ]));
    let switched = successful_json(command(root, &workspace).args([
        "--json",
        "switch",
        "claude",
        "--dry-run",
    ]));
    let args = switched["launch"]["args"]
        .as_array()
        .expect("launch arguments");
    assert!(args.iter().any(|arg| arg == SESSION_ID));
    assert!(
        !args.iter().any(|arg| arg == "--fork-session"),
        "switch must keep the exact task-bound session: {switched}"
    );
}

#[test]
fn native_export_carries_repository_identity_for_relocated_continuation() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let workspace = root.join("source");
    let relocated = root.join("relocated");
    fs::create_dir_all(&workspace).unwrap();
    for args in [
        vec!["init", "--quiet"],
        vec![
            "remote",
            "add",
            "origin",
            "https://example.invalid/acme/continuity.git",
        ],
    ] {
        assert!(
            Command::new("git")
                .current_dir(&workspace)
                .args(args)
                .status()
                .unwrap()
                .success()
        );
    }
    let sessions = root.join("codex/sessions/2026/01/01");
    fs::create_dir_all(&sessions).unwrap();
    fs::write(sessions.join(format!("rollout-2026-01-01T00-00-00-{SESSION_ID}.jsonl")), format!("{}\n{}\n",
        json!({"type":"session_meta","timestamp":"2026-01-01T00:00:00Z","payload":{"id":SESSION_ID,"cwd":workspace,"git":{"branch":"main"}}}),
        json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Synthetic portable request"}]}})
    )).unwrap();
    let output = root.join("export.json");
    successful_json(command(root, &workspace).args([
        "--json",
        "export",
        &format!("codex:{SESSION_ID}"),
        "--output",
        output.to_str().unwrap(),
    ]));
    let bundle: PortableBundle = serde_json::from_slice(&fs::read(&output).unwrap()).unwrap();
    assert!(
        bundle
            .snapshot
            .workspace
            .git
            .remote_fingerprint
            .as_ref()
            .is_some_and(|value| !value.is_empty()),
        "native export must carry relocation identity"
    );
    // Do not replace historical branch/head with the current repository state.
    assert_eq!(
        bundle.snapshot.workspace.git.branch.as_deref(),
        Some("main")
    );
    fs::rename(&workspace, &relocated).unwrap();
    successful_json(command(root, &relocated).args(["--json", "import", output.to_str().unwrap()]));
    successful_json(command(root, &relocated).args([
        "--json",
        "task",
        "start",
        "relocated",
        "--from",
        &format!("imported:{}", bundle.manifest.bundle_id),
    ]));
    successful_json(command(root, &relocated).args(["--json", "switch", "codex", "--dry-run"]));
}
