#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::{
    env, fs,
    io::{BufRead, BufReader, Read, Write},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc::{self, Receiver},
    thread,
    time::{Duration, Instant},
};

use omnis_adapters::{CursorCliAdapter, ProviderAdapter};
use serde_json::{Value, json};
use wait_timeout::ChildExt;

const SOURCE_ID: &str = "11111111-1111-4111-8111-111111111111";
const MESSAGES: [(&str, &str); 4] = [
    (
        "user",
        "Synthetic Cursor opening request OMNI_CURSOR_FIRST_7319",
    ),
    (
        "assistant",
        "Synthetic Cursor opening answer OMNI_CURSOR_SECOND_5162",
    ),
    (
        "user",
        "Synthetic Cursor final request OMNI_CURSOR_THIRD_2846",
    ),
    (
        "assistant",
        "Synthetic Cursor final answer OMNI_CURSOR_LAST_9034",
    ),
];
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_ACP_OUTPUT_BYTES: u64 = 4 * 1024 * 1024;

#[test]
#[ignore = "requires OMNI_TEST_CURSOR_LOAD=1, OMNI_TEST_CURSOR_BIN, and Cursor auth in environment"]
fn installed_cursor_loads_imported_history_without_prompting() {
    assert_eq!(
        env::var("OMNI_TEST_CURSOR_LOAD").as_deref(),
        Ok("1"),
        "explicit credential-configured Cursor load opt-in is required"
    );
    let binary = env::var_os("OMNI_TEST_CURSOR_BIN")
        .map(PathBuf::from)
        .expect("OMNI_TEST_CURSOR_BIN");
    assert!(
        binary.is_absolute() && binary.is_file(),
        "installed Cursor binary"
    );
    let credentials = ["CURSOR_API_KEY", "CURSOR_AUTH_TOKEN"]
        .into_iter()
        .filter_map(|name| {
            env::var_os(name)
                .filter(|value| !value.is_empty())
                .map(|value| (name, value))
        })
        .collect::<Vec<_>>();
    assert!(
        !credentials.is_empty(),
        "Cursor conformance requires environment authentication; credential files are never copied"
    );

    let fixture = Fixture::new();
    let id = fixture.materialize(&binary);
    let imported_database = fixture.imported_database(&id);
    let imported_bytes = fs::read(&imported_database).expect("generated Cursor database");

    // ACP has its own session namespace. Copy only this generated synthetic graph into it.
    let acp_session = fixture.config.join("acp-sessions").join(&id);
    fs::create_dir_all(&acp_session).expect("isolated ACP session directory");
    fs::copy(&imported_database, acp_session.join("store.db"))
        .expect("synthetic graph for provider-owned ACP loader");
    fs::write(
        acp_session.join("meta.json"),
        json!({"schemaVersion": 1, "cwd": fixture.workspace}).to_string(),
    )
    .expect("isolated ACP workspace metadata");

    let mut command = fixture.command(&binary);
    command.arg("acp").envs(credentials);
    let mut server = CursorServer::start(command);
    let (initialized, _) = server.request(
        1,
        "initialize",
        &json!({
            "protocolVersion": 1,
            "clientCapabilities": {"fs": {"readTextFile": false, "writeTextFile": false}, "terminal": false},
            "clientInfo": {"name": "omnisession-conformance", "version": "1"},
        }),
    );
    assert_eq!(initialized["agentCapabilities"]["loadSession"], true);
    let (_, notifications) = server.request(
        2,
        "session/load",
        &json!({"sessionId": id, "cwd": fixture.workspace, "mcpServers": []}),
    );
    let replayed = replayed_messages(&notifications, &id);
    let expected = MESSAGES.map(|(role, text)| (role.to_owned(), text.to_owned()));
    assert!(
        replayed == expected,
        "provider-owned load changed synthetic history"
    );
    assert_eq!(
        server.client_requests, 0,
        "history loading requested a client action"
    );
    assert!(
        fs::read(&imported_database).expect("original synthetic Cursor database") == imported_bytes,
        "ACP loading changed the original imported session"
    );
    fixture.assert_source_unchanged();
}

