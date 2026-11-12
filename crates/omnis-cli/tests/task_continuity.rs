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

fn inspect_workspace(root: &Path, directory: &Path, source: &str) -> Command {
    let mut cli = command(root, directory);
    let launcher = std::env::current_exe().unwrap();
    cli.env("OMNI_CODEX_BIN", &launcher)
        .env("OMNI_CLAUDE_BIN", launcher)
        .args(["--json", "inspect", source]);
    cli
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
fn codex_keeps_its_relative_store_when_launching_in_the_recorded_subdirectory() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let repo = root.join("repo");
    let member = repo.join("member");
    let home = repo.join(".fixture-codex");
    let sessions = home.join("sessions");
    fs::create_dir_all(&member).unwrap();
    fs::create_dir_all(&sessions).unwrap();
    workspace_git(&repo, &["init", "--quiet"]);
    let recorded = omnis_core::canonicalize_path(&member).unwrap();
    fs::write(
        sessions.join(format!("rollout-2026-01-01T00-00-00-{SESSION_ID}.jsonl")),
        json!({"type":"session_meta", "payload":{"id":SESSION_ID, "cwd":recorded}}).to_string(),
    )
    .unwrap();
    fs::create_dir_all(root.join("state")).unwrap();
    fs::write(
        root.join("state/profiles.toml"),
        "[profiles.configured]\nagent = \"codex\"\n[profiles.configured.env]\nFIXTURE_OPTION = \"configured\"\n",
    )
    .unwrap();

    let session = format!("codex:{SESSION_ID}");
    let selected = omnis_core::canonicalize_path(&home).unwrap();
    for (verb, profile) in [("resume", "codex"), ("fork", "configured")] {
        let mut cli = command(root, &repo);
        cli.env("CODEX_HOME", ".fixture-codex").args([
            "--json",
            verb,
            &session,
            "--in",
            profile,
            "--dry-run",
        ]);
        if verb == "resume" {
            cli.arg("--no-fork");
        }
        let plan = successful_json(&mut cli);
        assert_eq!(plan["launch"]["cwd"], json!(recorded));
        let env = plan["launch"]["env"].as_array().unwrap();
        let planned_home = env.iter().find(|e| e["name"] == "CODEX_HOME").unwrap();
        let planned_home = Path::new(planned_home["value"].as_str().unwrap());
        assert!(planned_home.is_absolute());
        assert_eq!(
            omnis_core::canonicalize_path(planned_home).unwrap(),
            selected
        );
        if profile == "configured" {
            assert!(env.contains(&json!({"name":"FIXTURE_OPTION","value":"configured"})));
        }
    }
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
fn codex_native_launch_uses_the_surviving_turn_directory() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let initial = root.join("initial");
    let moved = root.join("moved");
    let sessions = root.join("codex/sessions");
    for dir in [&initial, &moved, &sessions] {
        fs::create_dir_all(dir).unwrap();
    }
    let rollout = sessions.join(format!("rollout-2026-01-01T00-00-00-{SESSION_ID}.jsonl"));
    let mut records = vec![
        json!({"type":"session_meta","payload":{"id":SESSION_ID,"cwd":initial,
            "git":{"commit_hash":"0000000000000000000000000000000000000000"}}}),
    ];
    for (directory, message) in [(&initial, "Original turn"), (&moved, "Moved turn")] {
        records.push(json!({"type":"event_msg","payload":{"type":"task_started"}}));
        records.push(json!({"type":"turn_context","payload":{"cwd":directory}}));
        records
            .push(json!({"type":"event_msg","payload":{"type":"user_message","message":message}}));
    }
    let write = |records: &[Value]| {
        let lines = records
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(&rollout, lines).unwrap();
    };
    let source = format!("codex:{SESSION_ID}");
    let check_launch = |directory: &Path, expected: bool, cold_needs_cd: bool| {
        let check_plan = |verb: &str, set_cwd: bool| {
            let mut cli = command(root, directory);
            cli.args(["--json", verb, &source, "--in", "codex", "--dry-run"]);
            if verb == "resume" {
                cli.arg("--no-fork");
            }
            let plan = successful_json(&mut cli);
            assert_eq!(
                plan["launch"]["cwd"],
                json!(omnis_core::canonicalize_path(directory).unwrap())
            );
            let args = plan["launch"]["args"].as_array().unwrap();
            assert!(args.iter().any(|arg| arg == verb), "{plan}");
            assert_eq!(
                args.windows(2)
                    .any(|pair| pair[0] == "--cd" && pair[1] == "."),
                set_cwd,
                "{plan}"
            );
            assert_eq!(plan["fidelity"]["repository_matches"], expected);
        };
        check_plan("resume", cold_needs_cd);
        let inspected = successful_json(&mut inspect_workspace(root, directory, &source));
        assert_eq!(inspected["repository_matches"], expected);
        // Inspection caches the effective directory. The cache does not establish the native startup directory.
        check_plan("fork", true);
    };
    write(&records);
    check_launch(&moved, true, true);
    check_codex_profile_directory(root, &moved, &source);
    let wrong = command(root, &initial)
        .args(["--json", "fork", &source, "--in", "codex", "--dry-run"])
        .output()
        .unwrap();
    assert!(!wrong.status.success());
    assert!(String::from_utf8_lossy(&wrong.stderr).contains("differs from current"));
    records.push(json!({"type":"event_msg","payload":{"type":"thread_rolled_back","num_turns":1}}));
    write(&records);
    check_launch(&initial, false, false);

    records[0]["payload"].as_object_mut().unwrap().remove("cwd");
    records.truncate(1);
    write(&records);
    let forced = successful_json(command(root, &moved).args([
        "--json",
        "fork",
        &source,
        "--in",
        "codex",
        "--allow-workspace-mismatch",
        "--dry-run",
    ]));
    let flags = forced["launch"]["args"].as_array().unwrap();
    assert!(
        flags
            .windows(2)
            .any(|pair| pair[0] == "--cd" && pair[1] == ".")
    );
}

