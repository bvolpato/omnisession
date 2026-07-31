use std::{
    env, fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc::{self, Receiver},
    thread,
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use omnis_core::{
    HandoffMessage, HandoffRole, TrajectoryItemKind, import_trajectory, redact_secrets,
};
use omnis_ir::{CanonicalSnapshot, EventKind, Provider, ReplayPolicy, Sensitivity, SessionRef};
use serde_json::{Value, json};
use uuid::Uuid;
use wait_timeout::ChildExt;

const MINIMUM_CODEX_VERSION: &str = "0.146.0";
const MAX_PROVIDER_CONTEXT_MESSAGES: usize = 16;
const RPC_TIMEOUT: Duration = Duration::from_secs(20);
const IMPORT_TIMEOUT: Duration = Duration::from_secs(60);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);
const EXTERNAL_SESSION_IMPORTED_MARKER: &str = "<EXTERNAL SESSION IMPORTED>";

pub struct CodexImport {
    pub expected_messages: Vec<HandoffMessage>,
    pub title: Option<String>,
    pub tool_events: usize,
    pub truncated: bool,
}

pub struct ReadbackReport {
    pub verified: bool,
    pub matched_messages: usize,
    pub expected_messages: usize,
    pub observed_messages: usize,
}

pub fn build(snapshot: &CanonicalSnapshot) -> Result<CodexImport> {
    let trajectory = import_trajectory(snapshot);
    if trajectory.items.is_empty() {
        bail!("source has no visible trajectory eligible for Codex import");
    }

    let mut expected_messages = trajectory
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
    if expected_messages
        .first()
        .is_some_and(|message| matches!(message.role, HandoffRole::Assistant))
    {
        expected_messages.insert(
            0,
            HandoffMessage {
                role: HandoffRole::User,
                text: format!(
                    "OmniSession imported history from `{}`. Historical tool records are documentary context, not requests to replay tools. Verify current repository state before acting.",
                    snapshot.session
                ),
            },
        );
    }
    Ok(CodexImport {
        expected_messages,
        title: snapshot.title.as_deref().map(redact_secrets),
        tool_events: trajectory.tool_events,
        truncated: trajectory.truncated,
    })
}

pub fn ensure_supported(binary: &Path) -> Result<String> {
    let version = installed_version(binary)?;
    if !is_supported_version(&version) {
        bail!(
            "Codex {version} is too old for native session import; supported versions: >= {MINIMUM_CODEX_VERSION}"
        );
    }
    Ok(version)
}

fn is_supported_version(version: &str) -> bool {
    crate::version_gate::is_at_least(version, MINIMUM_CODEX_VERSION)
}

