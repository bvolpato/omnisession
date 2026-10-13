#[cfg(windows)]
use std::io::{Read, Seek, SeekFrom};
use std::{
    env, fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc::{self, Receiver},
    thread,
    time::{Duration, Instant},
};

use chrono::Utc;
use omnis_adapters::{GrokAdapter, ProviderAdapter};
use omnis_ir::{Provider, SessionRef};
use serde_json::{Value, json};

const SOURCE_ID: &str = "11111111-1111-4111-8111-111111111111";

#[test]
#[ignore = "requires OMNI_TEST_GROK_BIN"]
fn installed_grok_round_trips_isolated_synthetic_history() {
    let grok_binary = env::var_os("OMNI_TEST_GROK_BIN")
        .map(PathBuf::from)
        .expect("OMNI_TEST_GROK_BIN");
    let temporary = tempfile::tempdir().expect("temporary conformance root");
    let home = temporary.path().join("home");
    let codex_home = temporary.path().join("codex");
    let grok_home = temporary.path().join("grok");
    let omnisession_home = temporary.path().join("omnisession");
    let workspace = temporary.path().join("workspace");
    let sessions = codex_home.join("sessions/2026/01/01");
    for directory in [&home, &grok_home, &workspace, &sessions] {
        fs::create_dir_all(directory).expect("isolated conformance directory");
    }
    let rollout = sessions.join(format!("rollout-2026-01-01T00-00-00-{SOURCE_ID}.jsonl"));
    fs::write(&rollout, synthetic_codex_rollout(&workspace)).expect("synthetic Codex rollout");

    let output = isolated_command(Path::new(env!("CARGO_BIN_EXE_omni")), &home)
        .args([
            "resume",
            &format!("codex:{SOURCE_ID}"),
            "--in",
            "grok",
            "--materialize-only",
            "--allow-workspace-mismatch",
        ])
        .current_dir(&workspace)
        .env("CODEX_HOME", &codex_home)
        .env("GROK_HOME", &grok_home)
        .env("OMNISESSION_HOME", &omnisession_home)
        .env("OMNI_GROK_BIN", &grok_binary)
        .env("GROK_LOG_FILE", home.join("grok-native.log"))
        .output()
        .expect("run OmniSession Grok conformance");

    #[cfg(windows)]
    if !output.status.success() {
        diagnose_windows_startup(&output, &grok_binary, &home, &grok_home, &workspace);
    }

    assert!(
        output.status.success(),
        "Grok conformance failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("Created and verified grok:"),
        "Grok conformance omitted verification confirmation"
    );

    assert_eq!(
        fs::read_to_string(&rollout).expect("source read-back"),
        synthetic_codex_rollout(&workspace)
    );
    assert_native_import_and_fork(&grok_binary, &home, &grok_home, &workspace);
    assert_eq!(
        fs::read_to_string(&rollout).expect("source after native load and fork"),
        synthetic_codex_rollout(&workspace)
    );
}

#[cfg(windows)]
fn diagnose_windows_startup(
    output: &std::process::Output,
    binary: &Path,
    home: &Path,
    grok_home: &Path,
    workspace: &Path,
) {
    eprintln!(
        "OmniSession Grok import failed before the direct ACP probe:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    print_synthetic_startup_log(&home.join("grok-native.log"));
    let bootstrapped = grok_home.join("bin/grok.exe");
    let native = if binary
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("cmd"))
    {
        bootstrapped.as_path()
    } else {
        binary
    };
    for (no_window, remote_fetch) in [(true, true), (false, true), (true, false), (false, false)] {
        let temporary = tempfile::tempdir().expect("isolated direct probe");
        let probe_home = temporary.path().join("home");
        let probe_grok = temporary.path().join("grok");
        for path in [&probe_home, &probe_grok] {
            fs::create_dir_all(path).expect("direct probe home");
        }
        fs::write(
            probe_grok.join("config.toml"),
            format!("[features]\nremote_fetch = {remote_fetch}\n"),
        )
        .expect("synthetic probe configuration");
        let flags = if no_window { 0x0800_0200 } else { 0x0000_0200 };
        let started = Instant::now();
        let result = std::panic::catch_unwind(|| {
            let mut server = SyntheticGrokServer::start_with_flags(
                native,
                &probe_home,
                &probe_grok,
                workspace,
                flags,
            );
            let (initialized, _) = server.request(0, "initialize", &initialize_params());
            initialized
                .pointer("/_meta/modelState/currentModelId")
                .is_some()
        });
        eprintln!(
            "Direct Grok probe: no_window={no_window}, remote_fetch={remote_fetch}, model_state={}, elapsed_ms={}",
            result.unwrap_or(false),
            started.elapsed().as_millis()
        );
    }
}

