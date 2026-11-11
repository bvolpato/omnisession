//! Provider launches driven through real `omni` processes.
//! Windows npm launchers run through `node.exe`, never `cmd.exe`.

#[cfg(unix)]
use std::{
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::{Command, Output, Stdio},
};

#[cfg(unix)]
use serde_json::json;

#[cfg(unix)]
const SOURCE_ID: &str = "11111111-1111-4111-8111-111111111111";
#[cfg(unix)]
const LAUNCHER: &str = r#"#!/bin/sh
if [ "$1" = "--version" ]; then
    printf '0.0.0 (Claude Code)\n'
    exit 0
fi
for prompt do printf '%s\000' "$prompt" >> "$LAUNCH_CAPTURE/args"; done
handoff=${prompt#*\`}
handoff=${handoff%%\`*}
printf '%s' "$handoff" > "$LAUNCH_CAPTURE/path"
pwd > "$LAUNCH_CAPTURE/cwd"
read -r input
[ "$input" = "synthetic stdin" ] || exit 91
/bin/cp "$handoff" "$LAUNCH_CAPTURE/document" 2>/dev/null || exit 92
printf 'synthetic stdout\n'
printf 'synthetic stderr\n' >&2
if [ "$LAUNCH_SIGNAL_EXIT" = "1" ]; then kill -TERM "$$"; fi
exit "$LAUNCH_EXIT_CODE"
"#;

#[cfg(any(unix, windows))]
fn codex_sqlite_override(root: &std::path::Path) -> String {
    let home = serde_json::to_string(&root.join("codex")).expect("UTF-8 fixture path");
    format!("sqlite_home={home}")
}

#[cfg(unix)]
#[test]
fn codex_resumes_in_the_surviving_subdirectory_of_a_matching_repository() {
    let fixture = Fixture::new();
    let subdirectory = fixture.workspace.join("nested");
    let caller = fixture.workspace.join("sibling");
    fs::create_dir(&subdirectory).unwrap();
    fs::create_dir(&caller).unwrap();
    fixture.init_git();
    let path = fixture.root.join(format!(
        "codex/sessions/rollout-2026-01-01T00-00-00-{SOURCE_ID}.jsonl"
    ));
    let mut file = fs::OpenOptions::new().append(true).open(path).unwrap();
    writeln!(
        file,
        "\n{}",
        json!({"type":"turn_context","payload":{"cwd":subdirectory}})
    )
    .unwrap();
    drop(file);

    let plan = |options: &[&str]| {
        let output = fixture
            .command()
            .current_dir(&caller)
            .args([
                "--json",
                "resume",
                SOURCE_ID,
                "--in",
                "codex",
                "--no-fork",
                "--dry-run",
            ])
            .args(options)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()["launch"].clone()
    };
    let resumed = plan(&[]);
    assert_eq!(resumed["cwd"], json!(subdirectory));
    assert_eq!(
        resumed["args"],
        json!([
            "--cd",
            ".",
            "-c",
            codex_sqlite_override(&fixture.root),
            "resume",
            SOURCE_ID
        ])
    );
    assert_eq!(plan(&["--allow-workspace-mismatch"])["cwd"], json!(caller));
}

#[cfg(unix)]
#[test]
fn a_cold_native_fork_uses_the_directory_inherited_at_its_bookmark() {
    const CHILD: &str = "22222222-2222-4222-8222-222222222222";
    let fixture = Fixture::new();
    let inherited = fixture.workspace.join("inherited");
    fs::create_dir(&inherited).unwrap();
    fixture.init_git();
    let rollout = |id| {
        fixture.root.join(format!(
            "codex/sessions/rollout-2026-01-01T00-00-00-{id}.jsonl"
        ))
    };
    let header = |id, ordinal| {
        json!({"type":"session_meta","ordinal":ordinal,"payload":{
            "id":id,"cwd":fixture.workspace,"history_mode":"paginated"
        }})
    };
    let context = json!({"type":"turn_context","ordinal":1,"payload":{"cwd":inherited}});
    let parent = format!("{}\n{context}\n", header(SOURCE_ID, 0));
    let mut child = header(CHILD, 2);
    child["payload"]["history_base"] = json!({
        "thread_id":SOURCE_ID,"end_byte_offset":parent.len(),"end_ordinal_exclusive":2
    });
    fs::write(rollout(SOURCE_ID), parent).unwrap();
    fs::write(rollout(CHILD), format!("{child}\n")).unwrap();
    let codex = fixture.root.join("fake-codex");
    fs::write(
        &codex,
        "#!/bin/sh\n[ \"$1\" = --version ] && echo 'codex-cli 0.167.0'\n",
    )
    .unwrap();
    fs::set_permissions(&codex, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(
        output_after_write(Command::new(&codex).arg("--version"))
            .status
            .success()
    );

    let output = fixture
        .command()
        .env("OMNI_CODEX_BIN", &codex)
        .args(["--json", "fork", CHILD, "--in", "codex", "--dry-run"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let plan: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(plan["launch"]["cwd"], json!(inherited));
    let args = plan["launch"]["args"].as_array().unwrap();
    assert!(args.ends_with(&[json!("fork"), json!(CHILD)]));
}

#[cfg(unix)]
#[test]
fn legacy_opencode_resolves_the_last_listed_id_and_retries_export_in_its_directory() {
    let fixture = Fixture::new();
    let directory = fixture.workspace.join("subdir");
    fs::create_dir(&directory).unwrap();
    fixture.init_git();
    let id = "ses_selected";
    let mut rows: Vec<_> = (0..10_000)
        .map(|i| json!({"id": format!("ses_other_{i}")}))
        .collect();
    rows.push(json!({"id": id, "directory": directory}));
    let listing = fixture.root.join("list.json");
    fs::write(&listing, serde_json::to_vec(&rows).unwrap()).unwrap();
    let opencode = fixture.root.join("opencode");
    fs::write(&opencode, r#"#!/bin/sh
case "$1:$2:$3" in
    --version:*) echo 1.0.0 ;;
    --pure:session:list) cat "$LIST_FILE" ;;
    --pure:export:ses_selected)
        [ . -ef "$SESSION_DIR" ] || exit 1
        echo '{"info":{"id":"ses_selected"},"messages":[{"info":{"role":"user"},"parts":[{"type":"text","text":"Recorded directory"}]}]}' ;;
    *) exit 1 ;;
esac
"#).unwrap();
    fs::set_permissions(&opencode, fs::Permissions::from_mode(0o700)).unwrap();
    let primed = output_after_write(Command::new(&opencode).arg("--version"));
    assert!(primed.status.success());
    let run = |args: &[&str]| {
        fixture
            .command()
            .env("OMNI_OPENCODE_BIN", &opencode)
            .env("LIST_FILE", &listing)
            .env("SESSION_DIR", &directory)
            .arg("--json")
            .args(args)
            .output()
            .unwrap()
    };
    let resumed = run(&["resume", id, "--dry-run"]);
    let shown = run(&["show", "opencode:ses_selected"]);
    assert!(
        resumed.status.success() && shown.status.success(),
        "resume: {}\nshow: {}",
        String::from_utf8_lossy(&resumed.stderr),
        String::from_utf8_lossy(&shown.stderr)
    );
    assert!(String::from_utf8_lossy(&shown.stdout).contains("Recorded directory"));
}

#[cfg(unix)]
#[test]
fn resume_launch_keeps_provider_exit_status() {
    for (exit_code, signal_exit) in [(23, false), (143, true)] {
        let fixture = Fixture::new();
        let mut child = fixture
            .command()
            .args(["resume", &format!("codex:{SOURCE_ID}"), "--in", "claude"])
            .env("LAUNCH_EXIT_CODE", exit_code.to_string())
            .env("LAUNCH_SIGNAL_EXIT", if signal_exit { "1" } else { "0" })
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("launch synthetic resume");
        child
            .stdin
            .take()
            .expect("child stdin")
            .write_all(b"synthetic stdin\n")
            .expect("provide inherited stdin");
        let output = child.wait_with_output().expect("wait for synthetic resume");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("synthetic stderr"), "{stderr}");
        assert_eq!(output.status.code(), Some(exit_code), "{stderr}");
        let path = fs::read_to_string(fixture.capture.join("path")).unwrap();
        let handoff = PathBuf::from(path);
        assert_eq!(handoff.parent(), Some(fixture.root.join("tmp").as_path()));
        assert_eq!(
            fs::read(&handoff).unwrap(),
            fs::read(fixture.capture.join("document")).unwrap()
        );
    }
}

#[cfg(unix)]
#[test]
fn bare_id_forks_a_codex_subagent_thread_that_listings_hide() {
    const SUBAGENT_ID: &str = "22222222-2222-4222-8222-222222222222";
    let fixture = Fixture::new();
    fs::write(
        fixture.root.join(format!(
            "codex/sessions/rollout-2026-01-01T00-00-01-{SUBAGENT_ID}.jsonl"
        )),
        json!({
            "timestamp": "2026-01-01T00:00:01Z",
            "type": "session_meta",
            "payload": {
                "id": SUBAGENT_ID,
                "session_id": SOURCE_ID,
                "cwd": fixture.workspace,
                "source": {"subagent": {"thread_spawn": {"parent_thread_id": SOURCE_ID, "depth": 1}}}
            }
        })
        .to_string(),
    )
    .expect("write synthetic subagent thread");

    let list = fixture
        .command()
        .args(["--json", "list", "--provider", "codex"])
        .output()
        .expect("list sessions");
    let listed = String::from_utf8_lossy(&list.stdout);
    assert!(list.status.success(), "{listed}");
    assert!(listed.contains(SOURCE_ID), "{listed}");
    assert!(!listed.contains(SUBAGENT_ID), "{listed}");

    let fork = fixture
        .command()
        .args(["--json", "fork", SUBAGENT_ID, "--in", "codex", "--dry-run"])
        .output()
        .expect("plan fork");
    let stdout = String::from_utf8_lossy(&fork.stdout);
    assert!(
        fork.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&fork.stderr)
    );
    let plan: serde_json::Value = serde_json::from_str(&stdout).expect("fork plan JSON");
    assert_eq!(
        plan["launch"]["args"],
        json!([
            "-c",
            codex_sqlite_override(&fixture.root),
            "fork",
            SUBAGENT_ID
        ])
    );
}

#[cfg(unix)]
struct Fixture {
    _temporary: tempfile::TempDir,
    root: PathBuf,
    workspace: PathBuf,
    capture: PathBuf,
    launcher: PathBuf,
}

#[cfg(unix)]
impl Fixture {
    fn new() -> Self {
        let temporary = tempfile::tempdir().expect("temporary fixture");
        let root = temporary.path().canonicalize().expect("canonical fixture");
        let fixture = Self {
            workspace: root.join("workspace with spaces"),
            capture: root.join("capture"),
            launcher: root.join("fake-claude"),
            root,
            _temporary: temporary,
        };
        for directory in [
            fixture.root.join("home"),
            fixture.root.join("tmp"),
            fixture.root.join("codex/sessions"),
            fixture.workspace.clone(),
            fixture.capture.clone(),
        ] {
            fs::create_dir_all(directory).expect("isolated fixture directory");
        }
        fs::write(&fixture.launcher, LAUNCHER).expect("write synthetic launcher");
        fs::set_permissions(&fixture.launcher, fs::Permissions::from_mode(0o700))
            .expect("make synthetic launcher executable");
        // Run it once before omni does, so omni never finds the new script busy.
        let primed = output_after_write(Command::new(&fixture.launcher).arg("--version"));
        assert!(
            primed.status.success(),
            "synthetic launcher version probe failed"
        );

        let mut records = vec![json!({
            "timestamp": "2026-01-01T00:00:00Z",
            "type": "session_meta",
            "payload": {"id": SOURCE_ID, "cwd": fixture.workspace}
        })];
        for index in 0..8 {
            records.push(json!({
                "timestamp": "2026-01-01T00:00:01Z",
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": if index % 2 == 0 { "user" } else { "assistant" },
                    "content": [{"type": "input_text", "text": format!("起点 {index} {} 終点", "共同作業".repeat(950))}]
                }
            }));
        }
        for index in 0..12 {
            records.push(json!({
                "timestamp": "2026-01-01T00:00:02Z",
                "type": "response_item",
                "payload": {
                    "type": "function_call_output",
                    "call_id": format!("synthetic-tool-{index}"),
                    "output": "共同作業".repeat(600)
                }
            }));
        }
        fs::write(
            fixture.root.join(format!(
                "codex/sessions/rollout-2026-01-01T00-00-00-{SOURCE_ID}.jsonl"
            )),
            records
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .expect("write synthetic history");
        fixture
    }

    fn init_git(&self) {
        assert!(
            Command::new("git")
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("HOME", self.root.join("home"))
                .args(["init", "--quiet"])
                .arg(&self.workspace)
                .status()
                .unwrap()
                .success()
        );
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_omni"));
        command
            .env_clear()
            .current_dir(&self.workspace)
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.root.join("home"))
            .env("TMPDIR", self.root.join("tmp"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("OMNISESSION_HOME", self.root.join("state"))
            .env("CODEX_HOME", self.root.join("codex"))
            .env("CLAUDE_CONFIG_DIR", self.root.join("claude"))
            .env("OMNI_CLAUDE_BIN", &self.launcher)
            .env("LAUNCH_CAPTURE", &self.capture)
            .env("OMNI_NO_UPDATE_CHECK", "1");
        command
    }
}

/// Runs a synthetic launcher written moments ago. A child that a concurrent test forked while the
/// script was open for writing keeps that descriptor until it execs, and Linux refuses to run the
/// script meanwhile, so busy executables are retried briefly.
#[cfg(unix)]
fn output_after_write(command: &mut Command) -> Output {
    for _ in 0..50 {
        match command.output() {
            Err(error) if error.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            result => return result.expect("run synthetic launcher"),
        }
    }
    command.output().expect("run synthetic launcher")
}

/// Codex and Grok launches on Windows, where npm installs providers as `.cmd` command shims.
///
/// Each fake provider is a Node script behind a current npm command shim. It records its argv,
/// working directory, and parent PID, so a `cmd.exe` hop or a verbatim `\\?\` working directory
/// fails the test.
#[cfg(windows)]
mod windows {
    use std::{
        env,
        ffi::OsString,
        fs,
        io::Write,
        os::windows::process::CommandExt,
        path::{Path, PathBuf},
        process::{Command, Output, Stdio},
        time::Duration,
    };

    use serde_json::{Value, json};
    use wait_timeout::ChildExt;

    const CODEX_SOURCE_ID: &str = "11111111-1111-4111-8111-111111111111";
    const GROK_SOURCE_ID: &str = "22222222-2222-4222-8222-222222222222";
    /// Exit code of a fake provider that survived Ctrl+Break.
    const INTERRUPTED_EXIT_CODE: i32 = 42;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const TIMEOUT: Duration = Duration::from_secs(120);
    /// Current npm `cmd-shim` output, up to the package script path.
    const NPM_COMMAND_SHIM_PREFIX: &str = "@ECHO off\r\nGOTO start\r\n:find_dp0\r\nSET dp0=%~dp0\r\nEXIT /b\r\n:start\r\nSETLOCAL\r\nCALL :find_dp0\r\n\r\nIF EXIST \"%dp0%\\node.exe\" (\r\n  SET \"_prog=%dp0%\\node.exe\"\r\n) ELSE (\r\n  SET \"_prog=node\"\r\n)\r\n\r\nendLocal & goto #_undefined_# 2>NUL || title %COMSPEC% & set PATHEXT=%PATHEXT:;.JS;=;% & \"%_prog%\"  \"%dp0%\\";
    /// Sends Ctrl+Break to the process group led by the parent of `$env:OMNI_TEST_BREAK_CHILD`.
    const SEND_BREAK_SCRIPT: &str = "$ErrorActionPreference = 'Stop'; \
        $group = (Get-CimInstance -ClassName Win32_Process -Filter ('ProcessId = ' + $env:OMNI_TEST_BREAK_CHILD)).ParentProcessId; \
        $quote = [char]34; \
        Add-Type -Namespace OmniTest -Name Console -MemberDefinition ('[System.Runtime.InteropServices.DllImport(' + $quote + 'kernel32.dll' + $quote + ', SetLastError = true)] public static extern bool GenerateConsoleCtrlEvent(uint ctrlEvent, uint processGroupId);'); \
        if (-not [OmniTest.Console]::GenerateConsoleCtrlEvent(1, [uint32]$group)) { throw ('GenerateConsoleCtrlEvent failed: ' + [System.Runtime.InteropServices.Marshal]::GetLastWin32Error()) }";
    const PROVIDER_SCRIPT: &str = r#"#!/usr/bin/env node
"use strict";
const childProcess = require("child_process");
const fs = require("fs");
const path = require("path");

const args = process.argv.slice(2);
const env = process.env;
if (args.length === 1 && args[0] === "--version") {
  // Older than every native import gate, so explicit imports fall back to semantic handoff.
  fs.appendFileSync(path.join(env.FAKE_PROVIDER_CAPTURE, "version-probes"), "--version\n");
  fs.writeSync(1, "synthetic 0.0.0\n");
  process.exit(0);
}

const capture = env.FAKE_PROVIDER_CAPTURE;
const launch = { args, cwd: process.cwd(), ppid: process.ppid };
const handoff = (args[args.length - 1] || "").split("`")[1];
if (handoff !== undefined) {
  launch.handoff = handoff;
  fs.copyFileSync(handoff, path.join(capture, "document"));
}
if (env.FAKE_PROVIDER_STDIN === "1") launch.stdin = fs.readFileSync(0, "utf8");
fs.writeFileSync(path.join(capture, "launch.json"), JSON.stringify(launch));
fs.writeSync(1, "synthetic stdout\n");
fs.writeSync(2, "synthetic stderr\n");

if (env.FAKE_PROVIDER_BREAK_SCRIPT) {
  let interrupted = false;
  process.on("SIGBREAK", () => { interrupted = true; });
  // Without CREATE_NO_WINDOW or a new group, the sender shares omni's console group.
  const sender = childProcess.spawnSync(
    path.join(env.SystemRoot, "System32", "WindowsPowerShell", "v1.0", "powershell.exe"),
    ["-NoLogo", "-NoProfile", "-NonInteractive", "-Command", env.FAKE_PROVIDER_BREAK_SCRIPT],
    { env: { ...env, OMNI_TEST_BREAK_CHILD: String(process.pid) }, stdio: "ignore" },
  );
  const deadline = Date.now() + 60000;
  const poll = () => {
    if (interrupted) {
      // Outlive a parent that the same event would end without a handler of its own.
      setTimeout(() => process.exit(42), 500);
    } else if (Date.now() > deadline) {
      fs.writeSync(2, `console break was not delivered; sender exited with ${sender.status}\n`);
      process.exit(3);
    } else {
      setTimeout(poll, 10);
    }
  };
  poll();
} else {
  process.exit(Number(env.FAKE_PROVIDER_EXIT_CODE || "0"));
}
"#;

    #[test]
    fn npm_providers_resume_and_fork_as_direct_omni_children() {
        let Some(fixture) = Fixture::new() else {
            return;
        };
        for (provider, id) in [("codex", CODEX_SOURCE_ID), ("grok", GROK_SOURCE_ID)] {
            let output = fixture
                .command()
                .args(["--json", "list", "--provider", provider])
                .output()
                .expect("list workspace sessions");
            assert_success(&output, "list workspace sessions");
            let listed: Value = serde_json::from_slice(&output.stdout).expect("session list JSON");
            assert!(
                listed["sessions"]
                    .as_array()
                    .expect("listed sessions")
                    .iter()
                    .any(|session| session["session"]["id"] == id),
                "{provider} session recorded with a verbatim cwd is missing: {listed}"
            );
        }

        let codex_sqlite = super::codex_sqlite_override(&fixture.root);
        for (source, fork, expected) in [
            (
                format!("codex:{CODEX_SOURCE_ID}"),
                false,
                vec!["-c", codex_sqlite.as_str(), "resume", CODEX_SOURCE_ID],
            ),
            (
                format!("codex:{CODEX_SOURCE_ID}"),
                true,
                vec!["-c", codex_sqlite.as_str(), "fork", CODEX_SOURCE_ID],
            ),
            (
                format!("grok:{GROK_SOURCE_ID}"),
                false,
                vec!["--resume", GROK_SOURCE_ID],
            ),
            (
                format!("grok:{GROK_SOURCE_ID}"),
                true,
                vec!["--resume", GROK_SOURCE_ID, "--fork-session"],
            ),
        ] {
            let mut command = fixture.command();
            command.args(["resume", &source]);
            if fork {
                command.arg("--fork");
            }
            let (output, omni) = run(&mut command, None);
            assert_success(&output, &format!("resume {source} (fork: {fork})"));
            assert!(String::from_utf8_lossy(&output.stdout).contains("synthetic stdout"));
            let launch = fixture.take_launch();
            assert_eq!(launch_args(&launch), expected, "{source} (fork: {fork})");
            fixture.assert_direct_launch(&launch, omni);
        }

        let mut command = fixture.command();
        command
            .args(["resume", &format!("grok:{GROK_SOURCE_ID}")])
            .env("FAKE_PROVIDER_EXIT_CODE", "23");
        let (output, omni) = run(&mut command, None);
        assert!(
            !output.status.success(),
            "provider failure was not reported"
        );
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("target exited with status exit code: 23"),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        fixture.assert_direct_launch(&fixture.take_launch(), omni);
    }

    #[test]
    fn npm_providers_start_new_sessions_with_private_handoff() {
        let Some(fixture) = Fixture::new() else {
            return;
        };
        for (source, target, marker) in [
            (format!("codex:{CODEX_SOURCE_ID}"), "grok", "起点 7"),
            (format!("grok:{GROK_SOURCE_ID}"), "codex", "Grok 起点"),
        ] {
            let (output, omni) = run(
                fixture
                    .command()
                    .args(["resume", &source, "--in", target])
                    .env("TMP", fixture.root.join("tmp"))
                    .env("TEMP", fixture.root.join("tmp")),
                None,
            );
            assert_success(&output, &format!("resume {source} in {target}"));
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(stderr.contains("using semantic handoff"), "{stderr}");

            let launch = fixture.take_launch();
            fixture.assert_direct_launch(&launch, omni);
            let arguments = launch_args(&launch);
            let (prompt, options) = arguments.split_last().expect("provider handoff prompt");
            if target == "codex" {
                let home = super::codex_sqlite_override(&fixture.root);
                assert_eq!(options, ["-c", home.as_str()]);
            } else {
                assert!(options.is_empty(), "{target} argv: {arguments:?}");
            }
            assert!(
                prompt.starts_with("Read `") && !prompt.contains(marker),
                "{target} prompt must point at a private handoff: {prompt}"
            );
            let handoff = PathBuf::from(launch["handoff"].as_str().expect("handoff path"));
            assert_eq!(
                handoff.canonicalize().unwrap().parent(),
                Some(fixture.root.join("tmp").as_path())
            );
            let document = fs::read_to_string(fixture.capture.join("document"))
                .expect("provider read handoff");
            assert!(
                document.contains(marker),
                "{target} handoff omitted source history"
            );
            assert_eq!(fs::read_to_string(&handoff).unwrap(), document);
        }
    }

    #[test]
    fn omni_waits_for_provider_after_console_break() {
        let Some(fixture) = Fixture::new() else {
            return;
        };
        let log = fixture.root.join("omni.log");
        let file = fs::File::create(&log).expect("omni log");
        let mut omni = fixture
            .command()
            .args(["resume", &format!("codex:{CODEX_SOURCE_ID}")])
            .env("FAKE_PROVIDER_BREAK_SCRIPT", SEND_BREAK_SCRIPT)
            // A hidden console of its own keeps the event away from this test, and a new process
            // group lets the provider target only omni and its descendants.
            .creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP)
            .stdout(file.try_clone().expect("clone omni log"))
            .stderr(file)
            .spawn()
            .expect("launch omni");
        let Some(status) = omni.wait_timeout(TIMEOUT).expect("wait for omni") else {
            let _ = omni.kill();
            let _ = omni.wait();
            panic!(
                "omni did not exit:\n{}",
                fs::read_to_string(&log).unwrap_or_default()
            );
        };
        let log = fs::read_to_string(&log).unwrap_or_default();
        assert!(
            log.contains(&format!(
                "target exited with status exit code: {INTERRUPTED_EXIT_CODE}"
            )),
            "omni ended before the provider exited ({status}):\n{log}"
        );
        assert_eq!(status.code(), Some(INTERRUPTED_EXIT_CODE), "{log}");
    }

    struct Fixture {
        _temporary: tempfile::TempDir,
        root: PathBuf,
        workspace: PathBuf,
        capture: PathBuf,
        path: OsString,
        codex: PathBuf,
        grok: PathBuf,
    }

    impl Fixture {
        /// Returns `None` when Node or Git is unavailable outside CI.
        fn new() -> Option<Self> {
            let node_directory = tool_directory("node.exe")?;
            // Workspace capture runs Git before cross-provider launches.
            let git_directory = tool_directory("git.exe")?;
            let system =
                PathBuf::from(env::var_os("SystemRoot").expect("SystemRoot")).join("System32");
            let temporary = tempfile::tempdir().expect("temporary fixture");
            // Canonical temporary paths carry the verbatim prefix that `omni` must not pass on.
            let root = temporary.path().canonicalize().expect("canonical fixture");
            let npm = root.join("npm");
            let fixture = Self {
                workspace: root.join("workspace with spaces"),
                capture: root.join("capture"),
                path: env::join_paths([node_directory, git_directory, system])
                    .expect("fixture PATH"),
                codex: install_npm_provider(&npm, "codex"),
                grok: install_npm_provider(&npm, "grok"),
                root,
                _temporary: temporary,
            };
            for directory in [
                fixture.root.join("home"),
                fixture.root.join("tmp"),
                fixture.workspace.clone(),
                fixture.capture.clone(),
            ] {
                fs::create_dir_all(directory).expect("isolated fixture directory");
            }
            fixture.write_codex_source();
            fixture.write_grok_source();
            Some(fixture)
        }

        fn write_codex_source(&self) {
            let directory = self
                .root
                .join("codex")
                .join("sessions")
                .join("2026")
                .join("01")
                .join("01");
            fs::create_dir_all(&directory).expect("Codex source directory");
            let mut records = vec![json!({
                "timestamp": "2026-01-01T00:00:00Z",
                "type": "session_meta",
                "payload": {"id": CODEX_SOURCE_ID, "cwd": self.workspace}
            })];
            for index in 0..8 {
                records.push(json!({
                    "timestamp": "2026-01-01T00:00:01Z",
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": if index % 2 == 0 { "user" } else { "assistant" },
                        "content": [{"type": "input_text", "text": format!("起点 {index} {} 終点", "共同作業".repeat(950))}]
                    }
                }));
            }
            fs::write(
                directory.join(format!(
                    "rollout-2026-01-01T00-00-00-{CODEX_SOURCE_ID}.jsonl"
                )),
                records
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
            .expect("write synthetic Codex history");
        }

        fn write_grok_source(&self) {
            let session = self
                .root
                .join("grok")
                .join("sessions")
                .join("workspace-hash")
                .join(GROK_SOURCE_ID);
            fs::create_dir_all(&session).expect("Grok source directory");
            fs::write(
                session.join("summary.json"),
                json!({"id": GROK_SOURCE_ID, "cwd": self.workspace, "num_messages": 2}).to_string(),
            )
            .expect("write synthetic Grok summary");
            let update = |kind: &str, text: &str| {
                json!({"params": {"update": {"sessionUpdate": kind, "content": {"type": "text", "text": text}}}})
                    .to_string()
            };
            fs::write(
                session.join("updates.jsonl"),
                format!(
                    "{}\n{}\n",
                    update("user_message_chunk", "Grok 起点 question"),
                    update("agent_message_chunk", "Grok 終点 answer")
                ),
            )
            .expect("write synthetic Grok updates");
        }

        fn command(&self) -> Command {
            let mut command = Command::new(env!("CARGO_BIN_EXE_omni"));
            for (name, _) in env::vars_os() {
                if name
                    .to_string_lossy()
                    .to_ascii_uppercase()
                    .starts_with("OMNI_")
                {
                    command.env_remove(name);
                }
            }
            let home = self.root.join("home");
            command
                .current_dir(&self.workspace)
                .env("PATH", &self.path)
                .env_remove("CODEX_SQLITE_HOME")
                .env("HOME", &home)
                .env("USERPROFILE", &home)
                .env("OMNISESSION_HOME", self.root.join("state"))
                .env("CLAUDE_CONFIG_DIR", self.root.join("claude"))
                .env("CODEX_HOME", self.root.join("codex"))
                .env("GROK_HOME", self.root.join("grok"))
                .env("HERMES_HOME", self.root.join("hermes"))
                .env("PI_CODING_AGENT_DIR", self.root.join("pi"))
                .env("CURSOR_AGENT_HOME", self.root.join("cursor"))
                .env("ANTIGRAVITY_CLI_HOME", self.root.join("antigravity-cli"))
                .env("ANTIGRAVITY_IDE_HOME", self.root.join("antigravity-ide"))
                .env(
                    "ANTIGRAVITY_DESKTOP_HOME",
                    self.root.join("antigravity-desktop"),
                )
                .env(
                    "ANTIGRAVITY_IDE_USER_DATA_DIR",
                    self.root.join("antigravity-ide-user-data"),
                )
                .env("OMNI_CODEX_BIN", &self.codex)
                .env("OMNI_GROK_BIN", &self.grok)
                .env("OMNI_NO_UPDATE_CHECK", "1")
                .env("FAKE_PROVIDER_CAPTURE", &self.capture)
                .stdin(Stdio::null());
            command
        }

        fn take_launch(&self) -> Value {
            let path = self.capture.join("launch.json");
            let launch = serde_json::from_slice(&fs::read(&path).expect("provider launch record"))
                .expect("provider launch JSON");
            fs::remove_file(path).expect("reset provider launch record");
            launch
        }

        /// The provider must be a direct `omni` child in the ordinary workspace path.
        fn assert_direct_launch(&self, launch: &Value, omni: u32) {
            assert_eq!(
                launch["ppid"].as_u64(),
                Some(u64::from(omni)),
                "provider did not run as a direct omni child: {launch}"
            );
            let cwd = launch["cwd"].as_str().expect("provider cwd");
            assert!(!cwd.starts_with(r"\\?\"), "verbatim provider cwd: {cwd}");
            assert_eq!(
                Path::new(cwd),
                omnis_core::canonicalize_path(&self.workspace).expect("ordinary workspace")
            );
        }
    }

    /// Writes a Node package script behind the command shim npm generates for it.
    fn install_npm_provider(npm: &Path, name: &str) -> PathBuf {
        let package = npm
            .join("node_modules")
            .join("@omnisession-test")
            .join(name);
        fs::create_dir_all(&package).expect("synthetic provider package");
        fs::write(package.join("cli.js"), PROVIDER_SCRIPT).expect("synthetic provider script");
        let shim = npm.join(format!("{name}.cmd"));
        fs::write(
            &shim,
            format!(
                "{NPM_COMMAND_SHIM_PREFIX}node_modules\\@omnisession-test\\{name}\\cli.js\" %*\r\n"
            ),
        )
        .expect("synthetic npm command shim");
        shim
    }

    /// Directory on `PATH` holding `executable`.
    ///
    /// CI images ship Node and Git, so a missing tool fails there and skips elsewhere.
    fn tool_directory(executable: &str) -> Option<PathBuf> {
        let directory = env::var_os("PATH").and_then(|path| {
            env::split_paths(&path).find(|directory| directory.join(executable).is_file())
        });
        if directory.is_none() {
            assert!(
                env::var_os("CI").is_none(),
                "Windows launch tests require {executable} on PATH"
            );
            eprintln!("skipping Windows launch test: {executable} is not on PATH");
        }
        directory
    }

    fn run(command: &mut Command, stdin: Option<&[u8]>) -> (Output, u32) {
        if stdin.is_some() {
            command.stdin(Stdio::piped());
        }
        let mut child = command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("launch omni");
        let pid = child.id();
        if let Some(input) = stdin {
            child
                .stdin
                .take()
                .expect("omni stdin")
                .write_all(input)
                .expect("provide inherited stdin");
        }
        (child.wait_with_output().expect("wait for omni"), pid)
    }

    fn launch_args(launch: &Value) -> Vec<&str> {
        launch["args"]
            .as_array()
            .expect("provider argv")
            .iter()
            .map(|argument| argument.as_str().expect("UTF-8 argument"))
            .collect()
    }

    fn assert_success(output: &Output, action: &str) {
        assert!(
            output.status.success(),
            "{action} failed: {}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn provider_shim_command_is_not_available() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_omni"))
        .args(["shim", "install"])
        .output()
        .expect("run removed subcommand");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unrecognized subcommand 'shim'"));
}

#[cfg(unix)]
#[test]
fn provider_executable_name_does_not_dispatch_to_an_agent() {
    let temporary = tempfile::tempdir().expect("temporary executable directory");
    let alias = temporary.path().join("claude");
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_omni"), &alias)
        .expect("synthetic executable alias");
    let output = Command::new(alias)
        .env_clear()
        .env("HOME", temporary.path())
        .env("PATH", "/usr/bin:/bin")
        .arg("--version")
        .output()
        .expect("run omni through a provider executable name");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        concat!("omni ", env!("CARGO_PKG_VERSION"))
    );
}
