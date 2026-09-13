use std::{
    io::{self, ErrorKind, Read, Seek},
    path::Path,
    path::PathBuf,
    process::{Command, Stdio},
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use omnis_ir::{EventKind, Provider, ReplayPolicy, SessionRef};
use serde_json::{Value, json};
use wait_timeout::ChildExt;

use crate::{
    LaunchPlan, LaunchTarget, NativeSession, ProviderAdapter, ProviderInstallation,
    support::{
        EventBuilder, is_batch_launcher, omitted_images_text, parse_timestamp, paths_match,
        provider_executable, sort_sessions, string_at, validate_provider, value_at,
    },
};

/// Most sessions one all-project listing returns.
const ALL_PROJECT_SESSION_LIMIT: usize = 10_000;

/// Reads `OpenCode` sessions through its CLI.
///
/// The executable resolves once, with the same `OMNI_OPENCODE_BIN` rules as the CLI shim.
#[derive(Clone, Debug)]
pub struct OpenCodeAdapter {
    binary: Option<PathBuf>,
    notes: Arc<Mutex<Vec<String>>>,
}

impl Default for OpenCodeAdapter {
    fn default() -> Self {
        Self {
            binary: opencode_executable(),
            notes: Arc::default(),
        }
    }
}

/// Lists session records for one project, or for every project when `project` is `None`.
///
/// `opencode session list` covers only its working directory's project, so all-project listing
/// queries root sessions through the documented `opencode db` command, like `Session.listGlobal`.
fn list_session_values(
    binary: &Path,
    project: Option<&Path>,
) -> Result<(Option<Value>, Vec<String>)> {
    const SESSION_LIST: [&str; 4] = ["session", "list", "--format", "json"];
    if project.is_some() {
        return Ok((
            command_json_if_installed(binary, &SESSION_LIST, project)?,
            Vec::new(),
        ));
    }
    let query = format!(
        "SELECT id, title, directory, time_created AS created, time_updated AS updated \
         FROM session WHERE parent_id IS NULL AND time_archived IS NULL \
         ORDER BY time_updated DESC, id DESC LIMIT {ALL_PROJECT_SESSION_LIMIT}"
    );
    match command_json_if_installed(binary, &["db", &query, "--format", "json"], None) {
        Ok(listed) => {
            let truncated = listed
                .as_ref()
                .is_some_and(|value| session_values(value).len() >= ALL_PROJECT_SESSION_LIMIT);
            let notes = truncated
                .then(|| {
                    format!(
                        "OpenCode listing stopped after the {ALL_PROJECT_SESSION_LIMIT} newest sessions."
                    )
                })
                .into_iter()
                .collect();
            Ok((listed, notes))
        }
        // Releases without `opencode db` still list the current directory's project.
        Err(_) => Ok((
            command_json_if_installed(binary, &SESSION_LIST, None)?,
            vec![
                "OpenCode could not list sessions across projects through `opencode db`; only the current directory's project is listed."
                    .to_owned(),
            ],
        )),
    }
}

/// Resolves an `OpenCode` executable that discovery may run directly.
fn opencode_executable() -> Option<PathBuf> {
    // RFC 005 forbids running `.cmd`/`.bat` providers through `cmd.exe`. The CLI shim routes
    // validated npm launchers through `node.exe`, but that routing isn't shared with adapters yet,
    // so on Windows a batch launcher counts as not installed. Overrides get the same refusal and
    // never fall back to PATH.
    provider_executable(Provider::OpenCode)
        .filter(|binary| !cfg!(windows) || !is_batch_launcher(binary))
}

fn not_installed() -> anyhow::Error {
    anyhow::Error::from(io::Error::from(ErrorKind::NotFound))
        .context("no directly executable OpenCode found on PATH or through OMNI_OPENCODE_BIN")
}

/// Refuses `.cmd`/`.bat` launchers on Windows, where `Command` would run them through `cmd.exe`.
///
/// Exact-binary entry points get the same RFC 005 refusal as discovery.
fn ensure_direct_executable(binary: &Path) -> Result<()> {
    if cfg!(windows) && is_batch_launcher(binary) {
        return Err(anyhow!(
            "refusing to run batch OpenCode launcher `{}` through cmd.exe",
            binary.display()
        ));
    }
    Ok(())
}

#[derive(Default)]
struct OpenCodeMetadata {
    id: Option<String>,
    title: Option<String>,
    project_path: Option<PathBuf>,
    git_branch: Option<String>,
    created_at: Option<DateTime<Utc>>,
    updated_at: Option<DateTime<Utc>>,
}

fn command_json(binary: &Path, arguments: &[&str], cwd: Option<&Path>) -> Result<Value> {
    command_json_if_installed(binary, arguments, cwd)?.ok_or_else(|| {
        anyhow::Error::from(io::Error::from(ErrorKind::NotFound)).context(format!(
            "failed to execute `opencode {}`",
            arguments.join(" ")
        ))
    })
}

/// Runs an `OpenCode` command. Returns `None` when the executable does not exist.
fn command_json_if_installed(
    binary: &Path,
    arguments: &[&str],
    cwd: Option<&Path>,
) -> Result<Option<Value>> {
    const MAX_OUTPUT_SIZE: u64 = 128 * 1024 * 1024;
    ensure_direct_executable(binary)?;
    let mut output_file = tempfile::tempfile().context("creating OpenCode output buffer")?;
    let mut command = Command::new(binary);
    command
        .arg("--pure")
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::from(output_file.try_clone()?))
        .stderr(Stdio::null());
    // Import read-back exports run while omni holds Ctrl+C. Their own process group keeps a
    // terminal Ctrl+C from killing them mid-read, and the bounded wait below still reaps them.
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        // A missing working directory also reports NotFound, and that is a real failure.
        Err(error) if error.kind() == ErrorKind::NotFound && cwd.is_none_or(Path::is_dir) => {
            return Ok(None);
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to execute `opencode {}`", arguments.join(" ")));
        }
    };
    let status = match child.wait_timeout(Duration::from_secs(30)) {
        Ok(Some(status)) => status,
        waited => {
            let _ = child.kill();
            let _ = child.wait();
            return match waited {
                Err(error) => Err(error).context("waiting for OpenCode command"),
                Ok(_) => Err(anyhow!("`opencode {}` timed out", arguments.join(" "))),
            };
        }
    };
    if !status.success() {
        return Err(anyhow!(
            "`opencode {}` exited with status {}",
            arguments.join(" "),
            status
        ));
    }
    if output_file.metadata()?.len() > MAX_OUTPUT_SIZE {
        return Err(anyhow!("OpenCode JSON exceeds safe read limit"));
    }
    output_file.rewind()?;
    let mut output = Vec::new();
    output_file
        .take(MAX_OUTPUT_SIZE + 1)
        .read_to_end(&mut output)?;
    parse_command_json(
        &output,
        matches!(arguments.first(), Some(&"session" | &"db")),
    )
    .map(Some)
}

