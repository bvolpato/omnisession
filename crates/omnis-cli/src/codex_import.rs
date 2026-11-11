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
    CODEX_IMPORT_MARKER, HandoffMessage, HandoffRole, TrajectoryItemKind, is_harness_envelope,
    redact_secrets, reports_omitted_events,
};
use omnis_ir::{CanonicalSnapshot, EventKind, Provider, ReplayPolicy, Sensitivity, SessionRef};
use serde_json::{Value, json};
use uuid::Uuid;
use wait_timeout::ChildExt;

use crate::recovery::ImportSource;
use crate::{
    interrupt::HelperProcess,
    launch_profile::{AgentTarget, LaunchProfile},
    provider_compatibility::MINIMUM_CODEX_VERSION,
};
mod profile;
use profile::{ImportProfile, plain_codex_supplies};

const MAX_PROVIDER_CONTEXT_MESSAGES: usize = 16;
const RPC_TIMEOUT: Duration = Duration::from_secs(20);
const IMPORT_TIMEOUT: Duration = Duration::from_secs(60);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

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
    let version = crate::version_gate::installed_version(binary, Provider::Codex)?;
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
    let receipt = submit_import(&mut server, &source_path, cwd, import).map_err(|error| {
        if error.is::<ImportRejected>() {
            error
        } else {
            crate::transfer::rollback_failure(
                error,
                "Codex may have created a thread. Check the Codex session list before retrying"
                    .to_owned(),
            )
        }
    })?;
    let (id, failure) = match receipt {
        ImportReceipt::Rejected(reason) => bail!("{reason}"),
        ImportReceipt::Created { thread_id, failure } => (thread_id, failure),
    };
    let target = SessionRef::new(Provider::Codex, &id);
    let verified = match failure {
        Some(error) => Err(error),
        None => verify_visible_turns(&mut server, &id, import)
            .context("verifying visible Codex turns and model provider")
            .and_then(|()| server.shutdown().context("flushing imported Codex thread")),
    };
    if let Err(error) = verified {
        // A lost connection can still have reported the generated thread before it closed.
        drop(server);
        return Err(combine_rollback_error(
            error,
            rollback(binary, cwd, &target, import),
        ));
    }
    Ok(target)
}

enum ImportReceipt {
    Created {
        thread_id: String,
        failure: Option<anyhow::Error>,
    },
    Rejected(String),
}

impl ImportReceipt {
    fn merge(self, next: Self) -> Result<Self> {
        match (self, next) {
            (
                Self::Created { thread_id, failure },
                Self::Created {
                    thread_id: other,
                    failure: later,
                },
            ) if thread_id == other => Ok(Self::Created {
                thread_id,
                failure: failure.or(later),
            }),
            (rejected @ Self::Rejected(_), Self::Rejected(_)) => Ok(rejected),
            _ => bail!("Codex reported conflicting outcomes for the submitted session"),
        }
    }

    fn interrupted(self, error: anyhow::Error) -> Self {
        match self {
            Self::Created { thread_id, failure } => Self::Created {
                thread_id,
                failure: Some(match failure {
                    Some(failure) => failure.context(error),
                    None => error,
                }),
            },
            rejected @ Self::Rejected(_) => rejected,
        }
    }
}