struct Fixture {
    _temporary: tempfile::TempDir,
    root: PathBuf,
    config: PathBuf,
    workspace: PathBuf,
    source: PathBuf,
    source_bytes: Vec<u8>,
}

impl Fixture {
    fn new() -> Self {
        let temporary = tempfile::tempdir().expect("temporary Cursor conformance root");
        let root = temporary.path().canonicalize().expect("canonical fixture");
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700))
            .expect("private authenticated conformance root");
        let config = root.join("cursor");
        let workspace = root.join("workspace");
        let sessions = root.join("codex/sessions/2026/01/01");
        for directory in [&workspace, &sessions, &config, &root.join("home")] {
            fs::create_dir_all(directory).expect("isolated fixture directory");
        }
        let mut records = vec![json!({
            "timestamp": "2026-01-01T00:00:00Z", "type": "session_meta",
            "payload": {"id": SOURCE_ID, "cwd": workspace, "cli_version": "synthetic", "model_provider": "synthetic"},
        })];
        records.extend(MESSAGES.map(|(role, text)| {
            json!({
                "timestamp": "2026-01-01T00:00:01Z", "type": "response_item",
                "payload": {"type": "message", "role": role, "content": [{
                    "type": if role == "user" { "input_text" } else { "output_text" }, "text": text,
                }]},
            })
        }));
        let source_bytes = (records
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
            + "\n")
            .into_bytes();
        let source = sessions.join(format!("rollout-2026-01-01T00-00-00-{SOURCE_ID}.jsonl"));
        fs::write(&source, &source_bytes).expect("synthetic Codex source");
        Self {
            _temporary: temporary,
            root,
            config,
            workspace,
            source,
            source_bytes,
        }
    }

    fn command(&self, binary: &Path) -> Command {
        let mut command = Command::new(binary);
        command
            .env_clear()
            .env("PATH", env::var_os("PATH").expect("test PATH"))
            .env("HOME", self.root.join("home"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("CURSOR_CONFIG_DIR", &self.config)
            .env("CURSOR_DATA_DIR", self.root.join("cursor-data"))
            .env("CURSOR_AGENT_HOME", self.config.join("chats"))
            .env("CODEX_HOME", self.root.join("codex"))
            .env("OMNISESSION_HOME", self.root.join("omni"))
            .current_dir(&self.workspace);
        command
    }

    fn materialize(&self, binary: &Path) -> String {
        let output = tempfile::tempfile().expect("bounded import output");
        let mut child = self
            .command(Path::new(env!("CARGO_BIN_EXE_omni")))
            .args([
                "resume",
                &format!("codex:{SOURCE_ID}"),
                "--in",
                "cursor",
                "--materialize-only",
            ])
            .env("OMNI_CURSOR_AGENT_BIN", binary)
            .stdin(Stdio::null())
            .stdout(Stdio::from(
                output.try_clone().expect("import output handle"),
            ))
            .stderr(Stdio::null())
            .spawn()
            .expect("isolated synthetic Cursor import");
        let status = child
            .wait_timeout(REQUEST_TIMEOUT)
            .expect("wait for import");
        if status.is_none() {
            let _ = child.kill();
            let _ = child.wait();
        }
        assert!(
            status.is_some_and(|status| status.success()),
            "synthetic Cursor import failed or timed out"
        );
        let mut output = output;
        std::io::Seek::rewind(&mut output).expect("rewind import output");
        let mut text = String::new();
        output
            .take(64 * 1024)
            .read_to_string(&mut text)
            .expect("read import result");
        text.lines()
            .find_map(|line| {
                line.strip_prefix("Created and verified cursor-cli:")?
                    .strip_suffix('.')
            })
            .map(str::to_owned)
            .expect("verified Cursor session ID")
    }

    fn imported_database(&self, id: &str) -> PathBuf {
        CursorCliAdapter::with_root(self.config.join("chats"))
            .list_sessions(Some(&self.workspace))
            .expect("synthetic session discovery")
            .into_iter()
            .find(|session| session.session.id == id)
            .and_then(|session| session.source_path)
            .and_then(|metadata| {
                metadata
                    .parent()
                    .map(|directory| directory.join("store.db"))
            })
            .expect("generated Cursor database path")
    }

    fn assert_source_unchanged(&self) {
        assert!(
            fs::read(&self.source).expect("synthetic source read-back") == self.source_bytes,
            "native loading changed the source session"
        );
    }
}

struct CursorServer {
    child: Child,
    stdin: ChildStdin,
    records: Receiver<Value>,
    client_requests: usize,
}

impl CursorServer {
    fn start(mut command: Command) -> Self {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("isolated installed Cursor ACP server");
        let stdin = child.stdin.take().expect("ACP stdin");
        let stdout = child.stdout.take().expect("ACP stdout");
        let (sender, records) = mpsc::sync_channel(64);
        thread::spawn(move || {
            for line in BufReader::new(stdout.take(MAX_ACP_OUTPUT_BYTES)).lines() {
                let Ok(line) = line else { break };
                let Ok(record) = serde_json::from_str(&line) else {
                    break;
                };
                if sender.send(record).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            stdin,
            records,
            client_requests: 0,
        }
    }

    fn send(&mut self, record: &Value) {
        serde_json::to_writer(&mut self.stdin, record).expect("ACP request");
        self.stdin.write_all(b"\n").expect("ACP newline");
        self.stdin.flush().expect("ACP flush");
    }

    fn request(&mut self, id: u64, method: &str, params: &Value) -> (Value, Vec<Value>) {
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        let deadline = Instant::now() + REQUEST_TIMEOUT;
        let mut notifications = Vec::new();
        loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .expect("Cursor ACP request deadline");
            let record = self
                .records
                .recv_timeout(remaining)
                .expect("Cursor ACP response failed or timed out");
            if let Some(action) = record.get("method").and_then(Value::as_str)
                && let Some(request_id) = record.get("id")
            {
                self.client_requests += 1;
                let response = if action == "session/request_permission" {
                    json!({"jsonrpc": "2.0", "id": request_id, "result": {"outcome": {"outcome": "cancelled"}}})
                } else {
                    json!({"jsonrpc": "2.0", "id": request_id, "error": {"code": -32601, "message": "Conformance client denies actions"}})
                };
                self.send(&response);
            } else if record.get("id").and_then(Value::as_u64) == Some(id) {
                assert!(
                    record.get("error").is_none(),
                    "Cursor ACP {method} failed (code {:?})",
                    record["error"]["code"].as_i64()
                );
                return (
                    record.get("result").expect("Cursor ACP result").clone(),
                    notifications,
                );
            } else {
                notifications.push(record);
            }
        }
    }
}

impl Drop for CursorServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn replayed_messages(records: &[Value], session: &str) -> Vec<(String, String)> {
    let mut messages: Vec<(String, String)> = Vec::new();
    for record in records {
        if record["method"] != "session/update" || record["params"]["sessionId"] != session {
            continue;
        }
        let update = &record["params"]["update"];
        let role = match update["sessionUpdate"].as_str() {
            Some("user_message_chunk") => "user",
            Some("agent_message_chunk") => "assistant",
            _ => continue,
        };
        if update["content"]["type"] != "text" {
            continue;
        }
        let text = update["content"]["text"]
            .as_str()
            .expect("Cursor text chunk");
        if let Some((last_role, last_text)) = messages.last_mut()
            && last_role == role
        {
            last_text.push_str(text);
        } else {
            messages.push((role.to_owned(), text.to_owned()));
        }
    }
    messages
}
