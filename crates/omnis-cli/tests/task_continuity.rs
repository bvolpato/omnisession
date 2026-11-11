use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use omnis_ir::PortableBundle;
use serde_json::{Value, json};

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
    fs::write(
        projects.join(format!("{SESSION_ID}.jsonl")),
        json!({"type":"user", "sessionId":SESSION_ID, "uuid":"synthetic-user", "cwd":workspace, "timestamp":"2026-01-01T00:00:00Z", "message":{"role":"user","content":"Synthetic continuity request"}}).to_string(),
    )
    .unwrap();
    fs::write(
        root.join("claude/history.jsonl"),
        json!({"sessionId":SESSION_ID,"project":workspace,"timestamp":1_767_225_600_000_i64})
            .to_string(),
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
    fs::write(
        projects.join(format!("{REPLACEMENT_ID}.jsonl")),
        json!({"type":"user", "sessionId":REPLACEMENT_ID, "uuid":"synthetic-replacement", "cwd":workspace, "timestamp":"2026-01-02T00:00:00Z", "message":{"role":"user","content":"Synthetic replacement request"}}).to_string(),
    )
    .unwrap();
    let bound = successful_json(command(root, &workspace).args([
        "--json",
        "task",
        "bind",
        &format!("claude:{REPLACEMENT_ID}"),
    ]));
    assert_eq!(bound["session"]["id"], REPLACEMENT_ID);
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
    let rollout = sessions.join(format!("rollout-{SESSION_ID}.jsonl"));
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
    let fork = |directory: &Path, expected: bool| {
        let inspected =
            successful_json(command(root, directory).args(["--json", "inspect", &source]));
        let plan = successful_json(command(root, directory).args([
            "--json",
            "fork",
            &source,
            "--in",
            "codex",
            "--dry-run",
        ]));
        assert_eq!(
            plan["launch"]["cwd"],
            json!(omnis_core::canonicalize_path(directory).unwrap())
        );
        assert_eq!(plan["fidelity"]["repository_matches"], expected);
        assert_eq!(inspected["repository_matches"], expected);
    };
    write(&records);
    fork(&moved, true);
    let wrong = command(root, &initial)
        .args(["--json", "fork", &source, "--in", "codex", "--dry-run"])
        .output()
        .unwrap();
    assert!(!wrong.status.success());
    assert!(String::from_utf8_lossy(&wrong.stderr).contains("differs from current"));
    records.push(json!({"type":"event_msg","payload":{"type":"thread_rolled_back","num_turns":1}}));
    write(&records);
    fork(&initial, false);
}

#[test]
fn codex_source_reads_update_project_discovery_and_search() {
    for show_first in [true, false] {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        let original = root.join("original");
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
        fs::File::open(&replacement)
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
        fs::File::open(&source)
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

#[test]
fn codex_repository_matches_its_recorded_remote_and_commit() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let workspace = root.join("workspace");
    fs::create_dir(&workspace).unwrap();
    let git = |args: &[&str]| {
        let output = Command::new("git")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("GIT_CONFIG_COUNT")
            .env_remove("GIT_CONFIG_PARAMETERS")
            .current_dir(&workspace)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    };
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
    fs::write(
        sessions.join(format!("rollout-{SESSION_ID}.jsonl")),
        record.to_string(),
    )
    .unwrap();
    let source = format!("codex:{SESSION_ID}");
    let check = |expected| {
        let inspected =
            successful_json(command(root, &workspace).args(["--json", "inspect", &source]));
        let fork = successful_json(command(root, &workspace).args([
            "--json",
            "fork",
            &source,
            "--in",
            "codex",
            "--dry-run",
        ]));
        assert_eq!(inspected["repository_matches"], expected);
        assert_eq!(fork["fidelity"]["repository_matches"], expected);
    };
    check(true);
    git(&[
        "remote",
        "set-url",
        "origin",
        "https://example.invalid/other.git",
    ]);
    check(false);
    let bundle_file = root.join("export.json");
    successful_json(command(root, &workspace).args([
        "--json",
        "export",
        &source,
        "--output",
        bundle_file.to_str().unwrap(),
    ]));
    let bundle: PortableBundle = serde_json::from_slice(&fs::read(bundle_file).unwrap()).unwrap();
    assert_eq!(
        bundle.snapshot.workspace.git.remote_fingerprint,
        Some(omnis_core::fingerprint(origin.as_bytes()))
    );
    assert_eq!(bundle.snapshot.workspace.git.head, Some(head));
    git(&["remote", "set-url", "origin", origin]);
    git(&commit);
    check(false);
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
fn missing_target_binary_falls_back_to_semantic_handoff() {
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