fn submit_import(
    server: &mut AppServer,
    source_path: &Path,
    cwd: &Path,
    import: &CodexImport,
) -> Result<ImportReceipt> {
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
    let import_id = result["importId"]
        .as_str()
        .filter(|id| !id.is_empty())
        .context("Codex import response omitted import ID")?;
    server.wait_for_import(import_id, source_path, deadline)
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
    // Codex reports the canonical source path in its completion notice.
    fs::canonicalize(&path).context("resolving isolated Codex import session")
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
struct ImportRejected(String);

impl std::fmt::Display for ImportRejected {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ImportRejected {}

fn import_receipt(notification: &Value, source_path: &Path) -> Result<Option<ImportReceipt>> {
    let results = notification
        .get("itemTypeResults")
        .and_then(Value::as_array)
        .context("Codex completion omitted import results")?;
    let mut session_results = results
        .iter()
        .filter(|result| result.get("itemType").and_then(Value::as_str) == Some("SESSIONS"));
    let Some(sessions) = session_results.next() else {
        return Ok(None);
    };
    if session_results.next().is_some() {
        bail!("Codex completion reported multiple session import results");
    }
    let successes = sessions["successes"]
        .as_array()
        .context("Codex session import result omitted successes")?;
    let failures = sessions["failures"]
        .as_array()
        .context("Codex session import result omitted failures")?;
    if successes.is_empty() && failures.is_empty() {
        return Ok(None);
    }
    let failure = if let Some(first) = failures.first() {
        let message = first["message"]
            .as_str()
            .map_or_else(|| "unknown session import error".to_owned(), redact_secrets);
        let detail = format!("Codex session import failed: {message}");
        if successes.is_empty() {
            if failures.len() == 1 && rejected_before_creation(first, source_path) {
                return Ok(Some(ImportReceipt::Rejected(detail)));
            }
            bail!("{detail}");
        }
        Some(anyhow!(detail))
    } else {
        None
    };
    let [success] = successes.as_slice() else {
        bail!(
            "Codex session import returned {} targets; expected exactly one",
            successes.len()
        );
    };
    if !matches_import_source(success, source_path) {
        bail!("Codex session import success did not identify the submitted source");
    }
    let id = success["target"]
        .as_str()
        .context("Codex session import omitted target thread ID")?;
    let id = Uuid::parse_str(id).context("Codex imported an invalid thread ID")?;
    Ok(Some(ImportReceipt::Created {
        thread_id: id.to_string(),
        failure,
    }))
}

fn matches_import_source(item: &Value, source_path: &Path) -> bool {
    item["itemType"] == "SESSIONS" && item["source"].as_str().map(Path::new) == Some(source_path)
}

fn rejected_before_creation(failure: &Value, source_path: &Path) -> bool {
    let stage = failure["failureStage"].as_str();
    let subtype = failure["subErrorType"].as_str();
    // Codex acquires one permit for the whole sessions batch before it considers its sole source.
    if stage == Some("session_permit") {
        return failure["itemType"] == "SESSIONS"
            && failure.get("source") == Some(&Value::Null)
            && subtype == Some("failed_to_acquire_import_permit");
    }
    matches_import_source(failure, source_path)
        && match stage {
            Some("session_missing" | "session_source_path" | "session_prepare") => true,
            Some("session_persist") => subtype.is_some_and(|code| {
                code == "failed_to_load_session_config"
                    || code
                        .strip_prefix("failed_to_load_session_config_")
                        .is_some_and(|kind| {
                            matches!(
                                kind,
                                "not_found"
                                    | "permission_denied"
                                    | "already_exists"
                                    | "invalid_input"
                                    | "invalid_data"
                                    | "is_a_directory"
                                    | "not_a_directory"
                                    | "timed_out"
                                    | "write_zero"
                                    | "unexpected_eof"
                                    | "storage_full"
                                    | "quota_exceeded"
                                    | "file_too_large"
                                    | "read_only_filesystem"
                                    | "other"
                            )
                        })
            }),
            _ => false,
        }
}

fn verify_visible_turns(server: &mut AppServer, id: &str, import: &CodexImport) -> Result<()> {
    let result = server.request(
        "thread/read",
        &json!({ "threadId": id, "includeTurns": true }),
    )?;
    verify_read_turns(&result["thread"], import)
}

fn verify_read_turns(thread: &Value, import: &CodexImport) -> Result<()> {
    import.profile.verify(thread)?;
    let expected = import.expected_messages.as_slice();
    let turns = thread
        .get("turns")
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
                if let Some(text) = item.get("text").and_then(Value::as_str) {
                    actual.push(HandoffMessage {
                        role: HandoffRole::Assistant,
                        text: text.to_owned(),
                    });
                }
            }
            _ => {}
        }
    }
    // Since the minimum supported version, Codex appends this display notification
    // after the imported messages, even when the final reply has identical text.
    if actual.last().is_some_and(|message| {
        message.role == HandoffRole::Assistant && message.text == CODEX_IMPORT_MARKER
    }) {
        actual.pop();
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
                    return Err(ImportRejected(error_message).into());
                }
                bail!("{error_message}");
            }
            return message
                .get("result")
                .cloned()
                .context("Codex app-server response omitted result");
        }
    }

    fn wait_for_import(
        &self,
        import_id: &str,
        source_path: &Path,
        deadline: Instant,
    ) -> Result<ImportReceipt> {
        let mut progress: Option<ImportReceipt> = None;
        loop {
            let message = match self.receive_before(deadline) {
                Ok(message) => message,
                Err(error) => {
                    return match progress {
                        Some(receipt) => Ok(receipt
                            .interrupted(error.context("waiting for Codex import completion"))),
                        None => Err(error),
                    };
                }
            };
            let completed = match message["method"].as_str() {
                Some("externalAgentConfig/import/progress") => false,
                Some("externalAgentConfig/import/completed") => true,
                _ => continue,
            };
            let params = &message["params"];
            if params["importId"].as_str() != Some(import_id) {
                continue;
            }
            if let Some(receipt) = import_receipt(params, source_path)? {
                let receipt = match progress {
                    Some(previous) => previous.merge(receipt)?,
                    None => receipt,
                };
                if completed {
                    return Ok(receipt);
                }
                progress = Some(receipt);
            } else if completed {
                bail!("Codex completion omitted the session import outcome");
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
    fn visible_import_marker_cannot_replace_a_missing_reply() {
        let import = CodexImport {
            expected_messages: vec![
                HandoffMessage {
                    role: HandoffRole::User,
                    text: "Repeat the marker".to_owned(),
                },
                HandoffMessage {
                    role: HandoffRole::Assistant,
                    text: CODEX_IMPORT_MARKER.to_owned(),
                },
            ],
            title: None,
            tool_events: 0,
            truncated: false,
            profile: ImportProfile::default(),
        };
        let user =
            json!({"type":"userMessage","content":[{"type":"text","text":"Repeat the marker"}]});
        let marker = json!({"type":"agentMessage","text":CODEX_IMPORT_MARKER});
        let thread = |items: Vec<Value>| json!({"turns":[{"items": items}]});
        assert!(verify_read_turns(&thread(vec![user.clone(), marker.clone()]), &import).is_err());
        assert!(verify_read_turns(&thread(vec![user, marker.clone(), marker]), &import).is_ok());
    }

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
        duplicate.events[0] = message(0, EventKind::MessageUser, CODEX_IMPORT_MARKER);
        assert!(!readback_report(&duplicate, &expected).verified);
        duplicate.events[0] = message(0, EventKind::MessageAssistant, "# AGENTS.md instructions");
        assert!(!readback_report(&duplicate, &expected).verified);

        let mut with_literal_marker = with_provider_context.clone();
        with_literal_marker.events.push(message(
            6,
            EventKind::MessageAssistant,
            CODEX_IMPORT_MARKER,
        ));
        assert!(!readback_report(&with_literal_marker, &expected).verified);
        let mut expected_literal = expected.to_vec();
        expected_literal.push(HandoffMessage {
            role: HandoffRole::Assistant,
            text: CODEX_IMPORT_MARKER.to_owned(),
        });
        assert!(readback_report(&with_literal_marker, &expected_literal).verified);
        assert!(!readback_report(&with_provider_context, &expected_literal).verified);

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
