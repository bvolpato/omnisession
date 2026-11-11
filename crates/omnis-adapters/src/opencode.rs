use std::{
    io::{self, ErrorKind, Read, Seek},
    path::Path,
    path::PathBuf,
    process::{Command, Output, Stdio},
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use omnis_ir::{EventKind, Provider, ReplayPolicy, SessionRef};
use serde_json::{Value, json};
use wait_timeout::ChildExt;

use crate::{
    LaunchPlan, LaunchTarget, NativeSession, ProviderAdapter, ProviderInstallation,
    bounded_provider_output, provider_process,
    support::{
        EventBuilder, omitted_images_text, parse_timestamp, paths_match, provider_executable,
        sort_sessions, string_at, validate_provider, value_at,
    },
};

/// Most sessions one all-project listing returns.
const ALL_PROJECT_SESSION_LIMIT: usize = 10_000;

/// Reads `OpenCode` sessions through its CLI.
///
/// The executable resolves once, with the same `OMNI_OPENCODE_BIN` rules as the CLI.
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
    if is_opencode_v2(binary) {
        if let Some(project) = project {
            return Ok((
                command_json_if_installed(
                    binary,
                    &["session", "list", "--format", "json"],
                    Some(project),
                )?,
                Vec::new(),
            ));
        }
        let api_args = [
            "api",
            "--standalone",
            "session.list",
            "--param",
            "parentID=null",
            "--param",
            "archived=false",
            "--param",
            "limit=10000",
        ];
        return match command_json_if_installed(binary, &api_args, None) {
            Ok(listed) => {
                let count = listed.as_ref().map_or(0, |v| session_values(v).len());
                let notes = (count >= ALL_PROJECT_SESSION_LIMIT)
                    .then(|| {
                        format!(
                            "OpenCode listing stopped after the {ALL_PROJECT_SESSION_LIMIT} newest sessions."
                        )
                    })
                    .into_iter()
                    .collect();
                Ok((listed, notes))
            }
            Err(_) => Ok((
                command_json_if_installed(binary, &["session", "list", "--format", "json"], None)?,
                vec![
                    "OpenCode could not list sessions across projects through `/api/session`; only the current directory's project is listed."
                        .to_owned(),
                ],
            )),
        };
    }
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

/// Resolves an `OpenCode` executable with validated npm launcher support on Windows.
fn opencode_executable() -> Option<PathBuf> {
    provider_executable(Provider::OpenCode).and_then(runnable_opencode_binary)
}

fn runnable_opencode_binary(binary: PathBuf) -> Option<PathBuf> {
    provider_process(&binary).ok().map(|_| binary)
}

fn not_installed() -> anyhow::Error {
    anyhow::Error::from(io::Error::from(ErrorKind::NotFound))
        .context("no runnable OpenCode found on PATH or through OMNI_OPENCODE_BIN")
}

fn ensure_provider_executable(binary: &Path) -> Result<()> {
    provider_process(binary).map(drop)
}

/// Detects whether an `OpenCode` executable is version 2.x or later.
///
/// Unsuccessful, timed out, or unknown output defaults to false (preserving v1 behavior).
#[must_use]
pub fn is_opencode_v2(binary: &Path) -> bool {
    let Ok(mut command) = provider_process(binary) else {
        return false;
    };
    command
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    let Ok(mut child) = command.spawn() else {
        return false;
    };
    // A timeout sends OpenCode 1 arguments to OpenCode 2, so a busy machine must not cause one.
    let Ok(Some(status)) = child.wait_timeout(Duration::from_secs(15)) else {
        let _ = child.kill();
        let _ = child.wait();
        return false;
    };
    if !status.success() {
        return false;
    }
    let mut output = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        let _ = stdout.read_to_string(&mut output);
    }
    let trimmed = output.trim();
    let version_str = trimmed.strip_prefix("opencode").unwrap_or(trimmed).trim();
    let version_str = version_str.strip_prefix('v').unwrap_or(version_str).trim();
    version_str
        .split('.')
        .next()
        .and_then(|major| major.parse::<u64>().ok())
        .is_some_and(|major| major >= 2)
}

