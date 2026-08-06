use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use omnis_ir::{EventKind, Provider, ReplayPolicy, SessionRef};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

use crate::{
    LaunchPlan, LaunchTarget, NativeSession, ProviderAdapter, ProviderInstallation,
    support::{
        EventBuilder, executable, paths_match, provider_file, provider_root, sort_sessions,
        sqlite_snapshot, validate_provider,
    },
};

const MAX_MESSAGES: usize = 100_000;
const PREVIEW_MESSAGES: usize = 1_024;

/// Read-only adapter for Hermes Agent's documented `SQLite` session store.
#[derive(Clone, Debug)]
pub struct HermesAdapter {
    root: Option<PathBuf>,
}

impl HermesAdapter {
    #[must_use]
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        Self {
            root: Some(root.into()),
        }
    }

    fn database(&self) -> Result<PathBuf> {
        let root = self
            .root
            .as_deref()
            .context("Hermes data root was not found")?;
        provider_file(root, &root.join("state.db"))
            .ok_or_else(|| anyhow!("Hermes state database was not found"))
    }

    fn snapshot(&self) -> Result<crate::support::SqliteSnapshot> {
        let root = self
            .root
            .as_deref()
            .context("Hermes data root was not found")?;
        sqlite_snapshot(root, &self.database()?).context("failed to snapshot Hermes state database")
    }

    fn read_snapshot(
        &self,
        session: &SessionRef,
        message_limit: Option<usize>,
    ) -> Result<omnis_ir::CanonicalSnapshot> {
        validate_provider(session, Provider::Hermes)?;
        let snapshot = self.snapshot()?;
        validate_schema(&snapshot.connection)?;
        let metadata = session_metadata(&snapshot.connection, &session.id)?
            .with_context(|| format!("Hermes session `{}` was not found", session.id))?;
        let messages = session_messages(&snapshot.connection, &session.id, message_limit)?;
        if messages.is_empty() && metadata.message_count > 0 {
            bail!(
                "Hermes session `{}` declares history but has no active readable messages",
                session.id
            );
        }

        let captured_at = messages
            .last()
            .and_then(|message| timestamp(message.timestamp))
            .or(metadata.ended_at)
            .unwrap_or(metadata.started_at);
        let mut builder = EventBuilder::new(Provider::Hermes, &session.id);
        builder.set_provider_version(Some(format!(
            "schema-{}",
            schema_version(&snapshot.connection)?
        )));
        push_session_metadata(&mut builder, &metadata);
        for message in &messages {
            push_message(&mut builder, message);
        }
        Ok(builder.snapshot(
            session.clone(),
            metadata.title,
            metadata.cwd,
            metadata.git_branch,
            captured_at,
        ))
    }
}

impl Default for HermesAdapter {
    fn default() -> Self {
        Self {
            root: provider_root("HERMES_HOME", &[".hermes"]),
        }
    }
}

impl ProviderAdapter for HermesAdapter {
    fn provider(&self) -> Provider {
        Provider::Hermes
    }

    fn probe(&self) -> ProviderInstallation {
        let executable = executable("hermes");
        ProviderInstallation {
            provider: Provider::Hermes,
            installed: executable.is_some() || self.database().is_ok(),
            executable,
            data_root: self.root.clone(),
        }
    }

    fn list_sessions(&self, project: Option<&Path>) -> Result<Vec<NativeSession>> {
        if self.database().is_err() {
            return Ok(Vec::new());
        }
        let snapshot = self.snapshot()?;
        validate_schema(&snapshot.connection)?;
        let mut statement = snapshot.connection.prepare(
            "SELECT s.id, s.title, s.cwd, s.git_branch, s.started_at, s.ended_at, \
                    s.message_count, \
                    (SELECT MAX(m.timestamp) FROM messages m \
                     WHERE m.session_id = s.id AND m.active = 1), \
                    (SELECT m.content FROM messages m \
                     WHERE m.session_id = s.id AND m.active = 1 AND m.role = 'user' \
                       AND m.content IS NOT NULL AND TRIM(m.content) != '' \
                     ORDER BY m.id LIMIT 1) \
             FROM sessions s \
             WHERE COALESCE(s.archived, 0) = 0 \
             ORDER BY COALESCE((SELECT MAX(m.timestamp) FROM messages m \
                                WHERE m.session_id = s.id AND m.active = 1), \
                               s.ended_at, s.started_at) DESC, s.id",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(SessionMetadata {
                id: row.get(0)?,
                title: row.get(1)?,
                cwd: row.get::<_, Option<String>>(2)?.map(PathBuf::from),
                git_branch: row.get(3)?,
                started_at: timestamp(row.get(4)?).unwrap_or(DateTime::<Utc>::UNIX_EPOCH),
                ended_at: row.get::<_, Option<f64>>(5)?.and_then(timestamp),
                message_count: usize::try_from(row.get::<_, i64>(6)?.max(0)).unwrap_or_default(),
                updated_at: row.get::<_, Option<f64>>(7)?.and_then(timestamp),
                first_user_message: row.get(8)?,
                ..SessionMetadata::default()
            })
        })?;

