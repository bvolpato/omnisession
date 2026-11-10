use omnis_core::{
    HandoffMessage, HandoffRole, ImportTrajectory, TrajectoryItemKind, import_trajectory,
    native_trajectory_signature, needs_recovery_handoff, readback_trajectory, redact_secrets,
};
use omnis_ir::{CanonicalSnapshot, Provider, SessionRef};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    env, fs,
    io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};
use wait_timeout::ChildExt;

const CODEX_SOURCE_ID: &str = "11111111-1111-4111-8111-111111111111";
const CLAUDE_SOURCE_ID: &str = "22222222-2222-4222-8222-222222222222";
const COMPATIBILITY_MANIFEST: &str = include_str!("../provider-compatibility.json");
const REAL_COMMAND_OUTPUT_LIMIT: usize = 128 * 1024 * 1024;
const REAL_COMMAND_TIMEOUT: Duration = Duration::from_secs(120);
const REAL_SOURCE_HASH_MAX_BYTES: u64 = 1024 * 1024 * 1024;
const REAL_SOURCE_HASH_TIMEOUT: Duration = Duration::from_secs(120);

const REAL_TARGETS: &[(&str, &str, &str)] = &[
    ("claude", "OMNI_TEST_CLAUDE_BIN", "OMNI_CLAUDE_BIN"),
    ("codex", "OMNI_TEST_CODEX_BIN", "OMNI_CODEX_BIN"),
    ("opencode", "OMNI_TEST_OPENCODE_BIN", "OMNI_OPENCODE_BIN"),
    ("grok", "OMNI_TEST_GROK_BIN", "OMNI_GROK_BIN"),
    ("hermes", "OMNI_TEST_HERMES_BIN", "OMNI_HERMES_BIN"),
    (
        "antigravity",
        "OMNI_TEST_ANTIGRAVITY_BIN",
        "OMNI_ANTIGRAVITY_BIN",
    ),
    ("pi", "OMNI_TEST_PI_BIN", "OMNI_PI_BIN"),
    ("omp", "OMNI_TEST_OMP_BIN", "OMNI_OMP_BIN"),
    ("cursor", "OMNI_TEST_CURSOR_BIN", "OMNI_CURSOR_AGENT_BIN"),
    (
        "cursor-ide",
        "OMNI_TEST_CURSOR_IDE_BIN",
        "OMNI_CURSOR_IDE_BIN",
    ),
];

fn set_fixture_mode(path: &Path, mode: u32) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(path, fs::Permissions::from_mode(mode))
            .expect("set fixture permissions");
    }
    #[cfg(not(unix))]
    let _ = (path, mode);
}

#[test]
#[ignore = "requires OMNI_TEST_CLAUDE_BIN"]
fn installed_claude_round_trips_isolated_synthetic_history() {
    let fixture = Fixture::new();
    let source = fixture.write_codex_source();
    fixture.assert_materializes(
        &format!("codex:{CODEX_SOURCE_ID}"),
        "claude",
        "OMNI_TEST_CLAUDE_BIN",
        "OMNI_CLAUDE_BIN",
        &source,
    );
}

#[test]
#[ignore = "requires OMNI_TEST_CODEX_BIN"]
fn installed_codex_keeps_the_claude_reply_that_follows_a_compaction_summary() {
    let fixture = Fixture::new();
    let source = fixture.write_claude_source();
    let reply = "77777777-7777-4777-8777-777777777777";
    let boundary = "88888888-8888-4888-8888-888888888888";
    let summary = "99999999-9999-4999-8999-999999999999";
    let mut file = fs::OpenOptions::new().append(true).open(&source).unwrap();
    for record in [
        json!({
            "type": "system", "subtype": "compact_boundary", "uuid": boundary,
            "sessionId": CLAUDE_SOURCE_ID, "parentUuid": null, "logicalParentUuid": reply,
            "compactMetadata": {
                "preservedSegment": {"anchorUuid": summary, "headUuid": reply, "tailUuid": reply},
                "preservedMessages": {"anchorUuid": summary, "uuids": [reply], "allUuids": [reply]}
            }
        }),
        json!({
            "type": "user", "uuid": summary, "parentUuid": boundary,
            "sessionId": CLAUDE_SOURCE_ID, "isCompactSummary": true, "isVisibleInTranscriptOnly": true,
            "message": {"role": "user", "content": "The final answer is still pending."}
        }),
    ] {
        writeln!(file, "{record}").unwrap();
    }
    drop(file);

    let target = fixture.assert_materializes(
        &format!("claude:{CLAUDE_SOURCE_ID}"),
        "codex",
        "OMNI_TEST_CODEX_BIN",
        "OMNI_CODEX_BIN",
        &source,
    );
    let saved = fixture.snapshot(&target);
    let next_import = import_trajectory(&saved);
    assert!(!needs_recovery_handoff(&saved, next_import.truncated));
    let [notice, summary, reply] = next_import.items.as_slice() else {
        panic!("expected a recovery file reference, a summary, and the retained reply");
    };
    assert_eq!(notice.kind, TrajectoryItemKind::Assistant);
    assert!(notice.text.starts_with("[OmniSession recovery file]\n"));
    let path = Path::new(notice.text.lines().last().unwrap());
    assert!(path.starts_with(fixture.root.join("tmp")));
    assert!(
        fs::read_to_string(path)
            .unwrap()
            .contains("Synthetic opening question")
    );
    assert_eq!(summary.kind, TrajectoryItemKind::Assistant);
    assert!(summary.text.contains("The final answer is still pending."));
    assert_eq!(reply.kind, TrajectoryItemKind::Assistant);
    assert_eq!(reply.text, "Synthetic final answer");
}

#[test]
#[ignore = "requires OMNI_TEST_CODEX_BIN"]
fn installed_codex_round_trips_isolated_synthetic_history() {
    let fixture = Fixture::new();
    fixture.write_workspace_instructions();
    let source = fixture.write_claude_source();
    let target = fixture.assert_materializes(
        &format!("claude:{CLAUDE_SOURCE_ID}"),
        "codex",
        "OMNI_TEST_CODEX_BIN",
        "OMNI_CODEX_BIN",
        &source,
    );
    let snapshot = fixture.snapshot(&target);
    assert!(
        snapshot.events.iter().any(|event| {
            event
                .payload
                .get("total_tokens")
                .and_then(serde_json::Value::as_u64)
                .is_some_and(|tokens| tokens > 0)
        }),
        "Codex import did not persist non-zero context usage"
    );

    let state = fixture.root.join("omnisession");
    fs::create_dir_all(&state).unwrap();
    set_fixture_mode(&state, 0o700);
    let profile = state.join("profiles.toml");
    fs::write(
        &profile,
        r#"[profiles.codex-local]
agent = "codex"
args = ["--profile", "work", "-c", 'model_providers.gateway.base_url="http://127.0.0.1:1/v1"']
"#,
    )
    .unwrap();
    set_fixture_mode(&profile, 0o600);
    fs::write(
        fixture.codex.join("work.config.toml"),
        r#"model_provider = "gateway"
[model_providers.gateway]
name = "Synthetic gateway"
base_url = "http://127.0.0.1:1/v1"
"#,
    )
    .unwrap();
    let target = fixture.assert_materializes(
        &format!("claude:{CLAUDE_SOURCE_ID}"),
        "codex-local",
        "OMNI_TEST_CODEX_BIN",
        "OMNI_CODEX_BIN",
        &source,
    );
    let id = target.strip_prefix("codex:").unwrap();
    let rollout = walkdir::WalkDir::new(fixture.codex.join("sessions"))
        .into_iter()
        .filter_map(Result::ok)
        .find(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .ends_with(&format!("{id}.jsonl"))
        })
        .expect("imported rollout");
    let first = BufReader::new(fs::File::open(rollout.path()).unwrap())
        .lines()
        .next()
        .unwrap()
        .unwrap();
    let meta: serde_json::Value = serde_json::from_str(&first).unwrap();
    assert_eq!(meta["payload"]["model_provider"], "gateway");
}

