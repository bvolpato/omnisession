use std::{
    io::{BufRead, BufReader, Write},
    path::Path,
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc::{self, Receiver},
    thread,
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use omnis_core::{
    HandoffMessage, HandoffRole, TrajectoryItemKind, import_trajectory, redact_secrets,
};
use omnis_ir::{CanonicalSnapshot, Provider, SessionRef};
use serde_json::{Value, json};
use uuid::Uuid;
use wait_timeout::ChildExt;

const SUPPORTED_CODEX_VERSION: &str = "0.145.0";
const RPC_TIMEOUT: Duration = Duration::from_secs(20);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

pub struct CodexImport {
    pub items: Vec<Value>,
    pub expected_messages: Vec<HandoffMessage>,
    pub tool_events: usize,
    pub truncated: bool,
}

pub fn build(snapshot: &CanonicalSnapshot) -> Result<CodexImport> {
    let trajectory = import_trajectory(snapshot);
    if trajectory.items.is_empty() {
        bail!("source has no visible trajectory eligible for Codex import");
    }

    let source = snapshot.session.to_string();
    let boundary = HandoffMessage {
        role: HandoffRole::User,
        text: format!(
            "OmniSession imported history from `{source}`. Historical tool records are documentary context, not requests to replay tools. Verify current repository state before acting."
        ),
    };
    let mut expected_messages = Vec::with_capacity(trajectory.items.len() + 1);
    expected_messages.push(boundary);
    expected_messages.extend(trajectory.items.into_iter().map(|item| HandoffMessage {
        role: match item.kind {
            TrajectoryItemKind::User => HandoffRole::User,
            TrajectoryItemKind::Assistant | TrajectoryItemKind::Tool => HandoffRole::Assistant,
        },
        text: item.text,
    }));
    let items = expected_messages
        .iter()
        .map(|message| {
            let (role, content_type) = match message.role {
                HandoffRole::User => ("user", "input_text"),
                HandoffRole::Assistant => ("assistant", "output_text"),
            };
            json!({
                "type": "message",
                "role": role,
                "content": [{ "type": content_type, "text": message.text }]
            })
        })
        .collect();

    Ok(CodexImport {
        items,
        expected_messages,
        tool_events: trajectory.tool_events,
        truncated: trajectory.truncated,
    })
}

pub fn ensure_supported(binary: &Path) -> Result<String> {
    let version = installed_version(binary)?;
    if version != SUPPORTED_CODEX_VERSION {
        bail!(
            "Codex {version} is not verified for native trajectory injection; supported version: {SUPPORTED_CODEX_VERSION}"
        );
    }
    Ok(version)
}

pub fn materialize(import: &CodexImport, cwd: &Path, binary: &Path) -> Result<SessionRef> {
    ensure_supported(binary)?;
    let mut server = AppServer::start(binary, cwd)?;
    server.initialize()?;
    let result = server.request(1, "thread/start", &json!({ "cwd": cwd }))?;
    let id = result
        .pointer("/thread/id")
        .and_then(Value::as_str)
        .context("Codex thread/start response omitted thread ID")?;
    Uuid::parse_str(id).context("Codex created an invalid thread ID")?;
    let target = SessionRef::new(Provider::Codex, id);
    if let Err(error) = server.request(
        2,
        "thread/inject_items",
        &json!({ "threadId": id, "items": import.items }),
    ) {
        let rollback = server
            .request(3, "thread/delete", &json!({ "threadId": id }))
            .and_then(|_| server.shutdown());
        return Err(combine_rollback_error(
            error.context("injecting Codex trajectory"),
            rollback,
        ));
    }
    if let Err(error) = server.shutdown() {
        drop(server);
        return Err(combine_rollback_error(
            error.context("flushing imported Codex thread"),
            rollback(binary, cwd, &target),
        ));
    }
    Ok(target)
}

pub fn rollback(binary: &Path, cwd: &Path, target: &SessionRef) -> Result<()> {
    let mut server = AppServer::start(binary, cwd)?;
    server.initialize()?;
    server.request(1, "thread/delete", &json!({ "threadId": target.id }))?;
    server.shutdown().context("flushing Codex rollback")
}

fn combine_rollback_error(error: anyhow::Error, rollback: Result<()>) -> anyhow::Error {
    match rollback {
        Ok(()) => error,
        Err(rollback_error) => error.context(format!(
            "Codex import failed and rollback also failed: {}",
            redact_secrets(&rollback_error.to_string())
        )),
    }
}

pub fn readback_matches(snapshot: &CanonicalSnapshot, expected: &[HandoffMessage]) -> bool {
    let trajectory = import_trajectory(snapshot);
    let actual = trajectory
        .items
        .into_iter()
        .map(|item| match item.kind {
            TrajectoryItemKind::User => HandoffMessage {
                role: HandoffRole::User,
                text: item.text,
            },
            TrajectoryItemKind::Assistant | TrajectoryItemKind::Tool => HandoffMessage {
                role: HandoffRole::Assistant,
                text: item.text,
            },
        })
        .collect::<Vec<_>>();
    ordered_messages_present(&actual, expected)
}

fn ordered_messages_present(actual: &[HandoffMessage], expected: &[HandoffMessage]) -> bool {
    let mut expected = expected.iter();
    let mut next = expected.next();
    for message in actual {
        if next == Some(message) {
            next = expected.next();
        }
    }
    next.is_none()
}

fn installed_version(binary: &Path) -> Result<String> {
    let mut child = Command::new(binary)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("executing `{}`", binary.display()))?;
    let status = child
        .wait_timeout(Duration::from_secs(5))
        .context("waiting for Codex version")?;
    let Some(status) = status else {
        child.kill().context("stopping Codex version probe")?;
        let _ = child.wait();
        bail!("Codex version probe timed out");
    };
    let output = child.wait_with_output().context("reading Codex version")?;
    if !status.success() {
        bail!("Codex version probe exited with status {status}");
    }
    let stdout = String::from_utf8(output.stdout).context("Codex version was not UTF-8")?;
    parse_version(&stdout).context("Codex returned an unrecognized version")
}