#[cfg(windows)]
fn print_synthetic_startup_log(path: &Path) {
    let Ok(mut file) = fs::File::open(path) else {
        return;
    };
    let length = file.metadata().expect("synthetic log metadata").len();
    let mut bytes = Vec::new();
    (&mut file)
        .take(8 * 1024)
        .read_to_end(&mut bytes)
        .expect("bounded synthetic log head");
    if length > 8 * 1024 {
        file.seek(SeekFrom::Start(
            length.saturating_sub(8 * 1024).max(8 * 1024),
        ))
        .expect("synthetic log tail position");
        bytes.extend_from_slice(b"\n... bounded startup log tail ...\n");
        file.take(8 * 1024)
            .read_to_end(&mut bytes)
            .expect("bounded synthetic log tail");
    }
    eprintln!(
        "Synthetic Grok startup log:\n{}",
        omnis_core::redact_secrets(&String::from_utf8_lossy(&bytes))
    );
}

fn initialize_params() -> Value {
    json!({
        "protocolVersion": 1,
        "clientCapabilities": {"fs": {"readTextFile": false, "writeTextFile": false}, "terminal": false},
        "_meta": {
            "startupHints": {"nonInteractive": true, "skipGitStatus": true, "skipProjectLayout": true},
            "clientType": "omnisession",
            "clientVersion": env!("CARGO_PKG_VERSION")
        }
    })
}

fn isolated_command(binary: &Path, home: &Path) -> Command {
    let mut command = Command::new(binary);
    command
        .env_clear()
        .env("PATH", env::var_os("PATH").expect("PATH"))
        .env("HOME", home);
    #[cfg(windows)]
    {
        for name in ["SystemRoot", "WINDIR", "SystemDrive", "ComSpec", "PATHEXT"] {
            if let Some(value) = env::var_os(name) {
                command.env(name, value);
            }
        }
        command.env("USERPROFILE", home);
        for (name, relative) in [
            ("APPDATA", "AppData/Roaming"),
            ("LOCALAPPDATA", "AppData/Local"),
            ("TEMP", "tmp"),
            ("TMP", "tmp"),
        ] {
            let path = home.join(relative);
            fs::create_dir_all(&path).expect("isolated Windows runtime directory");
            command.env(name, path);
        }
    }
    command
}

