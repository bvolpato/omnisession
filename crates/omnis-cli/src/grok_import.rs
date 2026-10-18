use std::{
    collections::BTreeMap,
    fmt,
    io::{BufRead, BufReader, Write},
    path::Path,
    process::{Child, ChildStdin, Stdio},
    sync::{
        OnceLock,
        mpsc::{self, Receiver},
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{SecondsFormat, Utc};
use omnis_core::{
    HandoffRole, NativeTrajectoryItem, import_trajectory, native_trajectory_items,
    native_trajectory_signature, readback_trajectory, redact_secrets, without_call_ids,
};
use omnis_ir::{CanonicalSnapshot, Provider, SessionRef};
use serde_json::{Value, json};
use uuid::Uuid;
use wait_timeout::ChildExt;

use crate::{
    interrupt::{HelperProcess, wait_or_kill},
    provider_compatibility::MINIMUM_GROK_VERSION,
};
const RPC_TIMEOUT: Duration = Duration::from_secs(30);
const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(45);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);

pub struct GrokImport {
    pub target: SessionRef,
    pub expected_items: Vec<NativeTrajectoryItem>,
    pub native_tool_records: usize,
    pub history_items: usize,
    pub tool_events: usize,
    pub truncated: bool,
    summary: Value,
    model_id: OnceLock<String>,
    updates: Vec<Value>,
}

pub fn build(snapshot: &CanonicalSnapshot, cwd: &Path) -> Result<GrokImport> {
    let trajectory = import_trajectory(snapshot);
    if trajectory.items.is_empty() {
        bail!("source has no visible trajectory eligible for Grok import");
    }

    let history_items = trajectory.items.len();
    let source = snapshot.session.to_string();
    let items = native_trajectory_items(&trajectory);
    let native_tool_records = items
        .iter()
        .filter(|item| matches!(item, NativeTrajectoryItem::Tool { .. }))
        .count();

    let id = Uuid::new_v4().to_string();
    let target = SessionRef::new(Provider::Grok, &id);
    let now = Utc::now();
    let timestamp = now.to_rfc3339_opts(SecondsFormat::Millis, true);
    let unix_seconds = u64::try_from(now.timestamp()).unwrap_or_default();
    let cwd_text = cwd
        .to_str()
        .context("Grok native import requires a UTF-8 workspace path")?;
    let title = snapshot
        .title
        .as_deref()
        .map(redact_secrets)
        .filter(|title| !title.trim().is_empty())
        .unwrap_or_else(|| format!("Imported from {source}"));
    let envelope = |update: Value| {
        json!({
            "timestamp": unix_seconds,
            "method": "session/update",
            "params": { "sessionId": target.id, "update": update }
        })
    };
    let mut updates = Vec::with_capacity(items.len() + native_tool_records);
    for item in &items {
        match item {
            NativeTrajectoryItem::Message { role, text } => updates.push(envelope(json!({
                "sessionUpdate": match role {
                    HandoffRole::User => "user_message_chunk",
                    HandoffRole::Assistant => "agent_message_chunk",
                },
                "messageId": Uuid::new_v4().to_string(),
                "content": { "type": "text", "text": text }
            }))),
            NativeTrajectoryItem::Tool {
                call_id,
                name,
                input,
                output,
                is_error,
            } => {
                updates.push(envelope(json!({
                    "sessionUpdate": "tool_call",
                    "toolCallId": call_id,
                    "title": name,
                    "kind": "other",
                    "status": "pending",
                    "rawInput": input
                })));
                updates.push(envelope(json!({
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": call_id,
                    "status": if *is_error { "failed" } else { "completed" },
                    "content": [{ "type": "content", "content": { "type": "text", "text": output } }]
                })));
            }
        }
    }
    let summary = json!({
        "info": { "id": id, "cwd": cwd_text },
        "session_summary": title,
        "created_at": timestamp,
        "updated_at": timestamp,
        "num_messages": updates.len(),
        "num_chat_messages": items.len()
    });

    Ok(GrokImport {
        target,
        expected_items: without_call_ids(items),
        native_tool_records,
        history_items,
        tool_events: trajectory.tool_events,
        truncated: trajectory.truncated,
        summary,
        model_id: OnceLock::new(),
        updates,
    })
}

pub fn ensure_supported(binary: &Path) -> Result<String> {
    let version = installed_version(binary)?;
    if !is_supported_version(&version) {
        bail!(
            "Grok {version} is too old for native trajectory import; supported versions: >= {MINIMUM_GROK_VERSION}"
        );
    }
    Ok(version)
}

fn is_supported_version(version: &str) -> bool {
    crate::version_gate::is_release_at_least(version, MINIMUM_GROK_VERSION)
}

pub fn materialize(import: &GrokImport, binary: &Path, cwd: &Path) -> Result<()> {
    ensure_supported(binary)?;
    let mut server = GrokServer::start(binary, cwd)?;
    let initialized = server.initialize()?;
    let provider_model = initialized_model(&initialized)?;
    import.model_id.get_or_init(|| provider_model);
    let params = json!({
        "sessionId": import.target.id,
        "cwd": cwd,
        "state": { "summary": import_summary(import) },
        "updates": import.updates
    });
    let result = match server.request(1, "_x.ai/session/import", &params) {
        Ok(result) => result,
        Err(error) => {
            drop(server);
            if stored_import_matches(import, binary, cwd) {
                return Err(combine_rollback_error(
                    error.context("importing Grok trajectory"),
                    rollback(import, binary, cwd),
                ));
            }
            return Err(error).context("importing Grok trajectory");
        }
    };
    if result.get("imported").and_then(Value::as_bool) != Some(true) {
        bail!("Grok refused to import generated session because target ID already exists");
    }

    if let Err(error) = verify_import(&mut server, import, cwd) {
        let rollback = server
            .request(
                5,
                "_x.ai/session/delete",
                &json!({ "sessionId": import.target.id, "cwd": cwd }),
            )
            .and_then(|result| {
                if result.get("success").and_then(Value::as_bool) == Some(true) {
                    Ok(())
                } else {
                    bail!("Grok did not confirm target-session deletion")
                }
            })
            .and_then(|()| server.shutdown());
        return Err(combine_rollback_error(error, rollback));
    }
    if let Err(error) = server.shutdown() {
        drop(server);
        return Err(combine_rollback_error(
            error.context("flushing imported Grok session"),
            rollback(import, binary, cwd),
        ));
    }
    Ok(())
}

pub fn rollback(import: &GrokImport, binary: &Path, cwd: &Path) -> Result<()> {
    ensure_supported(binary)?;
    let mut server = GrokServer::start(binary, cwd)?;
    server.initialize()?;
    verify_import(&mut server, import, cwd)
        .context("refusing to delete changed Grok target session")?;
    let result = server.request(
        1,
        "_x.ai/session/delete",
        &json!({ "sessionId": import.target.id, "cwd": cwd }),
    )?;
    if result.get("success").and_then(Value::as_bool) != Some(true) {
        bail!("Grok did not confirm target-session deletion");
    }
    server.shutdown().context("flushing Grok rollback")
}

// Tags a failed rollback so fallbacks refuse to launch while the generated session may remain.
fn combine_rollback_error(error: anyhow::Error, rollback: Result<()>) -> anyhow::Error {
    crate::error_after_rollback(
        error,
        rollback.map_err(|rollback_error| anyhow!(redact_secrets(&rollback_error.to_string()))),
        "Grok",
    )
}

fn stored_import_matches(import: &GrokImport, binary: &Path, cwd: &Path) -> bool {
    let result = (|| {
        let mut server = GrokServer::start(binary, cwd)?;
        server.initialize()?;
        verify_import(&mut server, import, cwd)
    })();
    result.is_ok()
}

pub fn readback_matches(snapshot: &CanonicalSnapshot, expected: &[NativeTrajectoryItem]) -> bool {
    readback_trajectory(snapshot)
        .is_some_and(|trajectory| native_trajectory_signature(&trajectory) == expected)
}

fn verify_import(server: &mut GrokServer, import: &GrokImport, cwd: &Path) -> Result<()> {
    let state = server.request(
        2,
        "_x.ai/session/state",
        &json!({ "sessionId": import.target.id, "cwd": cwd }),
    )?;
    let returned_summary = state
        .get("summary")
        .or_else(|| state.pointer("/state/summary"))
        .context("Grok state read-back omitted summary")?;
    let returned_id = returned_summary.pointer("/info/id").and_then(Value::as_str);
    if returned_id != Some(import.target.id.as_str()) {
        bail!("Grok state read-back returned a different session ID");
    }
    if let Some(path) = first_summary_mismatch(returned_summary, &import_summary(import), "$") {
        bail!("Grok state read-back changed imported summary field `{path}`");
    }

    let result = server.request(
        3,
        "_x.ai/session/updates",
        &json!({ "sessionId": import.target.id, "cwd": cwd }),
    )?;
    let updates = result
        .get("updates")
        .and_then(Value::as_array)
        .or_else(|| result.as_array())
        .context("Grok updates read-back omitted updates")?;
    let actual = updates.iter().map(normalized_update).collect::<Vec<_>>();
    let expected = import
        .updates
        .iter()
        .map(normalized_update)
        .collect::<Vec<_>>();
    if actual != expected {
        bail!("Grok updates read-back did not match imported trajectory");
    }

    Ok(())
}

fn initialized_model(initialized: &Value) -> Result<String> {
    initialized
        .pointer("/_meta/modelState/currentModelId")
        .and_then(Value::as_str)
        .filter(|model| {
            !model.trim().is_empty() && model.len() <= 256 && !model.chars().any(char::is_control)
        })
        .map(str::to_owned)
        .context(
            "Grok initialization omitted a valid current model; refusing to guess an import model",
        )
}

fn import_summary(import: &GrokImport) -> Value {
    let mut summary = import.summary.clone();
    if let Some(model) = import.model_id.get() {
        summary["current_model_id"] = json!(model);
    }
    summary
}

fn first_summary_mismatch(actual: &Value, expected: &Value, path: &str) -> Option<String> {
    match expected {
        Value::Object(expected) => {
            let Some(actual) = actual.as_object() else {
                return Some(path.to_owned());
            };
            expected.iter().find_map(|(key, expected)| {
                let next = format!("{path}.{key}");
                actual.get(key).map_or_else(
                    || Some(next.clone()),
                    |actual| first_summary_mismatch(actual, expected, &next),
                )
            })
        }
        Value::Array(expected) => {
            let Some(actual) = actual.as_array() else {
                return Some(path.to_owned());
            };
            if actual.len() != expected.len() {
                return Some(path.to_owned());
            }
            actual
                .iter()
                .zip(expected)
                .enumerate()
                .find_map(|(index, (actual, expected))| {
                    first_summary_mismatch(actual, expected, &format!("{path}[{index}]"))
                })
        }
        _ => (actual != expected).then(|| path.to_owned()),
    }
}

fn normalized_update(record: &Value) -> Value {
    json!({
        "timestamp": record.get("timestamp"),
        "sessionId": record.pointer("/params/sessionId"),
        "update": record.pointer("/params/update"),
    })
}

fn installed_version(binary: &Path) -> Result<String> {
    let mut child = crate::provider_launch::provider_process(binary)?
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .outside_terminal_group()
        .spawn()
        .with_context(|| format!("executing `{}`", binary.display()))?;
    let Some(status) = wait_or_kill(&mut child, crate::version_gate::PROBE_TIMEOUT)
        .context("waiting for Grok version")?
    else {
        bail!("Grok version probe timed out");
    };
    let output = child.wait_with_output().context("reading Grok version")?;
    if !status.success() {
        bail!("Grok version probe exited with status {status}");
    }
    let stdout = String::from_utf8(output.stdout).context("Grok version was not UTF-8")?;
    crate::version_gate::find_version(&stdout).context("Grok returned an unrecognized version")
}

struct GrokServer {
    child: Child,
    stdin: Option<ChildStdin>,
    messages: Receiver<Result<Value, String>>,
}

impl GrokServer {
    fn start(binary: &Path, cwd: &Path) -> Result<Self> {
        let mut child = crate::provider_launch::provider_process(binary)?
            .args(["agent", "--no-leader", "stdio"])
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .outside_terminal_group()
            .spawn()
            .with_context(|| format!("starting `{}` ACP agent", binary.display()))?;
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            let _ = child.kill();
            let _ = child.wait();
            bail!("opening Grok ACP pipes");
        };
        let (sender, messages) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let value = line.map_err(|error| error.to_string()).and_then(|line| {
                    serde_json::from_str(&line).map_err(|error| error.to_string())
                });
                if sender.send(value).is_err() {
                    break;
                }
            }
        });
        Ok(Self {
            child,
            stdin: Some(stdin),
            messages,
        })
    }

    fn initialize(&mut self) -> Result<Value> {
        self.request(
            0,
            "initialize",
            &json!({
                "protocolVersion": 1,
                "clientCapabilities": {
                    "fs": { "readTextFile": false, "writeTextFile": false },
                    "terminal": false
                },
                "_meta": {
                    "startupHints": {
                        "nonInteractive": true,
                        "skipGitStatus": true,
                        "skipProjectLayout": true
                    },
                    "clientType": "omnisession",
                    "clientVersion": env!("CARGO_PKG_VERSION")
                }
            }),
        )
    }

    fn request(&mut self, id: u64, method: &str, params: &Value) -> Result<Value> {
        let deadline = Instant::now() + request_timeout(method);
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        }))?;
        let stdin = self.stdin.as_mut().context("Grok ACP input is closed")?;
        wait_for_response(stdin, &self.messages, id, method, deadline).map_err(|error| {
            let process = match self.child.try_wait() {
                Ok(Some(status)) => format!("exited with {status}"),
                Ok(None) => "still running".to_owned(),
                Err(error) => format!("status unavailable: {error}"),
            };
            error.context(format!("Grok ACP process {process}"))
        })
    }

    fn send(&mut self, value: &Value) -> Result<()> {
        let stdin = self.stdin.as_mut().context("Grok ACP input is closed")?;
        write_rpc_message(stdin, value)
    }

    fn shutdown(&mut self) -> Result<()> {
        self.stdin.take();
        if self
            .child
            .wait_timeout(SHUTDOWN_TIMEOUT)
            .context("waiting for Grok ACP shutdown")?
            .is_none()
        {
            self.child.kill().context("stopping Grok ACP agent")?;
            self.child.wait().context("reaping Grok ACP agent")?;
        }
        Ok(())
    }
}

