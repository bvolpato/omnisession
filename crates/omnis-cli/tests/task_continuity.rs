use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use omnis_ir::PortableBundle;
use serde_json::{Value, json};

#[path = "task_continuity/cursor_workspace.rs"]
mod cursor_workspace;

#[cfg(unix)]
#[path = "task_continuity/probe_signals.rs"]
mod probe_signals;

const SESSION_ID: &str = "11111111-1111-4111-8111-111111111111";
const REPLACEMENT_ID: &str = "22222222-2222-4222-8222-222222222222";

fn command(root: &Path, workspace: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_omni"));
    command
        .current_dir(workspace)
        .env("HOME", root.join("home"))
        .env("USERPROFILE", root.join("home"))
        .env("OMNISESSION_HOME", root.join("state"))
        .env("ANTIGRAVITY_CLI_HOME", root.join("antigravity-cli"))
        .env("ANTIGRAVITY_IDE_HOME", root.join("antigravity-ide"))
        .env("ANTIGRAVITY_DESKTOP_HOME", root.join("antigravity-desktop"))
        .env(
            "ANTIGRAVITY_IDE_USER_DATA_DIR",
            root.join("antigravity-ide-user-data"),
        )
        .env("CLAUDE_CONFIG_DIR", root.join("claude"))
        .env("CODEX_HOME", root.join("codex"))
        .env("OMNI_CLAUDE_BIN", root.join("missing-claude"))
        .env("OMNI_CODEX_BIN", root.join("missing-codex"))
        .env("OMNI_NO_UPDATE_CHECK", "1");
    command
}

fn write_synthetic_claude(root: &Path, workspace: &Path) {
    let projects = root.join("claude/projects/synthetic");
    fs::create_dir_all(workspace).unwrap();
    fs::create_dir_all(&projects).unwrap();
    write_claude_transcript(&projects, workspace, SESSION_ID);
    fs::write(
        root.join("claude/history.jsonl"),
        json!({"sessionId":SESSION_ID,"project":workspace,"timestamp":1_767_225_600_000_i64})
            .to_string(),
    )
    .unwrap();
}

fn write_claude_transcript(projects: &Path, workspace: &Path, id: &str) {
    fs::write(
        projects.join(format!("{id}.jsonl")),
        json!({"type":"user", "sessionId":id, "uuid":"synthetic-user", "cwd":workspace, "timestamp":"2026-01-01T00:00:00Z", "message":{"role":"user","content":"Synthetic continuity request"}}).to_string(),
    )
    .unwrap();
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
fn claude_fork_checks_the_workspace_without_scanning_the_git_tree() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let workspace = root.join("workspace");
    write_synthetic_claude(root, &workspace);
    assert!(
        Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&workspace)
            .status()
            .unwrap()
            .success()
    );
    let trace = root.join("git-trace.jsonl");
    let report = successful_json(
        command(root, &workspace)
            .env("GIT_TRACE2_EVENT", &trace)
            .args([
                "--json",
                "fork",
                &format!("claude:{SESSION_ID}"),
                "--in",
                "codex",
                "--dry-run",
            ]),
    );
    assert_eq!(report["fidelity"]["repository_matches"], true);
    let commands = fs::read_to_string(trace)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|event| event["event"] == "cmd_name")
        .map(|event| event["name"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert!(commands.iter().any(|cmd| cmd == "rev-parse"));
    assert!(
        commands
            .iter()
            .all(|cmd| !matches!(cmd.as_str(), "status" | "diff" | "ls-files")),
        "path-only workspace matching scanned the Git tree: {commands:?}"
    );
}

#[test]
fn same_provider_switch_resumes_bound_session_without_forking() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let workspace = root.join("workspace");
    write_synthetic_claude(root, &workspace);
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
fn bind_replaces_a_deleted_prior_session() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let workspace = root.join("workspace");
    write_synthetic_claude(root, &workspace);
    successful_json(command(root, &workspace).args([
        "--json",
        "task",
        "start",
        "continuity",
        "--from",
        &format!("claude:{SESSION_ID}"),
    ]));
    let projects = root.join("claude/projects/synthetic");
    fs::remove_file(projects.join(format!("{SESSION_ID}.jsonl"))).unwrap();
    write_claude_transcript(&projects, &workspace, REPLACEMENT_ID);
    let bound = successful_json(command(root, &workspace).args([
        "--json",
        "task",
        "bind",
        &format!("claude:{REPLACEMENT_ID}"),
    ]));
    assert_eq!(bound["session"]["id"], REPLACEMENT_ID);
}

#[cfg(unix)]
#[test]
fn switch_keeps_a_branch_rebound_during_the_provider_call() {
    use std::os::unix::fs::PermissionsExt;

    for (target, override_var) in [("claude", "OMNI_CLAUDE_BIN"), ("pi", "OMNI_PI_BIN")] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let workspace = root.join("workspace");
        write_synthetic_claude(root, &workspace);
        write_claude_transcript(
            &root.join("claude/projects/synthetic"),
            &workspace,
            REPLACEMENT_ID,
        );
        let source = format!("claude:{SESSION_ID}");
        successful_json(
            command(root, &workspace).args(["--json", "task", "start", "work", "--from", &source]),
        );

        // A separate CLI command moves the head during Claude's resume or Pi's import check.
        let provider = root.join("provider");
        fs::write(
            &provider,
            r#"#!/bin/sh
case "$1" in
    --resume|--version) "$TEST_OMNI" task bind "$TEST_BIND" >/dev/null || exit 1 ;;
esac
if [ "$1" = "--version" ]; then echo 0.82.0; fi
"#,
        )
        .unwrap();
        fs::set_permissions(&provider, fs::Permissions::from_mode(0o700)).unwrap();
        let result = command(root, &workspace)
            .env("OMNI_MODE", "default")
            .env(override_var, provider)
            .env("PI_CODING_AGENT_SESSION_DIR", root.join("pi/sessions"))
            .env("TEST_OMNI", env!("CARGO_BIN_EXE_omni"))
            .env("TEST_BIND", format!("claude:{REPLACEMENT_ID}"))
            .args(["switch", target])
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let status = successful_json(command(root, &workspace).args(["--json", "task", "status"]));
        assert_eq!(status["session"]["id"], REPLACEMENT_ID, "{target} switch");
        if target == "pi" {
            let store = omnis_store::Store::open(root.join("state/store.sqlite3")).unwrap();
            assert!(
                store
                    .handoff_lineage()
                    .unwrap()
                    .iter()
                    .any(|handoff| handoff.source.to_string() == source
                        && handoff.target.provider == omnis_ir::Provider::Pi)
            );
        }
    }
}

