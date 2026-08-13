use std::{
    collections::{HashMap, HashSet},
    env, fs,
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use omnis_ir::{EventKind, Provider, ReplayPolicy, SessionRef};
use serde_json::{Value, json};

use crate::{
    LaunchPlan, LaunchTarget, NativeSession, ProviderAdapter, ProviderInstallation,
    support::{
        EventBuilder, executable, json_lines, json_lines_preview, nested_files, parse_timestamp,
        paths_match, provider_root, sort_sessions, string_at, validate_provider,
    },
};

const PI_SESSION_VERSION: u64 = 3;
const PREVIEW_RECORDS: usize = 1_024;
const MAX_HEADER_SCAN_BYTES: u64 = 1024 * 1024;
const MAX_HEADER_LINE_BYTES: u64 = 64 * 1024;

/// Read-only Pi coding-agent session adapter.
#[derive(Clone, Debug)]
pub struct PiAdapter {
    sessions_root: Option<PathBuf>,
}

impl PiAdapter {
    #[must_use]
    pub fn with_root(sessions_root: impl Into<PathBuf>) -> Self {
        Self {
            sessions_root: Some(sessions_root.into()),
        }
    }

    fn session_files(&self) -> Vec<PathBuf> {
        self.sessions_root
            .as_deref()
            .map(|root| {
                nested_files(root, 1, None)
                    .into_iter()
                    .filter(|path| {
                        path.extension()
                            .is_some_and(|extension| extension.eq_ignore_ascii_case("jsonl"))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn find_session(&self, id: &str) -> Result<PathBuf> {
        self.session_files()
            .into_iter()
            .find(|path| read_header(path).is_ok_and(|header| header.id == id))
            .ok_or_else(|| anyhow!("Pi session `{id}` was not found"))
    }
}

impl Default for PiAdapter {
    fn default() -> Self {
        let sessions_root = env::var_os("PI_CODING_AGENT_SESSION_DIR")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                provider_root("PI_CODING_AGENT_DIR", &[".pi", "agent"])
                    .map(|root| root.join("sessions"))
            });
        Self { sessions_root }
    }
}

#[derive(Clone, Debug)]
struct PiHeader {
    id: String,
    cwd: PathBuf,
    timestamp: Option<DateTime<Utc>>,
}

fn header(records: &[Value]) -> Result<PiHeader> {
    let record = records
        .first()
        .context("Pi session contains no valid JSONL records")?;
    header_record(record)
}

fn header_record(record: &Value) -> Result<PiHeader> {
    if record.get("type").and_then(Value::as_str) != Some("session") {
        bail!("Pi session did not start with a session header");
    }
    if record.get("version").and_then(Value::as_u64) != Some(PI_SESSION_VERSION) {
        bail!("Pi session is not supported session format v{PI_SESSION_VERSION}");
    }
    let id = record
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .context("Pi session header omitted ID")?
        .to_owned();
    let cwd = record
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|cwd| !cwd.is_empty())
        .context("Pi session header omitted working directory")?;
    Ok(PiHeader {
        id,
        cwd: PathBuf::from(cwd),
        timestamp: parse_timestamp(record.get("timestamp")),
    })
}

/// Mirrors Pi's bounded header discovery: skip malformed leading lines, then
/// require first parsed entry to be a v3 session header. Discovery never scans
/// a full transcript.
fn read_header(path: &Path) -> Result<PiHeader> {
    let file = fs::File::open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        bail!("Pi session path is not a regular file");
    }
    let mut reader = BufReader::new(file);
    let mut scanned = 0_u64;
    while scanned < MAX_HEADER_SCAN_BYTES {
        let remaining = MAX_HEADER_SCAN_BYTES - scanned;
        let mut line = Vec::new();
        let mut bounded = reader.take(remaining.min(MAX_HEADER_LINE_BYTES) + 1);
        let read = bounded.read_until(b'\n', &mut line)?;
        reader = bounded.into_inner();
        if read == 0 {
            break;
        }
        let read = u64::try_from(read)?;
        scanned = scanned.saturating_add(read);
        if read > MAX_HEADER_LINE_BYTES {
            bail!("Pi session header line exceeds safe scan limit");
        }
        let first = line.iter().position(|byte| !byte.is_ascii_whitespace());
        let last = line.iter().rposition(|byte| !byte.is_ascii_whitespace());
        let Some((first, last)) = first.zip(last) else {
            continue;
        };
        if let Ok(record) = serde_json::from_slice::<Value>(&line[first..=last]) {
            return header_record(&record);
        }
    }
    bail!("Pi session contains no valid v3 header within safe scan limit")
}

fn latest_timestamp(records: &[Value], fallback: DateTime<Utc>) -> DateTime<Utc> {
    records
        .iter()
        .filter_map(|record| parse_timestamp(record.get("timestamp")))
        .max()
        .unwrap_or(fallback)
}

fn session_path(records: &[Value], allow_missing_ancestors: bool) -> Result<Vec<&Value>> {
    let entries = records
        .iter()
        .skip(1)
        .filter(|record| record.get("type").and_then(Value::as_str) != Some("session"))
        .collect::<Vec<_>>();
    let mut by_id = HashMap::new();
    let mut leaf = None;
    for entry in &entries {
        let Some(id) = entry.get("id").and_then(Value::as_str) else {
            continue;
        };
        if id.is_empty() {
            continue;
        }
        by_id.insert(id, *entry);
        leaf = Some(id);
    }

    let mut path = Vec::new();
    let mut seen = HashSet::new();
    let mut current = leaf;
    while let Some(id) = current {
        if !seen.insert(id) {
            bail!("Pi session tree contains a parent cycle");
        }
        let Some(entry) = by_id.get(id).copied() else {
            if allow_missing_ancestors {
                break;
            }
            bail!("Pi session tree contains a missing leaf");
        };
        path.push(entry);
        current = entry.get("parentId").and_then(Value::as_str);
    }
    path.reverse();
    Ok(path)
}

fn contextual_path(path: Vec<&Value>) -> Vec<&Value> {
    let Some((compaction_index, compaction)) = path
        .iter()
        .enumerate()
        .rev()
        .find(|(_, entry)| entry.get("type").and_then(Value::as_str) == Some("compaction"))
    else {
        return path;
    };
    let Some(first_kept) = compaction.get("firstKeptEntryId").and_then(Value::as_str) else {
        return path;
    };
    let mut retained = vec![*compaction];
    let mut found_first_kept = false;
    for entry in &path[..compaction_index] {
        if entry.get("id").and_then(Value::as_str) == Some(first_kept) {
            found_first_kept = true;
        }
        if found_first_kept {
            retained.push(*entry);
        }
    }
    retained.extend_from_slice(&path[compaction_index + 1..]);
    retained
}

fn text_blocks(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::String(text)) if !text.is_empty() => vec![text.to_owned()],
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
            .collect(),
        _ => Vec::new(),
    }
}

