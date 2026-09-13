//! Semantic shim handoffs and provider launches driven through real `omni` processes.
//!
//! Unix fakes are shell scripts. Windows fakes are npm command shims, which `omni` must start
//! through `node.exe` and never through `cmd.exe`.

#[cfg(unix)]
use std::{
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::{Command, Output, Stdio},
};

#[cfg(unix)]
use omnis_core::capture_workspace;
#[cfg(unix)]
use omnis_ir::{BundleManifest, CanonicalSnapshot, PortableBundle, SCHEMA_VERSION};
#[cfg(unix)]
use serde_json::json;
#[cfg(unix)]
use uuid::Uuid;
#[cfg(unix)]
use wait_timeout::ChildExt;

#[cfg(unix)]
const SOURCE_ID: &str = "11111111-1111-4111-8111-111111111111";
#[cfg(unix)]
const LAUNCHER: &str = r#"#!/bin/sh
if [ "$1" = "--version" ]; then
    printf '0.0.0 (Claude Code)\n'
    if [ "$SHIM_FAIL_SPAWN" = "1" ]; then /bin/chmod -x "$0"; fi
    exit 0
fi
for prompt do printf '%s\000' "$prompt" >> "$SHIM_CAPTURE/args"; done
handoff=${prompt#*\`}
handoff=${handoff%%\`*}
printf '%s' "$handoff" > "$SHIM_CAPTURE/path"
pwd > "$SHIM_CAPTURE/cwd"
read -r input
[ "$input" = "synthetic stdin" ] || exit 91
/bin/cp "$handoff" "$SHIM_CAPTURE/document" 2>/dev/null || exit 92
if [ "$SHIM_WAIT_FOR_SIGNAL" = "1" ]; then
    trap 'exit 143' TERM
    trap 'exit 130' INT
    printf '%s' "$$" > "$SHIM_CAPTURE/pid"
    while :; do read -r input || exit 93; done
fi
printf 'synthetic stdout\n'
printf 'synthetic stderr\n' >&2
if [ "$SHIM_SIGNAL_EXIT" = "1" ]; then kill -TERM "$$"; fi
exit "$SHIM_EXIT_CODE"
"#;

#[cfg(unix)]
#[test]
fn semantic_shim_uses_private_file_for_long_history_and_cleans_up_after_exit() {
    for (exit_code, signal_exit) in [(0, false), (23, false), (143, true)] {
        let fixture = Fixture::new();
        let output = fixture.run_shim(exit_code, signal_exit);
        assert_eq!(
            output.status.code(),
            Some(exit_code),
            "shim failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"synthetic stdout\n");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("using semantic handoff"));
        assert!(stderr.contains("synthetic stderr"));
        assert!(!stderr.contains("起点"));

        let arguments = fs::read(fixture.capture.join("args")).expect("captured argv");
        let arguments = arguments
            .split(|byte| *byte == 0)
            .filter(|argument| !argument.is_empty())
            .collect::<Vec<_>>();
        assert_eq!(arguments.len(), 2);
        assert_eq!(arguments[0], b"--dangerously-skip-permissions");
        assert!(arguments[1].len() < 4096);
        assert!(!String::from_utf8_lossy(arguments[1]).contains("起点"));
        let cwd = fs::read_to_string(fixture.capture.join("cwd")).expect("captured cwd");
        assert_eq!(cwd.trim(), fixture.workspace.to_str().expect("UTF-8 cwd"));

        let document =
            fs::read_to_string(fixture.capture.join("document")).expect("child could read handoff");
        assert!(
            document.len() > 131_072,
            "fixture must exceed Linux argv limit"
        );
        assert!(document.contains("起点"));
        assert!(document.contains("終点"));
        let handoff = PathBuf::from(
            fs::read_to_string(fixture.capture.join("path")).expect("captured handoff path"),
        );
        assert!(handoff.starts_with(fixture.root.join("state/handoffs")));
        assert!(
            !handoff.exists(),
            "handoff must be removed after child exit"
        );
        let mode = fs::metadata(fixture.capture.join("document"))
            .expect("copied handoff permissions")
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0, "handoff must be private");
        fixture.assert_handoff_directory_empty();
    }
}

#[cfg(unix)]
#[test]
#[allow(clippy::too_many_lines)]
fn semantic_shim_maps_relocated_import_and_preserves_imported_binding() {
    let fixture = Fixture::new();
    for args in [
        vec!["init", "--quiet"],
        vec![
            "remote",
            "add",
            "origin",
            "https://example.invalid/acme/continuity.git",
        ],
    ] {
        let output = Command::new("git")
            .args(args)
            .current_dir(&fixture.workspace)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", fixture.root.join("home"))
            .output()
            .expect("configure synthetic repository");
        assert!(output.status.success());
    }
    let output = fixture
        .command()
        .args(["--json", "show", &format!("codex:{SOURCE_ID}")])
        .output()
        .expect("read synthetic snapshot");
    assert!(output.status.success());
    let mut snapshot: CanonicalSnapshot =
        serde_json::from_slice(&output.stdout).expect("synthetic snapshot");
    snapshot.workspace = capture_workspace(&fixture.workspace).expect("capture repository");
    assert!(snapshot.workspace.git.remote_fingerprint.is_some());
    let missing_workspace = fixture.root.join("old-machine/workspace");
    snapshot.workspace.root.clone_from(&missing_workspace);
    snapshot
        .workspace
        .current_dir
        .clone_from(&missing_workspace);
    snapshot.workspace.git.worktree = Some(missing_workspace);
    let bundle = PortableBundle {
        manifest: BundleManifest {
            schema_version: SCHEMA_VERSION.to_owned(),
            bundle_id: Uuid::from_u128(77),
            created_at: snapshot.captured_at,
            source: snapshot.session.clone(),
            event_count: snapshot.events.len(),
            redactions: Vec::new(),
        },
        snapshot,
        fidelity: None,
    };
    let bundle_path = fixture.root.join("relocated.json");
    fs::write(
        &bundle_path,
        serde_json::to_vec(&bundle).expect("encode bundle"),
    )
    .expect("write relocated bundle");
    let output = fixture
        .command()
        .arg("import")
        .arg(bundle_path)
        .output()
        .expect("import bundle");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let imported = format!("imported:{}", bundle.manifest.bundle_id);
    let output = fixture
        .command()
        .args(["task", "bind", &imported])
        .output()
        .expect("bind relocated source");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let output = fixture.run_shim(0, false);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        fs::read_to_string(fixture.capture.join("document"))
            .expect("relocated handoff")
            .contains("起点")
    );
    let output = fixture
        .command()
        .args(["--json", "task", "status"])
        .output()
        .expect("read binding after handoff");
    assert!(output.status.success());
    let status: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("task status JSON");
    assert_eq!(status["session"]["provider"], "imported");
    assert_eq!(
        status["session"]["id"],
        bundle.manifest.bundle_id.to_string()
    );
    fixture.assert_handoff_directory_empty();
}

#[cfg(unix)]
#[test]
fn semantic_shim_forwards_parent_signals_and_removes_private_handoff() {
    for (signal, expected_code) in [("TERM", 143), ("INT", 130)] {
        let fixture = Fixture::new();
        let mut child = fixture
            .command()
            .args(["shim", "exec", "claude", "--", "--continue"])
            .env("SHIM_WAIT_FOR_SIGNAL", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("launch waiting shim");
        let mut input = child.stdin.take().unwrap();
        input.write_all(b"synthetic stdin\n").unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let pid_path = fixture.capture.join("pid");
        while !pid_path.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let provider_pid = fs::read_to_string(pid_path).expect("waiting provider PID");
        assert!(
            Command::new("kill")
                .args([&format!("-{signal}"), &child.id().to_string()])
                .status()
                .unwrap()
                .success()
        );
        let status = child
            .wait_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        let provider_alive = Command::new("kill")
            .args(["-0", &provider_pid])
            .output()
            .unwrap()
            .status
            .success();
        // Always clean up the exact synthetic process, including on the old failing code.
        if provider_alive {
            let _ = Command::new("kill").args(["-KILL", &provider_pid]).status();
        }
        if status.is_none() {
            child.kill().expect("stop timed-out synthetic shim");
        }
        drop(input);
        let output = child.wait_with_output().expect("collect shim output");
        assert_eq!(
            status.and_then(|status| status.code()),
            Some(expected_code),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!provider_alive, "provider must not outlive terminated shim");
        fixture.assert_handoff_directory_empty();
    }
}

#[cfg(unix)]
#[test]
fn semantic_shim_removes_handoff_when_provider_spawn_fails() {
    let fixture = Fixture::new();
    let output = fixture
        .command()
        .args(["shim", "exec", "claude", "--", "--continue"])
        .env("SHIM_FAIL_SPAWN", "1")
        .output()
        .expect("run failing synthetic shim");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("using semantic handoff"));
    assert!(stderr.contains("Permission denied"), "{stderr}");
    fixture.assert_handoff_directory_empty();
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
            fixture.root.join("codex/sessions"),
            fixture.workspace.clone(),
            fixture.capture.clone(),
        ] {
            fs::create_dir_all(directory).expect("isolated fixture directory");
        }
        fs::write(&fixture.launcher, LAUNCHER).expect("write synthetic launcher");
        fs::set_permissions(&fixture.launcher, fs::Permissions::from_mode(0o700))
            .expect("make synthetic launcher executable");

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
        let output = fixture
            .command()
            .args([
                "task",
                "start",
                "handoff",
                "--from",
                &format!("codex:{SOURCE_ID}"),
            ])
            .output()
            .expect("bind synthetic source");
        assert!(
            output.status.success(),
            "binding failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        fixture
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_omni"));
        command
            .env_clear()
            .current_dir(&self.workspace)
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.root.join("home"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("OMNISESSION_HOME", self.root.join("state"))
            .env("CODEX_HOME", self.root.join("codex"))
            .env("CLAUDE_CONFIG_DIR", self.root.join("claude"))
            .env("OMNI_CLAUDE_BIN", &self.launcher)
            .env("SHIM_CAPTURE", &self.capture)
            .env("OMNI_NO_UPDATE_CHECK", "1");
        command
    }

    fn run_shim(&self, exit_code: i32, signal_exit: bool) -> Output {
        let mut child = self
            .command()
            .args([
                "shim",
                "exec",
                "claude",
                "--",
                "--dangerously-skip-permissions",
                "--continue",
            ])
            .env("SHIM_EXIT_CODE", exit_code.to_string())
            .env("SHIM_SIGNAL_EXIT", if signal_exit { "1" } else { "0" })
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("launch synthetic shim");
        child
            .stdin
            .take()
            .expect("child stdin")
            .write_all(b"synthetic stdin\n")
            .expect("provide inherited stdin");
        child.wait_with_output().expect("wait for synthetic shim")
    }

    fn assert_handoff_directory_empty(&self) {
        let directory = self.root.join("state/handoffs");
        assert_eq!(
            fs::read_dir(&directory).expect("handoff directory").count(),
            0
        );
        let mode = fs::metadata(directory)
            .expect("handoff directory permissions")
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0);
    }
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
  // Older than every native import gate, so imports fall back to semantic handoff.
  fs.writeSync(1, "synthetic 0.0.0\n");
  if (env.FAKE_PROVIDER_FAIL_SPAWN === "1") fs.unlinkSync(__filename);
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

        for (source, fork, expected) in [
            (
                format!("codex:{CODEX_SOURCE_ID}"),
                false,
                vec!["resume", CODEX_SOURCE_ID],
            ),
            (
                format!("codex:{CODEX_SOURCE_ID}"),
                true,
                vec!["fork", CODEX_SOURCE_ID],
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
                fixture.command().args(["resume", &source, "--in", target]),
                None,
            );
            assert_success(&output, &format!("resume {source} in {target}"));
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(stderr.contains("using semantic handoff"), "{stderr}");

            let launch = fixture.take_launch();
            fixture.assert_direct_launch(&launch, omni);
            let arguments = launch_args(&launch);
            assert_eq!(arguments.len(), 1, "{target} argv: {arguments:?}");
            assert!(
                arguments[0].starts_with("Read `") && !arguments[0].contains(marker),
                "{target} prompt must point at a private handoff: {}",
                arguments[0]
            );
            let handoff = PathBuf::from(launch["handoff"].as_str().expect("handoff path"));
            assert!(handoff.starts_with(fixture.root.join("state").join("handoffs")));
            assert!(
                !handoff.exists(),
                "handoff must be removed after provider exit"
            );
            let document = fs::read_to_string(fixture.capture.join("document"))
                .expect("provider read handoff");
            assert!(
                document.contains(marker),
                "{target} handoff omitted source history"
            );
            fixture.assert_handoff_directory_empty();
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
        assert_eq!(status.code(), Some(1), "{log}");
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
        /// Returns `None` when `node.exe` is unavailable outside CI.
        fn new() -> Option<Self> {
            let node_directory = node_directory()?;
            let system =
                PathBuf::from(env::var_os("SystemRoot").expect("SystemRoot")).join("System32");
            let temporary = tempfile::tempdir().expect("temporary fixture");
            // Canonical temporary paths carry the verbatim prefix that `omni` must not pass on.
            let root = temporary.path().canonicalize().expect("canonical fixture");
            let npm = root.join("npm");
            let fixture = Self {
                workspace: root.join("workspace with spaces"),
                capture: root.join("capture"),
                path: env::join_paths([node_directory, system]).expect("fixture PATH"),
                codex: install_npm_provider(&npm, "codex"),
                grok: install_npm_provider(&npm, "grok"),
                root,
                _temporary: temporary,
            };
            for directory in [
                fixture.root.join("home"),
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

        fn assert_handoff_directory_empty(&self) {
            let directory = self.root.join("state").join("handoffs");
            assert_eq!(
                fs::read_dir(directory).expect("handoff directory").count(),
                0
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

    /// CI images ship Node, so a missing interpreter fails there and skips elsewhere.
    fn node_directory() -> Option<PathBuf> {
        let directory = env::var_os("PATH").and_then(|path| {
            env::split_paths(&path).find(|directory| directory.join("node.exe").is_file())
        });
        if directory.is_none() {
            assert!(
                env::var_os("CI").is_none(),
                "Windows launch tests require node.exe on PATH"
            );
            eprintln!("skipping Windows launch test: node.exe is not on PATH");
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