/// Finds one model identifier accepted by installed `OpenCode` CLI.
///
/// Imported historical messages require model metadata even though next turn
/// uses user's current target selection. The executable resolves like [`OpenCodeAdapter`].
///
/// # Errors
///
/// Returns not-installed, process, timeout, output-limit, or malformed model-list errors.
pub fn installed_opencode_model(cwd: &Path) -> Result<(String, String)> {
    let binary = opencode_executable().ok_or_else(not_installed)?;
    installed_opencode_model_with_binary(&binary, cwd)
}

/// Finds one model identifier using an exact `OpenCode` executable.
///
/// # Errors
///
/// Returns Windows batch-launcher refusal, process, timeout, output-limit, or malformed model-list
/// errors.
pub fn installed_opencode_model_with_binary(binary: &Path, cwd: &Path) -> Result<(String, String)> {
    const MAX_OUTPUT_SIZE: u64 = 8 * 1024 * 1024;
    ensure_direct_executable(binary)?;
    let mut output_file = tempfile::tempfile().context("creating OpenCode model buffer")?;
    let mut child = Command::new(binary)
        .args(["--pure", "models"])
        .current_dir(cwd)
        .stdout(Stdio::from(output_file.try_clone()?))
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("failed to execute `{}` models", binary.display()))?;
    let Some(status) = child.wait_timeout(Duration::from_secs(30))? else {
        child.kill().context("stopping timed-out OpenCode models")?;
        child.wait().context("reaping timed-out OpenCode models")?;
        return Err(anyhow!("`opencode models` timed out"));
    };
    if !status.success() {
        return Err(anyhow!("`opencode models` exited with status {status}"));
    }
    if output_file.metadata()?.len() > MAX_OUTPUT_SIZE {
        return Err(anyhow!("OpenCode model list exceeds safe read limit"));
    }
    output_file.rewind()?;
    let mut output = String::new();
    output_file
        .take(MAX_OUTPUT_SIZE + 1)
        .read_to_string(&mut output)?;
    output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.chars().any(char::is_control))
        .find_map(|line| {
            let (provider, model) = line.split_once('/')?;
            (!provider.is_empty() && !model.is_empty())
                .then(|| (provider.to_owned(), model.to_owned()))
        })
        .ok_or_else(|| anyhow!("OpenCode returned no usable model identifiers"))
}