fn raw_type(entry: &Value) -> Option<String> {
    entry.get("type").and_then(Value::as_str).map(str::to_owned)
}

fn entry_timestamp(entry: &Value) -> Option<DateTime<Utc>> {
    parse_timestamp(entry.get("timestamp"))
}

fn emit_entry(builder: &mut EventBuilder, entry: &Value) {
    let timestamp = entry_timestamp(entry);
    let raw_type = raw_type(entry);
    match raw_type.as_deref() {
        Some("message") => emit_message(builder, entry, timestamp, raw_type),
        Some("compaction") => builder.push(
            EventKind::CompactionCreated,
            json!({
                "summary": entry.get("summary").cloned().unwrap_or(Value::Null),
                "first_kept_entry_id": entry.get("firstKeptEntryId").cloned().unwrap_or(Value::Null),
                "tokens_before": entry.get("tokensBefore").cloned().unwrap_or(Value::Null),
            }),
            timestamp,
            ReplayPolicy::HistoricalOnly,
            raw_type,
            None,
        ),
        Some("branch_summary") => builder.push(
            EventKind::HandoffCreated,
            json!({
                "summary": entry.get("summary").cloned().unwrap_or(Value::Null),
                "from_id": entry.get("fromId").cloned().unwrap_or(Value::Null),
            }),
            timestamp,
            ReplayPolicy::HistoricalOnly,
            raw_type,
            None,
        ),
        Some(
            "model_change" | "thinking_level_change" | "label" | "session_info" | "custom"
            | "custom_message",
        ) => builder.push(
            EventKind::ProviderEvent,
            selected_entry_metadata(entry),
            timestamp,
            ReplayPolicy::HistoricalOnly,
            raw_type,
            None,
        ),
        Some(_) | None => builder.push(
            EventKind::ProviderEvent,
            json!({
                "type": "pi_unsupported_entry",
                "entry_type": raw_type,
            }),
            timestamp,
            ReplayPolicy::HistoricalOnly,
            raw_type,
            None,
        ),
    }
}

