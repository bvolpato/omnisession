use std::{
    io::{Read, Seek},
    path::Path,
    path::PathBuf,
    process::{Command, Stdio},
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
        EventBuilder, executable, parse_timestamp, paths_match, sort_sessions, string_at,
        validate_provider, value_at,
    },
};

#[derive(Clone, Copy, Debug, Default)]
pub struct OpenCodeAdapter;

#[derive(Default)]
struct OpenCodeMetadata {
    id: Option<String>,
    title: Option<String>,
    project_path: Option<PathBuf>,
    git_branch: Option<String>,
    created_at: Option<DateTime<Utc>>,
    updated_at: Option<DateTime<Utc>>,
}

fn command_json(arguments: &[&str], cwd: Option<&Path>) -> Result<Value> {
    const MAX_OUTPUT_SIZE: u64 = 128 * 1024 * 1024;
    let mut output_file = tempfile::tempfile().context("creating OpenCode output buffer")?;
    let mut command = Command::new("opencode");
    command
        .arg("--pure")
        .args(arguments)
        .stdout(Stdio::from(output_file.try_clone()?))
        .stderr(Stdio::null());
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("failed to execute `opencode {}`", arguments.join(" ")))?;
    let Some(status) = child.wait_timeout(Duration::from_secs(30))? else {
        child
            .kill()
            .context("stopping timed-out OpenCode command")?;
        child.wait().context("reaping timed-out OpenCode command")?;
        return Err(anyhow!("`opencode {}` timed out", arguments.join(" ")));
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
    parse_command_json(&output, arguments.first() == Some(&"session"))
}

/// Finds one model identifier accepted by installed `OpenCode` CLI.
///
/// Imported historical messages require model metadata even though next turn
/// uses user's current target selection.
///
/// # Errors
///
/// Returns process, timeout, output-limit, or malformed model-list errors.
pub fn installed_opencode_model(cwd: &Path) -> Result<(String, String)> {
    const MAX_OUTPUT_SIZE: u64 = 8 * 1024 * 1024;
    let mut output_file = tempfile::tempfile().context("creating OpenCode model buffer")?;
    let mut child = Command::new("opencode")
        .args(["--pure", "models"])
        .current_dir(cwd)
        .stdout(Stdio::from(output_file.try_clone()?))
        .stderr(Stdio::null())
        .spawn()
        .context("failed to execute `opencode models`")?;
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

fn session_directory(id: &str) -> Option<PathBuf> {
    let sessions = command_json(&["session", "list", "--format", "json"], None).ok()?;
    session_values(&sessions)
        .iter()
        .find_map(|value| {
            let metadata = metadata(value);
            (metadata.id.as_deref() == Some(id)).then_some(metadata.project_path)?
        })
        .filter(|path| path.is_dir())
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
                &["time", "created"],
                &["info", "time", "created"],
            ],
        )),
        updated_at: parse_timestamp(value_at(
            value,
            &[
                &["updated_at"],
                &["updatedAt"],
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
        if let Some(text) = message.get("content").and_then(Value::as_str) {
            if let Some(kind) = message_kind.clone().filter(|_| !text.is_empty()) {
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
        for part in parts {
            match part.get("type").and_then(Value::as_str) {
                Some("text") => {
                    let Some(kind) = message_kind.clone() else {
                        continue;
                    };
                    let Some(text) = part
                        .get("text")
                        .and_then(Value::as_str)
                        .filter(|text| !text.is_empty())
                    else {
                        continue;
                    };
                    builder.push(
                        kind,
                        json!({ "text": text }),
                        timestamp,
                        ReplayPolicy::Contextual,
                        Some("text".to_owned()),
                        None,
                    );
                }
                Some("tool" | "tool_call" | "tool_result") => {
                    let failed = string_at(part, &[&["state", "status"], &["status"]])
                        .is_some_and(|status| matches!(status, "error" | "failed"));
                    let completed = string_at(part, &[&["state", "status"], &["status"]])
                        .is_some_and(|status| matches!(status, "completed" | "success"));
                    let kind = if failed {
                        EventKind::ToolFailed
                    } else if completed
                        || part.get("type").and_then(Value::as_str) == Some("tool_result")
                    {
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
                _ => {}
            }
        }
    }
}

impl ProviderAdapter for OpenCodeAdapter {
    fn provider(&self) -> Provider {
        Provider::OpenCode
    }

    fn probe(&self) -> ProviderInstallation {
        ProviderInstallation {
            provider: Provider::OpenCode,
            installed: executable("opencode").is_some(),
            executable: executable("opencode"),
            data_root: None,
        }
    }

    fn list_sessions(&self, project: Option<&Path>) -> Result<Vec<NativeSession>> {
        let value = command_json(&["session", "list", "--format", "json"], project)?;
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
                event_count,
                source_path: None,
            });
        }
        sort_sessions(&mut sessions);
        Ok(sessions)
    }

    fn read_session(&self, session: &SessionRef) -> Result<omnis_ir::CanonicalSnapshot> {
        validate_provider(session, Provider::OpenCode)?;
        let cwd = session_directory(&session.id).or_else(|| std::env::current_dir().ok());
        let export = command_json(&["export", &session.id], cwd.as_deref())?;
        let metadata = metadata(&export);
        if metadata.id.as_deref().is_some_and(|id| id != session.id) {
            return Err(anyhow!(
                "OpenCode exported a different session than `{}`",
                session.id
            ));
        }
        let captured_at = metadata.updated_at.unwrap_or_else(Utc::now);
        let mut builder = EventBuilder::new(Provider::OpenCode, &session.id);
        push_export_events(&mut builder, &export);
        Ok(builder.snapshot(
            session.clone(),
            metadata.title,
            metadata.project_path,
            metadata.git_branch,
            captured_at,
        ))
    }

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
}