#[test]
#[ignore = "requires OMNI_TEST_CURSOR_BIN"]
fn installed_cursor_round_trips_isolated_synthetic_history() {
    let fixture = Fixture::new();
    let source = fixture.write_codex_source();
    fixture.assert_materializes(
        &format!("codex:{CODEX_SOURCE_ID}"),
        "cursor",
        "OMNI_TEST_CURSOR_BIN",
        "OMNI_CURSOR_AGENT_BIN",
        &source,
    );
}

#[test]
#[ignore = "requires OMNI_TEST_PI_BIN"]
fn installed_pi_round_trips_isolated_synthetic_history() {
    let fixture = Fixture::new();
    let source = fixture.write_codex_source();
    fixture.assert_materializes(
        &format!("codex:{CODEX_SOURCE_ID}"),
        "pi",
        "OMNI_TEST_PI_BIN",
        "OMNI_PI_BIN",
        &source,
    );
}

#[test]
#[ignore = "requires OMNI_TEST_OMP_BIN"]
#[allow(clippy::too_many_lines)]
fn installed_omp_loads_imported_history_without_prompting() {
    let fixture = Fixture::new();
    let agent = fixture.home.join(".omp/agent");
    fs::create_dir_all(&agent).unwrap();
    fs::write(
        agent.join("models.yml"),
        json!({"providers":{"synthetic-omp":{
            "baseUrl":"http://127.0.0.1:1", "api":"openai-completions", "auth":"none",
            "models":[{"id":"historical-check", "name":"Synthetic history probe",
                "contextWindow":32768, "maxTokens":1024}]
        }}})
        .to_string(),
    )
    .unwrap();
    for (source_ref, source) in [
        (
            format!("codex:{CODEX_SOURCE_ID}"),
            fixture.write_codex_source(),
        ),
        (
            format!("claude:{CLAUDE_SOURCE_ID}"),
            fixture.write_claude_source(),
        ),
    ] {
        let source_before = fs::read(&source).unwrap();
        let target = fixture.assert_materializes(
            &source_ref,
            "omp",
            "OMNI_TEST_OMP_BIN",
            "OMNI_OMP_BIN",
            &source,
        );
        let binary = PathBuf::from(env::var_os("OMNI_TEST_OMP_BIN").unwrap());
        let listed = fixture
            .isolated_command()
            .args(["--json", "list", "--provider", "omp"])
            .output()
            .unwrap();
        assert_successful_command("OMP listing", &listed);
        let session_id = target.strip_prefix("omp:").unwrap();
        let path = find_file_ending_with(
            &fixture.root.join("omp/sessions"),
            &format!("_{session_id}.jsonl"),
        )
        .unwrap();
        fixture.assert_omp_permission_modes(&target, &binary, &path);
        for flag in ["--session", "--fork"] {
            let mut command = fixture.isolated_binary_command(&binary);
            command
                .args([
                    "--provider",
                    "synthetic-omp",
                    "--model",
                    "historical-check",
                    "--mode",
                    "rpc",
                    "--approval-mode",
                    "yolo",
                    "--no-tools",
                    "--no-lsp",
                    "--no-extensions",
                    "--no-skills",
                    "--no-rules",
                    "--no-title",
                    "--no-prewalk",
                    flag,
                ])
                .arg(&path)
                .arg("--session-dir")
                .arg(fixture.root.join("omp/sessions"))
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit());
            let response = run_omp_history_probe(command).unwrap_or_else(|error| {
                panic!("{source_ref} {flag}: OMP history probe failed: {error}")
            });
            assert_eq!(
                response["success"], true,
                "{source_ref} {flag}: OMP get_messages failed"
            );
            let messages = response["data"]["messages"].to_string();
            let opening = if source_ref.starts_with("claude:") {
                "Synthetic opening question"
            } else {
                "OMNISESSION_ALPHA_7319"
            };
            assert!(
                messages.contains(opening),
                "{source_ref} {flag}: imported opening content is missing"
            );
            assert!(
                messages.contains("Synthetic final answer"),
                "{source_ref} {flag}: imported final content is missing"
            );
            assert!(
                !messages.contains("synthetic-value"),
                "{source_ref} {flag}: sensitive synthetic field was imported"
            );
            if source_ref.starts_with("claude:") {
                assert!(messages.contains("hist_claude_"));
                assert!(
                    response["data"]["messages"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|message| message["role"] == "toolResult")
                );
            }
        }
        assert_eq!(fs::read(source).unwrap(), source_before);
    }
}

fn run_omp_history_probe(mut command: Command) -> Result<serde_json::Value, String> {
    let mut child = command.spawn().map_err(|error| error.to_string())?;
    let response = (|| {
        let stdout = child.stdout.take().expect("OMP RPC stdout");
        let (sender, receiver) = mpsc::sync_channel(1);
        thread::spawn(move || {
            let response = BufReader::new(stdout).lines().find_map(|line| match line {
                Ok(line) => serde_json::from_str::<serde_json::Value>(&line)
                    .ok()
                    .filter(|response| response["id"] == "history")
                    .map(Ok),
                Err(error) => Some(Err(error)),
            });
            let _ = sender.send(response);
        });
        let mut stdin = child.stdin.take().expect("OMP RPC stdin");
        stdin
            .write_all(b"{\"id\":\"history\",\"type\":\"get_messages\"}\n")
            .map_err(|error| error.to_string())?;
        receiver
            .recv_timeout(Duration::from_secs(30))
            .map_err(|error| format!("waiting for OMP history response: {error}"))?
            .ok_or_else(|| "OMP RPC stream closed before history response".to_owned())?
            .map_err(|error| error.to_string())
    })();
    let _ = child.kill();
    child.wait().map_err(|error| error.to_string())?;
    response
}

#[test]
#[ignore = "requires OMNI_TEST_ANTIGRAVITY_BIN"]
fn installed_antigravity_round_trips_isolated_synthetic_history() {
    let fixture = Fixture::new();
    fs::remove_file(fixture.antigravity.join("conversation_summaries.db"))
        .expect("remove only the fixture's synthetic Antigravity catalog");
    fs::remove_dir(&fixture.antigravity).expect("start without an initialized Antigravity profile");
    let source = fixture.write_codex_source();
    fixture.assert_materializes(
        &format!("codex:{CODEX_SOURCE_ID}"),
        "antigravity",
        "OMNI_TEST_ANTIGRAVITY_BIN",
        "OMNI_ANTIGRAVITY_BIN",
        &source,
    );
}

