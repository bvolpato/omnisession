use std::{
    io::{BufRead, BufReader, Write},
    path::Path,
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc::{self, Receiver},
    thread,
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{SecondsFormat, Utc};
use omnis_core::{
    HandoffMessage, HandoffRole, TrajectoryItemKind, import_trajectory, redact_secrets,
};
use omnis_ir::{CanonicalSnapshot, Provider, SessionRef};
use serde_json::{Value, json};
use uuid::Uuid;
use wait_timeout::ChildExt;

const MINIMUM_GROK_VERSION: &str = "0.2.114";
const RPC_TIMEOUT: Duration = Duration::from_secs(30);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);

pub struct GrokImport {
    pub target: SessionRef,
    pub expected_messages: Vec<HandoffMessage>,
    pub history_items: usize,
    pub tool_events: usize,
    pub truncated: bool,
    summary: Value,
    updates: Vec<Value>,
}

pub fn build(snapshot: &CanonicalSnapshot, cwd: &Path) -> Result<GrokImport> {
    let trajectory = import_trajectory(snapshot);
    if trajectory.items.is_empty() {
        bail!("source has no visible trajectory eligible for Grok import");
    }

    let history_items = trajectory.items.len();
    let source = snapshot.session.to_string();
    let messages = trajectory.items.into_iter().map(|item| HandoffMessage {
        role: match item.kind {
            TrajectoryItemKind::User => HandoffRole::User,
            TrajectoryItemKind::Assistant | TrajectoryItemKind::Tool => HandoffRole::Assistant,
        },
        text: item.text,
    });
    let expected_messages = messages.collect::<Vec<_>>();

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
    let summary = json!({
        "info": { "id": id, "cwd": cwd_text },
        "session_summary": title,
        "created_at": timestamp,
        "updated_at": timestamp,
        "num_messages": expected_messages.len(),
        "num_chat_messages": expected_messages.len(),
        "current_model_id": "grok-4.5"
    });
    let updates = expected_messages
        .iter()
        .map(|message| {
            let session_update = match message.role {
                HandoffRole::User => "user_message_chunk",
                HandoffRole::Assistant => "agent_message_chunk",
            };
            json!({
                "timestamp": unix_seconds,
                "method": "session/update",
                "params": {
                    "sessionId": target.id,
                    "update": {
                        "sessionUpdate": session_update,
                        "messageId": Uuid::new_v4().to_string(),
                        "content": { "type": "text", "text": message.text }
                    }
                }
            })
        })
        .collect();

    Ok(GrokImport {
        target,
        expected_messages,
        history_items,
        tool_events: trajectory.tool_events,
        truncated: trajectory.truncated,
        summary,
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
    crate::version_gate::is_at_least(version, MINIMUM_GROK_VERSION)
}

pub fn materialize(import: &GrokImport, binary: &Path, cwd: &Path) -> Result<()> {
    ensure_supported(binary)?;
    let mut server = GrokServer::start(binary, cwd)?;
    server.initialize()?;
    let params = json!({
        "sessionId": import.target.id,
        "cwd": cwd,
        "state": { "summary": import.summary },
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

fn combine_rollback_error(error: anyhow::Error, rollback: Result<()>) -> anyhow::Error {
    match rollback {
        Ok(()) => error,
        Err(rollback_error) => error.context(format!(
            "Grok import failed and rollback also failed: {}",
            redact_secrets(&rollback_error.to_string())
        )),
    }
}

fn stored_import_matches(import: &GrokImport, binary: &Path, cwd: &Path) -> bool {
    let result = (|| {
        let mut server = GrokServer::start(binary, cwd)?;
        server.initialize()?;
        verify_import(&mut server, import, cwd)
    })();
    result.is_ok()
}

pub fn readback_matches(snapshot: &CanonicalSnapshot, expected: &[HandoffMessage]) -> bool {
    let trajectory = import_trajectory(snapshot);
    let actual = trajectory
        .items
        .into_iter()
        .map(|item| HandoffMessage {
            role: match item.kind {
                TrajectoryItemKind::User => HandoffRole::User,
                TrajectoryItemKind::Assistant | TrajectoryItemKind::Tool => HandoffRole::Assistant,
            },
            text: item.text,
        })
        .collect::<Vec<_>>();
    !trajectory.truncated && actual == expected
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
    if let Some(path) = first_summary_mismatch(returned_summary, &import.summary, "$") {
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

#[cfg(test)]
fn coalesce_messages(messages: impl IntoIterator<Item = HandoffMessage>) -> Vec<HandoffMessage> {
    let mut result: Vec<HandoffMessage> = Vec::new();
    for message in messages {
        if let Some(previous) = result.last_mut()
            && previous.role == message.role
        {
            previous.text.push_str("\n\n");
            previous.text.push_str(&message.text);
        } else {
            result.push(message);
        }
    }
    result
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
        .context("waiting for Grok version")?;
    let Some(status) = status else {
        child.kill().context("stopping Grok version probe")?;
        let _ = child.wait();
        bail!("Grok version probe timed out");
    };
    let output = child.wait_with_output().context("reading Grok version")?;
    if !status.success() {
        bail!("Grok version probe exited with status {status}");
    }
    let stdout = String::from_utf8(output.stdout).context("Grok version was not UTF-8")?;
    parse_version(&stdout).context("Grok returned an unrecognized version")
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

struct GrokServer {
    child: Child,
    stdin: Option<ChildStdin>,
    messages: Receiver<Result<Value, String>>,
}

impl GrokServer {
    fn start(binary: &Path, cwd: &Path) -> Result<Self> {
        let mut child = Command::new(binary)
            .args(["agent", "--no-leader", "stdio"])
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("starting `{}` ACP agent", binary.display()))?;
        let stdin = child.stdin.take().context("opening Grok ACP stdin")?;
        let stdout = child.stdout.take().context("opening Grok ACP stdout")?;
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
        )?;
        Ok(())
    }

    fn request(&mut self, id: u64, method: &str, params: &Value) -> Result<Value> {
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        }))?;
        loop {
            let message = self
                .messages
                .recv_timeout(RPC_TIMEOUT)
                .map_err(|error| anyhow!("Grok ACP timed out or disconnected: {error}"))?
                .map_err(|error| anyhow!("invalid Grok ACP response: {error}"))?;
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
    }

    fn send(&mut self, value: &Value) -> Result<()> {
        let stdin = self.stdin.as_mut().context("Grok ACP input is closed")?;
        serde_json::to_writer(&mut *stdin, value).context("writing Grok ACP request")?;
        stdin
            .write_all(b"\n")
            .context("terminating Grok ACP request")?;
        stdin.flush().context("flushing Grok ACP request")
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
mod tests {
    use super::*;

    #[test]
    fn version_parser_reads_installed_shape() {
        assert_eq!(
            parse_version("grok 0.2.117 (f1c0609308) [stable]"),
            Some("0.2.117".to_owned())
        );
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
