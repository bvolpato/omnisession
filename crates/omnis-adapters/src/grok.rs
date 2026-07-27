use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use omnis_ir::{EventKind, Provider, ReplayPolicy, SessionRef};
use serde_json::{Value, json};

use crate::{
    LaunchPlan, LaunchTarget, NativeSession, ProviderAdapter, ProviderInstallation,
    support::{
        EventBuilder, executable, json_lines, nested_files, parse_timestamp, paths_match,
        provider_file, provider_root, read_json, sort_sessions, sqlite_snapshot, string_at,
        validate_provider, value_at,
    },
};

#[derive(Clone, Debug)]
pub struct GrokAdapter {
    sessions_root: Option<PathBuf>,
}

impl GrokAdapter {
    #[must_use]
    pub fn with_root(sessions_root: impl Into<PathBuf>) -> Self {
        Self {
            sessions_root: Some(sessions_root.into()),
        }
    }

    fn summaries(&self) -> Vec<PathBuf> {
        self.sessions_root
            .as_deref()
            .map(|root| nested_files(root, 2, Some("summary.json")))
            .unwrap_or_default()
    }

    fn find_summary(&self, id: &str) -> Result<(Value, PathBuf)> {
        self.direct_summary(id)
            .into_iter()
            .chain(self.summaries())
            .find_map(|path| {
                let value = read_json(&path).ok()?;
                (session_id(&value, &path).as_deref() == Some(id)).then_some((value, path))
            })
            .ok_or_else(|| anyhow!("Grok session `{id}` was not found"))
    }

    fn direct_summary(&self, id: &str) -> Option<PathBuf> {
        uuid::Uuid::parse_str(id).ok()?;
        let root = self.sessions_root.as_deref()?;
        let entries = fs::read_dir(root).ok()?;
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_dir() || file_type.is_symlink() {
                continue;
            }
            let candidate = entry.path().join(id).join("summary.json");
            if let Some(candidate) = provider_file(root, &candidate) {
                return Some(candidate);
            }
        }
        None
    }

    fn catalog_sessions(&self, project: Option<&Path>) -> Option<Vec<NativeSession>> {
        let root = self.sessions_root.as_deref()?;
        let database = provider_file(root, &root.join("session_search.sqlite"))?;
        let snapshot = sqlite_snapshot(root, &database).ok()?;
        let mut statement = snapshot
            .connection
            .prepare("SELECT session_id, cwd, updated_at, title FROM session_docs")
            .ok()?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            })
            .ok()?;
        let mut sessions = Vec::new();
        for row in rows.flatten() {
            let (id, cwd, updated_at, title) = row;
            if uuid::Uuid::parse_str(&id).is_err() {
                continue;
            }
            let project_path = PathBuf::from(cwd);
            if project.is_some_and(|requested| !paths_match(&project_path, requested)) {
                continue;
            }
            sessions.push(NativeSession {
                session: SessionRef::new(Provider::Grok, id),
                title,
                project_path: Some(project_path),
                git_branch: None,
                created_at: None,
                updated_at: DateTime::from_timestamp(updated_at, 0),
                event_count: 0,
                source_path: None,
            });
        }
        sort_sessions(&mut sessions);
        Some(sessions)
    }
}

impl Default for GrokAdapter {
    fn default() -> Self {
        Self {
            sessions_root: provider_root("GROK_HOME", &[".grok"]).map(|root| root.join("sessions")),
        }
    }
}

#[derive(Default)]
struct GrokMetadata {
    title: Option<String>,
    project_path: Option<PathBuf>,
    git_branch: Option<String>,
    created_at: Option<DateTime<Utc>>,
    updated_at: Option<DateTime<Utc>>,
}

type PendingMessage = (EventKind, String, Option<DateTime<Utc>>, Option<String>);

fn session_id(summary: &Value, path: &Path) -> Option<String> {
    string_at(
        summary,
        &[&["id"], &["session_id"], &["sessionId"], &["session", "id"]],
    )
    .map(str::to_owned)
    .or_else(|| {
        path.parent()?
            .file_name()?
            .to_str()
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
    })
}

fn metadata(summary: &Value) -> GrokMetadata {
    GrokMetadata {
        title: string_at(
            summary,
            &[
                &["generated_title"],
                &["session_summary"],
                &["title"],
                &["name"],
                &["session", "title"],
                &["metadata", "title"],
            ],
        )
        .map(str::to_owned),
        project_path: string_at(
            summary,
            &[
                &["info", "cwd"],
                &["cwd"],
                &["project_path"],
                &["projectPath"],
                &["session", "cwd"],
                &["metadata", "cwd"],
            ],
        )
        .map(PathBuf::from),
        git_branch: string_at(
            summary,
            &[
                &["git_branch"],
                &["gitBranch"],
                &["git", "branch"],
                &["metadata", "git_branch"],
            ],
        )
        .map(str::to_owned),
        created_at: parse_timestamp(value_at(
            summary,
            &[
                &["created_at"],
                &["createdAt"],
                &["created"],
                &["time", "created"],
            ],
        )),
        updated_at: parse_timestamp(value_at(
            summary,
            &[
                &["updated_at"],
                &["updatedAt"],
                &["updated"],
                &["time", "updated"],
            ],
        )),
    }
}