        let database = self.database()?;
        let mut sessions = Vec::new();
        for row in rows {
            let metadata = row?;
            if metadata.id.is_empty()
                || project.is_some_and(|requested| {
                    metadata
                        .cwd
                        .as_deref()
                        .is_none_or(|recorded| !paths_match(recorded, requested))
                })
            {
                continue;
            }
            let title = metadata
                .title
                .filter(|title| !title.trim().is_empty())
                .or_else(|| {
                    metadata
                        .first_user_message
                        .as_deref()
                        .and_then(content_text)
                })
                .map(|title| one_line(&title, 200));
            sessions.push(NativeSession {
                session: SessionRef::new(Provider::Hermes, metadata.id),
                title,
                project_path: metadata.cwd,
                git_branch: metadata.git_branch,
                created_at: Some(metadata.started_at),
                updated_at: metadata
                    .updated_at
                    .or(metadata.ended_at)
                    .or(Some(metadata.started_at)),
                updated_at_approximate: false,
                event_count: metadata.message_count,
                source_path: Some(database.clone()),
            });
        }
        sort_sessions(&mut sessions);
        Ok(sessions)
    }

    fn read_session(&self, session: &SessionRef) -> Result<omnis_ir::CanonicalSnapshot> {
        self.read_snapshot(session, None)
    }

    fn preview_session(&self, session: &SessionRef) -> Result<omnis_ir::CanonicalSnapshot> {
        self.read_snapshot(session, Some(PREVIEW_MESSAGES))
    }

    fn new_session_plan(&self, target: &LaunchTarget) -> Result<LaunchPlan> {
        Ok(LaunchPlan {
            program: "hermes".to_owned(),
            args: target.prompt.iter().cloned().collect(),
            cwd: target.cwd.clone(),
        })
    }

    fn launch_plan(&self, session: &SessionRef, target: &LaunchTarget) -> Result<LaunchPlan> {
        validate_provider(session, Provider::Hermes)?;
        if target.fork {
            bail!("Hermes native fork requires a materialized child session")
        }
        let mut args = vec!["--resume".to_owned(), session.id.clone()];
        args.extend(target.prompt.iter().cloned());
        Ok(LaunchPlan {
            program: "hermes".to_owned(),
            args,
            cwd: target.cwd.clone(),
        })
    }
}

#[derive(Default)]
struct SessionMetadata {
    id: String,
    title: Option<String>,
    cwd: Option<PathBuf>,
    git_branch: Option<String>,
    started_at: DateTime<Utc>,
    ended_at: Option<DateTime<Utc>>,
    updated_at: Option<DateTime<Utc>>,
    message_count: usize,
    first_user_message: Option<String>,
    source: Option<String>,
    model: Option<String>,
    parent_session_id: Option<String>,
    omnisession_source: Option<String>,
    branched_from: Option<String>,
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
    reasoning_tokens: u64,
}

fn session_metadata(connection: &Connection, id: &str) -> Result<Option<SessionMetadata>> {
    connection
        .query_row(
            "SELECT id, title, cwd, git_branch, started_at, ended_at, message_count, source, \
                    model, parent_session_id, input_tokens, output_tokens, cache_read_tokens, \
                    cache_write_tokens, reasoning_tokens, model_config \
             FROM sessions WHERE id = ?1",
            [id],
            |row| {
                let parent_session_id = row.get::<_, Option<String>>(9)?;
                let model_config = row.get::<_, Option<String>>(15)?;
                let (omnisession_source, branched_from) =
                    imported_lineage(model_config.as_deref(), parent_session_id.as_deref());
                Ok(SessionMetadata {
                    id: row.get(0)?,
                    title: row.get(1)?,
                    cwd: row.get::<_, Option<String>>(2)?.map(PathBuf::from),
                    git_branch: row.get(3)?,
                    started_at: timestamp(row.get(4)?).unwrap_or(DateTime::<Utc>::UNIX_EPOCH),
                    ended_at: row.get::<_, Option<f64>>(5)?.and_then(timestamp),
                    message_count: usize::try_from(row.get::<_, i64>(6)?.max(0))
                        .unwrap_or_default(),
                    source: row.get(7)?,
                    model: row.get(8)?,
                    parent_session_id,
                    omnisession_source,
                    branched_from,
                    input_tokens: nonnegative(row.get(10)?),
                    output_tokens: nonnegative(row.get(11)?),
                    cache_read_tokens: nonnegative(row.get(12)?),
                    cache_write_tokens: nonnegative(row.get(13)?),
                    reasoning_tokens: nonnegative(row.get(14)?),
                    ..SessionMetadata::default()
                })
            },
        )
        .optional()
        .map_err(Into::into)
}