fn request_timeout(method: &str) -> Duration {
    if method == "initialize" {
        // Cold startup can include model-catalog retries before the first ACP response.
        INITIALIZE_TIMEOUT
    } else {
        RPC_TIMEOUT
    }
}

#[derive(Default)]
struct RpcDiagnostics {
    notifications: usize,
    requests: usize,
    responses: usize,
    string_ids: usize,
    numeric_ids: usize,
    other_ids: usize,
    methods: BTreeMap<&'static str, usize>,
}

impl RpcDiagnostics {
    fn observe(&mut self, message: &Value) {
        let id = message.get("id").filter(|id| !id.is_null());
        if message.get("method").is_some() {
            let counter = if id.is_some() {
                &mut self.requests
            } else {
                &mut self.notifications
            };
            *counter = counter.saturating_add(1);
            let method = known_rpc_method(message.get("method").and_then(Value::as_str));
            let count = self.methods.entry(method).or_default();
            *count = count.saturating_add(1);
        } else {
            self.responses = self.responses.saturating_add(1);
        }
        if let Some(id) = id {
            let counter = if id.is_string() {
                &mut self.string_ids
            } else if id.is_number() {
                &mut self.numeric_ids
            } else {
                &mut self.other_ids
            };
            *counter = counter.saturating_add(1);
        }
    }
}