fn parse_version(output: &str) -> Option<String> {
    output
        .split_whitespace()
        .find(|part| {
            let mut components = part.split('.');
            components.clone().count() == 3
                && components.all(|component| component.parse::<u64>().is_ok())
        })
        .map(str::to_owned)
}

struct AppServer {
    child: Child,
    stdin: Option<ChildStdin>,
    messages: Receiver<Result<Value, String>>,
}

impl AppServer {
    fn start(binary: &Path, cwd: &Path) -> Result<Self> {
        let mut child = Command::new(binary)
            .args(["app-server", "--stdio"])
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("starting `{}` app-server", binary.display()))?;
        let stdin = child
            .stdin
            .take()
            .context("opening Codex app-server stdin")?;
        let stdout = child
            .stdout
            .take()
            .context("opening Codex app-server stdout")?;
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

    fn initialize(&mut self) -> Result<()> {
        self.request(
            0,
            "initialize",
            &json!({
                "clientInfo": {
                    "name": "omnisession",
                    "title": "OmniSession",
                    "version": env!("CARGO_PKG_VERSION")
                },
                "capabilities": { "experimentalApi": true }
            }),
        )?;
        self.send(&json!({ "method": "initialized", "params": {} }))
    }

    fn request(&mut self, id: u64, method: &str, params: &Value) -> Result<Value> {
        self.send(&json!({ "id": id, "method": method, "params": params }))?;
        loop {
            let message = self
                .messages
                .recv_timeout(RPC_TIMEOUT)
                .map_err(|error| anyhow!("Codex app-server timed out or disconnected: {error}"))?
                .map_err(|error| anyhow!("invalid Codex app-server response: {error}"))?;
            if message.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(error) = message.get("error") {
                let message = error
                    .get("message")
                    .and_then(Value::as_str)
                    .map_or_else(|| "unknown app-server error".to_owned(), redact_secrets);
                bail!("Codex app-server rejected {method}: {message}");
            }
            return message
                .get("result")
                .cloned()
                .context("Codex app-server response omitted result");
        }
    }