/// Derives CLI arguments for an `OpenCode` command based on detected CLI generation.
///
/// v1 prepends `--pure` and retains legacy command arguments.
/// v2 maps `export` to `session export` and `import` to `session import`.
/// Helpers use a private server rather than starting or reconfiguring the managed service.
///
/// # Errors
///
/// Returns unrecognized Windows batch launcher or access errors.
pub fn opencode_command_args(binary: &Path, arguments: &[&str]) -> Result<Vec<String>> {
    ensure_provider_executable(binary)?;
    if is_opencode_v2(binary) {
        let mut args = Vec::with_capacity(arguments.len() + 2);
        if matches!(arguments.first(), Some(&("export" | "import"))) {
            args.push("session".to_owned());
        }
        args.extend(arguments.iter().map(|s| (*s).to_owned()));
        if !arguments.contains(&"--standalone") {
            // OpenCode 2.0 accepts this flag on the leaf command.
            args.push("--standalone".to_owned());
        }
        Ok(args)
    } else {
        let mut args = Vec::with_capacity(arguments.len() + 1);
        args.push("--pure".to_owned());
        args.extend(arguments.iter().map(|s| (*s).to_owned()));
        Ok(args)
    }
}

/// Forks an `OpenCode` session using its v2 API and returns the new session ID.
///
/// # Errors
///
/// Returns unrecognized Windows batch-launcher, process, timeout, output-limit, or fork API errors.
pub fn fork_opencode_session_with_binary(
    binary: &Path,
    session_id: &str,
    cwd: Option<&Path>,
) -> Result<String> {
    ensure_provider_executable(binary)?;
    let endpoint = format!("{}/fork", session_endpoint(session_id)?);
    let output = command_json(
        binary,
        &["api", "--standalone", "POST", &endpoint, "-d", "{}"],
        cwd,
    )?;
    let child_id = string_at(&output, &[&["data", "id"], &["id"]])
        .ok_or_else(|| anyhow!("OpenCode fork returned malformed session info without an ID"))?;
    if child_id.is_empty() || child_id == session_id {
        bail!("OpenCode fork returned unchanged session ID `{child_id}`");
    }
    if !child_id.starts_with("ses") {
        bail!("OpenCode fork returned invalid session ID `{child_id}`");
    }
    Ok(child_id.to_owned())
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

fn session_endpoint(id: &str) -> Result<String> {
    if matches!(id, "" | "." | "..") {
        bail!("invalid OpenCode session ID path segment");
    }
    let encoded: String = id
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
                char::from(byte).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect();
    Ok(format!("/api/session/{encoded}"))
}

/// Captures JSON from a prepared `OpenCode` command without interactive input.
/// Legacy Unix releases flush large writes reliably when stdout is a raw terminal.
///
/// # Errors
///
/// Returns an error if the command cannot start, stdout exceeds the limit, an output
/// stream cannot be read, or the command or its output exceeds the deadline.
pub fn bounded_opencode_json_output(
    command: &mut Command,
    timeout: Duration,
    stdout_limit: usize,
    stderr_limit: Option<usize>,
) -> Result<Output> {
    opencode_json_output(command, timeout, stdout_limit, stderr_limit, None)
}

fn opencode_json_output(
    command: &mut Command,
    timeout: Duration,
    stdout_limit: usize,
    stderr_limit: Option<usize>,
    large_output_timeout: Option<Duration>,
) -> Result<Output> {
    command.stdin(Stdio::null());
    #[cfg(not(unix))]
    let _ = large_output_timeout;
    #[cfg(unix)]
    if command.get_args().next().is_some_and(|arg| arg == "--pure") {
        command
            .env("NO_COLOR", "1")
            .env("FORCE_COLOR", "0")
            .env("TERM", "dumb");
        return crate::provider_process::bounded_terminal_output(
            command,
            timeout,
            stdout_limit,
            stderr_limit,
            large_output_timeout,
        );
    }
    bounded_provider_output(command, timeout, Some(stdout_limit), stderr_limit)
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
    run_json(binary, arguments, cwd, None)
}

fn run_json(
    binary: &Path,
    arguments: &[&str],
    cwd: Option<&Path>,
    lookup_id: Option<&str>,
) -> Result<Option<Value>> {
    const MAX_OUTPUT_SIZE: usize = 128 * 1024 * 1024;
    const DIAGNOSTIC_LIMIT: usize = 8 * 1024;
    ensure_provider_executable(binary)?;
    // The 2.x API writes the HTTP status to stderr and the typed error to stdout.
    let exact_api = lookup_id.is_some() && arguments.first() == Some(&"api");
    let args = opencode_command_args(binary, arguments)?;
    let mut command = provider_process(binary)?;
    command.args(&args);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let output = match opencode_json_output(
        &mut command,
        Duration::from_secs(30),
        MAX_OUTPUT_SIZE,
        exact_api.then_some(DIAGNOSTIC_LIMIT),
        Some(Duration::from_secs(60)),
    ) {
        Ok(output) => output,
        // A missing working directory also reports NotFound, and that is a real failure.
        Err(error)
            if lookup_id.is_none()
                && error
                    .downcast_ref::<io::Error>()
                    .is_some_and(|io| io.kind() == ErrorKind::NotFound)
                && cwd.is_none_or(Path::is_dir) =>
        {
            return Ok(None);
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to execute `opencode {}`", arguments.join(" ")));
        }
    };
    let status = output.status;
    if !status.success() {
        if status.code() == Some(1)
            && let Some(id) = lookup_id
            && exact_api
            && output.stderr.trim_ascii() == b"HTTP 404 Not Found"
            && output.stdout.len() <= DIAGNOSTIC_LIMIT
            && let Ok(error) = serde_json::from_slice::<Value>(&output.stdout)
            && error["_tag"] == "SessionNotFoundError"
            && error["sessionID"] == id
        {
            return Ok(None);
        }
        return Err(anyhow!(
            "`opencode {}` exited with status {}",
            arguments.join(" "),
            status
        ));
    }
    parse_command_json(
        &output.stdout,
        lookup_id.is_none() && matches!(arguments.first(), Some(&"session" | &"db" | &"api")),
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
/// Returns unrecognized Windows batch-launcher, process, timeout, output-limit, or malformed model-list
/// errors.
pub fn installed_opencode_model_with_binary(binary: &Path, cwd: &Path) -> Result<(String, String)> {
    const MAX_OUTPUT_SIZE: u64 = 8 * 1024 * 1024;
    ensure_provider_executable(binary)?;
    let mut output_file = tempfile::tempfile().context("creating OpenCode model buffer")?;
    let args = opencode_command_args(binary, &["models"])?;
    let mut child = provider_process(binary)?
        .args(&args)
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

/// Queries the provider's metadata for an exact session ID in the supplied working directory.
/// An empty result means the provider confirmed that the session does not exist.
///
/// # Errors
///
/// Returns an error if the provider cannot verify presence or absence, including versions
/// without a metadata query. This function never infers absence from a failed transcript export.
pub fn find_opencode_session_with_binary(
    binary: &Path,
    id: &str,
    cwd: Option<&Path>,
) -> Result<Option<NativeSession>> {
    lookup_metadata(binary, id, cwd, is_opencode_v2(binary))
}

fn lookup_metadata(
    binary: &Path,
    id: &str,
    cwd: Option<&Path>,
    v2: bool,
) -> Result<Option<NativeSession>> {
    let value = if v2 {
        run_json(
            binary,
            &["api", "--standalone", "GET", &session_endpoint(id)?],
            cwd,
            Some(id),
        )?
        .map(|mut response| match response.get_mut("data") {
            Some(record) => record.take(),
            None => response,
        })
    } else {
        let query = format!(
            "SELECT id, title, directory, time_created AS created, time_updated AS updated \
             FROM session WHERE id = CAST(X'{}' AS TEXT)",
            hex::encode(id)
        );
        let rows = run_json(binary, &["db", &query, "--format", "json"], cwd, Some(id))?
            .context("OpenCode returned no database result for an exact session lookup")?;
        match rows.as_array().map(Vec::as_slice) {
            Some([]) => return Ok(None),
            Some([row]) => Some(row.clone()),
            _ => bail!("OpenCode returned invalid rows for an exact session lookup"),
        }
    };
    value.map(|value| exact_session(&value, id)).transpose()
}

fn exact_session(record: &Value, id: &str) -> Result<NativeSession> {
    native_session(record)
        .filter(|_| record["id"].as_str() == Some(id))
        .context("OpenCode exact lookup returned a missing or different session ID")
}

fn legacy_listed_session(binary: &Path, id: &str) -> Option<NativeSession> {
    let rows = command_json(binary, &["session", "list", "--format", "json"], None).ok()?;
    let row = session_values(&rows)
        .iter()
        .find(|row| metadata_id(row) == Some(id))?;
    native_session(row)
}

/// Reads one `OpenCode` session through an exact executable path.
///
/// # Errors
///
/// Returns unrecognized Windows batch-launcher, process, timeout, output-limit, or malformed-export
/// errors.
pub fn read_opencode_session_with_binary(
    binary: &Path,
    session: &SessionRef,
) -> Result<omnis_ir::CanonicalSnapshot> {
    validate_provider(session, Provider::OpenCode)?;
    let initial = read_opencode_session_with_binary_at(binary, session, None);
    if initial.is_ok() || is_opencode_v2(binary) {
        return initial;
    }
    // Legacy OpenCode may need the working directory from its project listing for export.
    let cwd = legacy_listed_session(binary, &session.id)
        .and_then(|record| record.project_path)
        .filter(|path| path.is_dir() && !same_file::is_same_file(path, ".").unwrap_or(false));
    match cwd {
        Some(cwd) => read_opencode_session_with_binary_at(binary, session, Some(&cwd)),
        None => initial,
    }
}

/// Reads one `OpenCode` session from a known target workspace.
///
/// # Errors
///
/// Returns unrecognized Windows batch-launcher, process, timeout, output-limit, or malformed-export
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

fn metadata_id(value: &Value) -> Option<&str> {
    string_at(
        value,
        &[&["id"], &["sessionID"], &["session_id"], &["info", "id"]],
    )
}

fn metadata(value: &Value) -> OpenCodeMetadata {
    OpenCodeMetadata {
        id: metadata_id(value).map(str::to_owned),
        title: string_at(value, &[&["title"], &["name"], &["info", "title"]]).map(str::to_owned),
        project_path: string_at(
            value,
            &[
                &["directory"],
                &["cwd"],
                &["project_path"],
                &["info", "directory"],
                &["location", "directory"],
                &["info", "location", "directory"],
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
        .or_else(|| value.get("data").and_then(Value::as_array))
        .map_or(&[], Vec::as_slice)
}

fn native_session(value: &Value) -> Option<NativeSession> {
    let metadata = metadata(value);
    let event_count = value
        .get("message_count")
        .or_else(|| value.get("messageCount"))
        .and_then(Value::as_u64)
        .and_then(|count| usize::try_from(count).ok())
        .unwrap_or(0);
    Some(NativeSession {
        session: SessionRef::new(Provider::OpenCode, metadata.id?),
        title: metadata.title,
        project_path: metadata.project_path,
        git_branch: metadata.git_branch,
        created_at: metadata.created_at,
        updated_at: metadata.updated_at,
        updated_at_approximate: false,
        event_count,
        source_path: None,
    })
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
        let role = string_at(message, &[&["role"], &["info", "role"], &["type"]]);
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
        if role == Some("assistant")
            && let Some(payload) = opencode_session_metadata(message)
        {
            builder.push(
                EventKind::ProviderEvent,
                payload,
                timestamp,
                ReplayPolicy::HistoricalOnly,
                Some("omnisession.session_metadata".to_owned()),
                None,
            );
        }
        push_export_content(builder, message, message_kind.as_ref(), timestamp);
    }
}

fn push_export_content(
    builder: &mut EventBuilder,
    message: &Value,
    message_kind: Option<&EventKind>,
    timestamp: Option<DateTime<Utc>>,
) {
    let user = message_kind == Some(&EventKind::MessageUser);
    let mut visible_text = false;
    let direct_text = message
        .get("content")
        .and_then(Value::as_str)
        .or_else(|| message.get("text").and_then(Value::as_str));
    if let Some(text) = direct_text
        && let Some(kind) = message_kind.cloned().filter(|_| !text.is_empty())
    {
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

    let parts = message
        .get("parts")
        .and_then(Value::as_array)
        .or_else(|| message.get("content").and_then(Value::as_array))
        .map_or(&[][..], Vec::as_slice);
    // OpenCode hides synthetic text beside a typed prompt: it is harness context such as
    // attached file contents. Messages made only of synthetic text stay visible, because
    // earlier OmniSession OpenCode imports wrote history that way.
    let authored_prompt = user
        && parts
            .iter()
            .any(|part| text_part(part).is_some() && !is_synthetic(part));
    let mut images = message
        .get("files")
        .and_then(Value::as_array)
        .map_or(0, |files| {
            files.iter().filter(|file| is_image(file)).count()
        });
    for part in parts {
        match part.get("type").and_then(Value::as_str) {
            Some("text") => {
                let Some((kind, text)) = message_kind.cloned().zip(text_part(part)) else {
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
                images += usize::from(is_image(part));
            }
            Some("tool" | "tool_call" | "tool_result") => {
                push_tool_part(builder, part, timestamp);
            }
            _ => {}
        }
    }
    // An image-only turn keeps a placeholder so the reply still follows a request.
    if user && !visible_text && images > 0 {
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

fn is_image(part: &Value) -> bool {
    string_at(part, &[&["mime"]]).is_some_and(|mime| mime.starts_with("image/"))
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
            &["model", "id"],
        ],
    );
    let provider = string_at(
        message,
        &[
            &["providerID"],
            &["provider_id"],
            &["info", "providerID"],
            &["info", "provider_id"],
            &["model", "providerID"],
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
        let mut sessions: Vec<_> = session_values(&value)
            .iter()
            .filter_map(native_session)
            .filter(|session| {
                project.is_none_or(|project| {
                    session
                        .project_path
                        .as_deref()
                        .is_some_and(|path| paths_match(path, project))
                })
            })
            .collect();
        sort_sessions(&mut sessions);
        Ok(sessions)
    }

    fn find_native_session(&self, id: &str) -> Result<Option<NativeSession>> {
        let Some(binary) = self.binary.as_deref() else {
            return Ok(None);
        };
        let v2 = is_opencode_v2(binary);
        // The v2 API rejects IDs without its session prefix before looking them up.
        if v2 && !id.starts_with("ses") {
            return Ok(None);
        }
        match lookup_metadata(binary, id, None, v2) {
            Err(error) if !v2 => {
                // A listed session can resume even if its transcript is too large to export.
                // Keep the legacy list command for versions without the db command.
                if let Some(record) = legacy_listed_session(binary, id) {
                    return Ok(Some(record));
                }
                // A listing can omit a session. Export failures can also reflect unreadable messages.
                let export = command_json(binary, &["export", id], None)
                    .with_context(|| format!("OpenCode exact metadata lookup failed: {error}"))?;
                exact_session(export.get("info").unwrap_or(&export), id).map(Some)
            }
            result => result,
        }
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
            env: Vec::new(),
        })
    }

    fn launch_plan(&self, session: &SessionRef, target: &LaunchTarget) -> Result<LaunchPlan> {
        validate_provider(session, Provider::OpenCode)?;
        let is_v2 = self.binary.as_deref().is_some_and(is_opencode_v2);
        if is_v2 {
            if target.fork {
                bail!(
                    "OpenCode 2 does not support CLI `--fork`; sessions must be forked via the v2 API before launch"
                );
            }
            let mut args = vec!["--session".to_owned(), session.id.clone()];
            if let Some(prompt) = &target.prompt {
                args.extend(["--prompt".to_owned(), prompt.clone()]);
            }
            return Ok(LaunchPlan {
                program: "opencode".to_owned(),
                args,
                cwd: target.cwd.clone(),
                env: Vec::new(),
            });
        }
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
            env: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_command_json, push_export_events};
    use crate::support::EventBuilder;
    use omnis_ir::{EventKind, Provider, ReplayPolicy, SessionRef};

    #[cfg(unix)]
    #[test]
    fn exact_session_lookup_distinguishes_hidden_missing_and_unreadable_sessions() {
        use crate::ProviderAdapter;
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        for (version, commands) in [
            (
                "1.0.0",
                r#"
case "$2:$3" in
    db:*) exit 2 ;;
    session:list) echo '[{"id":"ses_listed_other"},{"id":"ses_listed","directory":"/project"}]' ;;
    export:ses_hidden) echo '{"info":{"id":"ses_hidden","directory":"/project"}}' ;;
    export:ses_missing) echo 'Error: Session not found: ses_missing' >&2; exit 1 ;;
    export:ses_wrong) echo 'Error: Session not found: ses_other' >&2; exit 1 ;;
    export:ses_mismatch) echo '{"info":{"id":"ses_other"}}' ;;
    *) exit 1 ;;
esac"#,
            ),
            (
                "2.2.0",
                r#"
case "$3:$4" in
    session.list:*) echo '{"data":[]}' ;;
    GET:/api/session/ses_hidden) echo '{"data":{"id":"ses_hidden","location":{"directory":"/project"}}}' ;;
    GET:/api/session/ses_verbose) printf '%09000d' 0 >&2; echo '{"data":{"id":"ses_verbose"}}' ;;
    GET:/api/session/ses_path%2F%3F%23%25%20%C3%A9) echo '{"id":"ses_path/?#% é"}' ;;
    GET:/api/session/ses_missing) echo 'HTTP 404 Not Found' >&2; echo '{"_tag":"SessionNotFoundError","sessionID":"ses_missing"}'; exit 1 ;;
    GET:/api/session/ses_wrong) echo 'HTTP 404 Not Found' >&2; echo '{"_tag":"SessionNotFoundError","sessionID":"ses_other"}'; exit 1 ;;
    GET:/api/session/ses_server) echo 'HTTP 500 Internal Server Error' >&2; echo '{"_tag":"SessionNotFoundError","sessionID":"ses_server"}'; exit 1 ;;
    GET:/api/session/ses_mismatch) echo '{"data":{"id":"ses_other"}}' ;;
    GET:/api/session/ses_malformed) echo '{"sessionID":"ses_malformed"}' ;;
    *) exit 1 ;;