#[test]
fn binding_an_imported_session_to_an_existing_chat_keeps_only_binding_history() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let workspace = root.join("workspace");
    write_synthetic_claude(root, &workspace);
    write_claude_transcript(
        &root.join("claude/projects/synthetic"),
        &workspace,
        REPLACEMENT_ID,
    );
    let run = |args: &[&str]| successful_json(command(root, &workspace).arg("--json").args(args));
    let bundle = root.join("bundle.json");
    let bundle = bundle.to_str().unwrap();
    run(&[
        "export",
        &format!("claude:{SESSION_ID}"),
        "--output",
        bundle,
    ]);
    let imported = run(&["import", bundle]);
    let source = format!("imported:{}", imported["bundle_id"].as_str().unwrap());
    run(&["task", "start", "manual", "--from", &source]);
    run(&["task", "bind", &format!("claude:{REPLACEMENT_ID}")]);
    assert_eq!(run(&["task", "status"])["session"]["id"], REPLACEMENT_ID);
    let store = omnis_store::Store::open(root.join("state/store.sqlite3")).unwrap();
    assert!(store.handoff_lineage().unwrap().is_empty());
}

#[test]
fn same_provider_fork_can_use_the_current_workspace_after_the_old_one_moves() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let old = root.join("original");
    let moved = root.join("moved");
    write_synthetic_claude(root, &old);
    fs::rename(&old, &moved).unwrap();
    let source = format!("claude:{SESSION_ID}");
    let args = ["--json", "fork", &source, "--in", "claude", "--dry-run"];
    let refused = command(root, &moved).args(args).output().unwrap();
    assert!(!refused.status.success());
    let allowed = successful_json(
        command(root, &moved)
            .args(args)
            .arg("--allow-workspace-mismatch"),
    );
    assert_eq!(allowed["fidelity"]["repository_matches"], false);
    assert_eq!(
        allowed["launch"]["cwd"],
        json!(omnis_core::canonicalize_path(moved).unwrap())
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
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env_remove("GIT_CONFIG_COUNT")
                .env_remove("GIT_CONFIG_PARAMETERS")
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

#[cfg(unix)]
#[test]
fn export_refuses_network_workspace_before_writing_bundle() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    // POSIX resolves this UNC spelling locally, so the test never contacts a host.
    let network_workspace = "//attacker.invalid/share/repo";
    let sessions = root.join("codex/sessions/2026/01/01");
    fs::create_dir_all(&sessions).unwrap();
    fs::write(sessions.join(format!("rollout-2026-01-01T00-00-00-{SESSION_ID}.jsonl")), format!("{}\n{}\n",
        json!({"type":"session_meta","timestamp":"2026-01-01T00:00:00Z","payload":{"id":SESSION_ID,"cwd":network_workspace}}),
        json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Synthetic network workspace request"}]}})
    )).unwrap();
    let output = root.join("export.json");
    let exported = command(root, &workspace)
        .args([
            "export",
            &format!("codex:{SESSION_ID}"),
            "--output",
            output.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&exported.stderr);
    assert!(!exported.status.success(), "{stderr}");
    assert!(stderr.contains("network"), "{stderr}");
    assert!(
        !output.exists(),
        "export wrote a bundle that import rejects"
    );
}

#[test]
fn native_export_does_not_infer_fingerprint_from_a_stale_path() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let recorded = root.join("recorded");
    let current = root.join("current");
    fs::create_dir_all(&recorded).unwrap();
    fs::create_dir_all(&current).unwrap();
    for (workspace, remote) in [
        (
            recorded.as_path(),
            "https://example.invalid/acme/recorded.git",
        ),
        (
            current.as_path(),
            "https://example.invalid/acme/current.git",
        ),
    ] {
        for args in [
            vec!["init", "--quiet"],
            vec!["remote", "add", "origin", remote],
        ] {
            assert!(
                Command::new("git")
                    .env("GIT_CONFIG_GLOBAL", "/dev/null")
                    .env("GIT_CONFIG_NOSYSTEM", "1")
                    .env_remove("GIT_CONFIG_COUNT")
                    .env_remove("GIT_CONFIG_PARAMETERS")
                    .current_dir(workspace)
                    .args(args)
                    .status()
                    .unwrap()
                    .success()
            );
        }
    }
    let sessions = root.join("codex/sessions/2026/01/01");
    fs::create_dir_all(&sessions).unwrap();
    fs::write(
        sessions.join(format!("rollout-2026-01-01T00-00-00-{SESSION_ID}.jsonl")),
        format!(
            "{}\n{}\n",
            json!({"type":"session_meta","timestamp":"2026-01-01T00:00:00Z","payload":{"id":SESSION_ID,"cwd":recorded,"git":{"branch":"main"}}}),
            json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Synthetic portable request"}]}})
        ),
    )
    .unwrap();
    let output = root.join("export.json");
    successful_json(command(root, &current).args([
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
            .is_none_or(String::is_empty),
        "export must not stamp a reused path's repository identity"
    );
}

#[test]
fn dry_run_can_preview_a_semantic_handoff_without_the_target_binary() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let workspace = root.join("workspace");
    write_synthetic_claude(root, &workspace);
    let report = successful_json(command(root, &workspace).args([
        "--json",
        "resume",
        &format!("claude:{SESSION_ID}"),
        "--in",
        "codex",
        "--dry-run",
    ]));
    assert_eq!(report["fidelity"]["mode"], "semantic_handoff");
}

#[test]
fn materialize_only_does_not_fall_back_to_semantic_handoff() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let workspace = root.join("workspace");
    write_synthetic_claude(root, &workspace);
    let output = command(root, &workspace)
        .args([
            "resume",
            &format!("claude:{SESSION_ID}"),
            "--in",
            "codex",
            "--materialize-only",
        ])
        .output()
        .expect("run synthetic CLI");
    assert!(
        !output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("native import failed"),
        "materialize-only must keep the native import error: {stderr}"
    );
    assert!(
        !stderr.contains("using semantic handoff"),
        "materialize-only must not advertise semantic handoff: {stderr}"
    );
}

#[test]
fn antigravity_ide_resume_target_is_rejected() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let workspace = root.join("workspace");
    write_synthetic_claude(root, &workspace);
    let output = command(root, &workspace)
        .args([
            "resume",
            &format!("claude:{SESSION_ID}"),
            "--in",
            "antigravity-ide",
            "--dry-run",
        ])
        .output()
        .expect("run synthetic CLI");
    assert!(
        !output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("antigravity-cli") || stderr.contains("agy"),
        "rejected Antigravity IDE target must name the CLI: {stderr}"
    );
}

#[test]
fn inspection_requires_a_target_that_can_receive_history() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let workspace = root.join("workspace");
    write_synthetic_claude(root, &workspace);
    let source = format!("claude:{SESSION_ID}");
    for target in ["cursor-ide", "antigravity-ide", "imported"] {
        let output = command(root, &workspace)
            .env("OMNI_CURSOR_IDE_BIN", root.join("missing-cursor"))
            .args(["--json", "inspect", &source, "--target", target])
            .output()
            .unwrap();
        assert!(
            !output.status.success(),
            "inspection offered a transfer to {target}"
        );
        assert!(output.stdout.is_empty());
    }
}