#[test]
#[ignore = "requires OMNI_TEST_HERMES_BIN"]
fn installed_hermes_round_trips_isolated_synthetic_history() {
    let fixture = Fixture::new();
    let source = fixture.write_codex_source();
    let target = fixture.assert_materializes(
        &format!("codex:{CODEX_SOURCE_ID}"),
        "hermes",
        "OMNI_TEST_HERMES_BIN",
        "OMNI_HERMES_BIN",
        &source,
    );
    let binary = env::var_os("OMNI_TEST_HERMES_BIN")
        .map(PathBuf::from)
        .expect("OMNI_TEST_HERMES_BIN");
    let binaries = [("hermes", "OMNI_HERMES_BIN", binary)];
    let oracle = fixture.trajectory(&target, &binaries);
    let fork = fixture.materialize(&target, "hermes", &binaries, &oracle);
    let snapshot = fixture.snapshot(&fork);
    assert!(snapshot.events.iter().any(|event| {
        event
            .payload
            .get("parent_session_id")
            .and_then(serde_json::Value::as_str)
            == target.strip_prefix("hermes:")
    }));

    // The Claude seed's complete Read pair must reach Hermes as native rows, never marked observed.
    let claude_source = fixture.write_claude_source();
    let native = fixture.assert_materializes(
        &format!("claude:{CLAUDE_SOURCE_ID}"),
        "hermes",
        "OMNI_TEST_HERMES_BIN",
        "OMNI_HERMES_BIN",
        &claude_source,
    );
    let connection =
        rusqlite::Connection::open(fixture.hermes.join("state.db")).expect("Hermes state database");
    let rows: (i64, i64, i64) = connection
        .query_row(
            "SELECT COALESCE(SUM(role = 'assistant' AND tool_calls LIKE '%hist_claude_%'), 0),
                    COALESCE(SUM(role = 'tool' AND tool_call_id IS NOT NULL), 0),
                    COALESCE(SUM(observed), 0)
             FROM messages WHERE session_id = ?1",
            [native.strip_prefix("hermes:").expect("Hermes target")],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("native Hermes rows");
    assert_eq!(rows, (1, 1, 0), "Hermes native tool rows");
}

#[test]
#[ignore = "requires OMNI_TEST_CURSOR_IDE_BIN"]
fn installed_cursor_ide_round_trips_isolated_synthetic_history() {
    let fixture = Fixture::new();
    let source = fixture.write_codex_source();
    fixture.assert_materializes(
        &format!("codex:{CODEX_SOURCE_ID}"),
        "cursor-ide",
        "OMNI_TEST_CURSOR_IDE_BIN",
        "OMNI_CURSOR_IDE_BIN",
        &source,
    );
}

#[test]
#[ignore = "requires OMNI_TEST_LIVE_PROMPTS=1 and authenticated OMNI_TEST_PI_BIN"]
fn live_pi_consumes_imported_context() {
    assert_eq!(env::var("OMNI_TEST_LIVE_PROMPTS").as_deref(), Ok("1"));
    let binary = env::var_os("OMNI_TEST_PI_BIN")
        .map(PathBuf::from)
        .expect("OMNI_TEST_PI_BIN");
    let binaries = [("pi", "OMNI_PI_BIN", binary.clone())];
    let fixture = Fixture::new();
    fixture.write_codex_source();
    let seed = format!("codex:{CODEX_SOURCE_ID}");
    let oracle = fixture.trajectory(&seed, &binaries);
    let target = fixture.materialize(&seed, "pi", &binaries, &oracle);
    let target_id = target.strip_prefix("pi:").expect("Pi target ID");
    let target_path = find_file_ending_with(&fixture.pi_sessions, &format!("_{target_id}.jsonl"))
        .expect("materialized Pi session file");

    for (prompt, expected) in continuity_questions() {
        let mut command = Command::new(&binary);
        command
            .args([
                "--print",
                "--no-tools",
                "--no-extensions",
                "--no-skills",
                "--no-prompt-templates",
                "--no-context-files",
                "--no-approve",
                "--session",
            ])
            .arg(&target_path)
            .arg(prompt)
            .current_dir(&fixture.workspace)
            .env("PI_CODING_AGENT_SESSION_DIR", &fixture.pi_sessions)
            .env("PI_SKIP_VERSION_CHECK", "1")
            .env("PI_TELEMETRY", "0");
        let output = command.output().expect("run Pi live continuity probe");
        assert_live_answer("Pi", &expected, &output);
    }
}

#[test]
#[ignore = "requires OMNI_TEST_LIVE_PROMPTS=1 and authenticated OMNI_TEST_OPENCODE_BIN"]
fn live_opencode_consumes_imported_context() {
    assert_eq!(env::var("OMNI_TEST_LIVE_PROMPTS").as_deref(), Ok("1"));
    let binary = env::var_os("OMNI_TEST_OPENCODE_BIN")
        .map(PathBuf::from)
        .expect("OMNI_TEST_OPENCODE_BIN");
    let binaries = [("opencode", "OMNI_OPENCODE_BIN", binary.clone())];
    let fixture = Fixture::new();
    fixture.write_codex_source();
    let seed = format!("codex:{CODEX_SOURCE_ID}");
    let oracle = fixture.trajectory(&seed, &binaries);
    let target = fixture.materialize(&seed, "opencode", &binaries, &oracle);
    let target_id = target
        .strip_prefix("opencode:")
        .expect("OpenCode target ID");

    for (prompt, expected) in continuity_questions() {
        let output = Command::new(&binary)
            .args(["run", "--session", target_id, "--pure", "--format", "json"])
            .arg(prompt)
            .current_dir(&fixture.workspace)
            .env("OPENCODE_DB", &fixture.opencode_database)
            .env("OPENCODE_DISABLE_AUTOUPDATE", "1")
            .output()
            .expect("run OpenCode live continuity probe");
        assert_live_answer("OpenCode", &expected, &output);
    }
}

#[test]
#[ignore = "requires all five OMNI_TEST_*_BIN variables"]
fn installed_five_by_five_cross_provider_matrix() {
    let binaries = [
        ("claude", "OMNI_TEST_CLAUDE_BIN", "OMNI_CLAUDE_BIN"),
        ("codex", "OMNI_TEST_CODEX_BIN", "OMNI_CODEX_BIN"),
        ("opencode", "OMNI_TEST_OPENCODE_BIN", "OMNI_OPENCODE_BIN"),
        ("grok", "OMNI_TEST_GROK_BIN", "OMNI_GROK_BIN"),
        ("cursor", "OMNI_TEST_CURSOR_BIN", "OMNI_CURSOR_AGENT_BIN"),
    ]
    .map(|(provider, test_variable, runtime_variable)| {
        let binary =
            env::var_os(test_variable).map_or_else(|| panic!("{test_variable}"), PathBuf::from);
        (provider, runtime_variable, binary)
    });
    let fixture = Fixture::new();
    fixture.write_codex_source();
    let seed = format!("codex:{CODEX_SOURCE_ID}");
    let oracle = fixture.trajectory(&seed, &binaries);
    let mut sources = vec![("codex", seed.clone())];
    let mut completed = 0;

    for (target, _, _) in &binaries {
        if *target == "codex" {
            continue;
        }
        sources.push((
            target,
            fixture.materialize(&seed, target, &binaries, &oracle),
        ));
        completed += 1;
    }

    for (source_provider, source) in &sources {
        for (target, _, _) in &binaries {
            if source_provider == target || *source_provider == "codex" {
                continue;
            }
            fixture.materialize(source, target, &binaries, &oracle);
            completed += 1;
        }
    }

    assert_eq!(completed, 20);
}

#[test]
#[ignore = "requires all six OMNI_TEST_*_BIN variables"]
fn installed_six_by_six_cross_provider_matrix() {
    let binaries = [
        ("claude", "OMNI_TEST_CLAUDE_BIN", "OMNI_CLAUDE_BIN"),
        ("codex", "OMNI_TEST_CODEX_BIN", "OMNI_CODEX_BIN"),
        ("opencode", "OMNI_TEST_OPENCODE_BIN", "OMNI_OPENCODE_BIN"),
        ("grok", "OMNI_TEST_GROK_BIN", "OMNI_GROK_BIN"),
        ("cursor", "OMNI_TEST_CURSOR_BIN", "OMNI_CURSOR_AGENT_BIN"),
        ("pi", "OMNI_TEST_PI_BIN", "OMNI_PI_BIN"),
    ]
    .map(|(provider, test_variable, runtime_variable)| {
        let binary =
            env::var_os(test_variable).map_or_else(|| panic!("{test_variable}"), PathBuf::from);
        (provider, runtime_variable, binary)
    });
    let fixture = Fixture::new();
    fixture.write_codex_source();
    let seed = format!("codex:{CODEX_SOURCE_ID}");
    let oracle = fixture.trajectory(&seed, &binaries);
    let mut sources = vec![("codex", seed.clone())];
    let mut completed = 0;

    for (target, _, _) in &binaries {
        if *target == "codex" {
            continue;
        }
        sources.push((
            target,
            fixture.materialize(&seed, target, &binaries, &oracle),
        ));
        completed += 1;
    }

    for (source_provider, source) in &sources {
        for (target, _, _) in &binaries {
            if source_provider == target || *source_provider == "codex" {
                continue;
            }
            fixture.materialize(source, target, &binaries, &oracle);
            completed += 1;
        }
    }

    assert_eq!(completed, 30);
}

#[test]
#[ignore = "requires all ten OMNI_TEST_*_BIN variables"]
fn installed_ten_by_ten_cross_provider_matrix() {
    let binaries = [
        ("claude", "OMNI_TEST_CLAUDE_BIN", "OMNI_CLAUDE_BIN"),
        ("codex", "OMNI_TEST_CODEX_BIN", "OMNI_CODEX_BIN"),
        ("opencode", "OMNI_TEST_OPENCODE_BIN", "OMNI_OPENCODE_BIN"),
        ("grok", "OMNI_TEST_GROK_BIN", "OMNI_GROK_BIN"),
        ("hermes", "OMNI_TEST_HERMES_BIN", "OMNI_HERMES_BIN"),
        (
            "antigravity",
            "OMNI_TEST_ANTIGRAVITY_BIN",
            "OMNI_ANTIGRAVITY_BIN",
        ),
        ("pi", "OMNI_TEST_PI_BIN", "OMNI_PI_BIN"),
        ("omp", "OMNI_TEST_OMP_BIN", "OMNI_OMP_BIN"),
        ("cursor", "OMNI_TEST_CURSOR_BIN", "OMNI_CURSOR_AGENT_BIN"),
        (
            "cursor-ide",
            "OMNI_TEST_CURSOR_IDE_BIN",
            "OMNI_CURSOR_IDE_BIN",
        ),
    ]
    .map(|(provider, test_variable, runtime_variable)| {
        let binary =
            env::var_os(test_variable).map_or_else(|| panic!("{test_variable}"), PathBuf::from);
        (provider, runtime_variable, binary)
    });
    let fixture = Fixture::new();
    fixture.write_codex_source();
    let seed = format!("codex:{CODEX_SOURCE_ID}");
    let oracle = fixture.trajectory(&seed, &binaries);
    let mut sources = vec![("codex", seed.clone())];
    let mut completed = 0;

    for (target, _, _) in &binaries {
        if *target == "codex" {
            continue;
        }
        sources.push((
            target,
            fixture.materialize(&seed, target, &binaries, &oracle),
        ));
        completed += 1;
    }

    for (source_provider, source) in &sources {
        for (target, _, _) in &binaries {
            if source_provider == target || *source_provider == "codex" {
                continue;
            }
            fixture.materialize(source, target, &binaries, &oracle);
            completed += 1;
        }
    }

    assert_eq!(completed, 90);
}

#[test]
#[ignore = "requires explicit OMNI_TEST_REAL_SESSIONS and OMNI_TEST_REAL_TARGETS"]
#[allow(clippy::too_many_lines)]
fn real_native_sources_round_trip_to_isolated_targets() {
    let sources = real_session_refs();
    let targets = real_target_specs();
    let fixture = Fixture::new();
    let source_state = fixture.root.join("real-source-state");
    fs::create_dir_all(&source_state).expect("create isolated source state");

    for (source_index, source) in sources.iter().enumerate() {
        let source_path = locate_real_source(source, &source_state);
        let source_files = real_source_files(source.provider, &source_path);
        let source_before = hash_source_files(&source_files);
        let bundle = fixture
            .root
            .join(format!("real-source-{source_index}.json"));

        let source_ref = source.to_string();
        let mut export = Command::new(env!("CARGO_BIN_EXE_omni"));
        export
            .env("OMNISESSION_HOME", &source_state)
            .args(["--json", "export", &source_ref, "--output"])
            .arg(&bundle);
        let exported = run_private_command(&mut export, "real source export");
        assert_private_command_success("real source export", &exported);

        let mut import = fixture.isolated_command();
        import.args(["--json", "import"]).arg(&bundle);
        let imported_output = run_private_command(&mut import, "real bundle import");
        assert_private_command_success("real bundle import", &imported_output);
        let import_metadata: serde_json::Value = serde_json::from_slice(&imported_output.stdout)
            .unwrap_or_else(|_| panic!("real bundle import returned invalid metadata"));
        let imported: SessionRef = serde_json::from_value(import_metadata["session"].clone())
            .unwrap_or_else(|_| panic!("real bundle import omitted its session reference"));
        assert!(
            imported.provider == Provider::Imported,
            "real bundle import returned an unexpected session type"
        );
        let imported_ref = imported.to_string();

        let mut imported_readback = fixture.isolated_command();
        imported_readback.args(["--json", "show", &imported_ref]);
        let imported_snapshot_output =
            run_private_command(&mut imported_readback, "imported bundle read-back");
        assert_private_command_success("imported bundle read-back", &imported_snapshot_output);
        let imported_snapshot: CanonicalSnapshot =
            serde_json::from_slice(&imported_snapshot_output.stdout)
                .unwrap_or_else(|_| panic!("imported bundle returned invalid read-back metadata"));
        let expected = import_trajectory(&imported_snapshot);
        assert!(
            !expected.items.is_empty(),
            "real source {} exported no visible history",
            source.provider
        );
        assert!(
            expected
                .items
                .iter()
                .any(|item| item.kind == TrajectoryItemKind::User)
                && expected
                    .items
                    .iter()
                    .any(|item| item.kind == TrajectoryItemKind::Assistant),
            "real source {} needs visible user and assistant history",
            source.provider
        );

        for (target, _, runtime_variable, binary) in &targets {
            let import_temp = tempfile::tempdir_in(&fixture.root).expect("isolated import files");
            let mut materialize = fixture.isolated_real_target_command(
                Path::new(env!("CARGO_BIN_EXE_omni")),
                runtime_variable,
                binary,
            );
            materialize
                .env("TMPDIR", import_temp.path())
                .env("TMP", import_temp.path())
                .env("TEMP", import_temp.path())
                .args([
                    "resume",
                    &imported_ref,
                    "--in",
                    target,
                    "--materialize-only",
                ])
                .arg("--allow-workspace-mismatch");
            let materialized = run_private_command(&mut materialize, "real target materialization");
            assert_private_command_success("real target materialization", &materialized);
            let target_ref = materialized_target_ref(&materialized.stdout)
                .unwrap_or_else(|| panic!("{target} omitted its materialized session reference"));
            let target_session = target_ref
                .parse::<SessionRef>()
                .unwrap_or_else(|_| panic!("{target} returned an invalid session reference"));
            let target_provider = (*target)
                .parse::<Provider>()
                .unwrap_or_else(|_| panic!("real target mapping contains an invalid provider"));
            assert!(
                target_session.provider == target_provider,
                "{target} materialized a session for a different provider"
            );
            let target_ref = target_session.to_string();

            let mut readback = fixture.isolated_real_target_command(
                Path::new(env!("CARGO_BIN_EXE_omni")),
                runtime_variable,
                binary,
            );
            readback.args(["--json", "show", &target_ref]);
            let target_output = run_private_command(&mut readback, "real target read-back");
            assert_private_command_success("real target read-back", &target_output);
            let target_snapshot: CanonicalSnapshot = serde_json::from_slice(&target_output.stdout)
                .unwrap_or_else(|_| panic!("{target} returned invalid read-back metadata"));
            assert!(
                target_snapshot.session == target_session,
                "{target} read-back returned a different session reference"
            );
            let actual = readback_trajectory(&target_snapshot);
            let mut expected = expected.clone();
            if needs_recovery_handoff(&imported_snapshot, expected.truncated) {
                let files: Vec<_> = fs::read_dir(import_temp.path())
                    .unwrap()
                    .map(Result::unwrap)
                    .map(|entry| entry.path())
                    .filter(|path| {
                        path.extension().is_some_and(|ext| ext == "md") && path.is_file()
                    })
                    .collect();
                let [path] = files.as_slice() else {
                    panic!("{target} should publish one Markdown recovery file");
                };
                let notice = actual
                    .as_ref()
                    .and_then(|trajectory| {
                        trajectory.items.iter().find(|item| {
                            item.kind == TrajectoryItemKind::Assistant
                                && item.text.starts_with("[OmniSession recovery file]\n")
                                && item.text.lines().last().map(Path::new) == Some(path)
                        })
                    })
                    .unwrap_or_else(|| {
                        panic!("{target} omitted the recovery file from visible history")
                    });
                expected.items.insert(0, notice.clone());
            }
            let (matches, expected_count, actual_count) =
                real_readback_comparison(target, &expected, actual.as_ref());
            assert!(
                matches,
                "real session {} to {target} read-back mismatch (expected {} opaque trajectory items, observed {} opaque trajectory items)",
                source.provider, expected_count, actual_count
            );
        }

        let source_after = hash_source_files(&source_files);
        assert!(
            source_before == source_after,
            "real source {} changed during the isolated round-trip",
            source.provider
        );
    }
}

fn real_readback_comparison(
    target: &str,
    expected: &ImportTrajectory,
    actual: Option<&ImportTrajectory>,
) -> (bool, usize, usize) {
    if matches!(
        target,
        "claude" | "grok" | "hermes" | "pi" | "omp" | "opencode"
    ) {
        let expected = native_trajectory_signature(expected);
        let actual = actual.map(native_trajectory_signature);
        let actual_count = actual.as_ref().map_or(0, Vec::len);
        return (
            actual.as_ref().is_some_and(|actual| *actual == expected),
            expected.len(),
            actual_count,
        );
    }

    let expected = handoff_messages(expected);
    let actual = actual.map(handoff_messages);
    let actual_count = actual.as_ref().map_or(0, Vec::len);
    let matches = actual.as_ref().is_some_and(|actual| {
        if target == "codex" {
            actual
                .len()
                .checked_sub(expected.len())
                .is_some_and(|prefix| prefix <= 16 && actual[prefix..] == expected)
        } else {
            *actual == expected
        }
    });
    (matches, expected.len(), actual_count)
}

fn handoff_messages(trajectory: &ImportTrajectory) -> Vec<HandoffMessage> {
    trajectory
        .items
        .iter()
        .map(|item| HandoffMessage {
            role: if item.kind == TrajectoryItemKind::User {
                HandoffRole::User
            } else {
                HandoffRole::Assistant
            },
            text: item.text.clone(),
        })
        .collect()
}

fn real_session_refs() -> Vec<SessionRef> {
    let values = required_csv("OMNI_TEST_REAL_SESSIONS");
    let mut sources = Vec::with_capacity(values.len());
    for value in values {
        let source = value
            .parse::<SessionRef>()
            .unwrap_or_else(|_| panic!("OMNI_TEST_REAL_SESSIONS requires provider-qualified refs"));
        assert!(
            !matches!(source.provider, Provider::Imported | Provider::GenericAcp)
                && source.id.chars().all(|character| !character.is_control()),
            "OMNI_TEST_REAL_SESSIONS contains an unsupported native ref"
        );
        assert!(
            !sources.contains(&source),
            "OMNI_TEST_REAL_SESSIONS contains a duplicate ref"
        );
        sources.push(source);
    }
    sources
}

fn real_target_specs() -> Vec<(&'static str, &'static str, &'static str, PathBuf)> {
    let values = required_csv("OMNI_TEST_REAL_TARGETS");
    let mut targets = Vec::with_capacity(values.len());
    for value in values {
        let (target, test_variable, runtime_variable) = REAL_TARGETS
            .iter()
            .copied()
            .find(|(target, _, _)| *target == value)
            .unwrap_or_else(|| panic!("OMNI_TEST_REAL_TARGETS contains an unsupported target"));
        assert!(
            !targets
                .iter()
                .any(|(selected, _, _, _)| *selected == target),
            "OMNI_TEST_REAL_TARGETS contains a duplicate target"
        );
        let binary = env::var_os(test_variable)
            .map(PathBuf::from)
            .and_then(|path| path.canonicalize().ok())
            .unwrap_or_else(|| panic!("{test_variable} must name an installed target binary"));
        targets.push((target, test_variable, runtime_variable, binary));
    }
    targets
}

fn required_csv(name: &str) -> Vec<String> {
    let raw = env::var(name).unwrap_or_else(|_| panic!("{name} must be set explicitly"));
    let values = raw
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    assert!(!values.is_empty(), "{name} must contain at least one value");
    values
}

fn locate_real_source(source: &SessionRef, source_state: &Path) -> PathBuf {
    let provider = source.provider.to_string();
    let limit = usize::MAX.to_string();
    let mut command = Command::new(env!("CARGO_BIN_EXE_omni"));
    command.env("OMNISESSION_HOME", source_state).args([
        "--json",
        "list",
        "--provider",
        &provider,
        "--all-projects",
        "--limit",
        &limit,
    ]);
    let output = run_private_command(&mut command, "real source metadata listing");
    assert_private_command_success("real source metadata listing", &output);
    let metadata: serde_json::Value = serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|_| panic!("real source metadata listing returned invalid JSON"));
    let source_metadata = metadata["sessions"]
        .as_array()
        .and_then(|sessions| {
            sessions.iter().find(|session| {
                session["session"]["provider"].as_str() == Some(provider.as_str())
                    && session["session"]["id"].as_str() == Some(source.id.as_str())
            })
        })
        .unwrap_or_else(|| panic!("OMNI_TEST_REAL_SESSIONS ref was not found in native metadata"));
    let path = PathBuf::from(
        source_metadata["source_path"]
            .as_str()
            .expect("real source metadata did not include a native source path"),
    );
    if path.is_absolute() {
        path
    } else {
        env::current_dir()
            .expect("test current directory")
            .join(path)
    }
}