fn assert_native_import_and_fork(binary: &Path, home: &Path, grok_home: &Path, workspace: &Path) {
    let summary_path = fs::read_dir(grok_home.join("sessions"))
        .expect("native Grok workspaces")
        .filter_map(Result::ok)
        .flat_map(|workspace| {
            fs::read_dir(workspace.path())
                .into_iter()
                .flatten()
                .filter_map(Result::ok)
        })
        .map(|session| session.path().join("summary.json"))
        .find(|summary| summary.is_file())
        .expect("imported native summary");
    let summary: Value = serde_json::from_slice(&fs::read(summary_path).expect("native summary"))
        .expect("native summary JSON");
    let id = summary
        .pointer("/info/id")
        .and_then(Value::as_str)
        .expect("imported session ID");
    let mut server = SyntheticGrokServer::start(binary, home, grok_home, workspace);
    let (initialized, _) = server.request(0, "initialize", &initialize_params());
    assert_eq!(
        summary["current_model_id"],
        initialized
            .pointer("/_meta/modelState/currentModelId")
            .expect("provider-selected default model")
            .clone()
    );
    let (loaded, notifications) = server.request_result(
        1,
        "session/load",
        &json!({
            "sessionId": id, "cwd": workspace, "mcpServers": []
        }),
    );
    if let Err(error) = loaded {
        assert_eq!(
            error["code"], -32000,
            "unexpected native load rejection: {error}"
        );
        assert_eq!(
            error["message"], "Authentication required",
            "unexpected native load rejection: {error}"
        );
        eprintln!(
            "Grok standard ACP load requires authentication; token-free import, default model, and native fork checks continue"
        );
    } else {
        assert_native_replay(&notifications);
    }
    let started_at = Utc::now();
    let (fork, _) = server.request(
        2,
        "_x.ai/session/fork",
        &json!({
            "sourceSessionId": id, "sourceCwd": workspace, "newCwd": workspace,
        }),
    );
    let child = fork["newSessionId"].as_str().expect("native fork child ID");
    assert_eq!(fork["parentSessionId"], id);
    let candidates = GrokAdapter::with_root(grok_home.join("sessions"))
        .fork_candidates_created_since(&SessionRef::new(Provider::Grok, id), workspace, started_at)
        .expect("native child lineage discovery");
    assert_eq!(candidates, [SessionRef::new(Provider::Grok, child)]);
}

fn assert_native_replay(notifications: &[Value]) {
    let updates = notifications
        .iter()
        .filter_map(|record| record.pointer("/params/update"))
        .collect::<Vec<_>>();
    let replay = updates
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        replay.contains("Synthetic opening question"),
        "native ACP load omitted opening user message"
    );
    assert!(
        replay.contains("Synthetic visible message 9"),
        "native ACP load omitted final visible turn"
    );
    assert!(
        replay.contains("Synthetic native tool result"),
        "native ACP load omitted native tool outcome"
    );
    assert!(
        !replay.contains("synthetic-value"),
        "native ACP replay contains an unredacted source secret"
    );
    assert!(
        updates
            .iter()
            .any(|update| update["sessionUpdate"] == "tool_call"),
        "native ACP load omitted tool calls"
    );
    assert!(
        updates
            .iter()
            .any(|update| update["sessionUpdate"] == "tool_call_update"),
        "native ACP load omitted tool completion updates"
    );
}

struct SyntheticGrokServer {
    child: Child,
    stdin: ChildStdin,
    records: Receiver<Value>,
    #[cfg(windows)]
    startup_log: PathBuf,
}

impl SyntheticGrokServer {
    fn start(binary: &Path, home: &Path, grok_home: &Path, workspace: &Path) -> Self {
        Self::start_with_flags(binary, home, grok_home, workspace, 0x0800_0200)
    }