impl fmt::Display for RpcDiagnostics {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "notifications={}, requests={}, responses={}, string_ids={}, numeric_ids={}, other_ids={}; methods=",
            self.notifications,
            self.requests,
            self.responses,
            self.string_ids,
            self.numeric_ids,
            self.other_ids
        )?;
        for (index, (method, count)) in self.methods.iter().enumerate() {
            if index > 0 {
                formatter.write_str(",")?;
            }
            write!(formatter, "{method}:{count}")?;
        }
        Ok(())
    }
}

fn known_rpc_method(method: Option<&str>) -> &'static str {
    match method {
        Some("session/update") => "session/update",
        Some("session/request_permission") => "session/request_permission",
        Some("fs/read_text_file") => "fs/read_text_file",
        Some("fs/write_text_file") => "fs/write_text_file",
        Some("terminal/create") => "terminal/create",
        Some("terminal/output") => "terminal/output",
        Some("terminal/release") => "terminal/release",
        Some("terminal/kill") => "terminal/kill",
        Some("terminal/wait_for_exit") => "terminal/wait_for_exit",
        _ => "unknown",
    }
}

fn write_rpc_message(stdin: &mut impl Write, value: &Value) -> Result<()> {
    serde_json::to_writer(&mut *stdin, value).context("writing Grok ACP message")?;
    stdin
        .write_all(b"\n")
        .context("terminating Grok ACP message")?;
    stdin.flush().context("flushing Grok ACP message")
}