fn real_source_files(provider: Provider, primary: &Path) -> Vec<PathBuf> {
    let mut paths = vec![primary.to_path_buf()];
    if matches!(
        primary.extension().and_then(|extension| extension.to_str()),
        Some("db" | "sqlite" | "sqlite3" | "vscdb")
    ) {
        add_sqlite_files(&mut paths, primary);
    }
    match provider {
        Provider::CursorCli => {
            if let Some(database) = primary.parent().map(|directory| directory.join("store.db")) {
                add_sqlite_files(&mut paths, &database);
            }
        }
        Provider::Grok => {
            if let Some(directory) = primary.parent() {
                paths.push(directory.join("updates.jsonl"));
                paths.push(directory.join("chat_history.jsonl"));
            }
            let grok_root = env::var_os("GROK_HOME")
                .map(PathBuf::from)
                .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".grok")));
            if let Some(database) =
                grok_root.map(|root| root.join("sessions/session_search.sqlite"))
            {
                add_sqlite_files(&mut paths, &database);
            }
        }
        _ => {}
    }
    paths.sort();
    paths.dedup();
    paths
}

fn add_sqlite_files(paths: &mut Vec<PathBuf>, database: &Path) {
    paths.push(database.to_path_buf());
    for suffix in ["-wal", "-shm"] {
        let mut sidecar = database.as_os_str().to_os_string();
        sidecar.push(suffix);
        paths.push(PathBuf::from(sidecar));
    }
}