#[cfg(unix)]
#[test]
fn inspection_rejects_a_provider_whose_interpreter_is_missing() {
    use std::os::unix::fs::PermissionsExt;

    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let workspace = root.join("workspace");
    write_synthetic_claude(root, &workspace);
    let missing = root.join("missing-interpreter");
    for shebang in [
        format!("#!{}", missing.display()),
        format!("#!/usr/bin/env {}", missing.display()),
    ] {
        let wrapper = root.join("provider");
        fs::write(&wrapper, format!("{shebang}\n")).unwrap();
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700)).unwrap();
        for (target, variable) in [("codex", "OMNI_CODEX_BIN"), ("claude", "OMNI_CLAUDE_BIN")] {
            let result = command(root, &workspace)
                .env(variable, &wrapper)
                .args([
                    "--json",
                    "inspect",
                    &format!("claude:{SESSION_ID}"),
                    "--target",
                    target,
                ])
                .output()
                .unwrap();
            assert!(!result.status.success(), "{target} accepted {shebang}");
            assert!(result.stdout.is_empty());
            assert!(!result.stderr.is_empty());
        }
    }
}

#[cfg(unix)]
#[test]
fn inspection_checks_the_permission_mode_before_offering_a_handoff() {
    use std::os::unix::fs::PermissionsExt;

    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let workspace = root.join("workspace");
    write_synthetic_claude(root, &workspace);
    let omp = root.join("omp");
    fs::write(
        &omp,
        "#!/bin/sh\ncase \"$1\" in\n  --version) echo omp/1.0.0 ;;\n  --help) echo '  --approval-mode write' ;;\nesac\n",
    )
    .unwrap();
    fs::set_permissions(&omp, fs::Permissions::from_mode(0o700)).unwrap();
    let source = format!("claude:{SESSION_ID}");

    for args in [
        vec!["inspect", &source, "--target", "omp"],
        vec!["resume", &source, "--in", "omp", "--dry-run"],
    ] {
        let mut omni = command(root, &workspace);
        omni.env("OMNI_MODE", "always-ask")
            .env("OMNI_OMP_BIN", &omp)
            .args(["--json"])
            .args(args);
        let denied = omni.output().unwrap();
        assert!(!denied.status.success());
        assert!(denied.stdout.is_empty());
        assert!(String::from_utf8_lossy(&denied.stderr).contains("cannot validate `always-ask`"));

        let report = successful_json(omni.args(["--mode", "default"]));
        assert_eq!(
            report.get("fidelity").unwrap_or(&report)["mode"],
            "semantic_handoff"
        );
    }
}

#[cfg(unix)]
#[test]
fn permission_cache_rechecks_a_replaced_runtime_behind_the_same_launcher() {
    use std::os::unix::fs::PermissionsExt;

    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let workspace = root.join("workspace");
    write_synthetic_claude(root, &workspace);
    let omp = root.join("omp");
    fs::write(&omp, "#!/usr/bin/env fixture-omp-runtime\n").unwrap();
    fs::set_permissions(&omp, fs::Permissions::from_mode(0o700)).unwrap();
    let runtime = root.join("fixture-omp-runtime");
    let install = |choices: &str| {
        fs::write(&runtime, format!(
            "#!/bin/sh\ncase \"$2\" in --help) echo call >> \"$CALLS\"; echo '  --approval-mode <{choices}>';; --version) echo omp/1.0.0;; esac\n"
        )).unwrap();
        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).unwrap();
    };
    let calls = root.join("calls");
    let search = std::env::join_paths([root, Path::new("/usr/bin"), Path::new("/bin")]).unwrap();
    let preview = || {
        command(root, &workspace)
            .env("OMNI_OMP_BIN", &omp)
            .env("CALLS", &calls)
            .env("PATH", &search)
            .env("SYNTHETIC_TOKEN", "private-fixture-value")
            .args([
                "--json",
                "resume",
                &format!("claude:{SESSION_ID}"),
                "--in",
                "omp",
                "--mode",
                "always-ask",
                "--dry-run",
            ])
            .output()
            .unwrap()
    };
    install("always-ask|write");
    for _ in 0..2 {
        let output = preview();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert_eq!(fs::read_to_string(&calls).unwrap().lines().count(), 1);
    install("write");
    let denied = preview();
    assert!(!denied.status.success());
    assert!(String::from_utf8_lossy(&denied.stderr).contains("cannot validate `always-ask`"));
    assert_eq!(fs::read_to_string(&calls).unwrap().lines().count(), 2);
    let cache = fs::read_to_string(root.join("state/cache/launch-modes.json")).unwrap();
    assert!(!cache.contains("private-fixture-value"));
    assert!(!cache.contains(root.to_str().unwrap()));
}

#[cfg(unix)]
#[test]
fn inspection_requires_a_successful_launcher_probe() {
    use std::os::unix::fs::PermissionsExt;

    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let workspace = root.join("workspace");
    write_synthetic_claude(root, &workspace);
    let wrapper = root.join("provider");
    fs::write(
        &wrapper,
        r#"#!/bin/sh
case "$1" in
    --version|--help) [ "$1" = "$TEST_PROBE" ] || exit 1; echo 0.1.0 ;;
    *) [ "$TEST_PROBE" != neither ] ;;
esac
"#,
    )
    .unwrap();
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700)).unwrap();
    let source = format!("claude:{SESSION_ID}");
    for (target, variable, mode) in [
        ("codex", "OMNI_CODEX_BIN", "semantic_handoff"),
        ("claude", "OMNI_CLAUDE_BIN", "native_resume"),
    ] {
        for flag in ["--help", "--version", "neither"] {
            let result = command(root, &workspace)
                .env(variable, &wrapper)
                .env("TEST_PROBE", flag)
                .args(["--json", "inspect", &source, "--target", target])
                .output()
                .unwrap();
            if flag == "neither" {
                assert!(!result.status.success());
                assert!(result.stdout.is_empty());
            } else {
                assert!(
                    result.status.success(),
                    "{}",
                    String::from_utf8_lossy(&result.stderr)
                );
                let report: Value = serde_json::from_slice(&result.stdout).unwrap();
                assert_eq!(report["mode"], mode);
            }
        }
    }
}