struct MessageRow {
    id: i64,
    role: String,
    content: Option<String>,
    tool_call_id: Option<String>,
    tool_calls: Option<String>,
    tool_name: Option<String>,
    effect_disposition: Option<String>,
    timestamp: f64,
    finish_reason: Option<String>,
}

fn session_messages(
    connection: &Connection,
    session_id: &str,
    limit: Option<usize>,
) -> Result<Vec<MessageRow>> {
    let selected = "SELECT id, role, content, tool_call_id, tool_calls, tool_name, \
                           effect_disposition, timestamp, finish_reason \
                    FROM messages WHERE session_id = ?1 AND active = 1";
    let sql = if limit.is_some() {
        format!("SELECT * FROM ({selected} ORDER BY id DESC LIMIT ?2) ORDER BY id")
    } else {
        format!("{selected} ORDER BY id LIMIT ?2")
    };
    let limit = limit.unwrap_or(MAX_MESSAGES + 1);
    let mut statement = connection.prepare(&sql)?;
    let rows = statement.query_map(params![session_id, i64::try_from(limit)?], |row| {
        Ok(MessageRow {
            id: row.get(0)?,
            role: row.get(1)?,
            content: row.get(2)?,
            tool_call_id: row.get(3)?,
            tool_calls: row.get(4)?,
            tool_name: row.get(5)?,
            effect_disposition: row.get(6)?,
            timestamp: row.get(7)?,
            finish_reason: row.get(8)?,
        })
    })?;
    let messages = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    if limit > MAX_MESSAGES && messages.len() > MAX_MESSAGES {
        bail!("Hermes session exceeds safe message limit")
    }
    Ok(messages)
}

fn push_session_metadata(builder: &mut EventBuilder, metadata: &SessionMetadata) {
    let total_tokens = metadata
        .input_tokens
        .saturating_add(metadata.output_tokens)
        .saturating_add(metadata.cache_read_tokens)
        .saturating_add(metadata.cache_write_tokens)
        .saturating_add(metadata.reasoning_tokens);
    builder.push(
        EventKind::ProviderEvent,
        json!({
            "model": metadata.model,
            "source": metadata.source,
            "parent_session_id": metadata.parent_session_id,
            "omnisession_source": metadata.omnisession_source,
            "branched_from": metadata.branched_from,
            "total_tokens": total_tokens,
            "input_tokens": metadata.input_tokens,
            "output_tokens": metadata.output_tokens,
            "cache_read_tokens": metadata.cache_read_tokens,
            "cache_write_tokens": metadata.cache_write_tokens,
            "reasoning_tokens": metadata.reasoning_tokens,
            "token_usage": "cumulative",
        }),
        Some(metadata.started_at),
        ReplayPolicy::HistoricalOnly,
        Some("omnisession.session_metadata".to_owned()),
        None,
    );
}

fn imported_lineage(
    model_config: Option<&str>,
    parent_session_id: Option<&str>,
) -> (Option<String>, Option<String>) {
    let Some(config) = model_config
        .and_then(|value| serde_json::from_str::<Value>(value).ok())
        .and_then(|value| value.as_object().cloned())
    else {
        return (None, None);
    };
    let source = config
        .get("omnisession_source")
        .and_then(Value::as_str)
        .and_then(|value| value.parse::<SessionRef>().ok())
        .map(|session| session.to_string());
    let branched_from = config
        .get("_branched_from")
        .and_then(Value::as_str)
        .filter(|value| Some(*value) == parent_session_id)
        .map(str::to_owned);
    (source, branched_from)
}