fn hash_source_files(paths: &[PathBuf]) -> Vec<Option<[u8; 32]>> {
    paths
        .iter()
        .map(|path| {
            hash_source_file(path)
                .unwrap_or_else(|_| panic!("could not fingerprint real source files"))
        })
        .collect()
}

fn hash_source_file(path: &Path) -> io::Result<Option<[u8; 32]>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "real source fingerprint path is not a regular file",
        ));
    }
    if metadata.len() > REAL_SOURCE_HASH_MAX_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "real source fingerprint exceeds its size limit",
        ));
    }
    let mut file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut digest = Sha256::new();
    let mut buffer = [0; 16 * 1024];
    let started_at = Instant::now();
    let mut total_bytes = 0_u64;
    loop {
        if started_at.elapsed() > REAL_SOURCE_HASH_TIMEOUT {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "real source fingerprint exceeded its time limit",
            ));
        }
        let bytes = file.read(&mut buffer)?;
        if bytes == 0 {
            break;
        }
        total_bytes = total_bytes.saturating_add(bytes as u64);
        if total_bytes > REAL_SOURCE_HASH_MAX_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "real source fingerprint exceeded its size limit",
            ));
        }
        digest.update(&buffer[..bytes]);
    }
    let digest = digest.finalize();
    let mut hash = [0; 32];
    hash.copy_from_slice(&digest);
    Ok(Some(hash))
}

struct PrivateCommandOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
}

