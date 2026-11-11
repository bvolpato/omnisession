use std::{
    env, fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Stdio},
    sync::mpsc::{self, Receiver},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use omnis_adapters::CodexAdapter;
use omnis_core::{
    HandoffMessage, HandoffRole, TrajectoryItemKind, is_harness_envelope, redact_secrets,
    reports_omitted_events,
};
use omnis_ir::{CanonicalSnapshot, EventKind, Provider, ReplayPolicy, Sensitivity, SessionRef};
use serde_json::{Value, json};
use uuid::Uuid;
use wait_timeout::ChildExt;

use crate::recovery::ImportSource;
use crate::{
    interrupt::{HelperProcess, wait_or_kill},
    launch_profile::{AgentTarget, LaunchProfile},
    provider_compatibility::MINIMUM_CODEX_VERSION,
};
mod profile;
use profile::{ImportProfile, plain_codex_supplies};

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
    profile: ImportProfile,
}

pub struct ReadbackReport {
    pub verified: bool,
    pub history_incomplete: bool,
    pub matched_messages: usize,
    pub expected_messages: usize,
    pub observed_messages: usize,
}

pub fn build(source: &dyn ImportSource) -> Result<CodexImport> {
    let (snapshot, trajectory) = source.history()?;
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
        profile: ImportProfile::default(),
    })
}

impl CodexImport {
    pub(crate) fn for_launch_profile(
        mut self,
        profile: Option<&crate::launch_profile::LaunchProfile>,
    ) -> Result<Self> {
        if let Some(profile) = profile.filter(|p| !p.overrides_program()) {
            let args = profile.expanded_args(&crate::launch_profile::environment)?;
            self.profile = ImportProfile::resolve(&args, &codex_home()?)?;
        }
        Ok(self)
    }
}

/// Finds the launch profiles that can open `source`, when plain `codex` cannot.
///
/// Codex resumes a session only with the model provider that created it. The result is empty for
/// other agents, for sessions that plain `codex` can open, and when nothing can be determined.
/// A profile with its own program never matches, because that program decides what its arguments
/// mean.
pub(crate) fn profiles_for_recorded_provider(
    source: &SessionRef,
    targets: &[AgentTarget],
) -> Vec<AgentTarget> {
    if source.provider != Provider::Codex {
        return Vec::new();
    }
    let Ok(home) = codex_home() else {
        return Vec::new();
    };
    let Ok(Some(provider)) = CodexAdapter::default().recorded_model_provider(&source.id) else {
        return Vec::new();
    };
    if plain_codex_supplies(&home, &provider) {
        return Vec::new();
    }
    let supplies = |profile: &LaunchProfile| {
        !profile.overrides_program()
            && profile
                .expanded_args(&crate::launch_profile::environment)
                .and_then(|args| ImportProfile::resolve(&args, &home))
                .is_ok_and(|import| import.provider() == Some(provider.as_str()))
    };
    targets
        .iter()
        .copied()
        .filter(|target| {
            target.provider == Provider::Codex
                && target.profile.is_some_and(supplies)
                && target.check().is_ok()
        })
        .collect()
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
    crate::version_gate::is_release_at_least(version, MINIMUM_CODEX_VERSION)
}

pub fn materialize(import: &CodexImport, cwd: &Path, binary: &Path) -> Result<SessionRef> {
    ensure_supported(binary)?;
    let source_home = tempfile::tempdir().context("creating isolated Codex import source")?;
    let source_path = write_external_session(source_home.path(), cwd, import)?;
    let codex_home = codex_home()?;
    let mut server = AppServer::start(
        binary,
        cwd,
        &import.profile,
        Some((source_home.path(), &codex_home)),
    )?;
    server.initialize().map_err(|error| {
        if codex_home.is_dir() {
            error
        } else {
            error.context(format!(
                "Codex could not start with CODEX_HOME at {}. Use an existing, accessible directory",
                codex_home.display()
            ))
        }
    })?;
    let id = submit_import(&mut server, &source_path, cwd, import).map_err(|error| {
        if error.is::<RpcRejection>() {
            error
        } else {
            crate::transfer::rollback_failure(
                error,
                "Codex may have created a thread. Check the Codex session list before retrying"
                    .to_owned(),
            )
        }
    })?;
    let target = SessionRef::new(Provider::Codex, &id);
    if let Err(error) = verify_visible_turns(&mut server, &id, import) {
        let rollback = server
            .request("thread/delete", &json!({ "threadId": id }))
            .and_then(|_| server.shutdown());
        return Err(combine_rollback_error(
            error.context("verifying visible Codex turns and model provider"),
            rollback,
        ));
    }
    if let Err(error) = server.shutdown() {
        drop(server);
        return Err(combine_rollback_error(
            error.context("flushing imported Codex thread"),
            rollback(binary, cwd, &target, import),
        ));
    }
    Ok(target)
}