fn check_codex_profile_directory(root: &Path, selected: &Path, source: &str) {
    workspace_git(selected, &["init", "--quiet"]);
    fs::create_dir(selected.join("somewhere")).unwrap();
    fs::write(
        root.join("state/profiles.toml"),
        r#"[profiles.configured]
agent = "codex"
args = ["--cd=somewhere"]
[profiles.wrapper]
agent = "codex"
program = "${SYNTHETIC_WRAPPER}"
args = ["codex"]
[profiles.noninteractive]
agent = "codex"
args = ["exec"]
"#,
    )
    .unwrap();
    let args_for = |profile| {
        let plan = successful_json(
            command(root, selected)
                .env("SYNTHETIC_WRAPPER", std::env::current_exe().unwrap())
                .args(["--json", "fork", source, "--in", profile, "--dry-run"]),
        );
        plan["launch"]["args"].clone()
    };
    let sqlite = format!("sqlite_home={}", json!(root.join("codex")));
    assert_eq!(
        args_for("configured"),
        json!(["--cd=somewhere", "-c", sqlite, "fork", SESSION_ID])
    );
    assert_eq!(
        args_for("wrapper"),
        json!(["codex", "--cd", ".", "-c", sqlite, "fork", SESSION_ID])
    );
    let exec = successful_json(command(root, selected).args([
        "--json",
        "resume",
        source,
        "--in",
        "noninteractive",
        "--no-fork",
        "--dry-run",
    ]));
    assert_eq!(
        exec["launch"]["args"],
        json!(["exec", "--cd", ".", "-c", sqlite, "resume", SESSION_ID])
    );
}