fn run_private_command(command: &mut Command, label: &str) -> PrivateCommandOutput {
    let mut output_file = tempfile::tempfile()
        .unwrap_or_else(|_| panic!("{label} private output file could not be created"));
    let child_stdout = output_file
        .try_clone()
        .unwrap_or_else(|_| panic!("{label} private output file could not be duplicated"));
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(child_stdout))
        .stderr(Stdio::null());
    let mut child = command
        .spawn()
        .unwrap_or_else(|_| panic!("{label} could not start"));
    let deadline = Instant::now() + REAL_COMMAND_TIMEOUT;
    let poll_interval = Duration::from_millis(100);
    let status = loop {
        let now = Instant::now();
        let remaining = deadline.saturating_duration_since(now);
        match child.wait_timeout(remaining.min(poll_interval)) {
            Ok(Some(status)) => break status,
            Ok(None) => {
                let output_size = output_file
                    .metadata()
                    .unwrap_or_else(|_| panic!("{label} private output size is unavailable"))
                    .len();
                if output_size > REAL_COMMAND_OUTPUT_LIMIT as u64 {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("{label} exceeded the private output limit");
                }
                if now >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("{label} exceeded the 120 second timeout");
                }
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("{label} process wait failed");
            }
        }
    };
    output_file
        .seek(SeekFrom::Start(0))
        .unwrap_or_else(|_| panic!("{label} private output could not be rewound"));
    let mut stdout = Vec::new();
    output_file
        .take((REAL_COMMAND_OUTPUT_LIMIT + 1) as u64)
        .read_to_end(&mut stdout)
        .unwrap_or_else(|_| panic!("{label} private output could not be read"));
    assert!(
        stdout.len() <= REAL_COMMAND_OUTPUT_LIMIT,
        "{label} exceeded the private output limit"
    );
    PrivateCommandOutput { status, stdout }
}

fn assert_private_command_success(label: &str, output: &PrivateCommandOutput) {
    assert!(
        output.status.success(),
        "{label} failed with status {}",
        output.status
    );
}

fn materialized_target_ref(stdout: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(stdout).ok()?;
    text.lines().find_map(|line| {
        line.strip_prefix("Created and verified ")
            .and_then(|value| value.strip_suffix('.'))
            .map(str::to_owned)
    })
}