#[cfg(unix)]
#[test]
fn a_worker_holding_version_or_model_stdout_does_not_disable_native_import() {
    use std::os::unix::fs::PermissionsExt;

    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let workspace = root.join("workspace");
    write_synthetic_claude(root, &workspace);
    let pi = root.join("pi-launcher");
    fs::write(
        &pi,
        "#!/bin/sh\nif [ \"$1\" = --version ]; then printf '0.82.0\\n'; sleep 30 & fi\n",
    )
    .unwrap();
    fs::set_permissions(&pi, fs::Permissions::from_mode(0o700)).unwrap();
    let opencode = root.join("opencode-launcher");
    fs::write(
        &opencode,
        "#!/bin/sh\ncase \"$*\" in --version) echo 2.0.0 ;; *models*) (printf 'synthetic/'; sleep .1; echo model; sleep 30) & ;; esac\n",
    ).unwrap();
    fs::set_permissions(&opencode, fs::Permissions::from_mode(0o700)).unwrap();
    let source = format!("claude:{SESSION_ID}");
    let run = |args: &[&str]| {
        let output = command(root, &workspace)
            .env("OMNI_PI_BIN", &pi)
            .env("OMNI_OPENCODE_BIN", &opencode)
            .arg("--json")
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<Value>(&output.stdout).unwrap()
    };
    for (agent, expected) in [
        ("pi", "native_materialization"),
        ("opencode", "official_import"),
    ] {
        let inspection = run(&["inspect", &source, "--target", agent]);
        assert_eq!(inspection["mode"], expected);
        let preview = run(&[
            "resume",
            &source,
            "--in",
            agent,
            "--materialize-only",
            "--dry-run",
        ]);
        assert_eq!(preview["fidelity"]["mode"], inspection["mode"]);
    }
}