fn emit_message(
    builder: &mut EventBuilder,
    entry: &Value,
    timestamp: Option<DateTime<Utc>>,
    raw_type: Option<String>,
) {
    let Some(message) = entry.get("message") else {
        builder.push(
            EventKind::ProviderEvent,
            json!({ "type": "pi_message_without_payload" }),
            timestamp,
            ReplayPolicy::HistoricalOnly,
            raw_type,
            None,
        );
        return;
    };
    match message.get("role").and_then(Value::as_str) {
        Some("user") => emit_user_message(builder, message, timestamp, raw_type.as_ref()),
        Some("assistant") => emit_assistant_message(builder, message, timestamp, raw_type.as_ref()),
        Some("toolResult") => emit_tool_result(builder, message, timestamp, raw_type),
        Some("bashExecution") => emit_bash_execution(builder, message, timestamp, raw_type),
        Some(role) => builder.push(
            EventKind::ProviderEvent,
            json!({ "type": "pi_unsupported_message", "role": role }),
            timestamp,
            ReplayPolicy::HistoricalOnly,
            raw_type,
            None,
        ),
        None => builder.push(
            EventKind::ProviderEvent,
            json!({ "type": "pi_message_without_role" }),
            timestamp,
            ReplayPolicy::HistoricalOnly,
            raw_type,
            None,
        ),
    }
}

fn emit_user_message(
    builder: &mut EventBuilder,
    message: &Value,
    timestamp: Option<DateTime<Utc>>,
    raw_type: Option<&String>,
) {
    for text in text_blocks(message.get("content")) {
        builder.push(
            EventKind::MessageUser,
            json!({ "text": text }),
            timestamp,
            ReplayPolicy::Contextual,
            raw_type.cloned(),
            None,
        );
    }
}

fn emit_assistant_message(
    builder: &mut EventBuilder,
    message: &Value,
    timestamp: Option<DateTime<Utc>>,
    raw_type: Option<&String>,
) {
    if let Some(payload) = pi_session_metadata(message) {
        builder.push(
            EventKind::ProviderEvent,
            payload,
            timestamp,
            ReplayPolicy::HistoricalOnly,
            Some("omnisession.session_metadata".to_owned()),
            None,
        );
    }
    for block in message
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        emit_assistant_content(builder, block, timestamp, raw_type);
    }
}

fn pi_session_metadata(message: &Value) -> Option<Value> {
    let model = string_at(message, &[&["model"], &["modelId"]]);
    let usage = message.get("usage");
    let total_tokens = usage
        .map(|usage| {
            ["input", "output", "cacheRead", "cacheWrite"]
                .into_iter()
                .filter_map(|field| usage.get(field).and_then(Value::as_u64))
                .fold(0_u64, u64::saturating_add)
        })
        .filter(|tokens| *tokens > 0);
    (model.is_some() || total_tokens.is_some()).then(|| {
        json!({
            "model": model,
            "total_tokens": total_tokens,
            "token_usage": "incremental",
        })
    })
}

fn emit_assistant_content(
    builder: &mut EventBuilder,
    block: &Value,
    timestamp: Option<DateTime<Utc>>,
    raw_type: Option<&String>,
) {
    match block.get("type").and_then(Value::as_str) {
        Some("text") => {
            if let Some(text) = block.get("text").and_then(Value::as_str) {
                if !text.is_empty() {
                    builder.push(
                        EventKind::MessageAssistant,
                        json!({ "text": text }),
                        timestamp,
                        ReplayPolicy::Contextual,
                        raw_type.cloned(),
                        None,
                    );
                }
            }
        }
        Some("thinking") => builder.push(
            EventKind::ProviderEvent,
            json!({ "type": "pi_reasoning_omitted" }),
            timestamp,
            ReplayPolicy::HistoricalOnly,
            raw_type.cloned(),
            None,
        ),
        Some("toolCall") => builder.push(
            EventKind::ToolCalled,
            json!({
                "id": block.get("id").cloned().unwrap_or(Value::Null),
                "name": block.get("name").cloned().unwrap_or(Value::Null),
                "arguments": block.get("arguments").cloned().unwrap_or(Value::Null),
            }),
            timestamp,
            ReplayPolicy::HistoricalOnly,
            raw_type.cloned(),
            None,
        ),
        Some(_) | None => builder.push(
            EventKind::ProviderEvent,
            json!({
                "type": "pi_unsupported_assistant_content",
                "content_type": block.get("type").cloned().unwrap_or(Value::Null),
            }),
            timestamp,
            ReplayPolicy::HistoricalOnly,
            raw_type.cloned(),
            None,
        ),
    }
}