fn parse_command_json(output: &[u8], empty_session_list: bool) -> Result<Value> {
    if empty_session_list && output.iter().all(u8::is_ascii_whitespace) {
        return Ok(Value::Array(Vec::new()));
    }
    serde_json::from_slice(output).context("OpenCode returned malformed JSON")
}

fn session_directory(binary: &Path, id: &str) -> Option<PathBuf> {
    let sessions = command_json(binary, &["session", "list", "--format", "json"], None).ok()?;
    session_values(&sessions)
        .iter()
        .find_map(|value| {
            let metadata = metadata(value);
            (metadata.id.as_deref() == Some(id)).then_some(metadata.project_path)?
        })
        .filter(|path| path.is_dir())
}

/// Reads one `OpenCode` session through an exact executable path.
///
/// # Errors
///
/// Returns Windows batch-launcher refusal, process, timeout, output-limit, or malformed-export
/// errors.
pub fn read_opencode_session_with_binary(
    binary: &Path,
    session: &SessionRef,
) -> Result<omnis_ir::CanonicalSnapshot> {
    validate_provider(session, Provider::OpenCode)?;
    let cwd = session_directory(binary, &session.id).or_else(|| std::env::current_dir().ok());
    read_opencode_session_with_binary_at(binary, session, cwd.as_deref())
}

/// Reads one `OpenCode` session from a known target workspace.
///
/// # Errors
///
/// Returns Windows batch-launcher refusal, process, timeout, output-limit, or malformed-export
/// errors.
pub fn read_opencode_session_with_binary_at(
    binary: &Path,
    session: &SessionRef,
    cwd: Option<&Path>,
) -> Result<omnis_ir::CanonicalSnapshot> {
    validate_provider(session, Provider::OpenCode)?;
    let export = command_json(binary, &["export", &session.id], cwd)?;
    canonicalize_opencode_export(session, &export)
}

/// Converts one documented `opencode export` payload into canonical history.
///
/// # Errors
///
/// Returns an error when the payload identifies a different native session.
pub fn canonicalize_opencode_export(
    session: &SessionRef,
    export: &Value,
) -> Result<omnis_ir::CanonicalSnapshot> {
    validate_provider(session, Provider::OpenCode)?;
    let metadata = metadata(export);
    if metadata.id.as_deref().is_some_and(|id| id != session.id) {
        return Err(anyhow!(
            "OpenCode exported a different session than `{}`",
            session.id
        ));
    }
    let captured_at = metadata.updated_at.unwrap_or_else(Utc::now);
    let mut builder = EventBuilder::new(Provider::OpenCode, &session.id);
    push_export_events(&mut builder, export);
    Ok(builder.snapshot(
        session.clone(),
        metadata.title,
        metadata.project_path,
        metadata.git_branch,
        captured_at,
    ))
}