#[cfg(unix)]
fn has_warning(report: &Value, text: &str) -> bool {
    report["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|warning| warning.as_str().unwrap().contains(text))
}

#[cfg(unix)]
#[test]
fn cursor_ide_inspects_the_workspace_launcher_for_an_existing_chat() {
    use std::os::unix::fs::PermissionsExt;

    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let repo = root.join("repo");
    let nested = repo.join("nested");
    let workspace = repo.join("cursor-workspace");
    fs::create_dir_all(&nested).unwrap();
    std::os::unix::fs::symlink(&nested, &workspace).unwrap();
    assert!(
        Command::new("git")
            .args(["init", "--quiet"])
            .arg(&repo)
            .status()
            .unwrap()
            .success()
    );
    let cursor = root.join("cursor-data");
    fs::create_dir_all(cursor.join("globalStorage")).unwrap();
    let database = rusqlite::Connection::open(cursor.join("globalStorage/state.vscdb")).unwrap();
    database.execute_batch("CREATE TABLE composerHeaders(composerId, workspaceId, createdAt, lastUpdatedAt, isArchived, isSubagent, value)").unwrap();
    database
        .execute(
            "INSERT INTO composerHeaders VALUES (?1, NULL, 1, 2, 0, 0, ?2)",
            [SESSION_ID, &json!({"workspacePath": workspace}).to_string()],
        )
        .unwrap();
    drop(database);

    let desktop = root.join("Cursor.AppImage");
    let source = format!("cursor-ide:{SESSION_ID}");
    let run_at = |cwd: &Path, args: &[&str]| {
        command(root, cwd)
            .env("CURSOR_IDE_HOME", &cursor)
            .env("OMNI_CURSOR_IDE_BIN", &desktop)
            .args(args)
            .output()
            .unwrap()
    };
    let run = |args: &[&str]| run_at(&repo, args);
    let inspect = ["--json", "inspect", &source];
    let resume = ["--json", "resume", &source, "--dry-run"];
    for output in [run(&inspect), run(&resume)] {
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("OMNI_CURSOR_IDE_BIN"));
    }

    fs::write(&desktop, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&desktop, fs::Permissions::from_mode(0o700)).unwrap();
    let report: Value = serde_json::from_slice(&run(&inspect).stdout).unwrap();
    let resumed: Value = serde_json::from_slice(&run(&resume).stdout).unwrap();
    assert_eq!(report["mode"], "open_workspace");
    assert!(has_warning(&report, "no verifiable Cursor workspace ID"));
    assert_eq!(report, resumed["fidelity"]);
    assert_eq!(resumed["exact_chat_selection"], false);
    let selected = omnis_core::canonicalize_path(&workspace).unwrap();
    assert_eq!(resumed["launch"]["args"], json!([workspace]));
    assert_eq!(resumed["launch"]["cwd"], json!(selected));

    let container = root.join("configs");
    let physical = root.join("backing/nested");
    fs::create_dir(&container).unwrap();
    fs::create_dir_all(&physical).unwrap();
    let saved = physical.join("team.code-workspace");
    fs::write(&saved, r#"{"folders":[{"path":"../repo/nested"}]}"#).unwrap();
    let workspace_file = container.join("selected.code-workspace");
    std::os::unix::fs::symlink(&saved, &workspace_file).unwrap();
    let database = rusqlite::Connection::open(cursor.join("globalStorage/state.vscdb")).unwrap();
    database
        .execute(
            "UPDATE composerHeaders SET value = ?1",
            [json!({"workspacePath": workspace_file}).to_string()],
        )
        .unwrap();
    drop(database);
    let opened: Value = serde_json::from_slice(&run(&resume).stdout).unwrap();
    assert_eq!(opened["launch"]["args"], json!([workspace_file]));
    assert_eq!(
        opened["launch"]["cwd"],
        json!(omnis_core::canonicalize_path(&physical).unwrap())
    );
    assert_eq!(opened["fidelity"]["repository_matches"], true);
    assert!(!run_at(&container, &inspect).status.success());
    let other = run_at(
        &container,
        &[
            "--json",
            "resume",
            &source,
            "--dry-run",
            "--allow-workspace-mismatch",
        ],
    );
    let other: Value = serde_json::from_slice(&other.stdout).unwrap();
    assert_eq!(other["launch"], opened["launch"]);
    assert_eq!(other["fidelity"]["repository_matches"], false);
    assert_eq!(
        opened["fidelity"],
        serde_json::from_slice::<Value>(&run(&inspect).stdout).unwrap()
    );
}

#[cfg(unix)]
#[test]
fn cursor_agent_checks_and_resumes_in_the_chats_recorded_folder() {
    use std::os::unix::fs::PermissionsExt;

    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let repo = root.join("repo");
    let nested = repo.join("nested");
    fs::create_dir_all(&nested).unwrap();
    assert!(
        Command::new("git")
            .args(["init", "--quiet"])
            .arg(&repo)
            .status()
            .unwrap()
            .success()
    );
    let chats = root.join("chats");
    let session = chats.join("workspace-id").join(SESSION_ID);
    fs::create_dir_all(&session).unwrap();
    fs::write(
        session.join("meta.json"),
        json!({"id": SESSION_ID, "cwd": nested}).to_string(),
    )
    .unwrap();
    let agent = root.join("cursor-agent");
    fs::write(&agent, "#!/bin/sh\npwd > \"$PROBE_CWD\"\n").unwrap();
    fs::set_permissions(&agent, fs::Permissions::from_mode(0o700)).unwrap();
    let probed = root.join("probed");
    let source = format!("cursor-cli:{SESSION_ID}");
    let invocation = || {
        let mut omni = command(root, &repo);
        omni.env("CURSOR_AGENT_HOME", &chats)
            .env("OMNI_CURSOR_AGENT_BIN", &agent)
            .env("PROBE_CWD", &probed)
            .arg("--json");
        omni
    };
    let inspected = successful_json(invocation().args(["inspect", &source]));
    let resumed = successful_json(invocation().args(["resume", &source, "--no-fork", "--dry-run"]));
    let actual = omnis_core::canonicalize_path(&nested).unwrap();
    assert_eq!(resumed["launch"]["cwd"], json!(actual));
    assert_eq!(resumed["launch"]["args"], json!(["--resume", SESSION_ID]));
    assert_eq!(inspected, resumed["fidelity"]);
    assert_eq!(
        Path::new(fs::read_to_string(probed).unwrap().trim()),
        actual
    );
}

#[derive(Clone, PartialEq, prost::Message)]
struct IdeStep {
    #[prost(int32, tag = "1")]
    r#type: i32,
    #[prost(message, optional, tag = "19")]
    user_input: Option<IdeUserInput>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct IdeUserInput {
    #[prost(string, tag = "2")]
    user_response: String,
}

#[derive(Clone, PartialEq, prost::Message)]
struct IdeSummaries {
    #[prost(map = "string, message", tag = "1")]
    summaries: HashMap<String, IdeSummary>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct IdeSummary {
    #[prost(string, tag = "1")]
    summary: String,
    #[prost(uint32, tag = "2")]
    step_count: u32,
    #[prost(message, repeated, tag = "9")]
    workspaces: Vec<IdeWorkspace>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct IdeWorkspace {
    #[prost(string, tag = "1")]
    workspace_folder_absolute_uri: String,
}

fn write_desktop_step(connection: &rusqlite::Connection, index: u32, text: &str) {
    let step = IdeStep {
        r#type: 14,
        user_input: Some(IdeUserInput {
            user_response: text.to_owned(),
        }),
    };
    connection
        .execute(
            "INSERT OR REPLACE INTO steps VALUES (?1, ?2)",
            rusqlite::params![index, prost::Message::encode_to_vec(&step)],
        )
        .unwrap();
}

fn set_desktop_summary(home: &Path, title: &str, count: u32) {
    use prost::Message;

    let path = home.join("agyhub_summaries_proto.pb");
    let mut summaries = IdeSummaries::decode(fs::read(&path).unwrap().as_slice()).unwrap();
    let summary = summaries.summaries.get_mut(SESSION_ID).unwrap();
    title.clone_into(&mut summary.summary);
    summary.step_count = count;
    fs::write(path, summaries.encode_to_vec()).unwrap();
}

/// Antigravity desktop app store with one conversation database and its summary entry.
fn write_synthetic_antigravity_ide(root: &Path, workspace: &Path) -> PathBuf {
    let store = root.join("antigravity-ide");
    let conversations = store.join("conversations");
    fs::create_dir_all(&conversations).unwrap();
    fs::create_dir_all(workspace).unwrap();
    let connection =
        rusqlite::Connection::open(conversations.join(format!("{SESSION_ID}.db"))).unwrap();
    connection
        .execute_batch("CREATE TABLE steps (idx INTEGER PRIMARY KEY, step_payload BLOB);")
        .unwrap();
    write_desktop_step(&connection, 0, "Synthetic IDE continuity request");
    let mut uri_path = workspace.to_string_lossy().replace('\\', "/");
    if cfg!(windows) {
        uri_path.insert(0, '/');
    }
    let summaries = IdeSummaries {
        summaries: HashMap::from([(
            SESSION_ID.to_owned(),
            IdeSummary {
                summary: "Synthetic IDE session".to_owned(),
                step_count: 1,
                workspaces: vec![IdeWorkspace {
                    workspace_folder_absolute_uri: format!("file://{uri_path}").replace(' ', "%20"),
                }],
            },
        )]),
    };
    fs::write(
        store.join("agyhub_summaries_proto.pb"),
        prost::Message::encode_to_vec(&summaries),
    )
    .unwrap();
    store
}

#[test]
fn antigravity_ide_source_lists_and_continues_in_another_provider() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let workspace = root.join("workspace");
    let store = write_synthetic_antigravity_ide(root, &workspace);
    // Legacy Desktop roots can also contain brain artifacts. Their native summary cache
    // identifies the explicitly configured product without consulting other roots.
    fs::create_dir_all(store.join("brain")).unwrap();
    let listed = successful_json(
        command(root, &workspace)
            .env("ANTIGRAVITY_IDE_HOME", &store)
            .args([
                "--json",
                "list",
                "--provider",
                "antigravity-ide",
                "--all-projects",
            ]),
    );
    assert!(listed.to_string().contains(SESSION_ID), "{listed}");
    let report = successful_json(
        command(root, &workspace)
            .env("ANTIGRAVITY_IDE_HOME", &store)
            .args([
                "--json",
                "resume",
                &format!("antigravity-ide:{SESSION_ID}"),
                "--in",
                "codex",
                "--dry-run",
            ]),
    );
    assert_eq!(report["fidelity"]["mode"], "semantic_handoff");
}

#[test]
fn desktop_search_keeps_readable_steps_until_the_missing_history_arrives() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let workspace = root.join("workspace");
    let home = write_synthetic_antigravity_ide(root, &workspace);
    set_desktop_summary(&home, "Desktop session", 2);
    let database = home.join("conversations").join(format!("{SESSION_ID}.db"));
    let connection = rusqlite::Connection::open(database).unwrap();
    let opening = format!("{} readablebody", "opening ".repeat(64));
    write_desktop_step(&connection, 0, &opening);
    let run = |state, args: &[&str]| {
        successful_json(
            command(root, &workspace)
                .env("OMNISESSION_HOME", root.join(state))
                .env("ANTIGRAVITY_DESKTOP_HOME", &home)
                .arg("--json")
                .args(args),
        )
    };
    let search = |query| {
        run(
            "state",
            &["search", "--provider", "antigravity-desktop", query],
        )
    };
    for _ in 0..2 {
        let partial = search("readablebody");
        assert_eq!(partial["results"][0]["coverage"], "preview", "{partial}");
        assert_eq!(partial["index"]["failed"], 0);
    }

    run(
        "opened",
        &["show", &format!("antigravity-desktop:{SESSION_ID}")],
    );
    let cached = run(
        "opened",
        &[
            "search",
            "--cached",
            "--provider",
            "antigravity-desktop",
            "readablebody",
        ],
    );
    assert_eq!(cached["results"][0]["coverage"], "preview", "{cached}");

    // A large database keeps the recovered document within the indexer's preview budget.
    connection
        .execute_batch(
            "CREATE TABLE padding (data BLOB); INSERT INTO padding VALUES (zeroblob(17825792));",
        )
        .unwrap();
    write_desktop_step(&connection, 1, "Recovered suffix");
    assert_eq!(search("suffix")["results"][0]["coverage"], "preview");
    connection
        .execute("DELETE FROM steps WHERE idx=1", [])
        .unwrap();
    assert_eq!(search("suffix")["results"][0]["coverage"], "preview");
}

#[test]
fn desktop_restores_history_from_the_selected_home_after_another_home_completes() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let workspace_a = root.join("project-a");
    let workspace_b = root.join("project-b");
    let a = write_synthetic_antigravity_ide(&root.join("a"), &workspace_a);
    let b = write_synthetic_antigravity_ide(&root.join("b"), &workspace_b);
    let database = |home: &Path| home.join("conversations").join(format!("{SESSION_ID}.db"));
    let open = |home: &Path| rusqlite::Connection::open(database(home)).unwrap();
    let search = |home: &Path, query: &str, flags: &[&str]| {
        let result = successful_json(
            command(
                root,
                if home == a {
                    &workspace_a
                } else {
                    &workspace_b
                },
            )
            .env("ANTIGRAVITY_DESKTOP_HOME", home)
            .args([
                "--json",
                "search",
                "--provider",
                "antigravity-desktop",
                query,
            ])
            .args(flags),
        );
        result["results"].as_array().unwrap().len()
    };
    let db = open(&a);
    write_desktop_step(&db, 1, "asuffix");
    set_desktop_summary(&a, "Redwood session", 2);
    set_desktop_summary(&b, "Birch session", 1);
    assert_eq!(search(&a, "asuffix", &[]), 1);
    assert_eq!(search(&b, "continuity", &[]), 1);
    assert_eq!(search(&b, "asuffix", &["--cached"]), 0);

    db.execute("DELETE FROM steps WHERE idx=1", []).unwrap();
    fs::remove_file(a.join("agyhub_summaries_proto.pb")).unwrap();
    assert_eq!(search(&a, "asuffix", &[]), 1);
    assert_eq!(search(&a, "Redwood", &["--no-index"]), 1);
    assert_eq!(search(&a, "Birch", &["--no-index"]), 0);
    assert_eq!(
        search(&a, "asuffix", &["--project", workspace_b.to_str().unwrap()]),
        0
    );
    open(&b).execute("DELETE FROM steps", []).unwrap();
    set_desktop_summary(&b, "Birch session", 0);
    assert_eq!(search(&b, "asuffix", &[]), 0);
    assert_eq!(search(&b, "continuity", &["--cached"]), 0);
    assert_eq!(search(&a, "asuffix", &["--no-index"]), 1);
    drop(db);
    fs::rename(database(&a), a.join("old.db")).unwrap();
    fs::copy(database(&b), database(&a)).unwrap();
    assert_eq!(search(&a, "asuffix", &["--no-index"]), 0);
    assert_eq!(search(&a, "asuffix", &[]), 0);
}

fn desktop_search_coverage(
    root: &Path,
    workspace: &Path,
    home: &Path,
    query: &str,
    flags: &[&str],
) -> Option<String> {
    let report = successful_json(
        command(root, workspace)
            .env("ANTIGRAVITY_DESKTOP_HOME", home)
            .args([
                "--json",
                "search",
                "--provider",
                "antigravity-desktop",
                query,
            ])
            .args(flags),
    );
    report["results"].as_array().unwrap().first().map(|hit| {
        assert_eq!(hit["match"], "conversation", "{report}");
        hit["coverage"].as_str().unwrap().to_owned()
    })
}

fn write_opaque_desktop_step(db: &rusqlite::Connection, index: u32) {
    let step = IdeStep {
        r#type: 999,
        user_input: None,
    };
    db.execute(
        "INSERT OR REPLACE INTO steps VALUES (?1, ?2)",
        rusqlite::params![index, prost::Message::encode_to_vec(&step)],
    )
    .unwrap();
}

#[test]
fn desktop_partial_recovery_updates_bounded_and_opaque_search_documents() {
    for bounded in [true, false] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let workspace = root.join("workspace");
        let home = write_synthetic_antigravity_ide(&root.join("a"), &workspace);
        let db =
            rusqlite::Connection::open(home.join("conversations").join(format!("{SESSION_ID}.db")))
                .unwrap();
        let search =
            |word, flags: &[&str]| desktop_search_coverage(root, &workspace, &home, word, flags);
        if bounded {
            write_desktop_step(&db, 0, &"padding ".repeat(11_000));
        } else {
            write_desktop_step(&db, 0, "Synthetic opener");
            write_opaque_desktop_step(&db, 2);
        }
        let initial_steps = if bounded { 2 } else { 3 };
        write_desktop_step(&db, 1, "originaltail");
        set_desktop_summary(&home, "Desktop session", initial_steps);
        assert_eq!(
            search("originaltail", &[]).as_deref(),
            Some(if bounded { "head-tail" } else { "preview" })
        );
        if !bounded {
            let other = write_synthetic_antigravity_ide(&root.join("b"), &workspace);
            let other_db = rusqlite::Connection::open(
                other.join("conversations").join(format!("{SESSION_ID}.db")),
            )
            .unwrap();
            write_desktop_step(&other_db, 1, "foreignbody");
            set_desktop_summary(&other, "Other session", 2);
            assert_eq!(
                desktop_search_coverage(root, &workspace, &other, "foreignbody", &[]).as_deref(),
                Some("complete")
            );
        }

        write_desktop_step(&db, initial_steps, "recoveredtail");
        set_desktop_summary(&home, "Desktop session", initial_steps + 2);
        assert_eq!(search("recoveredtail", &[]).as_deref(), Some("preview"));
        assert_eq!(
            search("recoveredtail", &["--cached"]).as_deref(),
            Some("preview")
        );
        assert_eq!(
            search("originaltail", &["--cached"]).as_deref(),
            Some("preview")
        );
    }
}