fn push_message(builder: &mut EventBuilder, message: &MessageRow) {
    let timestamp = timestamp(message.timestamp);
    let text = message.content.as_deref().and_then(content_text);
    match message.role.as_str() {
        "user" if text.as_deref().is_some_and(|text| !text.is_empty()) => builder.push(
            EventKind::MessageUser,
            json!({ "text": text }),
            timestamp,
            ReplayPolicy::Contextual,
            Some("user".to_owned()),
            None,
        ),
        "assistant" => {
            if text.as_deref().is_some_and(|text| !text.is_empty()) {
                builder.push(
                    EventKind::MessageAssistant,
                    json!({ "text": text }),
                    timestamp,
                    ReplayPolicy::Contextual,
                    Some("assistant".to_owned()),
                    None,
                );
            }
            if let Some(tool_calls) = message.tool_calls.as_deref().and_then(parse_json) {
                for call in tool_calls.as_array().into_iter().flatten() {
                    builder.push(
                        EventKind::ToolCalled,
                        call.clone(),
                        timestamp,
                        ReplayPolicy::HistoricalOnly,
                        Some("assistant.tool_call".to_owned()),
                        None,
                    );
                }
            }
        }
        "tool" => {
            let failed = message
                .effect_disposition
                .as_deref()
                .or(message.finish_reason.as_deref())
                .is_some_and(|status| {
                    let status = status.to_ascii_lowercase();
                    status.contains("fail") || status.contains("error") || status.contains("deny")
                });
            builder.push(
                if failed {
                    EventKind::ToolFailed
                } else {
                    EventKind::ToolCompleted
                },
                json!({
                    "call_id": message.tool_call_id,
                    "name": message.tool_name,
                    "output": text,
                    "status": message.effect_disposition,
                }),
                timestamp,
                ReplayPolicy::HistoricalOnly,
                Some("tool".to_owned()),
                None,
            );
        }
        _ => builder.push(
            EventKind::ProviderEvent,
            json!({ "row_id": message.id, "role": message.role }),
            timestamp,
            ReplayPolicy::HistoricalOnly,
            Some("unknown_message_role".to_owned()),
            None,
        ),
    }
}

fn content_text(content: &str) -> Option<String> {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return None;
    }
    let Some(value) = parse_json(trimmed) else {
        return Some(content.to_owned());
    };
    let mut text = Vec::new();
    collect_text(&value, &mut text);
    (!text.is_empty())
        .then(|| text.join("\n"))
        .or_else(|| Some(content.to_owned()))
}

fn collect_text(value: &Value, text: &mut Vec<String>) {
    match value {
        Value::Array(values) => values.iter().for_each(|value| collect_text(value, text)),
        Value::Object(object) => {
            if let Some(value) = object.get("text").and_then(Value::as_str) {
                text.push(value.to_owned());
            } else if let Some(value) = object.get("content") {
                collect_text(value, text);
            }
        }
        Value::String(value) => text.push(value.clone()),
        _ => {}
    }
}

fn parse_json(value: &str) -> Option<Value> {
    matches!(value.trim_start().as_bytes().first(), Some(b'[' | b'{'))
        .then(|| serde_json::from_str(value).ok())
        .flatten()
}

fn timestamp(value: f64) -> Option<DateTime<Utc>> {
    let duration = Duration::try_from_secs_f64(value).ok()?;
    Some(DateTime::from(
        SystemTime::UNIX_EPOCH.checked_add(duration)?,
    ))
}

fn nonnegative(value: i64) -> u64 {
    u64::try_from(value).unwrap_or_default()
}

fn one_line(value: &str, limit: usize) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(limit)
        .collect()
}

fn schema_version(connection: &Connection) -> Result<i64> {
    connection
        .query_row("SELECT version FROM schema_version LIMIT 1", [], |row| {
            row.get(0)
        })
        .context("Hermes schema version was not readable")
}

fn validate_schema(connection: &Connection) -> Result<()> {
    for (table, required) in [
        (
            "sessions",
            &[
                "id",
                "source",
                "title",
                "cwd",
                "git_branch",
                "started_at",
                "ended_at",
                "message_count",
                "model",
                "model_config",
                "parent_session_id",
                "input_tokens",
                "output_tokens",
                "cache_read_tokens",
                "cache_write_tokens",
                "reasoning_tokens",
                "archived",
            ][..],
        ),
        (
            "messages",
            &[
                "id",
                "session_id",
                "role",
                "content",
                "tool_call_id",
                "tool_calls",
                "tool_name",
                "effect_disposition",
                "timestamp",
                "finish_reason",
                "active",
            ][..],
        ),
    ] {
        let mut statement = connection.prepare(&format!("PRAGMA table_info({table})"))?;
        let columns = statement
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<rusqlite::Result<HashSet<_>>>()?;
        if let Some(column) = required.iter().find(|column| !columns.contains(**column)) {
            bail!("Hermes database is missing required `{table}.{column}` column")
        }
    }
    let _ = schema_version(connection)?;
    Ok(())
}