fn emit_tool_result(
    builder: &mut EventBuilder,
    message: &Value,
    timestamp: Option<DateTime<Utc>>,
    raw_type: Option<String>,
) {
    let output = text_blocks(message.get("content")).join("\n");
    builder.push(
        if message.get("isError").and_then(Value::as_bool) == Some(true) {
            EventKind::ToolFailed
        } else {
            EventKind::ToolCompleted
        },
        json!({
            "tool_call_id": message.get("toolCallId").cloned().unwrap_or(Value::Null),
            "tool_name": message.get("toolName").cloned().unwrap_or(Value::Null),
            "output": output,
        }),
        timestamp,
        ReplayPolicy::HistoricalOnly,
        raw_type,
        None,
    );
}

fn emit_bash_execution(
    builder: &mut EventBuilder,
    message: &Value,
    timestamp: Option<DateTime<Utc>>,
    raw_type: Option<String>,
) {
    builder.push(
        EventKind::CommandExecuted,
        json!({
            "command": message.get("command").cloned().unwrap_or(Value::Null),
            "output": message.get("output").cloned().unwrap_or(Value::Null),
            "exit_code": message.get("exitCode").cloned().unwrap_or(Value::Null),
            "cancelled": message.get("cancelled").cloned().unwrap_or(Value::Null),
            "truncated": message.get("truncated").cloned().unwrap_or(Value::Null),
        }),
        timestamp,
        ReplayPolicy::HistoricalOnly,
        raw_type,
        None,
    );
}

fn selected_entry_metadata(entry: &Value) -> Value {
    let entry_type = entry.get("type").cloned().unwrap_or(Value::Null);
    match entry_type.as_str() {
        Some("model_change") => json!({
            "type": entry_type,
            "provider": entry.get("provider").cloned().unwrap_or(Value::Null),
            "model_id": entry.get("modelId").cloned().unwrap_or(Value::Null),
        }),
        Some("thinking_level_change") => json!({
            "type": entry_type,
            "thinking_level": entry.get("thinkingLevel").cloned().unwrap_or(Value::Null),
        }),
        Some("label") => json!({
            "type": entry_type,
            "target_id": entry.get("targetId").cloned().unwrap_or(Value::Null),
            "label": entry.get("label").cloned().unwrap_or(Value::Null),
        }),
        Some("session_info") => json!({
            "type": entry_type,
            "name": entry.get("name").cloned().unwrap_or(Value::Null),
        }),
        Some("custom" | "custom_message") => json!({
            "type": entry_type,
            "custom_type": entry.get("customType").cloned().unwrap_or(Value::Null),
        }),
        _ => json!({ "type": "pi_unsupported_entry" }),
    }
}

fn session_title(records: &[Value]) -> Option<String> {
    records
        .iter()
        .rev()
        .find(|record| record.get("type").and_then(Value::as_str) == Some("session_info"))
        .and_then(|record| string_at(record, &[&["name"]]))
        .map(str::to_owned)
}

fn snapshot_from_records(
    session: &SessionRef,
    records: &[Value],
) -> Result<omnis_ir::CanonicalSnapshot> {
    let header = header(records)?;
    if header.id != session.id {
        bail!(
            "Pi session file identifies `{}`, not requested session `{}`",
            header.id,
            session.id
        );
    }
    let captured_at = latest_timestamp(records, header.timestamp.unwrap_or_else(Utc::now));
    let mut builder = EventBuilder::new(Provider::Pi, &session.id);
    builder.set_provider_version(Some(PI_SESSION_VERSION.to_string()));
    for entry in contextual_path(session_path(records, false)?) {
        emit_entry(&mut builder, entry);
    }
    Ok(builder.snapshot(
        session.clone(),
        session_title(records),
        Some(header.cwd),
        None,
        captured_at,
    ))
}

fn snapshot_from_records_preview(
    session: &SessionRef,
    records: &[Value],
) -> Result<omnis_ir::CanonicalSnapshot> {
    let header = header(records)?;
    if header.id != session.id {
        bail!(
            "Pi session file identifies `{}`, not requested session `{}`",
            header.id,
            session.id
        );
    }
    let captured_at = latest_timestamp(records, header.timestamp.unwrap_or_else(Utc::now));
    let mut builder = EventBuilder::new(Provider::Pi, &session.id);
    builder.set_provider_version(Some(PI_SESSION_VERSION.to_string()));
    for entry in contextual_path(session_path(records, true)?) {
        emit_entry(&mut builder, entry);
    }
    Ok(builder.snapshot(
        session.clone(),
        session_title(records),
        Some(header.cwd),
        None,
        captured_at,
    ))
}