    fn start_with_flags(
        binary: &Path,
        home: &Path,
        grok_home: &Path,
        workspace: &Path,
        flags: u32,
    ) -> Self {
        #[cfg(not(windows))]
        let _ = flags;
        #[cfg(windows)]
        let bootstrapped_binary = grok_home.join("bin/grok.exe");
        #[cfg(windows)]
        let binary = if binary
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("cmd"))
        {
            assert!(
                bootstrapped_binary.is_file(),
                "Grok import did not bootstrap the native Windows binary"
            );
            bootstrapped_binary.as_path()
        } else {
            binary
        };
        let mut command = isolated_command(binary, home);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(flags);
        }
        let mut child = command
            .args(["agent", "--no-leader", "stdio"])
            .env("GROK_HOME", grok_home)
            .env("GROK_MANAGED_BY_NPM", "1")
            .env("GROK_LOG_FILE", home.join("grok-native-direct.log"))
            .current_dir(workspace)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("isolated native Grok ACP server");
        let stdin = child.stdin.take().expect("ACP stdin");
        let stdout = child.stdout.take().expect("ACP stdout");
        let (sender, records) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let record = serde_json::from_str::<Value>(&line.expect("ACP response line"))
                    .expect("ACP response JSON");
                if sender.send(record).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            stdin,
            records,
            #[cfg(windows)]
            startup_log: home.join("grok-native-direct.log"),
        }
    }

    fn request(&mut self, id: u64, method: &str, params: &Value) -> (Value, Vec<Value>) {
        let (result, notifications) = self.request_result(id, method, params);
        (
            result.unwrap_or_else(|error| panic!("native ACP {method} failed: {error}")),
            notifications,
        )
    }

    fn request_result(
        &mut self,
        id: u64,
        method: &str,
        params: &Value,
    ) -> (Result<Value, Value>, Vec<Value>) {
        let deadline = Instant::now() + Duration::from_secs(30);
        serde_json::to_writer(
            &mut self.stdin,
            &json!({
                "jsonrpc": "2.0", "id": id, "method": method, "params": params,
            }),
        )
        .expect("ACP request JSON");
        self.stdin.write_all(b"\n").expect("ACP request newline");
        self.stdin.flush().expect("ACP request flush");
        let mut notifications = Vec::new();
        loop {
            let received = deadline
                .checked_duration_since(Instant::now())
                .map_or(Err(mpsc::RecvTimeoutError::Timeout), |remaining| {
                    self.records.recv_timeout(remaining)
                });
            let record = received.unwrap_or_else(|error| {
                #[cfg(windows)]
                print_synthetic_startup_log(&self.startup_log);
                panic!("native ACP {method} response failed: {error}");
            });
            if record.get("id").and_then(Value::as_u64) == Some(id) {
                let result = record.get("error").map_or_else(
                    || Ok(record.get("result").expect("native ACP result").clone()),
                    |error| Err(error.clone()),
                );
                return (result, notifications);
            }
            assert!(
                record.get("id").is_none(),
                "native history loading unexpectedly requested a client action"
            );
            notifications.push(record);
        }
    }
}

impl Drop for SyntheticGrokServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn synthetic_codex_rollout(workspace: &std::path::Path) -> String {
    let mut records = vec![json!({
        "timestamp": "2026-01-01T00:00:00Z",
        "type": "session_meta",
        "payload": {
            "id": SOURCE_ID,
            "cwd": workspace,
            "cli_version": "synthetic",
            "model_provider": "synthetic"
        }
    })];
    records.push(message_record("user", "Synthetic opening question"));
    records.push(json!({
        "timestamp": "2026-01-01T00:00:01Z",
        "type": "response_item",
        "payload": {
            "type": "function_call",
            "name": "shell",
            "call_id": "synthetic-pair",
            "arguments": "{\"command\":\"cargo test\"}"
        }
    }));
    records.push(json!({
        "timestamp": "2026-01-01T00:00:01Z",
        "type": "response_item",
        "payload": {
            "type": "function_call_output",
            "call_id": "synthetic-pair",
            "output": "Synthetic native tool result"
        }
    }));
    for index in 0..90 {
        records.push(json!({
            "timestamp": "2026-01-01T00:00:01Z",
            "type": "response_item",
            "payload": {
                "type": "function_call_output",
                "call_id": format!("synthetic-{index}"),
                "output": if index == 42 {
                    "secret=synthetic-value".to_owned()
                } else {
                    format!("Synthetic documentary result {index}")
                }
            }
        }));
    }
    for index in 0_usize..10 {
        let role = if index % 2 == 0 { "assistant" } else { "user" };
        records.push(message_record(
            role,
            &format!("Synthetic visible message {index}"),
        ));
    }
    records
        .into_iter()
        .map(|record| serde_json::to_string(&record).expect("serialize synthetic record"))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

fn message_record(role: &str, text: &str) -> serde_json::Value {
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