fn wait_for_response(
    stdin: &mut impl Write,
    messages: &Receiver<Result<Value, String>>,
    id: u64,
    method: &str,
    deadline: Instant,
) -> Result<Value> {
    let mut diagnostics = RpcDiagnostics::default();
    let result = (|| {
        loop {
            let timeout = deadline
                .checked_duration_since(Instant::now())
                .context("request deadline elapsed")?;
            let message = messages
                .recv_timeout(timeout)
                .context("timed out or disconnected")?
                .map_err(|error| anyhow!("invalid Grok ACP response: {error}"))?;
            diagnostics.observe(&message);
            if message.get("method").is_some() {
                if let Some(id) = message.get("id").filter(|id| !id.is_null()) {
                    write_rpc_message(
                        stdin,
                        &json!({
                            "jsonrpc": "2.0", "id": id,
                            "error": {"code": -32601, "message": "Method not found"}
                        }),
                    )?;
                }
                continue;
            }
            if message.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(error) = message.get("error") {
                let message = error
                    .get("message")
                    .and_then(Value::as_str)
                    .map_or_else(|| "unknown ACP error".to_owned(), redact_secrets);
                bail!("Grok ACP rejected {method}: {message}");
            }
            return message
                .get("result")
                .cloned()
                .context("Grok ACP response omitted result");
        }
    })();
    result
        .with_context(|| format!("Grok ACP {method} (request {id}) failed; incoming {diagnostics}"))
}