#[cfg(unix)]
#[test]
fn a_same_size_in_place_rewrite_invalidates_a_large_native_workspace() {
    use std::io::{Seek as _, SeekFrom, Write as _};

    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let original = root.join("alpha");
    let replacement = root.join("bravo");
    let sessions = root.join("codex/sessions");
    for path in [&original, &replacement, &sessions] {
        fs::create_dir_all(path).unwrap();
    }
    let mut file =
        fs::File::create(sessions.join(format!("rollout-2026-01-01T00-00-00-{SESSION_ID}.jsonl")))
            .unwrap();
    writeln!(
        file,
        "{}",
        json!({"type":"session_meta","payload":{"id":SESSION_ID,"cwd":original}})
    )
    .unwrap();
    writeln!(file, "{}", json!({"type":"response_item","payload":{"type":"function_call_output","output":"x".repeat(5 * 1024 * 1024)}})).unwrap();
    let turn = |path| json!({"type":"turn_context","payload":{"cwd": path}});
    let position = file.stream_position().unwrap();
    writeln!(file, "{}", turn(&original)).unwrap();
    let modified = file.metadata().unwrap().modified().unwrap();
    let source = format!("codex:{SESSION_ID}");
    let launch = |cwd: &Path| {
        successful_json(command(root, cwd).args([
            "--json",
            "resume",
            &source,
            "--in",
            "codex",
            "--no-fork",
            "--dry-run",
        ]))
    };
    assert_eq!(launch(&original)["fidelity"]["repository_matches"], true);
    file.seek(SeekFrom::Start(position)).unwrap();
    writeln!(file, "{}", turn(&replacement)).unwrap();
    file.set_times(fs::FileTimes::new().set_modified(modified))
        .unwrap();
    let plan = launch(&replacement);
    assert_eq!(plan["fidelity"]["repository_matches"], true);
    assert_eq!(
        plan["launch"]["args"],
        json!([
            "--cd",
            ".",
            "-c",
            format!("sqlite_home={}", json!(root.join("codex"))),
            "resume",
            SESSION_ID
        ])
    );
}

#[test]
fn equal_size_rewrite_with_restored_mtime_invalidates_the_observed_project() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let initial = root.join("initial");
    let alternate_header = root.join("startup");
    let previous = root.join("alpha");
    let current = root.join("bravo");
    let sessions = root.join("codex/sessions");
    for directory in [&initial, &alternate_header, &previous, &current, &sessions] {
        fs::create_dir_all(directory).unwrap();
    }
    let rollout = sessions.join(format!("rollout-{SESSION_ID}.jsonl"));
    let timestamp = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_760_000_000);
    let write = |creation: &Path, directory: &Path| {
        let rows = [
            json!({"type":"session_meta","payload":{"id":SESSION_ID,"cwd":creation}}),
            json!({"type":"turn_context","payload":{"cwd":directory}}),
        ];
        fs::write(&rollout, rows.map(|row| row.to_string()).join("\n")).unwrap();
        let file = fs::File::options().write(true).open(&rollout).unwrap();
        file.set_modified(timestamp).unwrap();
        assert_eq!(file.metadata().unwrap().modified().unwrap(), timestamp);
        file.metadata().unwrap().len()
    };
    let source = format!("codex:{SESSION_ID}");
    let list = |directory: &Path| {
        let result = successful_json(command(root, directory).args([
            "--json",
            "list",
            "--provider",
            "codex",
        ]));
        result["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["project_path"].clone())
            .collect::<Vec<_>>()
    };
    let length = write(&alternate_header, &previous);
    assert_eq!(list(&alternate_header), [json!(alternate_header)]);
    assert_eq!(write(&initial, &previous), length);
    assert!(list(&alternate_header).is_empty());
    assert_eq!(list(&initial), [json!(initial)]);
    successful_json(command(root, &initial).args(["--json", "show", &source]));
    assert_eq!(list(&previous), [json!(previous)]);
    assert_eq!(write(&initial, &current), length);
    assert!(list(&previous).is_empty());
    successful_json(command(root, &initial).args(["--json", "index", "--provider", "codex"]));
    assert_eq!(list(&current), [json!(current)]);
    assert!(list(&initial).is_empty());

    assert_eq!(write(&initial, &previous), length);
    successful_json(command(root, &initial).args(["--json", "show", &source]));
    assert_eq!(list(&previous), [json!(previous)]);
    assert!(list(&current).is_empty());
}