#[test]
fn desktop_partial_recovery_keeps_searchable_rows_when_the_document_cap_moves() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let workspace = root.join("workspace");
    let home = write_synthetic_antigravity_ide(root, &workspace);
    let db =
        rusqlite::Connection::open(home.join("conversations").join(format!("{SESSION_ID}.db")))
            .unwrap();
    let search =
        |word, flags: &[&str]| desktop_search_coverage(root, &workspace, &home, word, flags);
    let padding = "padding ".repeat(7_500);
    let write_row = |index| {
        let marker = match index {
            16 => "recoveredmiddle",
            21 => "previousedge",
            _ => "body",
        };
        write_desktop_step(&db, index, &format!("row{index} {marker} {padding}"));
    };
    for index in (0..16).chain(21..37) {
        write_row(index);
    }
    set_desktop_summary(&home, "Desktop session", 38);
    assert_eq!(search("previousedge", &[]).as_deref(), Some("preview"));
    for index in 16..21 {
        write_row(index);
    }
    assert_eq!(search("recoveredmiddle", &[]).as_deref(), Some("preview"));
    assert_eq!(
        search("recoveredmiddle", &["--cached"]).as_deref(),
        Some("preview")
    );
    assert_eq!(
        search("previousedge", &["--cached"]).as_deref(),
        Some("preview")
    );

    for index in 37..47 {
        write_row(index);
    }
    set_desktop_summary(&home, "Desktop session", 48);
    assert_eq!(search("previousedge", &[]).as_deref(), Some("preview"));
    assert_eq!(
        search("previousedge", &["--cached"]).as_deref(),
        Some("preview")
    );
}