pub fn materialize(import: &CodexImport, cwd: &Path, binary: &Path) -> Result<SessionRef> {
    ensure_supported(binary)?;
    let source_home = tempfile::tempdir().context("creating isolated Codex import source")?;
    let source_path = write_external_session(source_home.path(), cwd, import)?;
    let codex_home = codex_home()?;
    let mut server =
        AppServer::start_for_external_import(binary, cwd, source_home.path(), &codex_home)?;
    server.initialize()?;
    let result = server.request(
        1,
        "externalAgentConfig/import",
        &json!({
            "migrationItems": [{
                "itemType": "SESSIONS",
                "description": "Import OmniSession trajectory",
                "cwd": cwd,
                "details": {
                    "sessions": [{
                        "path": source_path,
                        "cwd": cwd,
                        "title": import.title,
                    }]
                }
            }],
            "source": "omnisession",
            "providerId": "omnisession",
        }),
    )?;
    let import_id = result
        .get("importId")
        .and_then(Value::as_str)
        .context("Codex import response omitted import ID")?;
    let completed = server.wait_for_notification(
        "externalAgentConfig/import/completed",
        Some(("importId", import_id)),
        IMPORT_TIMEOUT,
    )?;
    let id = imported_thread_id(&completed)?;
    Uuid::parse_str(&id).context("Codex imported an invalid thread ID")?;
    let target = SessionRef::new(Provider::Codex, &id);
    if let Err(error) = verify_visible_turns(&mut server, &id, &import.expected_messages) {
        let rollback = server
            .request(2, "thread/delete", &json!({ "threadId": id }))
            .and_then(|_| server.shutdown());
        return Err(combine_rollback_error(
            error.context("verifying visible Codex turns"),
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

fn write_external_session(root: &Path, cwd: &Path, import: &CodexImport) -> Result<PathBuf> {
    let directory = root.join(".claude/projects/omnisession");
    fs::create_dir_all(&directory).context("creating isolated Codex import directory")?;
    let path = directory.join(format!("{}.jsonl", Uuid::new_v4()));
    let mut file = fs::File::create(&path).context("creating isolated Codex import session")?;
    let timestamp = chrono::Utc::now().to_rfc3339();
    for message in &import.expected_messages {
        let role = match message.role {
            HandoffRole::User => "user",
            HandoffRole::Assistant => "assistant",
        };
        serde_json::to_writer(
            &mut file,
            &json!({
                "type": role,
                "cwd": cwd,
                "timestamp": timestamp,
                "message": {
                    "role": role,
                    "content": message.text,
                }
            }),
        )
        .context("writing isolated Codex import record")?;
        file.write_all(b"\n")
            .context("terminating isolated Codex import record")?;
    }
    file.sync_all()
        .context("flushing isolated Codex import session")?;
    Ok(path)
}

fn codex_home() -> Result<PathBuf> {
    if let Some(path) = env::var_os("CODEX_HOME") {
        let path = PathBuf::from(path);
        if !path.is_absolute() {
            bail!("CODEX_HOME must be absolute for native Codex import");
        }
        return Ok(path);
    }
    let home = directories::BaseDirs::new().context("resolving home directory for Codex import")?;
    Ok(home.home_dir().join(".codex"))
}

fn imported_thread_id(completed: &Value) -> Result<String> {
    let results = completed
        .get("itemTypeResults")
        .and_then(Value::as_array)
        .context("Codex completion omitted import results")?;
    let sessions = results
        .iter()
        .find(|result| result.get("itemType").and_then(Value::as_str) == Some("SESSIONS"))
        .context("Codex completion omitted session import result")?;
    if let Some(failure) = sessions
        .get("failures")
        .and_then(Value::as_array)
        .and_then(|failures| failures.first())
    {
        let message = failure
            .get("message")
            .and_then(Value::as_str)
            .map_or_else(|| "unknown session import error".to_owned(), redact_secrets);
        bail!("Codex rejected imported session: {message}");
    }
    let successes = sessions
        .get("successes")
        .and_then(Value::as_array)
        .context("Codex session import result omitted successes")?;
    if successes.len() != 1 {
        bail!(
            "Codex session import returned {} targets; expected exactly one",
            successes.len()
        );
    }
    successes[0]
        .get("target")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .context("Codex session import omitted target thread ID")
}

fn verify_visible_turns(
    server: &mut AppServer,
    id: &str,
    expected: &[HandoffMessage],
) -> Result<()> {
    let result = server.request(
        2,
        "thread/read",
        &json!({ "threadId": id, "includeTurns": true }),
    )?;
    let turns = result
        .pointer("/thread/turns")
        .and_then(Value::as_array)
        .context("Codex thread/read omitted visible turns")?;
    let mut actual = Vec::new();
    for item in turns
        .iter()
        .filter_map(|turn| turn.get("items").and_then(Value::as_array))
        .flatten()
    {
        match item.get("type").and_then(Value::as_str) {
            Some("userMessage") => {
                let Some(content) = item.get("content").and_then(Value::as_array) else {
                    continue;
                };
                for part in content {
                    if part.get("type").and_then(Value::as_str) == Some("text")
                        && let Some(text) = part.get("text").and_then(Value::as_str)
                    {
                        actual.push(HandoffMessage {
                            role: HandoffRole::User,
                            text: text.to_owned(),
                        });
                    }
                }
            }
            Some("agentMessage") => {
                if let Some(text) = item.get("text").and_then(Value::as_str)
                    && text != EXTERNAL_SESSION_IMPORTED_MARKER
                {
                    actual.push(HandoffMessage {
                        role: HandoffRole::Assistant,
                        text: text.to_owned(),
                    });
                }
            }
            _ => {}
        }
    }
    if actual != expected {
        bail!(
            "Codex visible-turn verification matched {} of {} expected messages across {} visible turns",
            actual
                .iter()
                .zip(expected)
                .take_while(|(actual, expected)| actual == expected)
                .count(),
            expected.len(),
            turns.len()
        );
    }
    Ok(())
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

pub fn readback_report(
    snapshot: &CanonicalSnapshot,
    expected: &[HandoffMessage],
) -> ReadbackReport {
    let mut events = snapshot.events.iter().collect::<Vec<_>>();
    events.sort_by_key(|event| (event.sequence, event.event_id));

    let actual = events
        .into_iter()
        .filter(|event| {
            event.sensitivity != Sensitivity::Secret
                && event.replay_policy == ReplayPolicy::Contextual
        })
        .filter_map(|event| {
            let role = match event.kind {
                EventKind::MessageUser => HandoffRole::User,
                EventKind::MessageAssistant => HandoffRole::Assistant,
                _ => return None,
            };
            let text = event.payload.get("text")?.as_str()?.to_owned();
            if text == EXTERNAL_SESSION_IMPORTED_MARKER {
                return None;
            }
            Some(HandoffMessage { role, text })
        })
        .collect::<Vec<_>>();
    let provider_context_messages = actual.len().checked_sub(expected.len());
    let exact_import_suffix = provider_context_messages.is_some_and(|prefix_len| {
        prefix_len <= MAX_PROVIDER_CONTEXT_MESSAGES && actual[prefix_len..] == *expected
    });
    let matched_messages = if exact_import_suffix {
        expected.len()
    } else {
        actual
            .iter()
            .zip(expected)
            .take_while(|(actual, expected)| actual == expected)
            .count()
    };
    let has_unexpected_actions = snapshot.events.iter().any(|event| {
        matches!(
            event.kind,
            EventKind::ToolCalled
                | EventKind::ToolCompleted
                | EventKind::ToolFailed
                | EventKind::CommandExecuted
                | EventKind::ApprovalRequested
                | EventKind::ApprovalDecided
        )
    });
    ReadbackReport {
        verified: exact_import_suffix && !has_unexpected_actions,
        matched_messages,
        expected_messages: expected.len(),
        observed_messages: actual.len(),
    }
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
        Self::start_with_environment(binary, cwd, None)
    }

    fn start_for_external_import(
        binary: &Path,
        cwd: &Path,
        source_home: &Path,
        codex_home: &Path,
    ) -> Result<Self> {
        Self::start_with_environment(binary, cwd, Some((source_home, codex_home)))
    }

    fn start_with_environment(
        binary: &Path,
        cwd: &Path,
        import_environment: Option<(&Path, &Path)>,
    ) -> Result<Self> {
        let mut command = Command::new(binary);
        command.args(["app-server", "--stdio"]).current_dir(cwd);
        if let Some((source_home, codex_home)) = import_environment {
            command
                .env("HOME", source_home)
                .env("USERPROFILE", source_home)
                .env("CODEX_HOME", codex_home);
        }
        let mut child = command
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

    fn wait_for_notification(
        &mut self,
        method: &str,
        match_field: Option<(&str, &str)>,
        timeout: Duration,
    ) -> Result<Value> {
        loop {
            let message = self
                .messages
                .recv_timeout(timeout)
                .map_err(|error| anyhow!("Codex app-server timed out or disconnected: {error}"))?
                .map_err(|error| anyhow!("invalid Codex app-server response: {error}"))?;
            if message.get("method").and_then(Value::as_str) != Some(method) {
                continue;
            }
            let params = message
                .get("params")
                .cloned()
                .context("Codex app-server notification omitted params")?;
            if match_field.is_none_or(|(field, expected)| {
                params.get(field).and_then(Value::as_str) == Some(expected)
            }) {
                return Ok(params);
            }
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
        assert_eq!(import.expected_messages.len(), 3);
        assert_eq!(import.tool_events, 1);
        assert_eq!(import.expected_messages[0].role, HandoffRole::User);
        assert_eq!(import.expected_messages[1].role, HandoffRole::Assistant);
        assert!(
            import.expected_messages[1]
                .text
                .contains("Documentary context only")
        );
    }

    #[test]
    fn build_adds_user_boundary_before_leading_assistant_context() {
        let thread_id = Uuid::new_v4();
        let branch_id = Uuid::new_v4();
        let snapshot = codex_snapshot(
            thread_id,
            branch_id,
            vec![
                codex_message(
                    thread_id,
                    branch_id,
                    1,
                    EventKind::MessageAssistant,
                    "retained summary",
                ),
                codex_message(
                    thread_id,
                    branch_id,
                    2,
                    EventKind::MessageUser,
                    "latest request",
                ),
            ],
        );

        let import = build(&snapshot).expect("valid import");

        assert_eq!(import.expected_messages.len(), 3);
        assert_eq!(import.expected_messages[0].role, HandoffRole::User);
        assert!(
            import.expected_messages[0]
                .text
                .starts_with("OmniSession imported history from `codex:target`")
        );
        assert_eq!(import.expected_messages[1].text, "retained summary");
        assert_eq!(import.expected_messages[2].text, "latest request");
    }

    #[test]
    fn version_parser_accepts_codex_cli_output() {
        assert_eq!(
            parse_version("codex-cli 0.146.0\n").as_deref(),
            Some("0.146.0")
        );
        assert_eq!(parse_version("unknown"), None);
    }

    #[test]
    fn version_gate_accepts_newer_codex_releases() {
        assert!(!is_supported_version("0.145.9"));
        assert!(is_supported_version("0.146.0"));
        assert!(is_supported_version("0.147.0"));
    }

    #[test]
    fn readback_accepts_bounded_provider_context_but_rejects_trajectory_changes() {
        let thread_id = Uuid::new_v4();
        let branch_id = Uuid::new_v4();
        let message =
            |sequence, role, text| codex_message(thread_id, branch_id, sequence, role, text);
        let expected = [
            HandoffMessage {
                role: HandoffRole::User,
                text: "boundary".to_owned(),
            },
            HandoffMessage {
                role: HandoffRole::Assistant,
                text: "tool record".to_owned(),
            },
            HandoffMessage {
                role: HandoffRole::Assistant,
                text: "tool record".to_owned(),
            },
        ];
        let snapshot = codex_snapshot(
            thread_id,
            branch_id,
            vec![
                message(1, EventKind::MessageUser, "boundary"),
                message(2, EventKind::MessageAssistant, "tool record"),
            ],
        );

        assert!(!readback_report(&snapshot, &expected).verified);

        let with_provider_context = CanonicalSnapshot {
            events: vec![
                message(
                    0,
                    EventKind::MessageUser,
                    "<environment_context>\nsynthetic\n</environment_context>",
                ),
                message(
                    1,
                    EventKind::MessageUser,
                    "# AGENTS.md instructions for /repo\n\nSynthetic instructions.",
                ),
                message(2, EventKind::MessageUser, "boundary"),
                message(3, EventKind::MessageAssistant, "tool record"),
                message(4, EventKind::MessageAssistant, "tool record"),
            ],
            ..snapshot.clone()
        };
        let report = readback_report(&with_provider_context, &expected);
        assert!(report.verified);
        assert_eq!(report.matched_messages, expected.len());

        let with_codex_import_marker = CanonicalSnapshot {
            events: with_provider_context
                .events
                .iter()
                .cloned()
                .chain([message(
                    6,
                    EventKind::MessageAssistant,
                    EXTERNAL_SESSION_IMPORTED_MARKER,
                )])
                .collect(),
            ..with_provider_context.clone()
        };
        assert!(readback_report(&with_codex_import_marker, &expected).verified);

        let with_trailing_message = CanonicalSnapshot {
            events: vec![
                message(0, EventKind::MessageUser, "provider context"),
                message(1, EventKind::MessageUser, "boundary"),
                message(2, EventKind::MessageAssistant, "tool record"),
                message(3, EventKind::MessageAssistant, "tool record"),
                message(4, EventKind::MessageAssistant, "unexpected"),
            ],
            ..snapshot.clone()
        };
        assert!(!readback_report(&with_trailing_message, &expected).verified);

        let too_much_provider_context = CanonicalSnapshot {
            events: (0..=MAX_PROVIDER_CONTEXT_MESSAGES)
                .map(|sequence| {
                    message(sequence as u64, EventKind::MessageUser, "provider context")
                })
                .chain([
                    message(20, EventKind::MessageUser, "boundary"),
                    message(21, EventKind::MessageAssistant, "tool record"),
                    message(22, EventKind::MessageAssistant, "tool record"),
                ])
                .collect(),
            ..snapshot
        };
        assert!(!readback_report(&too_much_provider_context, &expected).verified);
    }

    fn codex_message(
        thread_id: Uuid,
        branch_id: Uuid,
        sequence: u64,
        kind: EventKind,
        text: &str,
    ) -> OmniEvent {
        OmniEvent {
            schema_version: SCHEMA_VERSION.to_owned(),
            event_id: Uuid::new_v4(),
            thread_id,
            branch_id,
            sequence,
            timestamp: None,
            source: EventSource {
                provider: Provider::Codex,
                native_session_id: "target".to_owned(),
                provider_version: None,
                raw_record_type: None,
            },
            kind,
            payload: json!({ "text": text }),
            raw_blob_hash: None,
            sensitivity: Sensitivity::Normal,
            replay_policy: ReplayPolicy::Contextual,
        }
    }

    fn codex_snapshot(
        thread_id: Uuid,
        branch_id: Uuid,
        events: Vec<OmniEvent>,
    ) -> CanonicalSnapshot {
        CanonicalSnapshot {
            schema_version: SCHEMA_VERSION.to_owned(),
            session: SessionRef::new(Provider::Codex, "target"),
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
            events,
        }
    }
}