fn metadata(value: &Value) -> OpenCodeMetadata {
    OpenCodeMetadata {
        id: string_at(
            value,
            &[&["id"], &["sessionID"], &["session_id"], &["info", "id"]],
        )
        .map(str::to_owned),
        title: string_at(value, &[&["title"], &["name"], &["info", "title"]]).map(str::to_owned),
        project_path: string_at(
            value,
            &[
                &["directory"],
                &["cwd"],
                &["project_path"],
                &["info", "directory"],
            ],
        )
        .map(PathBuf::from),
        git_branch: string_at(
            value,
            &[
                &["git", "branch"],
                &["gitBranch"],
                &["info", "git", "branch"],
            ],
        )
        .map(str::to_owned),
        created_at: parse_timestamp(value_at(
            value,
            &[
                &["created_at"],
                &["createdAt"],
                &["created"],
                &["time", "created"],
                &["info", "time", "created"],
            ],
        )),
        updated_at: parse_timestamp(value_at(
            value,
            &[
                &["updated_at"],
                &["updatedAt"],
                &["updated"],
                &["time", "updated"],
                &["info", "time", "updated"],
            ],
        )),
    }
}

fn session_values(value: &Value) -> &[Value] {
    value
        .as_array()
        .or_else(|| value.get("sessions").and_then(Value::as_array))
        .map_or(&[], Vec::as_slice)
}

fn message_values(value: &Value) -> &[Value] {
    value
        .get("messages")
        .and_then(Value::as_array)
        .or_else(|| value.get("data").and_then(Value::as_array))
        .map_or(&[], Vec::as_slice)
}

fn push_export_events(builder: &mut EventBuilder, export: &Value) {
    for message in message_values(export) {
        let role = string_at(message, &[&["role"], &["info", "role"]]);
        let message_kind = match role {
            Some("user") => Some(EventKind::MessageUser),
            Some("assistant") => Some(EventKind::MessageAssistant),
            _ => None,
        };
        let timestamp = parse_timestamp(value_at(
            message,
            &[
                &["timestamp"],
                &["time", "created"],
                &["info", "time", "created"],
            ],
        ));
        if role == Some("assistant") {
            if let Some(payload) = opencode_session_metadata(message) {
                builder.push(
                    EventKind::ProviderEvent,
                    payload,
                    timestamp,
                    ReplayPolicy::HistoricalOnly,
                    Some("omnisession.session_metadata".to_owned()),
                    None,
                );
            }
        }
        let mut visible_text = false;
        if let Some(text) = message.get("content").and_then(Value::as_str) {
            if let Some(kind) = message_kind.clone().filter(|_| !text.is_empty()) {
                visible_text = true;
                builder.push(
                    kind,
                    json!({ "text": text }),
                    timestamp,
                    ReplayPolicy::Contextual,
                    Some("message".to_owned()),
                    None,
                );
            }
        }

        let Some(parts) = message.get("parts").and_then(Value::as_array) else {
            continue;
        };
        // OpenCode hides synthetic text beside a typed prompt: it is harness context such as
        // attached file contents. Messages made only of synthetic text stay visible, because
        // earlier OmniSession OpenCode imports wrote history that way.
        let authored_prompt = role == Some("user")
            && parts
                .iter()
                .any(|part| text_part(part).is_some() && !is_synthetic(part));
        let mut images = 0_usize;
        for part in parts {
            match part.get("type").and_then(Value::as_str) {
                Some("text") => {
                    let Some((kind, text)) = message_kind.clone().zip(text_part(part)) else {
                        continue;
                    };
                    if authored_prompt && is_synthetic(part) {
                        builder.push(
                            EventKind::ProviderEvent,
                            json!({ "type": "opencode_synthetic_text_omitted" }),
                            timestamp,
                            ReplayPolicy::HistoricalOnly,
                            Some("text".to_owned()),
                            None,
                        );
                        continue;
                    }
                    visible_text = true;
                    builder.push(
                        kind,
                        json!({ "text": text }),
                        timestamp,
                        ReplayPolicy::Contextual,
                        Some("text".to_owned()),
                        None,
                    );
                }
                Some("file") => {
                    images += usize::from(
                        string_at(part, &[&["mime"]])
                            .is_some_and(|mime| mime.starts_with("image/")),
                    );
                }
                Some("tool" | "tool_call" | "tool_result") => {
                    push_tool_part(builder, part, timestamp);
                }
                _ => {}
            }
        }
        // An image-only turn keeps a placeholder so the reply still follows a request.
        if role == Some("user") && !visible_text && images > 0 {
            builder.push(
                EventKind::MessageUser,
                json!({ "text": omitted_images_text(images) }),
                timestamp,
                ReplayPolicy::Contextual,
                Some("file".to_owned()),
                None,
            );
        }
    }
}