fn push_updates(builder: &mut EventBuilder, updates: &[Value]) {
    let mut pending: Option<PendingMessage> = None;
    for record in updates {
        let timestamp = parse_timestamp(value_at(
            record,
            &[&["timestamp"], &["created_at"], &["createdAt"]],
        ));
        let update = value_at(record, &[&["params", "update"]]).unwrap_or(record);
        let raw_type =
            string_at(update, &[&["sessionUpdate"], &["type"], &["event"]]).map(str::to_owned);
        let update_type = raw_type.as_deref().unwrap_or("");
        let message_kind = match update_type {
            "user_message_chunk" | "user" | "user_message" => Some(EventKind::MessageUser),
            "agent_message_chunk" | "assistant" | "assistant_message" => {
                Some(EventKind::MessageAssistant)
            }
            _ => None,
        };
        if let Some(kind) = message_kind {
            let text = string_at(
                update,
                &[
                    &["content", "text"],
                    &["content"],
                    &["text"],
                    &["message", "content"],
                    &["message", "text"],
                ],
            )
            .unwrap_or("");
            match &mut pending {
                Some((pending_kind, pending_text, _, _)) if *pending_kind == kind => {
                    pending_text.push_str(text);
                }
                _ => {
                    flush_message(builder, &mut pending);
                    pending = Some((kind, text.to_owned(), timestamp, raw_type));
                }
            }
            continue;
        }

        flush_message(builder, &mut pending);
        let kind = match update_type {
            "tool_call" | "tool_use" => Some(EventKind::ToolCalled),
            "tool_call_update" | "tool_result" | "tool_completed" => {
                let failed = string_at(update, &[&["status"]])
                    .is_some_and(|status| matches!(status, "failed" | "error"));
                Some(if failed {
                    EventKind::ToolFailed
                } else {
                    EventKind::ToolCompleted
                })
            }
            "tool_error" | "tool_failed" => Some(EventKind::ToolFailed),
            "command" | "command_executed" => Some(EventKind::CommandExecuted),
            _ => None,
        };
        if let Some(kind) = kind {
            builder.push(
                kind,
                json!({
                    "id": value_at(update, &[&["toolCallId"], &["id"]]),
                    "name": string_at(update, &[&["title"], &["name"]]),
                    "status": string_at(update, &[&["status"]]),
                }),
                timestamp,
                ReplayPolicy::HistoricalOnly,
                raw_type,
                None,
            );
        }
    }
    flush_message(builder, &mut pending);
}

fn flush_message(builder: &mut EventBuilder, pending: &mut Option<PendingMessage>) {
    let Some((kind, text, timestamp, raw_type)) = pending.take() else {
        return;
    };
    if !text.is_empty() {
        builder.push(
            kind,
            json!({ "text": text }),
            timestamp,
            ReplayPolicy::Contextual,
            raw_type,
            None,
        );
    }
}

impl ProviderAdapter for GrokAdapter {
    fn provider(&self) -> Provider {
        Provider::Grok
    }

    fn probe(&self) -> ProviderInstallation {
        ProviderInstallation {
            provider: Provider::Grok,
            installed: executable("grok").is_some()
                || self.sessions_root.as_deref().is_some_and(Path::is_dir),
            executable: executable("grok"),
            data_root: self.sessions_root.clone(),
        }
    }

    fn list_sessions(&self, project: Option<&Path>) -> Result<Vec<NativeSession>> {
        if let Some(sessions) = self.catalog_sessions(project) {
            return Ok(sessions);
        }
        let mut sessions = Vec::new();
        for path in self.summaries() {
            let summary: Value = match read_json(&path) {
                Ok(summary) => summary,
                Err(_) => continue,
            };
            let Some(id) = session_id(&summary, &path) else {
                continue;
            };
            let metadata = metadata(&summary);
            if project.is_some_and(|project| {
                metadata
                    .project_path
                    .as_deref()
                    .is_none_or(|recorded| !paths_match(recorded, project))
            }) {
                continue;
            }
            sessions.push(NativeSession {
                session: SessionRef::new(Provider::Grok, id),
                title: metadata.title,
                project_path: metadata.project_path,
                git_branch: metadata.git_branch,
                created_at: metadata.created_at,
                updated_at: metadata.updated_at,
                event_count: 0,
                source_path: Some(path),
            });
        }
        sort_sessions(&mut sessions);
        Ok(sessions)
    }

    fn read_session(&self, session: &SessionRef) -> Result<omnis_ir::CanonicalSnapshot> {
        validate_provider(session, Provider::Grok)?;
        let (summary, path) = self.find_summary(&session.id)?;
        let metadata = metadata(&summary);
        let updates_path = path
            .parent()
            .context("Grok summary has no parent directory")?
            .join("updates.jsonl");
        let updates = if updates_path.is_file() {
            json_lines(&updates_path)?
        } else {
            Vec::new()
        };
        let captured_at = metadata.updated_at.unwrap_or_else(Utc::now);
        let mut builder = EventBuilder::new(Provider::Grok, &session.id);
        push_updates(&mut builder, &updates);
        Ok(builder.snapshot(
            session.clone(),
            metadata.title,
            metadata.project_path,
            metadata.git_branch,
            captured_at,
        ))
    }

    fn new_session_plan(&self, target: &LaunchTarget) -> Result<LaunchPlan> {
        Ok(LaunchPlan {
            program: "grok".to_owned(),
            args: target.prompt.iter().cloned().collect(),
            cwd: target.cwd.clone(),
        })
    }

    fn launch_plan(&self, session: &SessionRef, target: &LaunchTarget) -> Result<LaunchPlan> {
        validate_provider(session, Provider::Grok)?;
        let mut args = vec!["--resume".to_owned(), session.id.clone()];
        if target.fork {
            args.push("--fork-session".to_owned());
        }
        if let Some(prompt) = &target.prompt {
            args.push(prompt.clone());
        }
        Ok(LaunchPlan {
            program: "grok".to_owned(),
            args,
            cwd: target.cwd.clone(),
        })
    }
}