impl Drop for GrokServer {
    fn drop(&mut self) {
        self.stdin.take();
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[cfg(test)]
pub(crate) fn synthetic_store_readback(import: &GrokImport) -> CanonicalSnapshot {
    use omnis_adapters::{GrokAdapter, ProviderAdapter};

    let root = tempfile::tempdir().expect("synthetic Grok store");
    let session = root.path().join("workspace").join(&import.target.id);
    std::fs::create_dir_all(&session).expect("synthetic Grok session directory");
    std::fs::write(session.join("summary.json"), import.summary.to_string())
        .expect("synthetic Grok summary");
    let updates = import
        .updates
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(session.join("updates.jsonl"), updates).expect("synthetic Grok updates");
    GrokAdapter::with_root(root.path())
        .read_session(&import.target)
        .expect("synthetic Grok read-back")
}

#[cfg(test)]
mod tests {
    use omnis_adapters::{CodexAdapter, ProviderAdapter};
    use omnis_core::HandoffMessage;

    use super::*;

    #[test]
    fn native_tool_pair_counts_both_update_envelopes() {
        let temporary = tempfile::tempdir().expect("synthetic Codex home");
        let sessions = temporary.path().join("sessions");
        std::fs::create_dir(&sessions).expect("synthetic sessions directory");
        let id = "11111111-1111-4111-8111-111111111111";
        let records = [
            json!({"type": "session_meta", "payload": {"id": id, "cwd": temporary.path()}}),
            json!({"type": "response_item", "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Synthetic question"}]}}),
            json!({"type": "response_item", "payload": {"type": "function_call", "name": "shell", "call_id": "synthetic-pair", "arguments": "{\"command\":\"synthetic history only\"}"}}),
            json!({"type": "response_item", "payload": {"type": "function_call_output", "call_id": "synthetic-pair", "output": "Synthetic historical result"}}),
        ];
        std::fs::write(
            sessions.join(format!("rollout-{id}.jsonl")),
            records.map(|record| record.to_string()).join("\n"),
        )
        .expect("synthetic paired-tool rollout");
        let snapshot = CodexAdapter::with_root(temporary.path())
            .read_session(&SessionRef::new(Provider::Codex, id))
            .expect("read synthetic source");
        let import = build(&snapshot, temporary.path()).expect("build paired-tool import");

