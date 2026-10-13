use std::{
    env, fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc::{self, Receiver},
    thread,
    time::Duration,
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
        .output()
        .expect("run OmniSession Grok conformance");

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
    let (initialized, _) = server.request(0, "initialize", &json!({
        "protocolVersion": 1,
        "clientCapabilities": {"fs": {"readTextFile": false, "writeTextFile": false}, "terminal": false},
        "_meta": {"startupHints": {"nonInteractive": true, "skipGitStatus": true, "skipProjectLayout": true}}
    }));
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
}

impl SyntheticGrokServer {
    fn start(binary: &Path, home: &Path, grok_home: &Path, workspace: &Path) -> Self {
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
        let mut child = isolated_command(binary, home)
            .args(["agent", "--no-leader", "stdio"])
            .env("GROK_HOME", grok_home)
            .env("GROK_MANAGED_BY_NPM", "1")
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
            let record = self
                .records
                .recv_timeout(Duration::from_secs(30))
                .expect("native ACP response before timeout");
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