#[test]
fn codex_source_reads_update_project_discovery_and_search() {
    for show_first in [true, false] {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        let original = root.join("first");
        let moved = root.join("moved");
        let sessions = root.join("codex/sessions");
        for directory in [&original, &moved, &sessions] {
            fs::create_dir_all(directory).unwrap();
        }
        let rollout = sessions.join(format!("rollout-{SESSION_ID}.jsonl"));
        let write_rollout = |path: &Path, destination: &Path, reply: &str| {
            let rows = [
                json!({"type":"session_meta","payload":{"id":SESSION_ID,"cwd":original,
                    "git":{"branch":"original-only"}}}),
                json!({"type":"turn_context","payload":{"cwd":destination}}),
                json!({"type":"event_msg","payload":{"type":"user_message","message":"Synthetic request"}}),
                json!({"type":"event_msg","payload":{"type":"agent_message","message":reply}}),
            ];
            fs::write(path, rows.map(|row| row.to_string()).join("\n")).unwrap();
        };
        write_rollout(&rollout, &moved, "Synthetic pinecone");
        let source = format!("codex:{SESSION_ID}");
        if show_first {
            successful_json(command(root, &moved).args(["--json", "show", &source]));
        } else {
            successful_json(command(root, &moved).args(["--json", "index", "--provider", "codex"]));
        }
        let list = |project: &Path| {
            successful_json(command(root, project).args(["--json", "list", "--provider", "codex"]))
        };
        let search = |project: &Path, mode: &str| {
            successful_json(command(root, project).args([
                "--json",
                "search",
                "pinecone",
                "--provider",
                "codex",
                mode,
            ]))
        };
        let listed = list(&moved);
        assert_eq!(
            listed["sessions"][0]["session"]["id"], SESSION_ID,
            "{listed}"
        );
        assert_eq!(listed["sessions"][0]["project_path"], json!(moved));
        assert!(listed["sessions"][0]["git_branch"].is_null());
        assert!(list(&original)["sessions"].as_array().unwrap().is_empty());
        for mode in ["--cached", "--no-index"] {
            let found = search(&moved, mode);
            assert_eq!(found["results"][0]["session"], source, "{found}");
            assert_eq!(found["results"][0]["match"], "conversation");
            assert!(
                search(&original, mode)["results"]
                    .as_array()
                    .unwrap()
                    .is_empty()
            );
        }

        let replacement = sessions.join("replacement");
        let modified = fs::metadata(&rollout).unwrap().modified().unwrap();
        write_rollout(&replacement, &original, "Synthetic oakfruit");
        assert_eq!(
            fs::metadata(&rollout).unwrap().len(),
            fs::metadata(&replacement).unwrap().len()
        );
        fs::File::options()
            .write(true)
            .open(&replacement)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(modified))
            .unwrap();
        fs::rename(replacement, &rollout).unwrap();
        assert!(list(&moved)["sessions"].as_array().unwrap().is_empty());
        assert_eq!(
            list(&original)["sessions"][0]["git_branch"],
            "original-only"
        );
        assert!(
            search(&moved, "--cached")["results"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        successful_json(command(root, &original).args(["--json", "show", &source]));
        assert!(
            search(&original, "--cached")["results"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        let refreshed = successful_json(
            command(root, &original).args(["--json", "search", "oakfruit", "--cached"]),
        );
        assert_eq!(refreshed["results"][0]["session"], source, "{refreshed}");
    }
}

#[test]
fn codex_search_does_not_pair_one_homes_text_with_another_homes_workspace() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let original = root.join("initial");
    fs::create_dir(&original).unwrap();
    let homes = [root.join("codex-a"), root.join("codex-b")];
    let projects = [root.join("alpha"), root.join("bravo")];
    let markers = ["amberfalcon", "cobaltraven"];
    let source = format!("codex:{SESSION_ID}");
    for (i, home) in homes.iter().enumerate() {
        fs::create_dir(&projects[i]).unwrap();
        let sessions = home.join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        let source = sessions.join(format!("rollout-{SESSION_ID}.jsonl"));
        let rows = [
            json!({"type":"session_meta","payload":{"id":SESSION_ID,"cwd":original}}),
            json!({"type":"turn_context","payload":{"cwd":projects[i]}}),
            json!({"type":"event_msg","payload":{"type":"user_message","message":"Synthetic request"}}),
            json!({"type":"event_msg","payload":{"type":"agent_message","message":markers[i]}}),
        ];
        fs::write(&source, rows.map(|row| row.to_string()).join("\n")).unwrap();
        fs::File::options()
            .write(true)
            .open(&source)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(
                std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_760_000_000),
            ))
            .unwrap();
    }
    let run = |i: usize, args: &[&str]| {
        successful_json(
            command(root, &projects[i])
                .env("CODEX_HOME", &homes[i])
                .arg("--json")
                .args(args),
        )
    };
    for active in [0, 1, 0] {
        run(active, &["index", "--provider", "codex"]);
        for i in 0..2 {
            let foreign = run(
                i,
                &[
                    "search",
                    markers[1 - i],
                    "--provider",
                    "codex",
                    "--cached",
                    "--show-text",
                ],
            );
            assert!(
                foreign["results"].as_array().unwrap().is_empty(),
                "{foreign}"
            );
        }
        let own = run(
            active,
            &["search", markers[active], "--provider", "codex", "--cached"],
        );
        assert_eq!(own["results"][0]["session"], source, "{own}");
        assert_eq!(own["results"][0]["project_path"], json!(projects[active]));
    }
}