/// Non-empty text of a `text` part.
fn text_part(part: &Value) -> Option<&str> {
    (part.get("type").and_then(Value::as_str) == Some("text"))
        .then(|| part.get("text").and_then(Value::as_str))
        .flatten()
        .filter(|text| !text.is_empty())
}

fn is_synthetic(part: &Value) -> bool {
    part.get("synthetic").and_then(Value::as_bool) == Some(true)
}

fn push_tool_part(builder: &mut EventBuilder, part: &Value, timestamp: Option<DateTime<Utc>>) {
    let failed = string_at(part, &[&["state", "status"], &["status"]])
        .is_some_and(|status| matches!(status, "error" | "failed"));
    let completed = string_at(part, &[&["state", "status"], &["status"]])
        .is_some_and(|status| matches!(status, "completed" | "success"));
    let kind = if failed {
        EventKind::ToolFailed
    } else if completed || part.get("type").and_then(Value::as_str) == Some("tool_result") {
        EventKind::ToolCompleted
    } else {
        EventKind::ToolCalled
    };
    builder.push(
        kind,
        part.clone(),
        timestamp,
        ReplayPolicy::HistoricalOnly,
        part.get("type").and_then(Value::as_str).map(str::to_owned),
        None,
    );
}

fn opencode_session_metadata(message: &Value) -> Option<Value> {
    let model = string_at(
        message,
        &[
            &["modelID"],
            &["model_id"],
            &["info", "modelID"],
            &["info", "model_id"],
        ],
    );
    let provider = string_at(
        message,
        &[
            &["providerID"],
            &["provider_id"],
            &["info", "providerID"],
            &["info", "provider_id"],
        ],
    );
    let model =
        model.map(|model| provider.map_or_else(|| model.to_owned(), |p| format!("{p}/{model}")));
    let tokens = value_at(message, &[&["tokens"], &["info", "tokens"]]);
    let total_tokens = tokens
        .map(|tokens| {
            [
                &["input"][..],
                &["output"][..],
                &["reasoning"][..],
                &["cache", "read"][..],
                &["cache", "write"][..],
            ]
            .into_iter()
            .filter_map(|path| value_at(tokens, &[path]).and_then(Value::as_u64))
            .fold(0_u64, u64::saturating_add)
        })
        .filter(|tokens| *tokens > 0);
    let reasoning_mode = tokens
        .and_then(|tokens| tokens.get("reasoning"))
        .and_then(Value::as_u64)
        .is_some_and(|tokens| tokens > 0)
        .then_some("reasoning");
    (model.is_some() || reasoning_mode.is_some() || total_tokens.is_some()).then(|| {
        json!({
            "model": model,
            "reasoning_mode": reasoning_mode,
            "total_tokens": total_tokens,
            "token_usage": "incremental",
        })
    })
}

impl ProviderAdapter for OpenCodeAdapter {
    fn provider(&self) -> Provider {
        Provider::OpenCode
    }

    fn probe(&self) -> ProviderInstallation {
        ProviderInstallation {
            provider: Provider::OpenCode,
            installed: self.binary.is_some(),
            executable: self.binary.clone(),
            data_root: None,
        }
    }

