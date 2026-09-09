#![cfg(unix)]

use std::{
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::{Command, Output, Stdio},
};

use omnis_core::capture_workspace;
use omnis_ir::{BundleManifest, CanonicalSnapshot, PortableBundle, SCHEMA_VERSION};
use serde_json::json;
use uuid::Uuid;
use wait_timeout::ChildExt;

const SOURCE_ID: &str = "11111111-1111-4111-8111-111111111111";
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

struct Fixture {
    _temporary: tempfile::TempDir,
    root: PathBuf,
    workspace: PathBuf,
    capture: PathBuf,
    launcher: PathBuf,
}

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