fn workspace_git(directory: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env_remove("GIT_CONFIG_COUNT")
        .env_remove("GIT_CONFIG_PARAMETERS")
        .current_dir(directory)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn check_profile_worktree_identity(root: &Path, workspace: &Path, source: &str) {
    let other = root.join("other-worktree");
    workspace_git(
        workspace,
        &[
            "worktree",
            "add",
            "--quiet",
            "--detach",
            other.to_str().unwrap(),
        ],
    );
    let profiles = root.join("state/profiles.toml");
    fs::write(
        &profiles,
        "[profiles.checkout]\nagent = \"codex\"\nargs = [\"-C\", \"${PROFILE_WORKTREE}\"]\n",
    )
    .unwrap();
    // The launcher probe receives `-C <directory> --help`. Git accepts that profile syntax.
    let git = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|dir| dir.join(format!("git{}", std::env::consts::EXE_SUFFIX)))
        .find(|path| path.is_absolute() && path.is_file())
        .expect("Git executable used to create the worktrees");
    let inspect = |from: &Path, to: &Path| {
        let mut cli = inspect_workspace(root, from, source);
        successful_json(
            cli.env("OMNI_CODEX_BIN", &git)
                .env("PROFILE_WORKTREE", to)
                .args(["--target", "checkout"]),
        )
    };
    let fork = |from: &Path, to: &Path| {
        let mut cli = command(root, from);
        cli.env("PROFILE_WORKTREE", to).args([
            "--json",
            "fork",
            source,
            "--in",
            "checkout",
            "--dry-run",
        ]);
        cli
    };

    assert_eq!(inspect(workspace, &other)["repository_matches"], false);
    let rejected = fork(workspace, &other).output().unwrap();
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("--allow-workspace-mismatch"));
    let allowed = successful_json(fork(workspace, &other).arg("--allow-workspace-mismatch"));
    assert_eq!(allowed["fidelity"]["repository_matches"], false);
    assert_eq!(
        allowed["launch"]["cwd"],
        json!(omnis_core::canonicalize_path(workspace).unwrap())
    );

    let returned = successful_json(fork(&other, workspace).arg("--allow-workspace-mismatch"));
    assert_eq!(returned["fidelity"]["repository_matches"], true);
    let subdir = workspace.join("profile-subdir");
    fs::create_dir(&subdir).unwrap();
    let same = successful_json(&mut fork(workspace, Path::new("profile-subdir")));
    assert_eq!(same["fidelity"]["repository_matches"], true);
    assert_eq!(inspect(workspace, &subdir)["repository_matches"], true);
}