impl ProviderAdapter for PiAdapter {
    fn provider(&self) -> Provider {
        Provider::Pi
    }

    fn probe(&self) -> ProviderInstallation {
        ProviderInstallation {
            provider: Provider::Pi,
            installed: executable("pi").is_some()
                || self.sessions_root.as_deref().is_some_and(Path::is_dir),
            executable: executable("pi"),
            data_root: self.sessions_root.clone(),
        }
    }

    fn list_sessions(&self, project: Option<&Path>) -> Result<Vec<NativeSession>> {
        let mut sessions = Vec::new();
        for path in self.session_files() {
            let Ok(header) = read_header(&path) else {
                continue;
            };
            if project.is_some_and(|project| !paths_match(&header.cwd, project)) {
                continue;
            }
            let file_updated_at = fs::metadata(&path)
                .ok()
                .and_then(|metadata| metadata.modified().ok())
                .map(DateTime::<Utc>::from);
            sessions.push(NativeSession {
                session: SessionRef::new(Provider::Pi, header.id),
                title: None,
                project_path: Some(header.cwd),
                git_branch: None,
                created_at: header.timestamp,
                updated_at: file_updated_at.or(header.timestamp),
                updated_at_approximate: file_updated_at.is_some(),
                event_count: 0,
                source_path: Some(path),
            });
        }
        sort_sessions(&mut sessions);
        Ok(sessions)
    }

    fn read_session(&self, session: &SessionRef) -> Result<omnis_ir::CanonicalSnapshot> {
        validate_provider(session, Provider::Pi)?;
        let path = self.find_session(&session.id)?;
        snapshot_from_records(session, &json_lines(&path)?)
    }

    fn preview_session(&self, session: &SessionRef) -> Result<omnis_ir::CanonicalSnapshot> {
        validate_provider(session, Provider::Pi)?;
        let path = self.find_session(&session.id)?;
        snapshot_from_records_preview(session, &json_lines_preview(&path, PREVIEW_RECORDS)?)
    }

    fn new_session_plan(&self, target: &LaunchTarget) -> Result<LaunchPlan> {
        Ok(LaunchPlan {
            program: "pi".to_owned(),
            args: target.prompt.iter().cloned().collect(),
            cwd: target.cwd.clone(),
        })
    }

    fn launch_plan(&self, session: &SessionRef, target: &LaunchTarget) -> Result<LaunchPlan> {
        validate_provider(session, Provider::Pi)?;
        let mut args = if target.fork {
            vec!["--fork".to_owned(), session.id.clone()]
        } else {
            vec!["--session".to_owned(), session.id.clone()]
        };
        args.extend(target.prompt.iter().cloned());
        Ok(LaunchPlan {
            program: "pi".to_owned(),
            args,
            cwd: target.cwd.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contextual_path_uses_latest_compaction_and_active_tree_branch() {
        let records = vec![
            json!({"type": "session", "version": 3, "id": "session", "cwd": "/workspace"}),
            json!({"type": "message", "id": "root", "parentId": null}),
            json!({"type": "message", "id": "kept", "parentId": "root"}),
            json!({"type": "message", "id": "abandoned", "parentId": "root"}),
            json!({"type": "compaction", "id": "compact", "parentId": "kept", "firstKeptEntryId": "kept"}),
            json!({"type": "message", "id": "leaf", "parentId": "compact"}),
        ];
        let ids = contextual_path(session_path(&records, false).expect("session tree"))
            .into_iter()
            .filter_map(|entry| entry.get("id").and_then(Value::as_str))
            .collect::<Vec<_>>();
        assert_eq!(ids, ["compact", "kept", "leaf"]);
    }

    #[test]
    fn preview_tolerates_truncated_parent_chain() {
        let records = vec![
            json!({"type": "session", "version": 3, "id": "session", "cwd": "/workspace"}),
            json!({
                "type": "message", "id": "leaf", "parentId": "missing",
                "message": {"role": "user", "content": "tail", "timestamp": 1}
            }),
        ];
        let snapshot =
            snapshot_from_records_preview(&SessionRef::new(Provider::Pi, "session"), &records)
                .expect("partial Pi preview");
        assert_eq!(snapshot.events.len(), 1);
        assert_eq!(snapshot.events[0].kind, EventKind::MessageUser);
    }
}