#[test]
fn desktop_partial_recovery_preserves_message_boundaries_and_tracks_opaque_steps() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let workspace = root.join("workspace");
    let home = write_synthetic_antigravity_ide(root, &workspace);
    let db =
        rusqlite::Connection::open(home.join("conversations").join(format!("{SESSION_ID}.db")))
            .unwrap();
    let search =
        |word, flags: &[&str]| desktop_search_coverage(root, &workspace, &home, word, flags);
    write_desktop_step(&db, 0, "firstbody");
    write_desktop_step(&db, 1, "island\n\nbridge");
    set_desktop_summary(&home, "Desktop session", 2);
    assert_eq!(search("island-bridge", &[]).as_deref(), Some("complete"));

    db.execute("DELETE FROM steps WHERE idx=1", []).unwrap();
    write_desktop_step(&db, 2, "island\n\nrecoverednote\n\nbridge");
    set_desktop_summary(&home, "Desktop session", 4);
    assert!(search("recoverednote", &[]).is_none());
    assert!(search("island-bridge", &["--cached"]).is_some());

    write_desktop_step(&db, 1, "island\n\nbridge");
    write_opaque_desktop_step(&db, 2);
    assert_eq!(search("island-bridge", &[]).as_deref(), Some("preview"));
    assert_eq!(
        search("island-bridge", &["--cached"]).as_deref(),
        Some("preview")
    );
    write_desktop_step(&db, 2, "thirdstep");
    write_desktop_step(&db, 3, "finalstep");
    assert_eq!(search("finalstep", &[]).as_deref(), Some("complete"));
    assert_eq!(
        search("island-bridge", &["--cached"]).as_deref(),
        Some("complete")
    );
}

#[test]
fn desktop_search_accepts_a_richer_partial_after_a_complete_read() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let workspace = root.join("workspace");
    let home = write_synthetic_antigravity_ide(root, &workspace);
    let database =
        rusqlite::Connection::open(home.join("conversations").join(format!("{SESSION_ID}.db")))
            .unwrap();
    let search = |query: &str, flags: &[&str]| {
        desktop_search_coverage(root, &workspace, &home, query, flags)
    };
    write_desktop_step(&database, 0, "firstbody");
    write_desktop_step(&database, 1, "secondbody");
    set_desktop_summary(&home, "Desktop session", 2);
    assert_eq!(search("secondbody", &[]).as_deref(), Some("complete"));

    set_desktop_summary(&home, "Desktop session", 4);
    assert_eq!(search("secondbody", &[]).as_deref(), Some("complete"));
    database
        .execute("DELETE FROM steps WHERE idx=1", [])
        .unwrap();
    assert_eq!(search("secondbody", &[]).as_deref(), Some("complete"));
    write_desktop_step(&database, 0, "unrelatedbody");
    assert!(search("unrelatedbody", &[]).is_none());
    assert_eq!(
        search("secondbody", &["--cached"]).as_deref(),
        Some("complete")
    );

    write_desktop_step(&database, 0, "firstbody");
    write_desktop_step(&database, 1, "secondbody");
    write_desktop_step(&database, 2, "thirdbody");
    assert_eq!(search("thirdbody", &[]).as_deref(), Some("preview"));
    assert_eq!(
        search("thirdbody", &["--cached"]).as_deref(),
        Some("preview")
    );
    assert_eq!(
        search("secondbody", &["--cached"]).as_deref(),
        Some("preview")
    );
}

#[test]
fn desktop_partial_search_follows_the_selected_file_and_restored_middle_steps() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let workspace = root.join("workspace");
    let a = write_synthetic_antigravity_ide(&root.join("a"), &workspace);
    let b = write_synthetic_antigravity_ide(&root.join("b"), &workspace);
    let database = |home: &Path| home.join("conversations").join(format!("{SESSION_ID}.db"));
    let search = |home: &Path, word, flags: &[&str]| {
        let response = successful_json(
            command(root, &workspace)
                .env("ANTIGRAVITY_DESKTOP_HOME", home)
                .args([
                    "--json",
                    "search",
                    "--provider",
                    "antigravity-desktop",
                    word,
                ])
                .args(flags),
        );
        response["results"].as_array().unwrap().len()
    };
    let old = rusqlite::Connection::open(database(&a)).unwrap();
    write_desktop_step(&old, 1, "previoussuffix");
    set_desktop_summary(&a, "Previous", 2);
    assert_eq!(search(&a, "previoussuffix", &[]), 1);
    drop(old);

    fs::rename(database(&a), a.join("old.db")).unwrap();
    fs::copy(database(&b), database(&a)).unwrap();
    let current = rusqlite::Connection::open(database(&a)).unwrap();
    write_desktop_step(&current, 2, "readabletail");
    set_desktop_summary(&a, "Replacement", 4);
    assert_eq!(search(&a, "previoussuffix", &["--no-index"]), 0);
    assert_eq!(search(&a, "readabletail", &[]), 1);
    assert_eq!(search(&a, "previoussuffix", &["--cached"]), 0);
    write_desktop_step(&current, 1, "restoredmiddle");
    assert_eq!(search(&a, "restoredmiddle", &[]), 1);
    assert_eq!(search(&a, "readabletail", &["--cached"]), 1);

    let foreign = rusqlite::Connection::open(database(&b)).unwrap();
    write_desktop_step(&foreign, 1, "foreignsuffix");
    set_desktop_summary(&b, "Other home", 3);
    assert_eq!(search(&b, "foreignsuffix", &[]), 1);
    assert_eq!(search(&a, "readabletail", &["--no-index"]), 1);
    assert_eq!(search(&a, "foreignsuffix", &["--no-index"]), 0);
}