#[test]
fn codex_repository_matches_its_recorded_remote_and_commit() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let workspace = root.join("workspace");
    fs::create_dir(&workspace).unwrap();
    let git = |args: &[&str]| workspace_git(&workspace, args);
    let origin = "https://example.invalid/recorded.git";
    git(&["init", "--quiet"]);
    git(&[
        "remote",
        "add",
        "origin",
        "https://reader@example.invalid/recorded.git",
    ]);
    let commit = [
        "-c",
        "user.name=Test",
        "-c",
        "user.email=test@example.invalid",
        "-c",
        "commit.gpgsign=false",
        "commit",
        "--allow-empty",
        "-m",
        "fixture",
    ];
    git(&commit);
    let head = git(&["rev-parse", "HEAD"]);
    let sessions = root.join("codex/sessions");
    fs::create_dir_all(&sessions).unwrap();
    let record = json!({"type":"session_meta", "payload":{"id":SESSION_ID, "cwd":workspace,
        "git":{"commit_hash":head,"repository_url":origin}}});
    let request = json!({"type": "event_msg", "payload": {"type": "user_message", "message": "Synthetic request"}});
    fs::write(
        sessions.join(format!("rollout-2026-01-01T00-00-00-{SESSION_ID}.jsonl")),
        format!("{record}\n{request}\n"),
    )
    .unwrap();
    let source = format!("codex:{SESSION_ID}");
    let check = |directory: &Path, reference: &str, expected| {
        let inspected = successful_json(&mut inspect_workspace(root, directory, reference));
        let fork = successful_json(command(root, directory).args([
            "--json",
            "fork",
            reference,
            "--in",
            "codex",
            "--dry-run",
        ]));
        assert_eq!(inspected["repository_matches"], expected);
        assert_eq!(fork["fidelity"]["repository_matches"], expected);
    };
    check(&workspace, &source, true);
    check_profile_worktree_identity(root, &workspace, &source);
    git(&[
        "remote",
        "set-url",
        "origin",
        "https://example.invalid/other.git",
    ]);
    check(&workspace, &source, false);
    let bundle_file = root.join("export.json");
    successful_json(command(root, &workspace).args([
        "--json",
        "export",
        &source,
        "--output",
        bundle_file.to_str().unwrap(),
    ]));
    let bundle: PortableBundle = serde_json::from_slice(&fs::read(&bundle_file).unwrap()).unwrap();
    assert_eq!(
        bundle.snapshot.workspace.git.remote_fingerprint,
        Some(omnis_core::fingerprint(origin.as_bytes()))
    );
    assert_eq!(
        bundle.snapshot.workspace.git.head.as_deref(),
        Some(head.as_str())
    );
    git(&["remote", "set-url", "origin", origin]);
    git(&commit);
    check(&workspace, &source, false);
    let imported = format!("imported:{}", bundle.manifest.bundle_id);
    successful_json(command(root, &workspace).args([
        "--json",
        "import",
        bundle_file.to_str().unwrap(),
    ]));
    let moved = root.join("moved");
    fs::rename(&workspace, &moved).unwrap();
    check(&moved, &imported, false);
    workspace_git(&moved, &["checkout", "--quiet", "--detach", &head]);
    check(&moved, &imported, true);
}