struct Fixture {
    _temporary: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    workspace: PathBuf,
    claude: PathBuf,
    codex: PathBuf,
    cursor_chats: PathBuf,
    pi_sessions: PathBuf,
    antigravity: PathBuf,
    hermes: PathBuf,
    cursor_ide: PathBuf,
    opencode_database: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temporary = tempfile::tempdir().expect("temporary conformance root");
        let root =
            omnis_core::canonicalize_path(temporary.path()).expect("canonical conformance root");
        let fixture = Self {
            home: root.join("home"),
            workspace: root.join("workspace"),
            claude: root.join("claude"),
            codex: root.join("codex"),
            cursor_chats: root.join("cursor/chats"),
            pi_sessions: root.join("pi/sessions"),
            antigravity: root.join("antigravity"),
            hermes: root.join("hermes"),
            cursor_ide: root.join("cursor-ide/User"),
            opencode_database: root.join("opencode.db"),
            root,
            _temporary: temporary,
        };
        for directory in [
            &fixture.root.join("tmp"),
            &fixture.home,
            &fixture.workspace,
            &fixture.claude,
            &fixture.codex,
            &fixture.cursor_chats,
            &fixture.pi_sessions,
            &fixture.antigravity,
            &fixture.hermes,
            &fixture.cursor_ide,
        ] {
            fs::create_dir_all(directory).expect("isolated conformance directory");
        }
        fixture.create_antigravity_store();
        fixture.create_cursor_ide_store();
        fixture
    }

    fn write_workspace_instructions(&self) {
        fs::write(
            self.workspace.join("AGENTS.md"),
            "# Synthetic workspace instructions\n\nKeep imported history unchanged.\n",
        )
        .expect("synthetic workspace instructions");
    }

    fn write_codex_source(&self) -> PathBuf {
        let directory = self.codex.join("sessions/2026/01/01");
        fs::create_dir_all(&directory).expect("Codex source directory");
        let source = directory.join(format!(
            "rollout-2026-01-01T00-00-00-{CODEX_SOURCE_ID}.jsonl"
        ));
        let (_, continuity_marker) = continuity_question("opening-marker");
        let (_, constraint) = continuity_question("user-constraint");
        let (_, decision) = continuity_question("revised-decision");
        let (_, tool_result) = continuity_question("tool-result");
        let (_, pending) = continuity_question("pending-work");
        let records = [
            json!({
                "timestamp": "2026-01-01T00:00:00Z",
                "type": "session_meta",
                "payload": {
                    "id": CODEX_SOURCE_ID,
                    "cwd": self.workspace,
                    "cli_version": "synthetic",
                    "model_provider": "synthetic"
                }
            }),
            codex_message(
                "user",
                &format!(
                    "Remember continuity marker {continuity_marker}. Network constraint: {constraint}."
                ),
            ),
            codex_message("assistant", "Proposed strategy: OMNISESSION_RETRY_1000."),
            codex_message(
                "user",
                &format!("Reject that strategy. Accepted strategy: {decision}."),
            ),
            json!({
                "timestamp": "2026-01-01T00:00:03Z",
                "type": "response_item",
                "payload": {
                    "type": "function_call_output",
                    "call_id": "synthetic-tool",
                    "output": format!("Synthetic result: {tool_result}. secret=synthetic-value")
                }
            }),
            codex_message(
                "user",
                &format!("Pending action: {pending}. Keep the accepted strategy."),
            ),
            codex_message("assistant", "Synthetic final answer"),
        ];
        let document = records
            .iter()
            .map(serde_json::Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        fs::write(&source, document).expect("synthetic Codex source");
        source
    }

    fn write_claude_source(&self) -> PathBuf {
        let directory = self.claude.join("projects/synthetic");
        fs::create_dir_all(&directory).expect("Claude source directory");
        let source = directory.join(format!("{CLAUDE_SOURCE_ID}.jsonl"));
        let records = [
            json!({
                "type": "user",
                "uuid": "33333333-3333-4333-8333-333333333333",
                "sessionId": CLAUDE_SOURCE_ID,
                "timestamp": "2026-01-01T00:00:00Z",
                "cwd": self.workspace,
                "message": {"role": "user", "content": "Synthetic opening question"}
            }),
            json!({
                "type": "assistant",
                "uuid": "44444444-4444-4444-8444-444444444444",
                "sessionId": CLAUDE_SOURCE_ID,
                "timestamp": "2026-01-01T00:00:01Z",
                "cwd": self.workspace,
                "message": {
                    "role": "assistant",
                    "content": [
                        {"type": "text", "text": "Synthetic opening answer"},
                        {
                            "type": "tool_use",
                            "id": "synthetic-tool",
                            "name": "Read",
                            "input": {"path": "synthetic.txt"}
                        }
                    ]
                }
            }),
            json!({
                "type": "user",
                "uuid": "55555555-5555-4555-8555-555555555555",
                "sessionId": CLAUDE_SOURCE_ID,
                "timestamp": "2026-01-01T00:00:02Z",
                "cwd": self.workspace,
                "message": {
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": "synthetic-tool",
                        "content": "secret=synthetic-value"
                    }]
                }
            }),
            json!({
                "type": "user",
                "uuid": "66666666-6666-4666-8666-666666666666",
                "sessionId": CLAUDE_SOURCE_ID,
                "timestamp": "2026-01-01T00:00:03Z",
                "cwd": self.workspace,
                "message": {"role": "user", "content": "Synthetic final question"}
            }),
            json!({
                "type": "assistant",
                "uuid": "77777777-7777-4777-8777-777777777777",
                "sessionId": CLAUDE_SOURCE_ID,
                "timestamp": "2026-01-01T00:00:04Z",
                "cwd": self.workspace,
                "message": {
                    "role": "assistant",
                    "content": [{"type": "text", "text": "Synthetic final answer"}]
                }
            }),
        ];
        let document = records
            .iter()
            .map(serde_json::Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        fs::write(&source, document).expect("synthetic Claude source");
        source
    }

    fn assert_materializes(
        &self,
        source: &str,
        target: &str,
        test_binary_variable: &str,
        binary_variable: &str,
        source_path: &std::path::Path,
    ) -> String {
        let binary = env::var_os(test_binary_variable)
            .map_or_else(|| panic!("{test_binary_variable}"), PathBuf::from);
        let source_before = fs::read(source_path).expect("source before import");
        let output = self
            .command(source, target)
            .env(binary_variable, &binary)
            .output()
            .expect("run installed native conformance");

        assert_success(target, &output);
        assert_eq!(
            fs::read(source_path).expect("source after import"),
            source_before,
            "{target} import changed source session"
        );
        String::from_utf8(output.stdout)
            .expect("UTF-8 conformance output")
            .lines()
            .find_map(|line| {
                line.strip_prefix("Created and verified ")
                    .and_then(|value| value.strip_suffix('.'))
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| panic!("{target} conformance omitted target session ID"))
    }

    fn snapshot(&self, session: &str) -> CanonicalSnapshot {
        let output = self
            .isolated_command()
            .args(["--json", "show", session])
            .output()
            .expect("read materialized session");
        assert_successful_command("materialized session readback", &output);
        serde_json::from_slice(&output.stdout).expect("canonical materialized snapshot")
    }

    fn assert_omp_permission_modes(&self, target: &str, binary: &Path, session_path: &Path) {
        for (mode, prefix) in [
            ("default", vec![]),
            ("always-ask", vec!["--approval-mode", "always-ask"]),
            ("accept-edits", vec!["--approval-mode", "write"]),
            ("yolo", vec!["--approval-mode", "yolo"]),
        ] {
            let output = self
                .isolated_command()
                .env("OMNI_OMP_BIN", binary)
                .args(["--json", "resume", target, "--dry-run", "--mode", mode])
                .output()
                .expect("plan installed OMP permission mode");
            assert_successful_command("OMP permission mode", &output);
            let plan: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            let expected = prefix
                .into_iter()
                .chain(["--session", session_path.to_str().unwrap()])
                .collect::<Vec<_>>();
            assert_eq!(plan["launch"]["args"], json!(expected), "OMP {mode}");
        }
    }

    fn materialize(
        &self,
        source: &str,
        target: &str,
        binaries: &[(&str, &str, PathBuf)],
        oracle: &ImportTrajectory,
    ) -> String {
        let mut command = self.command(source, target);
        for (_, variable, binary) in binaries {
            command.env(variable, binary);
        }
        let output = command.output().expect("run installed matrix cell");
        assert_success(target, &output);
        let stdout = String::from_utf8(output.stdout).expect("UTF-8 conformance output");
        let target_session = stdout
            .lines()
            .find_map(|line| {
                line.strip_prefix("Created and verified ")
                    .and_then(|value| value.strip_suffix('.'))
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| panic!("{target} conformance omitted target session ID"));
        assert_eq!(
            self.trajectory(&target_session, binaries),
            *oracle,
            "{source} -> {target} changed canonical trajectory"
        );
        target_session
    }

    fn trajectory(&self, session: &str, binaries: &[(&str, &str, PathBuf)]) -> ImportTrajectory {
        let mut command = self.isolated_command();
        command.args(["--json", "show", session]);
        for (_, variable, binary) in binaries {
            command.env(variable, binary);
        }
        let output = command.output().expect("read installed matrix session");
        assert_successful_command("matrix readback", &output);
        let snapshot: CanonicalSnapshot =
            serde_json::from_slice(&output.stdout).expect("canonical matrix snapshot");
        let mut trajectory = import_trajectory(&snapshot);
        // Writers synthesize timestamps, and native tool records render different documentary text.
        for item in &mut trajectory.items {
            item.timestamp = None;
            item.tool = None;
        }
        trajectory
    }

    fn command(&self, source: &str, target: &str) -> Command {
        let mut command = self.isolated_command();
        command
            .args(["resume", source, "--in", target, "--materialize-only"])
            .current_dir(&self.workspace);
        command
    }

    fn isolated_command(&self) -> Command {
        self.isolated_binary_command(Path::new(env!("CARGO_BIN_EXE_omni")))
    }

    fn isolated_binary_command(&self, binary: &Path) -> Command {
        let mut command = Command::new(binary);
        command
            .current_dir(&self.workspace)
            .env("HOME", &self.home)
            .env("TMPDIR", self.root.join("tmp"))
            .env("TEMP", self.root.join("tmp"))
            .env("TMP", self.root.join("tmp"))
            .env("XDG_CONFIG_HOME", self.root.join("xdg"))
            .env("CLAUDE_CONFIG_DIR", &self.claude)
            .env("CODEX_HOME", &self.codex)
            .env("CURSOR_AGENT_HOME", &self.cursor_chats)
            .env("CURSOR_CONFIG_DIR", self.root.join("cursor"))
            .env("PI_CODING_AGENT_SESSION_DIR", &self.pi_sessions)
            .env("PI_CODING_AGENT_DIR", self.home.join(".omp/agent"))
            .env("OMP_PROFILE", "")
            .env("OMP_SESSION_DIR", self.root.join("omp/sessions"))
            .env("ANTIGRAVITY_CLI_HOME", &self.antigravity)
            .env("ANTIGRAVITY_IDE_HOME", self.root.join("antigravity-ide"))
            .env(
                "ANTIGRAVITY_DESKTOP_HOME",
                self.root.join("antigravity-desktop"),
            )
            .env(
                "ANTIGRAVITY_IDE_USER_DATA_DIR",
                self.root.join("antigravity-ide-user-data"),
            )
            .env("HERMES_HOME", &self.hermes)
            .env("CURSOR_IDE_HOME", &self.cursor_ide)
            .env("GROK_HOME", self.root.join("grok"))
            .env("OMNISESSION_HOME", self.root.join("omnisession"))
            .env("OPENCODE_TEST_HOME", &self.home)
            .env("OPENCODE_DB", &self.opencode_database)
            .env("OPENCODE_DISABLE_AUTOUPDATE", "1")
            .env("OPENCODE_CONFIG_CONTENT", "{}");
        command
    }

    fn isolated_real_target_command(
        &self,
        omni_binary: &Path,
        runtime_variable: &str,
        provider_binary: &Path,
    ) -> Command {
        for directory in [
            self.root.join("xdg"),
            self.root.join("xdg-data"),
            self.root.join("xdg-cache"),
            self.root.join("tmp"),
            self.root.join("cursor"),
            self.root.join("grok"),
            self.root.join("omp/sessions"),
            self.root.join("antigravity-ide"),
            self.root.join("antigravity-desktop"),
            self.root.join("antigravity-ide-user-data"),
        ] {
            fs::create_dir_all(directory).expect("create isolated real target directory");
        }

        let mut command = Command::new(omni_binary);
        command.env_clear();
        for variable in ["PATH", "SYSTEMROOT", "WINDIR", "COMSPEC", "PATHEXT"] {
            if let Some(value) = env::var_os(variable) {
                command.env(variable, value);
            }
        }
        command
            .current_dir(&self.workspace)
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("APPDATA", self.home.join("AppData/Roaming"))
            .env("LOCALAPPDATA", self.home.join("AppData/Local"))
            .env("TMPDIR", self.root.join("tmp"))
            .env("TMP", self.root.join("tmp"))
            .env("TEMP", self.root.join("tmp"))
            .env("XDG_CONFIG_HOME", self.root.join("xdg"))
            .env("XDG_DATA_HOME", self.root.join("xdg-data"))
            .env("XDG_CACHE_HOME", self.root.join("xdg-cache"))
            .env("CLAUDE_CONFIG_DIR", &self.claude)
            .env("CODEX_HOME", &self.codex)
            .env("CURSOR_AGENT_HOME", &self.cursor_chats)
            .env("CURSOR_CONFIG_DIR", self.root.join("cursor"))
            .env("PI_CODING_AGENT_SESSION_DIR", &self.pi_sessions)
            .env("PI_CODING_AGENT_DIR", self.home.join(".omp/agent"))
            .env("OMP_PROFILE", "")
            .env("OMP_SESSION_DIR", self.root.join("omp/sessions"))
            .env("ANTIGRAVITY_CLI_HOME", &self.antigravity)
            .env("ANTIGRAVITY_IDE_HOME", self.root.join("antigravity-ide"))
            .env(
                "ANTIGRAVITY_DESKTOP_HOME",
                self.root.join("antigravity-desktop"),
            )
            .env(
                "ANTIGRAVITY_IDE_USER_DATA_DIR",
                self.root.join("antigravity-ide-user-data"),
            )
            .env("HERMES_HOME", &self.hermes)
            .env("CURSOR_IDE_HOME", &self.cursor_ide)
            .env("GROK_HOME", self.root.join("grok"))
            .env("OMNISESSION_HOME", self.root.join("omnisession"))
            .env("OPENCODE_TEST_HOME", &self.home)
            .env("OPENCODE_DB", &self.opencode_database)
            .env("OPENCODE_DISABLE_AUTOUPDATE", "1")
            .env("OPENCODE_CONFIG_CONTENT", "{}")
            .env(runtime_variable, provider_binary);
        command
    }

    fn create_antigravity_store(&self) {
        let connection =
            rusqlite::Connection::open(self.antigravity.join("conversation_summaries.db"))
                .expect("Antigravity summary database");
        connection
            .execute_batch(
                r"
                CREATE TABLE conversation_summaries (
                    conversation_id text PRIMARY KEY, title text NOT NULL DEFAULT '',
                    preview text NOT NULL DEFAULT '', step_count integer NOT NULL DEFAULT 0,
                    last_modified_time datetime NOT NULL, workspace_uris text NOT NULL,
                    status text NOT NULL DEFAULT '', source text NOT NULL DEFAULT '',
                    project_id text NOT NULL DEFAULT '', agent_name text NOT NULL DEFAULT '',
                    parent_conversation_id text NOT NULL DEFAULT '', nesting_depth integer NOT NULL DEFAULT 0,
                    battle_id text NOT NULL DEFAULT '', winning_conversation_id text NOT NULL DEFAULT '',
                    not_fully_idle numeric NOT NULL DEFAULT false, killed numeric NOT NULL DEFAULT false,
                    last_user_input_time datetime NOT NULL, last_user_input_step_index integer NOT NULL DEFAULT -1,
                    app_data_dir text NOT NULL DEFAULT ''
                );
                PRAGMA user_version = 1;
                ",
            )
            .expect("Antigravity summary schema");
    }

    fn create_cursor_ide_store(&self) {
        let workspace_id = "cursor-ide-fixture";
        let workspace_root = self.cursor_ide.join("workspaceStorage").join(workspace_id);
        fs::create_dir_all(self.cursor_ide.join("globalStorage"))
            .expect("Cursor IDE global storage");
        fs::create_dir_all(&workspace_root).expect("Cursor IDE workspace storage");
        fs::write(
            workspace_root.join("workspace.json"),
            serde_json::to_vec(&json!({
                "folder": format!("file://{}", self.workspace.display())
            }))
            .expect("Cursor IDE workspace JSON"),
        )
        .expect("Cursor IDE workspace metadata");
        let connection =
            rusqlite::Connection::open(self.cursor_ide.join("globalStorage/state.vscdb"))
                .expect("Cursor IDE state database");
        connection
            .execute_batch(
                "PRAGMA user_version = 1;
                 CREATE TABLE composerHeaders (composerId TEXT PRIMARY KEY, workspaceId TEXT, createdAt INTEGER, lastUpdatedAt INTEGER, isArchived INTEGER, isSubagent INTEGER, recency INTEGER, checkpointAt INTEGER, value TEXT);
                 CREATE TABLE cursorDiskKV (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB);
                 CREATE TABLE ItemTable (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB);",
            )
            .expect("Cursor IDE state schema");
    }
}

fn assert_success(target: &str, output: &std::process::Output) {
    assert_successful_command(&format!("{target} conformance"), output);
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("Created and verified "),
        "{target} conformance omitted verification confirmation"
    );
}