    fn discovery_notes(&self) -> Vec<String> {
        self.notes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn list_sessions(&self, project: Option<&Path>) -> Result<Vec<NativeSession>> {
        let Some(binary) = self.binary.as_deref() else {
            return Ok(Vec::new());
        };
        let (listed, notes) = list_session_values(binary, project)?;
        *self.notes.lock().unwrap_or_else(PoisonError::into_inner) = notes;
        let Some(value) = listed else {
            return Ok(Vec::new());
        };
        let mut sessions = Vec::new();
        for value in session_values(&value) {
            let metadata = metadata(value);
            let Some(id) = metadata.id else {
                continue;
            };
            if project.is_some_and(|project| {
                metadata
                    .project_path
                    .as_deref()
                    .is_none_or(|recorded| !paths_match(recorded, project))
            }) {
                continue;
            }
            let event_count = value
                .get("message_count")
                .or_else(|| value.get("messageCount"))
                .and_then(Value::as_u64)
                .and_then(|count| usize::try_from(count).ok())
                .unwrap_or(0);
            sessions.push(NativeSession {
                session: SessionRef::new(Provider::OpenCode, id),
                title: metadata.title,
                project_path: metadata.project_path,
                git_branch: metadata.git_branch,
                created_at: metadata.created_at,
                updated_at: metadata.updated_at,
                updated_at_approximate: false,
                event_count,
                source_path: None,
            });
        }
        sort_sessions(&mut sessions);
        Ok(sessions)
    }

    fn read_session(&self, session: &SessionRef) -> Result<omnis_ir::CanonicalSnapshot> {
        validate_provider(session, Provider::OpenCode)?;
        let binary = self.binary.as_deref().ok_or_else(not_installed)?;
        read_opencode_session_with_binary(binary, session)
    }

    // Launch plans keep the command name. The CLI maps it to the real binary, honoring
    // `OMNI_OPENCODE_BIN`, and callers key provider routing on that name.
    fn new_session_plan(&self, target: &LaunchTarget) -> Result<LaunchPlan> {
        let mut args = Vec::new();
        if let Some(prompt) = &target.prompt {
            args.extend(["--prompt".to_owned(), prompt.clone()]);
        }
        Ok(LaunchPlan {
            program: "opencode".to_owned(),
            args,
            cwd: target.cwd.clone(),
        })
    }

    fn launch_plan(&self, session: &SessionRef, target: &LaunchTarget) -> Result<LaunchPlan> {
        validate_provider(session, Provider::OpenCode)?;
        let mut args = vec!["--session".to_owned(), session.id.clone()];
        if target.fork {
            args.push("--fork".to_owned());
        }
        if let Some(prompt) = &target.prompt {
            args.push(prompt.clone());
        }
        Ok(LaunchPlan {
            program: "opencode".to_owned(),
            args,
            cwd: target.cwd.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_command_json, push_export_events};
    use crate::support::EventBuilder;
    use omnis_ir::{EventKind, Provider, ReplayPolicy, SessionRef};

    #[cfg(windows)]
    #[test]
    fn exact_batch_launchers_are_refused_on_windows() {
        use super::{installed_opencode_model_with_binary, read_opencode_session_with_binary_at};

        let temporary = tempfile::tempdir().expect("temporary OpenCode launcher");
        let launcher = temporary.path().join("opencode.cmd");
        std::fs::write(&launcher, "@echo off\r\necho {}\r\n").expect("batch launcher");
        let session = SessionRef::new(Provider::OpenCode, "ses_synthetic");

        for error in [
            installed_opencode_model_with_binary(&launcher, temporary.path())
                .expect_err("model lookup must refuse batch launcher"),
            read_opencode_session_with_binary_at(&launcher, &session, Some(temporary.path()))
                .expect_err("export must refuse batch launcher"),
        ] {
            assert!(
                format!("{error:#}").contains("refusing to run batch OpenCode launcher"),
                "{error:#}"
            );
        }
    }

    #[test]
    fn export_parser_keeps_text_and_marks_tools_historical() {
        let export = serde_json::json!({
            "messages": [{
                "info": { "role": "assistant" },
                "parts": [
                    { "type": "text", "text": "visible" },
                    { "type": "tool", "tool": "bash", "state": { "status": "completed" } },
                    { "type": "reasoning", "text": "hidden" }
                ]
            }]
        });
        let mut builder = EventBuilder::new(Provider::OpenCode, "ses_test");
        push_export_events(&mut builder, &export);
        let snapshot = builder.snapshot(
            SessionRef::new(Provider::OpenCode, "ses_test"),
            None,
            None,
            None,
            chrono::Utc::now(),
        );

        assert_eq!(snapshot.events.len(), 2);
        assert_eq!(snapshot.events[0].kind, EventKind::MessageAssistant);
        assert_eq!(snapshot.events[1].kind, EventKind::ToolCompleted);
        assert_eq!(
            snapshot.events[1].replay_policy,
            ReplayPolicy::HistoricalOnly
        );
    }

    #[test]
    fn empty_session_list_is_valid_but_empty_export_is_not() {
        assert_eq!(
            parse_command_json(b"\n", true).expect("empty session list"),
            serde_json::json!([])
        );
        assert!(parse_command_json(b"\n", false).is_err());
    }

    #[test]
    fn synthetic_context_beside_an_authored_prompt_is_not_user_text() {
        let export = serde_json::json!({
            "messages": [
                {
                    "info": { "role": "user" },
                    "parts": [
                        { "type": "text", "text": "explain @src/lib.rs" },
                        { "type": "text", "synthetic": true, "text": "Called the Read tool with the following input: {\"filePath\":\"src/lib.rs\"}" },
                        { "type": "text", "synthetic": true, "text": "fn private_file_contents() {}" },
                        { "type": "file", "mime": "text/plain", "filename": "lib.rs", "url": "file:///workspace/src/lib.rs" }
                    ]
                },
                {
                    "info": { "role": "user" },
                    "parts": [
                        { "type": "file", "mime": "image/png", "filename": "screen.png", "url": "data:image/png;base64,iVBORw0KGgo=" }
                    ]
                },
                {
                    // Earlier OmniSession imports wrote every history part as synthetic.
                    "info": { "role": "user" },
                    "parts": [{ "type": "text", "synthetic": true, "text": "imported request" }]
                }
            ]
        });
        let mut builder = EventBuilder::new(Provider::OpenCode, "ses_test");
        push_export_events(&mut builder, &export);
        let snapshot = builder.snapshot(
            SessionRef::new(Provider::OpenCode, "ses_test"),
            None,
            None,
            None,
            chrono::Utc::now(),
        );

        assert_eq!(
            snapshot
                .events
                .iter()
                .filter(|event| event.kind == EventKind::MessageUser)
                .filter_map(|event| event.payload["text"].as_str())
                .collect::<Vec<_>>(),
            [
                "explain @src/lib.rs",
                "[1 image omitted]",
                "imported request"
            ]
        );
        let rendered = serde_json::to_string(&snapshot).expect("serialize OpenCode snapshot");
        assert!(!rendered.contains("private_file_contents"));
        assert!(!rendered.contains("iVBORw0KGgo"));
    }

    #[cfg(unix)]
    #[test]
    fn listing_without_a_project_covers_every_project() {
        use std::os::unix::fs::PermissionsExt;

        use crate::ProviderAdapter;

        // `session list` only sees the current directory's project; `db` sees the whole store.
        let temporary = tempfile::tempdir().expect("temporary OpenCode launcher");
        let binary = temporary.path().join("opencode");
        std::fs::write(
            &binary,
            r#"#!/bin/sh
current='{"id":"ses_current","title":"Current","directory":"/workspace/current","created":1767225600000,"updated":1767229200000}'
case "$2" in
  db) printf '[%s,{"id":"ses_other","title":"Other","directory":"/workspace/other","created":1767225600000,"updated":1767232800000}]' "$current" ;;
  session) printf '[%s]' "$current" ;;
  *) exit 1 ;;
esac
"#,
        )
        .expect("fake OpenCode");
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755))
            .expect("executable fake OpenCode");
        let adapter = super::OpenCodeAdapter {
            binary: Some(binary),
            ..super::OpenCodeAdapter::default()
        };

        let sessions = adapter.list_sessions(None).expect("all-project listing");

        assert_eq!(
            sessions
                .iter()
                .map(|session| session.session.id.as_str())
                .collect::<Vec<_>>(),
            ["ses_other", "ses_current"]
        );
        assert_eq!(
            sessions[0].updated_at,
            chrono::DateTime::from_timestamp_millis(1_767_232_800_000)
        );
        assert!(adapter.discovery_notes().is_empty());
    }
}