#[test]
fn project_queries_keep_relative_codex_workspaces_in_global_history() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let original = root.join("original");
    let unrelated = root.join("unrelated");
    let sessions = root.join("codex/sessions");
    for dir in [&original, &unrelated, &sessions] {
        fs::create_dir_all(dir).unwrap();
    }
    let source = format!("codex:{SESSION_ID}");
    let file = sessions.join(format!("rollout-{SESSION_ID}.jsonl"));
    for cwd in [".", ""] {
        let rows = [
            json!({"type":"session_meta","payload":{"id":SESSION_ID,"cwd":original,"git":{"branch":"creation"}}}),
            json!({"type":"turn_context","payload":{"cwd":cwd}}),
            json!({"type":"event_msg","payload":{"type":"user_message","message":"A synthetic silverleaf question"}}),
        ];
        fs::write(&file, rows.map(|row| row.to_string()).join("\n")).unwrap();
        let show = successful_json(command(root, &original).args(["--json", "show", &source]));
        assert_eq!(show["workspace"]["current_dir"], "");
        successful_json(command(root, &original).args(["--json", "index", "--provider", "codex"]));
        for (args, key) in [
            (&["--json", "list", "--provider", "codex"][..], "sessions"),
            (
                &["--json", "search", "silverleaf", "--cached"][..],
                "results",
            ),
        ] {
            for project in [&unrelated, &original] {
                assert!(
                    successful_json(command(root, project).args(args))[key]
                        .as_array()
                        .unwrap()
                        .is_empty()
                );
            }
            let global =
                successful_json(command(root, &unrelated).args(args).arg("--all-projects"));
            let hits = global[key].as_array().unwrap();
            assert_eq!(hits.len(), 1, "{global}");
            assert!(hits[0]["project_path"].is_null(), "{global}");
        }
    }
}

#[test]
fn a_relative_codex_turn_directory_cannot_identify_a_native_or_imported_workspace() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let caller = root.join("caller");
    fs::create_dir(&caller).unwrap();
    for args in [
        &["init", "--quiet"][..],
        &[
            "remote",
            "add",
            "origin",
            "https://example.invalid/caller.git",
        ],
    ] {
        assert!(
            Command::new("git")
                .current_dir(&caller)
                .args(args)
                .status()
                .unwrap()
                .success()
        );
    }
    let sessions = root.join("codex/sessions");
    fs::create_dir_all(&sessions).unwrap();
    let path = sessions.join(format!("rollout-2026-01-01T00-00-00-{SESSION_ID}.jsonl"));
    let header =
        json!({"type":"session_meta","payload":{"id":SESSION_ID,"cwd":root.join("creation")}});
    let turn = json!({"type":"turn_context","payload":{"cwd":"."}});
    let request = json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Synthetic request"}]}});
    fs::write(&path, format!("{header}\n{turn}\n{request}\n")).unwrap();
    let native = format!("codex:{SESSION_ID}");
    let check = |reference: &str| {
        for target in ["codex", "claude"] {
            let mut fork = command(root, &caller);
            fork.args(["--json", "fork", reference, "--in", target, "--dry-run"]);
            let blocked = fork.output().unwrap();
            assert!(!blocked.status.success());
            assert!(
                String::from_utf8_lossy(&blocked.stderr).contains("--allow-workspace-mismatch")
            );
            let allowed = successful_json(fork.arg("--allow-workspace-mismatch"));
            assert_eq!(allowed["fidelity"]["repository_matches"], false);
            let inspect = successful_json(
                inspect_workspace(root, &caller, reference).args(["--target", target]),
            );
            assert_eq!(inspect["repository_matches"], false);
        }
    };
    check(&native);
    let file = root.join("export.json");
    successful_json(command(root, &caller).args([
        "--json",
        "export",
        &native,
        "--output",
        file.to_str().unwrap(),
    ]));
    let bundle: PortableBundle = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
    assert!(bundle.snapshot.workspace.git.remote_fingerprint.is_none());
    successful_json(command(root, &caller).args(["--json", "import", file.to_str().unwrap()]));
    fs::remove_file(path).unwrap();
    check(&format!("imported:{}", bundle.manifest.bundle_id));
}

#[test]
fn native_export_carries_repository_identity_for_relocated_continuation() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let workspace = root.join("source");
    let relocated = root.join("relocated");
    fs::create_dir_all(&workspace).unwrap();
    workspace_git(&workspace, &["init", "--quiet"]);
    workspace_git(
        &workspace,
        &[
            "remote",
            "add",
            "origin",
            "https://example.invalid/acme/continuity.git",
        ],
    );
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
    let step = IdeStep {
        r#type: 14,
        user_input: Some(IdeUserInput {
            user_response: "Synthetic IDE continuity request".to_owned(),
        }),
    };
    connection
        .execute(
            "INSERT INTO steps VALUES (0, ?1)",
            [prost::Message::encode_to_vec(&step)],
        )
        .unwrap();
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