fn assert_successful_command(label: &str, output: &std::process::Output) {
    assert!(
        output.status.success(),
        "{label} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_live_answer(provider: &str, expected: &str, output: &std::process::Output) {
    assert!(
        output.status.success(),
        "{provider} live continuity probe exited with {}: {}",
        output.status,
        redact_secrets(&String::from_utf8_lossy(&output.stderr))
    );
    assert!(
        live_answer_text(provider, &output.stdout) == expected,
        "{provider} live continuity probe did not recover expected answer"
    );
}

fn live_answer_text(provider: &str, stdout: &[u8]) -> String {
    let output = String::from_utf8_lossy(stdout);
    if provider != "OpenCode" {
        return output.trim().to_owned();
    }
    output
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|event| event["type"] == "text")
        .filter_map(|event| event["part"]["text"].as_str().map(str::to_owned))
        .collect::<String>()
        .trim()
        .to_owned()
}

#[test]
fn continuity_probe_checks_answer_text_and_rejects_history_echoes() {
    let expected = "OMNISESSION_ALPHA_7319";
    let echo = format!(
        "{{\"type\":\"step_start\",\"history\":\"{expected}\"}}\n{{\"type\":\"text\",\"part\":{{\"text\":\"wrong answer\"}}}}\n"
    );
    assert!(echo.contains(expected));
    assert_eq!(
        live_answer_text("OpenCode", echo.as_bytes()),
        "wrong answer"
    );
    let answer = format!("{{\"type\":\"text\",\"part\":{{\"text\":\"{expected}\"}}}}\n");
    assert_eq!(live_answer_text("OpenCode", answer.as_bytes()), expected);
    assert_eq!(
        live_answer_text("Pi", format!("{expected}\n").as_bytes()),
        expected
    );
    assert_ne!(
        live_answer_text(
            "Pi",
            format!("Earlier: {expected}.\nwrong answer").as_bytes()
        ),
        expected
    );
}

fn continuity_questions() -> Vec<(String, String)> {
    let manifest: serde_json::Value =
        serde_json::from_str(COMPATIBILITY_MANIFEST).expect("compatibility manifest JSON");
    manifest["continuity_questions"]
        .as_array()
        .expect("continuity_questions array")
        .iter()
        .map(|question| {
            (
                question["prompt"]
                    .as_str()
                    .expect("continuity prompt")
                    .to_owned(),
                question["expected"]
                    .as_str()
                    .expect("continuity answer")
                    .to_owned(),
            )
        })
        .collect()
}

fn continuity_question(id: &str) -> (String, String) {
    let manifest: serde_json::Value =
        serde_json::from_str(COMPATIBILITY_MANIFEST).expect("compatibility manifest JSON");
    manifest["continuity_questions"]
        .as_array()
        .expect("continuity_questions array")
        .iter()
        .find(|question| question["id"].as_str() == Some(id))
        .map_or_else(
            || panic!("missing continuity question {id}"),
            |question| {
                let prompt = question["prompt"]
                    .as_str()
                    .expect("continuity question prompt")
                    .to_owned();
                let expected = question["expected"]
                    .as_str()
                    .expect("continuity question expected answer")
                    .to_owned();
                (prompt, expected)
            },
        )
}

fn find_file_ending_with(root: &std::path::Path, suffix: &str) -> Option<PathBuf> {
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(directory).ok()?.flatten() {
            let file_type = entry.file_type().ok()?;
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                pending.push(entry.path());
            } else if file_type.is_file()
                && entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.ends_with(suffix))
            {
                return Some(entry.path());
            }
        }
    }
    None
}

fn codex_message(role: &str, text: &str) -> serde_json::Value {
    json!({
        "timestamp": "2026-01-01T00:00:01Z",
        "type": "response_item",
        "payload": {
            "type": "message",
            "role": role,
            "content": [{
                "type": if role == "user" { "input_text" } else { "output_text" },
                "text": text
            }]
        }
    })
}