esac"#,
            ),
        ] {
            let binary = directory.path().join(version);
            std::fs::write(&binary, format!(
                "#!/bin/sh\nif [ \"$1\" = --version ]; then echo {version}; exit 0; fi\n{commands}\n"
            )).unwrap();
            std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
            let primed = crate::test_support::output_after_write(
                std::process::Command::new(&binary).arg("--version"),
            );
            assert!(primed.status.success());
            let adapter = super::OpenCodeAdapter {
                binary: Some(binary.clone()),
                notes: std::sync::Arc::default(),
            };
            assert!(
                adapter
                    .list_sessions(None)
                    .unwrap()
                    .iter()
                    .all(|record| record.session.id != "ses_hidden")
            );
            let record = adapter.find_native_session("ses_hidden").unwrap().unwrap();
            assert_eq!(record.session.id, "ses_hidden");
            assert_eq!(
                record.project_path.as_deref(),
                Some(std::path::Path::new("/project"))
            );
            if version == "1.0.0" {
                let listed = adapter.find_native_session("ses_listed").unwrap().unwrap();
                assert_eq!(
                    listed.project_path.as_deref(),
                    Some(std::path::Path::new("/project"))
                );
                assert!(
                    super::find_opencode_session_with_binary(&binary, "ses_listed", None).is_err()
                );
                assert!(adapter.find_native_session("ses_missing").is_err());
                assert!(
                    super::find_opencode_session_with_binary(&binary, "ses_hidden", None).is_err()
                );
            } else {
                for id in ["ses_path/?#% é", "ses_verbose"] {
                    assert!(adapter.find_native_session(id).unwrap().is_some(), "{id}");
                }
                assert!(
                    adapter
                        .find_native_session("ses_missing")
                        .unwrap()
                        .is_none()
                );
                assert!(adapter.find_native_session("foreign_id").unwrap().is_none());
                assert!(
                    super::find_opencode_session_with_binary(&binary, "foreign_id", None).is_err()
                );
            }
            for id in [
                "ses_wrong",
                "ses_mismatch",
                "ses_malformed",
                "ses_unreachable",
                "ses_server",
            ] {
                assert!(adapter.find_native_session(id).is_err(), "{version} {id}");
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn v1_metadata_lookup_requires_an_exact_database_result() {
        use super::find_opencode_session_with_binary;
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("opencode");
        let id = "ses_test";
        assert!(find_opencode_session_with_binary(&binary, id, None).is_err());
        std::fs::write(
            &binary,
            r#"#!/bin/sh
if [ "$1" = --version ]; then echo 1.18.18; exit 0; fi
[ "$1" = --pure ] && [ "$2" = db ] || exit 1
cat ./rows
"#,
        )
        .unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        let _ = crate::test_support::output_after_write(
            std::process::Command::new(&binary).arg("--version"),
        );
        let rows = dir.path().join("rows");
        let query = || find_opencode_session_with_binary(&binary, id, Some(dir.path()));
        std::fs::write(&rows, r#"[{"id":"ses_test","directory":"/project"}]"#).unwrap();
        assert_eq!(query().unwrap().unwrap().session.id, id);
        std::fs::write(&rows, "[]").unwrap();
        assert!(query().unwrap().is_none());
        for invalid in [
            "",
            "{}",
            r#"[{"id":"ses_other"}]"#,
            r#"[{"sessionID":"ses_test"}]"#,
            "[{},{}]",
        ] {
            std::fs::write(&rows, invalid).unwrap();
            assert!(query().is_err(), "invalid rows: {invalid:?}");
        }
        assert!(
            find_opencode_session_with_binary(&binary, id, Some(&dir.path().join("missing")))
                .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn api_forks_keep_session_ids_in_one_path_segment() {
        use std::os::unix::fs::PermissionsExt;

        let temporary = tempfile::tempdir().expect("temporary OpenCode executable");
        let binary = temporary.path().join("opencode");
        for (session_id, endpoint) in [
            ("ses_normal-123", "/api/session/ses_normal-123/fork"),
            (
                "ses/../ses_target?query#fragment% é",
                "/api/session/ses%2F..%2Fses_target%3Fquery%23fragment%25%20%C3%A9/fork",
            ),
        ] {
            std::fs::write(
                &binary,
                format!(
                    "#!/bin/sh\nif [ \"$1\" = --version ]; then printf '%s\\n' 2.2.0; exit 0; fi\n[ \"$1\" = api ] && [ \"$2\" = --standalone ] && [ \"$3\" = POST ] && [ \"$4\" = '{endpoint}' ] && [ \"$5\" = -d ] && [ \"$6\" = '{{}}' ] || exit 1\nprintf '%s\\n' '{{\"id\":\"ses_child\"}}'\n"
                ),
            )
            .expect("write OpenCode executable");
            std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700))
                .expect("executable OpenCode permissions");
            // A busy executable fails the version probe, and the fork then sends OpenCode 1
            // arguments. Run the script once here, so that a busy executable is retried.
            let primed = crate::test_support::output_after_write(
                std::process::Command::new(&binary).arg("--version"),
            );
            assert!(primed.status.success(), "prime fake OpenCode");
            assert_eq!(
                super::fork_opencode_session_with_binary(&binary, session_id, None)
                    .expect("fork the literal session ID"),
                "ses_child"
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn unrecognized_batch_launchers_are_refused_on_windows() {
        use super::{
            fork_opencode_session_with_binary, installed_opencode_model_with_binary,
            is_opencode_v2, opencode_command_args, read_opencode_session_with_binary_at,
        };

        let temporary = tempfile::tempdir().expect("temporary OpenCode launcher");
        let launcher = temporary.path().join("opencode.cmd");
        std::fs::write(&launcher, "@echo off\r\necho {}\r\n").expect("batch launcher");
        let session = SessionRef::new(Provider::OpenCode, "ses_synthetic");

        assert!(!is_opencode_v2(&launcher));

        for error in [
            installed_opencode_model_with_binary(&launcher, temporary.path())
                .expect_err("model lookup must refuse batch launcher"),
            read_opencode_session_with_binary_at(&launcher, &session, Some(temporary.path()))
                .expect_err("export must refuse batch launcher"),
            fork_opencode_session_with_binary(&launcher, "ses_synthetic", Some(temporary.path()))
                .expect_err("fork must refuse batch launcher"),
            opencode_command_args(&launcher, &["models"])
                .expect_err("command args must refuse batch launcher"),
        ] {
            assert!(
                format!("{error:#}").contains("refusing to execute unrecognized batch provider"),
                "{error:#}"
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn npm_launcher_supports_discovery_and_read_commands_on_windows() {
        use super::{OpenCodeAdapter, installed_opencode_model_with_binary, is_opencode_v2};
        use crate::ProviderAdapter;

        let temporary = tempfile::tempdir().expect("temporary npm OpenCode installation");
        let package = temporary.path().join("node_modules/opencode-ai/bin");
        std::fs::create_dir_all(&package).expect("npm package directory");
        std::fs::write(
            package.join("opencode"),
            r#"#!/usr/bin/env node
const args = process.argv.slice(2);
if (args.length === 1 && args[0] === "--version") {
    console.log("2.2.0");
} else if (args.includes("models")) {
    console.log("synthetic/test-model");
} else if (args.includes("session.list")) {
    console.log(JSON.stringify([{ id: "ses_synthetic", directory: process.cwd() }]));
} else {
    process.exit(1);
}
"#,
        )
        .expect("Node OpenCode fixture");
        let launcher = temporary.path().join("opencode.cmd");
        std::fs::write(
            &launcher,
            "@ECHO off\r\nGOTO start\r\n:find_dp0\r\nSET dp0=%~dp0\r\nEXIT /b\r\n:start\r\nSETLOCAL\r\nCALL :find_dp0\r\nIF EXIST \"%dp0%\\node.exe\" (\r\nSET \"_prog=%dp0%\\node.exe\"\r\n) ELSE (\r\nSET \"_prog=node\"\r\n)\r\nendLocal & goto #_undefined_# 2>NUL || title %COMSPEC% & set PATHEXT=%PATHEXT:;.JS;=;% & \"%_prog%\" \"%dp0%\\node_modules\\opencode-ai\\bin\\opencode\" %*\r\n",
        )
        .expect("npm command shim");
        match crate::provider_process(&launcher) {
            Err(error) if format!("{error:#}").contains("real `node.exe` not found") => return,
            result => {
                result.expect("validate npm OpenCode launcher");
            }
        }
        let adapter = OpenCodeAdapter {
            binary: super::runnable_opencode_binary(launcher.clone()),
            notes: std::sync::Arc::default(),
        };

        assert!(adapter.probe().installed);
        assert!(is_opencode_v2(&launcher));
        let sessions = adapter
            .list_sessions(None)
            .expect("list through npm launcher");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session.id, "ses_synthetic");
        assert_eq!(
            installed_opencode_model_with_binary(&launcher, temporary.path())
                .expect("models through npm launcher"),
            ("synthetic".to_owned(), "test-model".to_owned())
        );
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
        // The adapter runs this script right after the test writes it. Run it once here, so a busy
        // executable is retried; the fake records nothing, so the assertions below don't change.
        let primed = crate::test_support::output_after_write(
            std::process::Command::new(&binary)
                .args(["--pure", "session", "list", "--format", "json"]),
        );
        assert!(primed.status.success(), "prime fake OpenCode");
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

    #[cfg(unix)]
    #[test]
    fn v1_forks_and_v2_requires_api_fork_before_launch() {
        use std::os::unix::fs::PermissionsExt;

        use crate::{LaunchTarget, ProviderAdapter};

        let temporary = tempfile::tempdir().expect("temporary OpenCode launchers");
        let v1_bin = temporary.path().join("opencode-v1");
        std::fs::write(&v1_bin, "#!/bin/sh\necho '1.18.18'\n").expect("v1 script");
        std::fs::set_permissions(&v1_bin, std::fs::Permissions::from_mode(0o755)).unwrap();

        let v2_bin = temporary.path().join("opencode-v2");
        std::fs::write(&v2_bin, "#!/bin/sh\necho 'opencode v2.2.0'\n").expect("v2 script");
        std::fs::set_permissions(&v2_bin, std::fs::Permissions::from_mode(0o755)).unwrap();

        // v1 launch plans: fork has --fork, prompt is positional
        let v1_adapter = super::OpenCodeAdapter {
            binary: Some(v1_bin),
            ..super::OpenCodeAdapter::default()
        };
        let session = SessionRef::new(Provider::OpenCode, "ses_test");
        let fork_target = LaunchTarget {
            fork: true,
            prompt: Some("do work".to_owned()),
            ..LaunchTarget::default()
        };
        let v1_plan = v1_adapter.launch_plan(&session, &fork_target).unwrap();
        assert_eq!(v1_plan.args, ["--session", "ses_test", "--fork", "do work"]);

        // v2 launch plans: refuses fork, prompt uses --prompt
        let v2_adapter = super::OpenCodeAdapter {
            binary: Some(v2_bin),
            ..super::OpenCodeAdapter::default()
        };
        assert!(v2_adapter.launch_plan(&session, &fork_target).is_err());
        let in_place_target = LaunchTarget {
            fork: false,
            prompt: Some("do work".to_owned()),
            ..LaunchTarget::default()
        };
        let v2_plan = v2_adapter.launch_plan(&session, &in_place_target).unwrap();
        assert_eq!(
            v2_plan.args,
            ["--session", "ses_test", "--prompt", "do work"]
        );
    }

    #[test]
    fn canonicalize_v2_export_payload() {
        let payload = serde_json::json!({
            "info": {
                "id": "ses_v2_canonical",
                "title": "V2 Session",
                "location": { "directory": "/workspace/v2_project" },
                "time": { "created": 1_767_225_600_000_i64, "updated": 1_767_229_200_000_i64 }
            },
            "messages": [
                {
                    "id": "msg_u1",
                    "type": "user",
                    "time": { "created": 1_767_225_600_000_i64 },
                    "text": "User prompt v2",
                    "files": []
                },
                {
                    "id": "msg_a1",
                    "type": "assistant",
                    "time": { "created": 1_767_225_601_000_i64 },
                    "model": { "id": "claude-3-7", "providerID": "anthropic" },
                    "content": [
                        { "type": "text", "text": "Assistant answer v2" },
                        { "type": "tool", "id": "t1", "name": "bash", "state": { "status": "completed" } }
                    ],
                    "tokens": { "input": 50, "output": 25, "reasoning": 0, "cache": { "read": 5, "write": 0 } }
                }
            ]
        });
        let session = SessionRef::new(Provider::OpenCode, "ses_v2_canonical");
        let snapshot =
            super::canonicalize_opencode_export(&session, &payload).expect("canonicalize v2");
        assert_eq!(snapshot.session.id, "ses_v2_canonical");
        assert_eq!(snapshot.title.as_deref(), Some("V2 Session"));
        assert_eq!(
            snapshot.workspace.root,
            std::path::Path::new("/workspace/v2_project")
        );
        assert_eq!(snapshot.events.len(), 4);
        assert_eq!(snapshot.events[0].kind, EventKind::MessageUser);
        assert_eq!(snapshot.events[0].payload["text"], "User prompt v2");
        assert_eq!(snapshot.events[1].kind, EventKind::ProviderEvent);
        assert_eq!(snapshot.events[2].kind, EventKind::MessageAssistant);
        assert_eq!(snapshot.events[2].payload["text"], "Assistant answer v2");
        assert_eq!(snapshot.events[3].kind, EventKind::ToolCompleted);
    }
}