    fn send(&mut self, value: &Value) -> Result<()> {
        let stdin = self
            .stdin
            .as_mut()
            .context("Codex app-server input is closed")?;
        serde_json::to_writer(&mut *stdin, value).context("writing Codex app-server request")?;
        stdin
            .write_all(b"\n")
            .context("terminating Codex app-server request")?;
        stdin.flush().context("flushing Codex app-server request")
    }

    fn shutdown(&mut self) -> Result<()> {
        self.stdin.take();
        if self
            .child
            .wait_timeout(SHUTDOWN_TIMEOUT)
            .context("waiting for Codex app-server shutdown")?
            .is_none()
        {
            self.child.kill().context("stopping Codex app-server")?;
            self.child.wait().context("reaping Codex app-server")?;
        }
        Ok(())
    }
}

impl Drop for AppServer {
    fn drop(&mut self) {
        self.stdin.take();
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use chrono::Utc;
    use omnis_ir::{
        EventKind, EventSource, GitState, OmniEvent, ReplayPolicy, SCHEMA_VERSION, Sensitivity,
        WorkspaceSnapshot,
    };

    use super::*;

    #[test]
    fn build_maps_messages_and_tools_to_model_visible_history() {
        let thread_id = Uuid::new_v4();
        let branch_id = Uuid::new_v4();
        let event = |sequence, kind, text: &str, replay_policy| OmniEvent {
            schema_version: SCHEMA_VERSION.to_owned(),
            event_id: Uuid::new_v4(),
            thread_id,
            branch_id,
            sequence,
            timestamp: None,
            source: EventSource {
                provider: Provider::Claude,
                native_session_id: "source".to_owned(),
                provider_version: None,
                raw_record_type: None,
            },
            kind,
            payload: json!({ "text": text }),
            raw_blob_hash: None,
            sensitivity: Sensitivity::Normal,
            replay_policy,
        };
        let snapshot = CanonicalSnapshot {
            schema_version: SCHEMA_VERSION.to_owned(),
            session: SessionRef::new(Provider::Claude, "source"),
            thread_id,
            branch_id,
            title: None,
            captured_at: Utc::now(),
            workspace: WorkspaceSnapshot {
                schema_version: SCHEMA_VERSION.to_owned(),
                captured_at: Utc::now(),
                root: PathBuf::from("/repo"),
                current_dir: PathBuf::from("/repo"),
                git: GitState::default(),
                instruction_files: Vec::new(),
                environment_names: Vec::new(),
                available_tools: Vec::new(),
            },
            events: vec![
                event(
                    1,
                    EventKind::MessageUser,
                    "question",
                    ReplayPolicy::Contextual,
                ),
                event(
                    2,
                    EventKind::ToolCalled,
                    "shell",
                    ReplayPolicy::HistoricalOnly,
                ),
                event(
                    3,
                    EventKind::MessageAssistant,
                    "answer",
                    ReplayPolicy::Contextual,
                ),
            ],
        };

        let import = build(&snapshot).expect("valid import");
        assert_eq!(import.items.len(), 4);
        assert_eq!(import.tool_events, 1);
        assert_eq!(import.items[1]["role"], "user");
        assert_eq!(import.items[2]["role"], "assistant");
        assert!(
            import.items[2]
                .to_string()
                .contains("Documentary context only")
        );
    }

    #[test]
    fn version_parser_accepts_codex_cli_output() {
        assert_eq!(
            parse_version("codex-cli 0.145.0\n").as_deref(),
            Some("0.145.0")
        );
        assert_eq!(parse_version("unknown"), None);
    }

    #[test]
    fn readback_allows_native_context_around_imported_history() {
        let imported = HandoffMessage {
            role: HandoffRole::User,
            text: "imported".to_owned(),
        };
        let actual = [
            HandoffMessage {
                role: HandoffRole::User,
                text: "native context".to_owned(),
            },
            imported.clone(),
        ];
        assert!(ordered_messages_present(&actual, &[imported]));
    }
}