        assert_eq!(import.native_tool_records, 1);
        assert_eq!(
            import.summary["num_messages"],
            json!(import.expected_items.len() + 1)
        );
        assert_eq!(
            import.summary["num_chat_messages"],
            json!(import.expected_items.len())
        );
    }

    #[test]
    fn initialization_can_respond_after_the_ordinary_rpc_budget() {
        let (sender, messages) = mpsc::channel();
        let started_at = Instant::now()
            .checked_sub(RPC_TIMEOUT)
            .expect("synthetic elapsed RPC budget");
        let peer = thread::spawn(move || {
            thread::sleep(Duration::from_millis(25));
            let _ = sender.send(Ok(json!({"id": 0, "result": {"initialized": true}})));
        });
        let result = wait_for_response(
            &mut Vec::new(),
            &messages,
            0,
            "initialize",
            started_at + request_timeout("initialize"),
        );
        peer.join().expect("synthetic delayed initialization");
        assert_eq!(
            result.expect("cold startup response"),
            json!({"initialized": true})
        );
    }

    #[test]
    fn ordinary_requests_keep_their_rpc_budget() {
        let (sender, messages) = mpsc::channel();
        let started_at = Instant::now()
            .checked_sub(RPC_TIMEOUT)
            .expect("synthetic elapsed RPC budget");
        let peer = thread::spawn(move || {
            thread::sleep(Duration::from_millis(25));
            let _ = sender.send(Ok(json!({"id": 1, "result": {"imported": true}})));
        });
        let result = wait_for_response(
            &mut Vec::new(),
            &messages,
            1,
            "_x.ai/session/import",
            started_at + request_timeout("_x.ai/session/import"),
        );
        peer.join().expect("synthetic delayed import");
        let error = result.expect_err("ordinary RPC must not receive the startup budget");
        assert!(
            error
                .chain()
                .any(|cause| cause.to_string() == "request deadline elapsed")
        );
    }

    #[test]
    fn initialization_still_has_a_finite_deadline() {
        let (sender, messages) = mpsc::channel();
        let started_at = Instant::now()
            .checked_sub(INITIALIZE_TIMEOUT)
            .expect("synthetic elapsed initialization budget");
        let peer = thread::spawn(move || {
            thread::sleep(Duration::from_millis(25));
            let _ = sender.send(Ok(json!({"id": 0, "result": {"initialized": true}})));
        });
        let result = wait_for_response(
            &mut Vec::new(),
            &messages,
            0,
            "initialize",
            started_at + request_timeout("initialize"),
        );
        peer.join().expect("synthetic expired initialization");
        let error = result.expect_err("initialization must remain bounded");
        assert!(
            error
                .chain()
                .any(|cause| cause.to_string() == "request deadline elapsed")
        );
    }

    #[test]
    fn reverse_requests_are_rejected_without_replaying_actions() {
        let (sender, messages) = mpsc::channel();
        for message in [
            json!({"jsonrpc": "2.0", "method": "session/update", "id": null}),
            json!({"jsonrpc": "2.0", "method": "fs/read_text_file", "id": "provider-request", "params": {"path": "synthetic-private-path"}}),
            json!({"jsonrpc": "2.0", "method": "terminal/create", "id": 0, "params": {"command": "synthetic-command"}}),
            json!({"jsonrpc": "2.0", "id": 0, "result": {"initialized": true}}),
        ] {
            sender.send(Ok(message)).expect("synthetic ACP message");
        }
        let mut replies = Vec::new();
        let result = wait_for_response(
            &mut replies,
            &messages,
            0,
            "initialize",
            Instant::now() + Duration::from_secs(1),
        )
        .expect("initialization after unsupported reverse requests");
        assert_eq!(result, json!({"initialized": true}));
        let replies = String::from_utf8(replies)
            .expect("ACP reply UTF-8")
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).expect("ACP reply JSON"))
            .collect::<Vec<_>>();
        assert_eq!(
            replies,
            ["provider-request".into(), json!(0)].map(|id| json!({
                "jsonrpc": "2.0", "id": id,
                "error": {"code": -32601, "message": "Method not found"}
            }))
        );
    }

    #[test]
    fn unrelated_notifications_cannot_extend_the_rpc_deadline() {
        let (sender, messages) = mpsc::channel();
        let started_at = Instant::now();
        let deadline = started_at + Duration::from_millis(25);
        let stop = started_at + Duration::from_millis(100);
        let peer = thread::spawn(move || {
            while Instant::now() < stop {
                if sender
                    .send(Ok(json!({"method": "session/update"})))
                    .is_err()
                {
                    break;
                }
                thread::sleep(Duration::from_millis(1));
            }
        });
        let error = wait_for_response(&mut Vec::new(), &messages, 0, "initialize", deadline)
            .expect_err("notifications must not keep initialization alive");
        peer.join().expect("synthetic notification peer");
        assert!(
            error.chain().any(|cause| matches!(
                cause.to_string().as_str(),
                "timed out waiting on channel" | "request deadline elapsed"
            )),
            "{error:#}"
        );
    }

    #[test]
    fn rpc_failure_diagnostics_omit_provider_strings_and_payloads() {
        let (sender, messages) = mpsc::channel();
        sender
            .send(Ok(json!({
                "method": format!("private-method-{}", "synthetic-secret".repeat(4096)),
                "id": "private-request-id", "params": {"text": "private-transcript"}
            })))
            .expect("opaque reverse request");
        drop(sender);
        let error = wait_for_response(
            &mut Vec::new(),
            &messages,
            0,
            "initialize",
            Instant::now() + Duration::from_secs(1),
        )
        .expect_err("disconnected synthetic provider");
        let message = format!("{error:#}");
        assert!(message.contains("requests=1"), "{message}");
        assert!(message.contains("string_ids=1"), "{message}");
        assert!(message.contains("unknown:1"), "{message}");
        assert!(!message.contains("private-") && !message.contains("synthetic-secret"));
        assert!(message.len() < 1024);
    }

    fn coalesce_messages(
        messages: impl IntoIterator<Item = HandoffMessage>,
    ) -> Vec<HandoffMessage> {
        let mut result: Vec<HandoffMessage> = Vec::new();
        for message in messages {
            match result.last_mut() {
                Some(previous) if previous.role == message.role => {
                    previous.text.push_str("\n\n");
                    previous.text.push_str(&message.text);
                }
                _ => result.push(message),
            }
        }
        result
    }

    #[test]
    fn failed_rollback_is_tagged_so_fallbacks_refuse_to_launch() {
        let rolled_back = combine_rollback_error(anyhow!("read-back failed"), Ok(()));
        assert!(!crate::rollback_failed(&rolled_back));

        let stranded = combine_rollback_error(
            anyhow!("read-back failed"),
            Err(anyhow!("delete failed: secret=synthetic-value")),
        );
        assert!(crate::rollback_failed(&stranded));
        let message = format!("{stranded:#}");
        assert!(message.contains("Grok import failed and rollback also failed: delete failed"));
        assert!(!message.contains("synthetic-value"), "{message}");
    }

    #[test]
    fn version_gate_accepts_newer_grok_releases() {
        assert!(!is_supported_version("0.2.113"));
        assert!(is_supported_version("0.2.114"));
        assert!(is_supported_version("0.2.117"));
        assert!(is_supported_version("0.3.0"));
        assert!(is_supported_version("1.0.0"));
    }

    #[test]
    fn import_model_comes_from_provider_initialization() {
        assert_eq!(
            initialized_model(
                &json!({"_meta": {"modelState": {"currentModelId": "synthetic-provider-default"}}})
            )
            .expect("provider default"),
            "synthetic-provider-default"
        );
        for initialized in [
            json!({}),
            json!({"_meta": {"modelState": {"currentModelId": ""}}}),
        ] {
            assert!(initialized_model(&initialized).is_err());
        }
    }

    #[test]
    fn adjacent_chunks_are_coalesced_like_grok_reader() {
        let messages = coalesce_messages([
            HandoffMessage {
                role: HandoffRole::User,
                text: "one".to_owned(),
            },
            HandoffMessage {
                role: HandoffRole::User,
                text: "two".to_owned(),
            },
            HandoffMessage {
                role: HandoffRole::Assistant,
                text: "three".to_owned(),
            },
        ]);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].text, "one\n\ntwo");
        assert_eq!(messages[1].text, "three");
    }

    #[test]
    fn grok_managed_summary_fields_do_not_fail_readback() {
        let expected = json!({
            "info": { "id": "synthetic", "cwd": "/workspace" },
            "num_messages": 2,
            "session_summary": "Synthetic session"
        });
        let returned = json!({
            "chat_format_version": 1,
            "git_remotes": [],
            "grok_home": "/isolated/grok",
            "info": { "id": "synthetic", "cwd": "/workspace" },
            "num_messages": 2,
            "sandbox_profile": "off",
            "session_summary": "Synthetic session"
        });

        assert_eq!(first_summary_mismatch(&returned, &expected, "$"), None);
        assert_eq!(
            first_summary_mismatch(
                &json!({
                    "info": { "id": "other", "cwd": "/workspace" },
                    "num_messages": 2,
                    "session_summary": "Synthetic session"
                }),
                &expected,
                "$"
            ),
            Some("$.info.id".to_owned())
        );
    }
}