fn submit_import(
    server: &mut AppServer,
    source_path: &Path,
    cwd: &Path,
    import: &CodexImport,
) -> Result<String> {
    let deadline = Instant::now() + rpc_timeout(IMPORT_TIMEOUT);
    let result = server.request(
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
        deadline,
    )?;
    let id = imported_thread_id(&completed)?;
    Uuid::parse_str(&id).context("Codex imported an invalid thread ID")?;
    Ok(id)
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

#[derive(Debug)]
struct RpcRejection(String);

impl std::fmt::Display for RpcRejection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for RpcRejection {}

fn imported_thread_id(completed: &Value) -> Result<String> {
    let results = completed
        .get("itemTypeResults")
        .and_then(Value::as_array)
        .context("Codex completion omitted import results")?;
    let mut session_results = results
        .iter()
        .filter(|result| result.get("itemType").and_then(Value::as_str) == Some("SESSIONS"));
    let sessions = session_results
        .next()
        .context("Codex completion omitted session import result")?;
    if session_results.next().is_some() {
        bail!("Codex completion reported multiple session import results");
    }
    if let Some(failure) = sessions
        .get("failures")
        .and_then(Value::as_array)
        .and_then(|failures| failures.first())
    {
        let message = failure
            .get("message")
            .and_then(Value::as_str)
            .map_or_else(|| "unknown session import error".to_owned(), redact_secrets);
        // Codex can report failure after it creates a thread, without returning the thread ID.
        bail!("Codex session import failed: {message}");
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

fn verify_visible_turns(server: &mut AppServer, id: &str, import: &CodexImport) -> Result<()> {
    let result = server.request(
        "thread/read",
        &json!({ "threadId": id, "includeTurns": true }),
    )?;
    import.profile.verify(&result["thread"])?;
    let expected = import.expected_messages.as_slice();
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

pub fn rollback(
    binary: &Path,
    cwd: &Path,
    target: &SessionRef,
    import: &CodexImport,
) -> Result<()> {
    let mut server = AppServer::start(binary, cwd, &import.profile, None)?;
    server.initialize()?;
    server.request("thread/delete", &json!({ "threadId": target.id }))?;
    server.shutdown().context("flushing Codex rollback")
}

// Tags a failed rollback so fallbacks refuse to launch while the generated thread may remain.
fn combine_rollback_error(error: anyhow::Error, rollback: Result<()>) -> anyhow::Error {
    crate::error_after_rollback(
        error,
        rollback.map_err(|rollback_error| anyhow!(redact_secrets(&rollback_error.to_string()))),
        "Codex",
    )
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
        prefix_len <= MAX_PROVIDER_CONTEXT_MESSAGES
            && actual[prefix_len..] == *expected
            && actual[..prefix_len].iter().all(|message| {
                message.role == HandoffRole::User && is_harness_envelope(&message.text)
            })
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
    let history_incomplete = snapshot.events.iter().any(reports_omitted_events);
    ReadbackReport {
        verified: exact_import_suffix && !has_unexpected_actions && !history_incomplete,
        history_incomplete,
        matched_messages,
        expected_messages: expected.len(),
        observed_messages: actual.len(),
    }
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
        .context("waiting for Codex version")?
    else {
        bail!("Codex version probe timed out");
    };
    let output = child.wait_with_output().context("reading Codex version")?;
    if !status.success() {
        bail!("Codex version probe exited with status {status}");
    }
    let stdout = String::from_utf8(output.stdout).context("Codex version was not UTF-8")?;
    crate::version_gate::find_version(&stdout).context("Codex returned an unrecognized version")
}

// Integration tests shorten this to bound a hung synthetic app-server. It never extends the default.
fn rpc_timeout(default: Duration) -> Duration {
    env::var("OMNI_TEST_CODEX_RPC_TIMEOUT_MS")
        .ok()
        .and_then(|millis| millis.parse().ok())
        .map_or(default, |millis| Duration::from_millis(millis).min(default))
}

struct AppServer {
    child: Child,
    next_request_id: u64,
    stdin: Option<ChildStdin>,
    messages: Receiver<Result<Value, String>>,
}

impl AppServer {
    fn start(
        binary: &Path,
        cwd: &Path,
        profile: &ImportProfile,
        import_environment: Option<(&Path, &Path)>,
    ) -> Result<Self> {
        let mut command = crate::provider_launch::provider_process(binary)?;
        command.args(["app-server", "--stdio"]).current_dir(cwd);
        profile.apply(&mut command);
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
            .outside_terminal_group()
            .spawn()
            .with_context(|| format!("starting `{}` app-server", binary.display()))?;
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            let _ = child.kill();
            let _ = child.wait();
            bail!("opening Codex app-server pipes");
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
            next_request_id: 0,
            stdin: Some(stdin),
            messages,
        })
    }

    fn initialize(&mut self) -> Result<()> {
        self.request(
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

    fn request(&mut self, method: &str, params: &Value) -> Result<Value> {
        let deadline = Instant::now() + rpc_timeout(RPC_TIMEOUT);
        let id = self.next_request_id;
        self.next_request_id += 1;
        self.send(&json!({ "id": id, "method": method, "params": params }))?;
        loop {
            let message = self.receive_before(deadline)?;
            if message.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(error) = message.get("error") {
                let detail = error
                    .get("message")
                    .and_then(Value::as_str)
                    .map(redact_secrets)
                    .context("malformed Codex app-server error response")?;
                let error_message = format!("Codex app-server rejected {method}: {detail}");
                // JSON-RPC rejects invalid requests, unknown methods, and invalid parameters
                // before executing the method. An internal error can follow a partial write.
                if matches!(
                    error.get("code").and_then(Value::as_i64),
                    Some(-32602..=-32600)
                ) {
                    return Err(RpcRejection(error_message).into());
                }
                bail!("{error_message}");
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
        deadline: Instant,
    ) -> Result<Value> {
        loop {
            let message = self.receive_before(deadline)?;
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

    fn receive_before(&self, deadline: Instant) -> Result<Value> {
        let message = match deadline.checked_duration_since(Instant::now()) {
            Some(remaining) => self.messages.recv_timeout(remaining),
            None => Err(mpsc::RecvTimeoutError::Timeout),
        };
        message
            .map_err(|error| anyhow!("Codex app-server timed out or disconnected: {error}"))?
            .map_err(|error| anyhow!("invalid Codex app-server response: {error}"))
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

        let mut duplicate = with_provider_context.clone();
        duplicate.events[0] = message(0, EventKind::MessageUser, "boundary");
        assert!(!readback_report(&duplicate, &expected).verified);
        duplicate.events[0] = message(0, EventKind::MessageAssistant, "unexpected answer");
        assert!(!readback_report(&duplicate, &expected).verified);

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
                    message(
                        sequence as u64,
                        EventKind::MessageUser,
                        "<environment_context>",
                    )
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

    #[test]
    fn matching_messages_cannot_verify_incomplete_history() {
        let thread_id = Uuid::new_v4();
        let branch_id = Uuid::new_v4();
        let expected = [HandoffMessage {
            role: HandoffRole::User,
            text: "retained".to_owned(),
        }];
        let retained = codex_message(thread_id, branch_id, 0, EventKind::MessageUser, "retained");
        for marker in [
            json!({ "native_history_unverified": true }),
            json!({ "omitted_events": 1 }),
            json!({ "unsupported_text_records": 1 }),
        ] {
            let mut notice = codex_message(thread_id, branch_id, 1, EventKind::ProviderEvent, "");
            notice.payload = marker;
            let incomplete = codex_snapshot(thread_id, branch_id, vec![retained.clone(), notice]);
            let report = readback_report(&incomplete, &expected);
            assert_eq!(report.matched_messages, expected.len());
            assert!(!report.verified);
            assert!(report.history_incomplete);
        }
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