#[test]
fn a_partial_desktop_read_publishes_the_moved_summary_with_its_retained_text() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let initial = root.join("initial");
    let moved = root.join("moved");
    let home = write_synthetic_antigravity_ide(root, &initial);
    let database =
        rusqlite::Connection::open(home.join("conversations").join(format!("{SESSION_ID}.db")))
            .unwrap();
    write_desktop_step(&database, 1, "retainedsuffix");
    set_desktop_summary(&home, "Initial conversation", 2);
    let run = |project: &Path, args: &[&str]| {
        successful_json(
            command(root, project)
                .env("ANTIGRAVITY_DESKTOP_HOME", &home)
                .arg("--json")
                .args(args),
        )
    };
    let source = format!("antigravity-desktop:{SESSION_ID}");
    run(&initial, &["show", &source]);
    let updated = write_synthetic_antigravity_ide(&root.join("updated"), &moved);
    set_desktop_summary(&updated, "Moved conversation", 2);
    let summary = "agyhub_summaries_proto.pb";
    fs::copy(updated.join(summary), home.join(summary)).unwrap();
    database
        .execute("DELETE FROM steps WHERE idx=1", [])
        .unwrap();
    assert_eq!(
        run(&moved, &["show", &source])["title"],
        "Moved conversation"
    );

    let search = [
        "search",
        "retainedsuffix",
        "--provider",
        "antigravity-desktop",
        "--cached",
        "--show-text",
    ];
    assert_eq!(
        run(&moved, &search)["results"][0]["title"],
        "Moved conversation"
    );
    assert_eq!(run(&initial, &search)["results"], serde_json::json!([]));
}

#[test]
fn antigravity_desktop_source_has_its_own_identity_and_continues_elsewhere() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let workspace = root.join("workspace");
    let store = write_synthetic_antigravity_ide(root, &workspace);
    let database = store.join("conversations").join(format!("{SESSION_ID}.db"));
    let before = fs::read(&database).unwrap();
    let listed = successful_json(
        command(root, &workspace)
            .env("ANTIGRAVITY_DESKTOP_HOME", &store)
            .args([
                "--json",
                "list",
                "--provider",
                "antigravity-desktop",
                "--all-projects",
            ]),
    );
    assert_eq!(
        listed["sessions"][0]["session"]["provider"],
        "antigravity-desktop"
    );
    assert_eq!(listed["sessions"][0]["session"]["id"], SESSION_ID);
    let report = successful_json(
        command(root, &workspace)
            .env("ANTIGRAVITY_DESKTOP_HOME", &store)
            .args([
                "--json",
                "resume",
                &format!("antigravity-desktop:{SESSION_ID}"),
                "--in",
                "codex",
                "--dry-run",
            ]),
    );
    assert_eq!(report["fidelity"]["mode"], "semantic_handoff");
    assert_eq!(fs::read(database).unwrap(), before);
}

#[cfg(unix)]
#[test]
fn cursor_ide_reports_missing_and_unverified_chat_records() {
    use std::os::unix::fs::PermissionsExt;

    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let workspace = root.join("workspace");
    fs::create_dir(&workspace).unwrap();
    let cursor = root.join("cursor");
    fs::create_dir_all(cursor.join("globalStorage")).unwrap();
    let database = rusqlite::Connection::open(cursor.join("globalStorage/state.vscdb")).unwrap();
    database.execute_batch(
        "CREATE TABLE composerHeaders(composerId, workspaceId, createdAt, lastUpdatedAt, isArchived, isSubagent, value);
         CREATE TABLE cursorDiskKV(key TEXT PRIMARY KEY, value TEXT);",
    ).unwrap();
    database
        .execute(
            "INSERT INTO composerHeaders VALUES (?1, NULL, 1, 2, 0, 0, ?2)",
            [SESSION_ID, &json!({"workspacePath": workspace}).to_string()],
        )
        .unwrap();
    let put = |key: String, value: Value| {
        database
            .execute(
                "INSERT INTO cursorDiskKV VALUES (?1, ?2)",
                [key, value.to_string()],
            )
            .unwrap();
    };
    put(
        format!("composerData:{SESSION_ID}"),
        json!({
            "composerId": SESSION_ID, "fullConversationHeadersOnly": [
                {"bubbleId":"user", "type":1}, {"bubbleId":"reply", "type":2}
            ]
        }),
    );
    put(
        format!("bubbleId:{SESSION_ID}:user"),
        json!({"bubbleId":"user", "type":1, "text":"Synthetic user question"}),
    );
    let desktop = root.join("Cursor.AppImage");
    fs::write(&desktop, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&desktop, fs::Permissions::from_mode(0o700)).unwrap();
    let source = format!("cursor-ide:{SESSION_ID}");
    let run = |args: &[&str]| {
        successful_json(
            command(root, &workspace)
                .env("CURSOR_IDE_HOME", &cursor)
                .env("OMNI_CURSOR_IDE_BIN", &desktop)
                .args(args),
        )
    };
    for (reply, feature, status, warning) in [
        (
            None,
            "Missing native conversation records",
            "omitted",
            "missing",
        ),
        (
            Some(json!({"bubbleId":"reply", "type":2, "futureFormat":true})),
            "Unverified native conversation records",
            "unsupported",
            "could not verify",
        ),
    ] {
        if let Some(reply) = reply {
            put(format!("bubbleId:{SESSION_ID}:reply"), reply);
        }
        let inspected = run(&["--json", "inspect", &source]);
        let resumed = run(&["--json", "resume", &source, "--dry-run"]);
        assert_eq!(inspected, resumed["fidelity"]);
        assert_eq!(inspected["mode"], "open_workspace");
        assert_eq!(resumed["exact_chat_selection"], false);
        let entries: HashMap<_, _> = inspected["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| (entry["feature"].as_str().unwrap(), entry))
            .collect();
        assert_eq!(entries[feature]["status"], status);
        assert_eq!(entries["Available native messages"]["status"], "preserved");
        assert_eq!(entries["Tool history"]["status"], "unsupported");
        assert!(!entries.contains_key("Native conversation"));
        assert_eq!(
            entries.values().any(|e| e["status"] == "omitted"),
            status == "omitted"
        );
        assert!(has_warning(&inspected, warning));
    }
}

#[test]
fn standalone_antigravity_ide_history_continues_elsewhere_without_mutation() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let store = root.join("standalone-ide");
    let logs = store
        .join("brain")
        .join(SESSION_ID)
        .join(".system_generated/logs");
    fs::create_dir_all(&logs).unwrap();
    let path = logs.join("transcript.jsonl");
    let bytes = [json!({"type":"USER_INPUT", "content":"Synthetic standalone IDE request", "workspacePaths":[workspace]}),
        json!({"type":"PLANNER_RESPONSE", "content":"Synthetic standalone IDE response"})]
        .iter().map(ToString::to_string).collect::<Vec<_>>().join("\n");
    fs::write(&path, &bytes).unwrap();
    let report = successful_json(
        command(root, &workspace)
            .env("ANTIGRAVITY_IDE_HOME", &store)
            .args([
                "--json",
                "resume",
                &format!("antigravity-ide:{SESSION_ID}"),
                "--in",
                "codex",
                "--dry-run",
            ]),
    );
    assert_eq!(report["fidelity"]["mode"], "semantic_handoff");
    assert_eq!(fs::read_to_string(path).unwrap(), bytes);
}
