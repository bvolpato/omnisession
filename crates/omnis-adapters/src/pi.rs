use std::{
    collections::{HashMap, HashSet},
    env, fs,
    io::{BufRead, BufReader, Read, Seek},
    path::{Path, PathBuf},
    time::SystemTime,
};

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use omnis_ir::{EventKind, Provider, ReplayPolicy, SessionRef};
use serde_json::{Value, json};

use crate::{
    IndexRead, LaunchPlan, LaunchTarget, NativeSession, ProviderAdapter, ProviderInstallation,
    support::{
        EventBuilder, JsonLineLocation, MAX_COLLECTED_TRANSCRIPT_FILE_SIZE,
        json_lines_preview_with_suffix, nested_files, omitted_images_text, parse_timestamp,
        paths_match, provider_executable, provider_file, provider_root, read_json_line_at,
        sort_sessions, string_at, validate_provider, visit_json_lines_with_offsets,
    },
};

const PI_SESSION_VERSION: u64 = 3;
const PREVIEW_RECORDS: usize = 1_024;
const MAX_HEADER_SCAN_BYTES: u64 = 1024 * 1024;
const MAX_HEADER_LINE_BYTES: u64 = 64 * 1024;

/// Read-only Pi coding-agent session adapter.
#[derive(Clone, Debug)]
pub struct PiAdapter {
    provider: Provider,
    sessions_root: Option<PathBuf>,
}

impl PiAdapter {
    #[must_use]
    pub fn with_root(sessions_root: impl Into<PathBuf>) -> Self {
        Self {
            provider: Provider::Pi,
            sessions_root: Some(sessions_root.into()),
        }
    }

    #[must_use]
    pub fn oh_my_pi() -> Self {
        Self {
            provider: Provider::OhMyPi,
            sessions_root: crate::oh_my_pi_sessions_root(),
        }
    }

    #[must_use]
    pub fn oh_my_pi_with_root(sessions_root: impl Into<PathBuf>) -> Self {
        Self {
            provider: Provider::OhMyPi,
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
            .find(|path| read_header(path, self.provider).is_ok_and(|header| header.id == id))
            .ok_or_else(|| anyhow!("Pi session `{id}` was not found"))
    }

    fn session_path(&self, id: &str, source_path: Option<&Path>) -> Result<PathBuf> {
        let hinted = source_path.and_then(|source_path| {
            let root = self.sessions_root.as_deref()?;
            let path = provider_file(root, source_path)?;
            if !path
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("jsonl"))
            {
                return None;
            }
            read_header(&path, self.provider)
                .ok()
                .filter(|header| header.id == id)
                .map(|_| path)
        });
        hinted.map_or_else(|| self.find_session(id), Ok)
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
        Self {
            provider: Provider::Pi,
            sessions_root,
        }
    }
}

#[derive(Clone, Debug)]
struct PiHeader {
    id: String,
    title: Option<String>,
    cwd: PathBuf,
    timestamp: Option<DateTime<Utc>>,
}

impl PiHeader {
    fn into_native(
        self,
        provider: Provider,
        path: PathBuf,
        modified: Option<DateTime<Utc>>,
    ) -> NativeSession {
        NativeSession {
            session: SessionRef::new(provider, self.id),
            title: self.title,
            project_path: Some(self.cwd),
            git_branch: None,
            created_at: self.timestamp,
            updated_at: modified.or(self.timestamp),
            updated_at_approximate: modified.is_some(),
            event_count: 0,
            source_path: Some(path),
        }
    }
}

fn header(records: &[Value], provider: Provider) -> Result<PiHeader> {
    let record = records
        .first()
        .context("Pi session contains no valid JSONL records")?;
    if provider == Provider::OhMyPi && record["type"] == "title" {
        let mut header = header_record(
            records
                .get(1)
                .context("Oh My Pi title slot has no session header")?,
        )?;
        header.title = string_at(record, &[&["title"]]).map(str::to_owned);
        return Ok(header);
    }
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
        title: string_at(record, &[&["title"]]).map(str::to_owned),
        cwd: PathBuf::from(cwd),
        timestamp: parse_timestamp(record.get("timestamp")),
    })
}

/// Mirrors Pi's bounded header discovery: skip malformed leading lines, then
/// require first parsed entry to be a v3 session header. Discovery never scans
/// a full transcript.
fn read_header(path: &Path, provider: Provider) -> Result<PiHeader> {
    let file = fs::File::open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        bail!("Pi session path is not a regular file");
    }
    let mut reader = BufReader::new(file);
    let mut scanned = 0_u64;
    let mut title_seen = false;
    let mut title = None;
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
            if provider == Provider::OhMyPi && !title_seen && record["type"] == "title" {
                title_seen = true;
                title = string_at(&record, &[&["title"]]).map(str::to_owned);
                continue;
            }
            let mut header = header_record(&record)?;
            if title_seen {
                header.title = title;
            }
            return Ok(header);
        }
    }
    bail!("Pi session contains no valid v3 header within safe scan limit")
}

#[derive(Default)]
enum HeaderState {
    #[default]
    Empty,
    OhMyPiTitle(Option<String>),
    Parsed(Result<PiHeader>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BranchEntryKind {
    Compaction,
    Reset,
    Other,
}

fn branch_entry_kind(provider: Provider, entry: &Value) -> BranchEntryKind {
    match entry["type"].as_str() {
        Some("compaction") => BranchEntryKind::Compaction,
        Some("reset_boundary") if provider == Provider::OhMyPi => BranchEntryKind::Reset,
        _ => BranchEntryKind::Other,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CompactionRole {
    Unmarked,
    Current,
    Retained,
}

#[derive(Clone, Debug)]
struct PiTopologyEntry {
    parent_id: Option<String>,
    kind: BranchEntryKind,
    first_kept_entry_id: Option<String>,
    location: JsonLineLocation,
}

#[derive(Default)]
struct PiTranscriptIndex {
    header: HeaderState,
    latest_title: Option<String>,
    latest_timestamp: Option<DateTime<Utc>>,
    by_id: HashMap<String, PiTopologyEntry>,
    leaf: Option<String>,
}

impl PiTranscriptIndex {
    fn observe(&mut self, provider: Provider, location: JsonLineLocation, record: &Value) {
        let first_record = matches!(&self.header, HeaderState::Empty);
        let header = match &self.header {
            HeaderState::Empty => Some(
                if provider == Provider::OhMyPi && record["type"] == "title" {
                    HeaderState::OhMyPiTitle(string_at(record, &[&["title"]]).map(str::to_owned))
                } else {
                    HeaderState::Parsed(header_record(record))
                },
            ),
            HeaderState::OhMyPiTitle(title) => Some(HeaderState::Parsed(
                header_record(record).map(|mut header| {
                    header.title.clone_from(title);
                    header
                }),
            )),
            HeaderState::Parsed(_) => None,
        };
        if let Some(header) = header {
            self.header = header;
        }

        if let Some(timestamp) = parse_timestamp(record.get("timestamp")) {
            self.latest_timestamp = Some(
                self.latest_timestamp
                    .take()
                    .map_or(timestamp, |latest| latest.max(timestamp)),
            );
        }
        if provider != Provider::OhMyPi
            && matches!(
                record["type"].as_str(),
                Some("session_info" | "title" | "title_change" | "session")
            )
            && let Some(title) = string_at(record, &[&["name"], &["title"]])
        {
            self.latest_title = Some(title.to_owned());
        }

        if first_record || record.get("type").and_then(Value::as_str) == Some("session") {
            return;
        }
        let Some(id) = record
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        else {
            return;
        };
        let kind = branch_entry_kind(provider, record);
        self.by_id.insert(
            id.to_owned(),
            PiTopologyEntry {
                parent_id: record
                    .get("parentId")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                kind,
                first_kept_entry_id: (kind == BranchEntryKind::Compaction)
                    .then(|| {
                        record
                            .get("firstKeptEntryId")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                    })
                    .flatten(),
                location,
            },
        );
        self.leaf = Some(id.to_owned());
    }

    fn take_header(&mut self) -> Result<PiHeader> {
        match std::mem::take(&mut self.header) {
            HeaderState::Empty => Err(anyhow!("Pi session contains no valid JSONL records")),
            HeaderState::OhMyPiTitle(_) => {
                Err(anyhow!("Oh My Pi title slot has no session header"))
            }
            HeaderState::Parsed(header) => header,
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
struct PiFileStamp {
    length: u64,
    modified: SystemTime,
    #[cfg(not(unix))]
    created: Option<SystemTime>,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    change_seconds: i64,
    #[cfg(unix)]
    change_nanoseconds: i64,
}

impl PiFileStamp {
    fn read(file: &fs::File) -> Result<Self> {
        Self::from_metadata(&file.metadata()?)
    }

    fn from_metadata(metadata: &fs::Metadata) -> Result<Self> {
        Ok(Self {
            length: metadata.len(),
            modified: metadata.modified()?,
            #[cfg(not(unix))]
            created: metadata.created().ok(),
            #[cfg(unix)]
            device: metadata.dev(),
            #[cfg(unix)]
            inode: metadata.ino(),
            #[cfg(unix)]
            change_seconds: metadata.ctime(),
            #[cfg(unix)]
            change_nanoseconds: metadata.ctime_nsec(),
        })
    }

    fn ensure_unchanged(&self, file: &fs::File) -> Result<()> {
        if *self != Self::read(file)? {
            bail!("Pi session changed during read");
        }
        Ok(())
    }
}

fn topology_path(
    index: &PiTranscriptIndex,
    allow_missing_ancestors: bool,
) -> Result<Vec<(JsonLineLocation, CompactionRole)>> {
    let mut path = Vec::new();
    let mut seen = HashSet::new();
    let mut current = index.leaf.as_deref();
    while let Some(id) = current {
        if !seen.insert(id) {
            bail!("Pi session tree contains a parent cycle");
        }
        let Some(entry) = index.by_id.get(id) else {
            if allow_missing_ancestors {
                break;
            }
            bail!("Pi session tree contains a missing leaf");
        };
        path.push((id, entry));
        current = entry.parent_id.as_deref();
    }
    path.reverse();

    Ok(order_compaction(
        path,
        |(_, entry)| entry.kind,
        |(_, entry)| entry.first_kept_entry_id.as_deref(),
        |(id, _)| Some(*id),
    )
    .map(|((_, entry), role)| (entry.location, role))
    .collect())
}

fn latest_timestamp(records: &[Value], fallback: DateTime<Utc>) -> DateTime<Utc> {
    records
        .iter()
        .filter_map(|record| parse_timestamp(record.get("timestamp")))
        .max()
        .unwrap_or(fallback)
}

#[cfg(test)]
fn session_path(
    records: &[Value],
    provider: Provider,
    allow_missing_ancestors: bool,
) -> Result<Vec<(&Value, CompactionRole)>> {
    let path = selected_branch(records, allow_missing_ancestors)?;
    Ok(ordered_entries(path, provider).collect())
}

fn ordered_entries(
    path: Vec<&Value>,
    provider: Provider,
) -> impl Iterator<Item = (&Value, CompactionRole)> {
    order_compaction(
        path,
        move |entry| branch_entry_kind(provider, entry),
        |entry| entry["firstKeptEntryId"].as_str(),
        |entry| entry["id"].as_str(),
    )
}

fn selected_branch(records: &[Value], allow_missing_ancestors: bool) -> Result<Vec<&Value>> {
    let mut by_id = HashMap::new();
    let mut leaf = None;
    for entry in records
        .iter()
        .skip(1)
        .filter(|record| record["type"] != "session")
    {
        let Some(id) = entry.get("id").and_then(Value::as_str) else {
            continue;
        };
        if id.is_empty() {
            continue;
        }
        by_id.insert(id, entry);
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

/// Position the latest compaction ahead of its retained entries and identify older summaries in that range.
/// An Oh My Pi clear starts a new task. Keep its marker before any subsequent compaction, even if
/// the compaction's retained entry ID points into the previous task.
fn order_compaction<T>(
    mut path: Vec<T>,
    entry_kind: impl Fn(&T) -> BranchEntryKind,
    first_kept_id: impl Fn(&T) -> Option<&str>,
    entry_id: impl Fn(&T) -> Option<&str>,
) -> impl Iterator<Item = (T, CompactionRole)> {
    let mut compactions = path
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, entry)| entry_kind(entry) == BranchEntryKind::Compaction);
    let latest = compactions.next().map(|(index, _)| index);
    let has_earlier_compaction = compactions.next().is_some();
    let boundary = latest.map(|latest| {
        let mut start = first_kept_id(&path[latest])
            .and_then(|id| {
                path[..latest]
                    .iter()
                    .position(|entry| entry_id(entry) == Some(id))
            })
            .unwrap_or(latest);
        if let Some(reset) = path[start..latest]
            .iter()
            .rposition(|entry| entry_kind(entry) == BranchEntryKind::Reset)
        {
            start += reset + 1;
        }
        path[start..=latest].rotate_right(1);
        start
    });
    path.into_iter().enumerate().map(move |(index, entry)| {
        let role = match boundary {
            Some(start) if index == start && has_earlier_compaction => CompactionRole::Current,
            Some(start) if index > start && entry_kind(&entry) == BranchEntryKind::Compaction => {
                CompactionRole::Retained
            }
            _ => CompactionRole::Unmarked,
        };
        (entry, role)
    })
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

fn emit_entry(
    builder: &mut EventBuilder,
    provider: Provider,
    entry: &Value,
    compaction_role: CompactionRole,
) {
    let timestamp = entry_timestamp(entry);
    let raw_type = raw_type(entry);
    match raw_type.as_deref() {
        Some("message") => emit_message(builder, entry, timestamp, raw_type),
        Some("compaction") => {
            let mut payload = json!({
                "summary": entry.get("summary"),
                "first_kept_entry_id": entry.get("firstKeptEntryId"),
                "tokens_before": entry.get("tokensBefore"),
            });
            match compaction_role {
                CompactionRole::Current => payload["active_compaction"] = Value::Bool(true),
                CompactionRole::Retained => payload["retained_by_compaction"] = Value::Bool(true),
                CompactionRole::Unmarked => {}
            }
            // Pi includes older retained summary messages. Oh My Pi only displays them in its archive.
            let policy =
                if compaction_role == CompactionRole::Retained && provider == Provider::OhMyPi {
                    ReplayPolicy::HistoricalOnly
                } else {
                    ReplayPolicy::Contextual
                };
            builder.push(
                EventKind::CompactionCreated,
                payload,
                timestamp,
                policy,
                raw_type,
                None,
            );
        }
        Some("reset_boundary" | "title_change") if provider == Provider::OhMyPi => builder.push(
            EventKind::ProviderEvent,
            selected_entry_metadata(entry),
            timestamp,
            ReplayPolicy::HistoricalOnly,
            raw_type,
            None,
        ),
        Some("branch_summary") => emit_branch_summary(builder, entry, timestamp, raw_type),
        Some("custom_message") => emit_custom_message(builder, entry, timestamp, raw_type),
        Some("model_change" | "thinking_level_change" | "label" | "session_info" | "custom") => {
            builder.push(
                EventKind::ProviderEvent,
                selected_entry_metadata(entry),
                timestamp,
                ReplayPolicy::HistoricalOnly,
                raw_type,
                None,
            );
        }
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
    let content = message.get("content");
    let mut texts = text_blocks(content);
    // An image-only turn keeps a placeholder so the reply still follows a request.
    let images = image_blocks(content);
    if texts.is_empty() && images > 0 {
        texts.push(omitted_images_text(images));
    }
    for text in texts {
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

fn image_blocks(value: Option<&Value>) -> usize {
    value.and_then(Value::as_array).map_or(0, |blocks| {
        blocks
            .iter()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("image"))
            .count()
    })
}

/// Pi shows branch summaries in its chat and sends them to the model, so the summary stays visible.
fn emit_branch_summary(
    builder: &mut EventBuilder,
    entry: &Value,
    timestamp: Option<DateTime<Utc>>,
    raw_type: Option<String>,
) {
    let from_id = entry.get("fromId").cloned().unwrap_or(Value::Null);
    // Pi leaves an empty summary out of model context.
    let Some(summary) = string_at(entry, &[&["summary"]]) else {
        builder.push(
            EventKind::ProviderEvent,
            json!({ "type": "branch_summary", "from_id": from_id }),
            timestamp,
            ReplayPolicy::HistoricalOnly,
            raw_type,
            None,
        );
        return;
    };
    builder.push(
        EventKind::MessageAssistant,
        json!({
            "text": format!("[Source branch summary]\n{summary}"),
            "from_id": from_id,
        }),
        timestamp,
        ReplayPolicy::Contextual,
        raw_type,
        None,
    );
}

/// Pi sends extension messages to the model as user input and shows those marked `display`.
fn emit_custom_message(
    builder: &mut EventBuilder,
    entry: &Value,
    timestamp: Option<DateTime<Utc>>,
    raw_type: Option<String>,
) {
    let content = entry.get("content");
    let text = text_blocks(content).join("\n");
    let images = image_blocks(content);
    let body = if text.is_empty() {
        (images > 0).then(|| omitted_images_text(images))
    } else {
        Some(text)
    };
    let displayed = entry.get("display").and_then(Value::as_bool) == Some(true);
    let Some(body) = body.filter(|_| displayed) else {
        builder.push(
            EventKind::ProviderEvent,
            selected_entry_metadata(entry),
            timestamp,
            ReplayPolicy::HistoricalOnly,
            raw_type,
            None,
        );
        return;
    };
    let label = string_at(entry, &[&["customType"]]).map_or_else(
        || "[Source extension message]".to_owned(),
        |custom_type| format!("[Source extension message: {custom_type}]"),
    );
    builder.push(
        EventKind::MessageUser,
        json!({
            "text": format!("{label}\n{body}"),
            "custom_type": entry.get("customType").cloned().unwrap_or(Value::Null),
        }),
        timestamp,
        ReplayPolicy::Contextual,
        raw_type,
        None,
    );
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
            if let Some(text) = block.get("text").and_then(Value::as_str)
                && !text.is_empty()
            {
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
        Some("reset_boundary") => json!({ "type": entry_type }),
        Some("title_change") => json!({ "type": entry_type, "title": entry.get("title") }),
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
        .filter(|record| {
            matches!(
                record["type"].as_str(),
                Some("session_info" | "title" | "title_change" | "session")
            )
        })
        .find_map(|record| string_at(record, &[&["name"], &["title"]]))
        .map(str::to_owned)
}

fn omp_branch_title(entry: &Value) -> Option<&str> {
    match entry["type"].as_str() {
        Some("title_change") => entry["title"].as_str(),
        Some("session_info") => entry["name"].as_str(),
        _ => None,
    }
    .filter(|title| !title.trim().is_empty())
}

#[cfg(test)]
fn snapshot_from_records(
    session: &SessionRef,
    records: &[Value],
    oversized_records: usize,
) -> Result<omnis_ir::CanonicalSnapshot> {
    let header = header(records, session.provider)?;
    if header.id != session.id {
        bail!(
            "Pi session file identifies `{}`, not requested session `{}`",
            header.id,
            session.id
        );
    }
    let captured_at = latest_timestamp(records, header.timestamp.unwrap_or_else(Utc::now));
    let mut builder = EventBuilder::new(session.provider, &session.id);
    builder.set_provider_version(Some(PI_SESSION_VERSION.to_string()));
    let mut title = if session.provider == Provider::OhMyPi {
        header.title
    } else {
        session_title(records)
    };
    for (entry, role) in session_path(records, session.provider, false)? {
        if session.provider == Provider::OhMyPi
            && let Some(name) = omp_branch_title(entry)
        {
            title = Some(name.to_owned());
        }
        emit_entry(&mut builder, session.provider, entry, role);
    }
    builder.push_oversized_record_notice(oversized_records, Some(captured_at));
    Ok(builder.snapshot(session.clone(), title, Some(header.cwd), None, captured_at))
}

fn snapshot_from_open_file(session: &SessionRef, path: &Path) -> Result<IndexRead> {
    let file = fs::File::open(path)?;
    let mut reader = BufReader::new(file);
    let stamp = PiFileStamp::read(reader.get_ref())?;
    let mut index = PiTranscriptIndex::default();
    let oversized_records = visit_json_lines_with_offsets(
        &mut reader,
        MAX_COLLECTED_TRANSCRIPT_FILE_SIZE,
        |location, record| {
            index.observe(session.provider, location, &record);
            Ok(())
        },
    )?;
    stamp.ensure_unchanged(reader.get_ref())?;

    let header = index.take_header()?;
    if header.id != session.id {
        bail!(
            "Pi session file identifies `{}`, not requested session `{}`",
            header.id,
            session.id
        );
    }
    let captured_at = index
        .latest_timestamp
        .unwrap_or_else(|| header.timestamp.unwrap_or_else(Utc::now));
    let locations = topology_path(&index, false)?;
    let mut title = if session.provider == Provider::OhMyPi {
        header.title.clone()
    } else {
        index.latest_title.take()
    };
    drop(index);
    let mut builder = EventBuilder::new(session.provider, &session.id);
    builder.set_provider_version(Some(PI_SESSION_VERSION.to_string()));
    let mut record_line = Vec::new();
    let mut read_position = reader.stream_position()?;
    for (location, role) in locations {
        let entry =
            match read_json_line_at(&mut reader, location, &mut read_position, &mut record_line) {
                Ok(entry) => entry,
                Err(error) => {
                    stamp.ensure_unchanged(reader.get_ref())?;
                    return Err(error);
                }
            };
        if session.provider == Provider::OhMyPi
            && let Some(name) = omp_branch_title(&entry)
        {
            title = Some(name.to_owned());
        }
        emit_entry(&mut builder, session.provider, &entry, role);
    }
    builder.push_oversized_record_notice(oversized_records, Some(captured_at));
    stamp.ensure_unchanged(reader.get_ref())?;
    if !fs::metadata(path)
        .ok()
        .and_then(|metadata| PiFileStamp::from_metadata(&metadata).ok())
        .is_some_and(|current| current == stamp)
    {
        bail!("Pi session changed during read");
    }
    let snapshot = builder.snapshot(
        session.clone(),
        title,
        Some(header.cwd.clone()),
        None,
        captured_at,
    );
    Ok(IndexRead {
        native_session: Some(header.into_native(
            session.provider,
            path.to_path_buf(),
            Some(stamp.modified.into()),
        )),
        snapshot,
        source_complete: true,
        provider_limited: false,
    })
}

/// Walk the selected parent chain back from EOF. A gap or unknown entry ends the title evidence.
fn sampled_omp_title<'a>(branch: &[&'a Value], suffix: &[Value]) -> (Option<&'a str>, bool) {
    if !branch
        .last()
        .zip(suffix.last())
        .is_some_and(|(leaf, last)| std::ptr::eq(*leaf, last))
    {
        return (None, false);
    }
    let range = suffix.as_ptr_range();
    let mut name = None;
    for &entry in branch
        .iter()
        .rev()
        .take_while(|entry| range.contains(&std::ptr::from_ref(**entry)))
    {
        match entry["type"].as_str() {
            Some("title_change") => {
                name = name.or_else(|| omp_branch_title(entry));
            }
            Some("reset_boundary") => return (name, true),
            Some(
                "message"
                | "model_usage"
                | "thinking_level_change"
                | "model_change"
                | "service_tier_change"
                | "compaction"
                | "branch_summary"
                | "custom"
                | "custom_message"
                | "label"
                | "ttsr_injection"
                | "session_init"
                | "mode_change"
                | "credential_pin",
            ) => {}
            _ => break,
        }
    }
    (name, false)
}

fn snapshot_from_records_preview(
    session: &SessionRef,
    records: &[Value],
    suffix_len: usize,
) -> Result<omnis_ir::CanonicalSnapshot> {
    let header = header(records, session.provider)?;
    if header.id != session.id {
        bail!(
            "Pi session file identifies `{}`, not requested session `{}`",
            header.id,
            session.id
        );
    }
    let captured_at = latest_timestamp(records, header.timestamp.unwrap_or_else(Utc::now));
    let mut builder = EventBuilder::new(session.provider, &session.id);
    builder.set_provider_version(Some(PI_SESSION_VERSION.to_string()));
    let path = selected_branch(records, true)?;
    let (rename, verified_reset) = if session.provider == Provider::OhMyPi {
        let suffix = &records[records.len().saturating_sub(suffix_len)..];
        sampled_omp_title(&path, suffix)
    } else {
        (None, false)
    };
    let title = if session.provider == Provider::OhMyPi {
        rename.map(str::to_owned).or(header.title)
    } else {
        session_title(records)
    };
    for (entry, role) in ordered_entries(path, session.provider) {
        emit_entry(&mut builder, session.provider, entry, role);
    }
    let mut snapshot =
        builder.snapshot(session.clone(), title, Some(header.cwd), None, captured_at);
    if let Some(name) = rename
        && let Some(event) = snapshot.events.iter_mut().rfind(|event| {
            event.source.raw_record_type.as_deref() == Some("title_change")
                && event.payload["title"] == name
        })
    {
        event.payload["verified_through_leaf"] = true.into();
    }
    if session.provider == Provider::OhMyPi
        && let Some(reset) = snapshot
            .events
            .iter_mut()
            .rfind(|event| event.source.raw_record_type.as_deref() == Some("reset_boundary"))
    {
        reset.payload["verified_through_leaf"] = verified_reset.into();
    }
    Ok(snapshot)
}

impl ProviderAdapter for PiAdapter {
    fn provider(&self) -> Provider {
        self.provider
    }

    fn probe(&self) -> ProviderInstallation {
        let executable = provider_executable(self.provider);
        ProviderInstallation {
            provider: self.provider,
            installed: executable.is_some()
                || self.sessions_root.as_deref().is_some_and(Path::is_dir),
            executable,
            data_root: self.sessions_root.clone(),
        }
    }

    fn list_sessions(&self, project: Option<&Path>) -> Result<Vec<NativeSession>> {
        let mut sessions = Vec::new();
        for path in self.session_files() {
            let Ok(header) = read_header(&path, self.provider) else {
                continue;
            };
            if project.is_some_and(|project| !paths_match(&header.cwd, project)) {
                continue;
            }
            let file_updated_at = fs::metadata(&path)
                .ok()
                .and_then(|metadata| metadata.modified().ok())
                .map(DateTime::<Utc>::from);
            sessions.push(header.into_native(self.provider, path, file_updated_at));
        }
        sort_sessions(&mut sessions);
        Ok(sessions)
    }

    fn read_session(&self, session: &SessionRef) -> Result<omnis_ir::CanonicalSnapshot> {
        self.read_session_at(session, None)
    }

    fn read_session_at(
        &self,
        session: &SessionRef,
        source_path: Option<&Path>,
    ) -> Result<omnis_ir::CanonicalSnapshot> {
        Ok(self.read_full_index_at(session, source_path)?.snapshot)
    }

    fn read_full_index_at(
        &self,
        session: &SessionRef,
        source_path: Option<&Path>,
    ) -> Result<IndexRead> {
        validate_provider(session, self.provider)?;
        let path = self.session_path(&session.id, source_path)?;
        snapshot_from_open_file(session, &path)
    }

    fn search_timestamp_file(
        &self,
        session: &SessionRef,
        source_path: Option<&Path>,
    ) -> Result<Option<PathBuf>> {
        validate_provider(session, self.provider)?;
        self.session_path(&session.id, source_path).map(Some)
    }

    fn preview_session(&self, session: &SessionRef) -> Result<omnis_ir::CanonicalSnapshot> {
        self.preview_session_at(session, None)
    }

    fn preview_session_at(
        &self,
        session: &SessionRef,
        source_path: Option<&Path>,
    ) -> Result<omnis_ir::CanonicalSnapshot> {
        validate_provider(session, self.provider)?;
        let path = self.session_path(&session.id, source_path)?;
        let (records, suffix_len) = json_lines_preview_with_suffix(&path, PREVIEW_RECORDS)?;
        snapshot_from_records_preview(session, &records, suffix_len)
    }

    fn new_session_plan(&self, target: &LaunchTarget) -> Result<LaunchPlan> {
        let mut args = Vec::new();
        if self.provider == Provider::OhMyPi
            && env::var_os("OMP_SESSION_DIR").is_some_and(|value| !value.is_empty())
            && let Some(root) = &self.sessions_root
        {
            args.extend([
                "--session-dir".to_owned(),
                root.to_string_lossy().into_owned(),
            ]);
        }
        args.extend(target.prompt.iter().cloned());
        Ok(LaunchPlan {
            program: self
                .provider
                .command()
                .context("missing Pi-family command")?
                .to_owned(),
            args,
            cwd: target.cwd.clone(),
            env: Vec::new(),
        })
    }

    fn launch_plan(&self, session: &SessionRef, target: &LaunchTarget) -> Result<LaunchPlan> {
        validate_provider(session, self.provider)?;
        let reference = if self.provider == Provider::OhMyPi {
            self.find_session(&session.id)?
                .into_os_string()
                .into_string()
                .map_err(|_| anyhow!("Oh My Pi session path is not UTF-8"))?
        } else {
            session.id.clone()
        };
        let mut args = if target.fork {
            vec!["--fork".to_owned(), reference]
        } else {
            vec!["--session".to_owned(), reference]
        };
        if self.provider == Provider::OhMyPi
            && target.fork
            && env::var_os("OMP_SESSION_DIR").is_some_and(|value| !value.is_empty())
            && let Some(root) = &self.sessions_root
        {
            args.extend([
                "--session-dir".to_owned(),
                root.to_string_lossy().into_owned(),
            ]);
        }
        args.extend(target.prompt.iter().cloned());
        Ok(LaunchPlan {
            program: self
                .provider
                .command()
                .context("missing Pi-family command")?
                .to_owned(),
            args,
            cwd: target.cwd.clone(),
            env: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_fixture(root: &Path, filename: &str, contents: &str) -> PathBuf {
        let directory = root.join("project");
        fs::create_dir_all(&directory).expect("create synthetic session directory");
        let path = directory.join(filename);
        fs::write(&path, contents).expect("write synthetic session");
        path
    }

    fn serialized_records(records: &[Value]) -> String {
        records
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
            + "\n"
    }

    fn assert_matches_snapshot_oracle(
        adapter: &PiAdapter,
        session: &SessionRef,
        path: &Path,
        records: &[Value],
        oversized_records: usize,
    ) -> omnis_ir::CanonicalSnapshot {
        let actual = adapter
            .read_session_at(session, Some(path))
            .expect("read indexed Pi transcript");
        let expected = snapshot_from_records(session, records, oversized_records)
            .expect("read in-memory Pi snapshot oracle");
        assert_eq!(
            serde_json::to_value(&actual).expect("serialize actual snapshot"),
            serde_json::to_value(&expected).expect("serialize oracle snapshot")
        );
        actual
    }

    #[test]
    fn discovered_path_hint_reads_without_rewalking_provider_tree() {
        let temporary = tempfile::tempdir().expect("temporary Pi root");
        let root = temporary.path();
        let path = root.join("project/nested/session.jsonl");
        fs::create_dir_all(path.parent().expect("session parent")).expect("create session tree");
        fs::write(
            &path,
            "{\"type\":\"session\",\"version\":3,\"id\":\"expected\",\"cwd\":\"/workspace\"}\n",
        )
        .expect("synthetic Pi session");
        let adapter = PiAdapter::with_root(root);
        let session = SessionRef::new(Provider::Pi, "expected");

        let snapshot = adapter
            .read_session_at(&session, Some(&path))
            .expect("validated discovered path");

        assert!(snapshot.events.is_empty());
        assert!(adapter.read_session(&session).is_err());
    }

    #[test]
    fn stale_or_wrong_session_path_hints_fall_back_to_exact_lookup() {
        let temporary = tempfile::tempdir().expect("temporary Pi root");
        let root = temporary.path();
        let actual = root.join("expected.jsonl");
        let wrong = root.join("wrong.jsonl");
        fs::write(
            &actual,
            "{\"type\":\"session\",\"version\":3,\"id\":\"expected\",\"cwd\":\"/workspace\"}\n",
        )
        .expect("expected synthetic session");
        fs::write(
            &wrong,
            "{\"type\":\"session\",\"version\":3,\"id\":\"other\",\"cwd\":\"/workspace\"}\n",
        )
        .expect("other synthetic session");
        let stale = root.join("removed.jsonl");
        let adapter = PiAdapter::with_root(root);
        let session = SessionRef::new(Provider::Pi, "expected");

        assert!(adapter.read_session_at(&session, Some(&stale)).is_ok());
        assert!(adapter.read_session_at(&session, Some(&wrong)).is_ok());
    }

    #[test]
    fn an_archived_compaction_at_the_start_of_a_new_snapshot_does_not_select_older_requests() {
        let records = [
            json!({"type":"session","id":"session","version":3,"cwd":"/workspace"}),
            json!({"type":"compaction","id":"older","parentId":null,"firstKeptEntryId":"absent","summary":"Older summary"}),
            json!({"type":"message","id":"old-user","parentId":"older","message":{"role":"user","content":"An old request"}}),
            json!({"type":"compaction","id":"current","parentId":"old-user","firstKeptEntryId":"missing","summary":"Current summary"}),
        ];
        let snapshot =
            snapshot_from_records(&SessionRef::new(Provider::Pi, "session"), &records, 0).unwrap();
        let plan = omnis_core::plan_semantic_handoff(&snapshot);
        assert!(plan.objective.is_none());
        assert_eq!(plan.recent_messages.len(), 1);
        assert!(plan.recent_messages[0].text.ends_with("Current summary"));
        assert!(omnis_core::needs_recovery_handoff(&snapshot, false));
    }

    #[test]
    fn latest_compaction_keeps_older_summaries_in_place_even_when_its_own_text_is_empty() {
        let message = |id: &str, parent: &str| {
            json!({
                "type": "message", "id": id, "parentId": parent, "message": {"role": "user", "content": id}
            })
        };
        for summary in ["latest summary", ""] {
            let records = [
                json!({"type":"session","version":3,"id":"session","cwd":"/workspace"}),
                json!({"type":"message","id":"root","parentId":null,"message":{"role":"user","content":"recover earlier decision"}}),
                message("first retained", "root"),
                json!({"type":"compaction","id":"older","parentId":"first retained","firstKeptEntryId":"first retained","summary":"older summary"}),
                message("second retained", "older"),
                message("discarded sibling", "root"),
                json!({"type":"compaction","id":"latest","parentId":"second retained","firstKeptEntryId":"first retained","summary":summary}),
                message("final turn", "latest"),
            ];
            let session = SessionRef::new(Provider::Pi, "session");
            let snapshot = snapshot_from_records(&session, &records, 0).unwrap();
            let trajectory = omnis_core::import_trajectory(&snapshot);
            let conversation = omnis_core::import_conversation(&snapshot);
            for texts in [
                trajectory
                    .items
                    .iter()
                    .map(|item| item.text.as_str())
                    .collect::<Vec<_>>(),
                conversation
                    .messages
                    .iter()
                    .map(|msg| msg.text.as_str())
                    .collect(),
            ] {
                let turns = texts
                    .iter()
                    .filter(|text| !text.starts_with("[OmniSession"))
                    .map(|text| text.lines().last().unwrap())
                    .collect::<Vec<_>>();
                let mut expected = vec![
                    "first retained",
                    "older summary",
                    "second retained",
                    "final turn",
                ];
                if !summary.is_empty() {
                    expected.insert(0, summary);
                }
                assert_eq!(turns, expected);
            }
            assert_eq!(trajectory.truncated, summary.is_empty());
            assert!(omnis_core::needs_recovery_handoff(&snapshot, false));
            assert!(omnis_core::readback_trajectory(&snapshot).is_none());
            let mut handoff = Vec::new();
            omnis_core::write_recovery_handoff(&snapshot, &mut handoff).unwrap();
            let history = String::from_utf8(handoff).unwrap();
            assert!(history.contains("recover earlier decision"));
            assert!(!history.contains("discarded sibling"));
        }
    }

    #[test]
    fn omp_keeps_the_clear_marker_before_a_later_compaction_and_archives_older_summaries() {
        let header = json!({"type":"session","version":3,"id":"session","cwd":"/workspace","timestamp":"2026-01-01T00:00:00Z"});
        let message = |id: &str, parent: Option<&str>| {
            json!({
                "type":"message","id":id,"parentId":parent,"timestamp":"2026-01-01T00:00:00Z",
                "message":{"role":"user","content":id}
            })
        };
        let compact = |id: &str, parent: &str, kept: &str| {
            json!({
                "type":"compaction","id":id,"parentId":parent,"timestamp":"2026-01-01T00:00:00Z",
                "summary":id,"tokensBefore":10,"firstKeptEntryId":kept
            })
        };
        let records = [
            json!({"type":"title","v":1,"title":"Archived title","pad":"","source":"user","updatedAt":"2026-01-01T00:00:00Z"}),
            header,
            message("old request", None),
            compact("pre-clear summary", "old request", "old request"),
            json!({"type":"reset_boundary","id":"clear","parentId":"pre-clear summary","timestamp":"2026-01-01T00:00:00Z"}),
            message("first retained", Some("clear")),
            compact("intermediate summary", "first retained", "first retained"),
            message("second retained", Some("intermediate summary")),
            compact("latest summary", "second retained", "old request"),
            json!({"type":"title_change","id":"rename","parentId":"latest summary","timestamp":"2026-01-01T00:00:00Z","title":"Current title","source":"user"}),
            message("final turn", Some("rename")),
        ];
        let temporary = tempfile::tempdir().unwrap();
        let path = write_fixture(
            temporary.path(),
            "session.jsonl",
            &serialized_records(&records),
        );
        let session = SessionRef::new(Provider::OhMyPi, "session");
        let adapter = PiAdapter::oh_my_pi_with_root(temporary.path());
        let snapshot = assert_matches_snapshot_oracle(&adapter, &session, &path, &records, 0);
        let preview = adapter.preview_session_at(&session, Some(&path)).unwrap();
        let reset_and_titles = |snapshot: &omnis_ir::CanonicalSnapshot| {
            let marker = snapshot
                .events
                .iter()
                .position(|event| event.source.raw_record_type.as_deref() == Some("reset_boundary"))
                .unwrap();
            assert!(
                snapshot.events[..marker]
                    .iter()
                    .any(|event| event.payload["text"] == "old request")
            );
            let after = &snapshot.events[marker + 1..];
            assert_eq!(after[0].payload["summary"], "latest summary");
            assert!(
                after
                    .iter()
                    .any(
                        |event| event.source.raw_record_type.as_deref() == Some("title_change")
                            && event.payload["title"] == "Current title"
                    )
            );
            assert!(
                after
                    .iter()
                    .any(|event| event.payload["summary"] == "intermediate summary"
                        && event.replay_policy == ReplayPolicy::HistoricalOnly)
            );
        };
        reset_and_titles(&snapshot);
        reset_and_titles(&preview);
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
        let snapshot = snapshot_from_records_preview(
            &SessionRef::new(Provider::Pi, "session"),
            &records,
            records.len(),
        )
        .expect("partial Pi preview");
        assert_eq!(snapshot.events.len(), 1);
        assert_eq!(snapshot.events[0].kind, EventKind::MessageUser);
    }

    #[test]
    fn omp_preview_clears_a_title_only_when_the_tail_confirms_the_active_reset() {
        let root = tempfile::tempdir().unwrap();
        let adapter = PiAdapter::oh_my_pi_with_root(root.path());
        let session = SessionRef::new(Provider::OhMyPi, "current");
        let records = [
            json!({"type":"session","version":3,"id":"current","cwd":"/workspace"}),
            json!({"type":"message","id":"old","message":{"role":"user","content":"Archived request"}}),
            json!({"type":"reset_boundary","id":"clear","parentId":"old"}),
            json!({"type":"message","id":"reply","parentId":"clear","message":{"role":"assistant","content":[{"type":"text","text":"Ready"}]}}),
        ];
        let mut source = serialized_records(&records);
        let path = write_fixture(root.path(), "current.jsonl", &source);
        let read = || adapter.preview_session_at(&session, Some(&path)).unwrap();
        let snapshot = read();
        assert!(omnis_core::session_search_title_is_authoritative(
            &snapshot, false
        ));
        assert_eq!(
            omnis_core::session_preview(&snapshot).title,
            omnis_core::SessionPreviewTitle::AfterClear(None)
        );

        let padding =
            "x".repeat(usize::try_from(crate::support::MAX_TRANSCRIPT_LINE_SIZE).unwrap());
        let sibling = json!({"type":"message","id":"other","parentId":"old","message":{"role":"user","content":padding}});
        source.push_str(&sibling.to_string());
        fs::write(&path, source).unwrap();
        let sample = read();
        assert!(!omnis_core::session_search_title_is_authoritative(
            &sample, false
        ));
        assert_eq!(
            omnis_core::session_preview(&sample).title,
            omnis_core::SessionPreviewTitle::Unverified
        );
    }

    #[test]
    fn visible_summaries_extension_messages_and_images_reach_the_conversation() {
        let records = vec![
            json!({"type": "session", "version": 3, "id": "session", "cwd": "/workspace"}),
            json!({
                "type": "message", "id": "1", "parentId": null,
                "message": {"role": "user", "content": [{"type": "image", "data": "iVBORw0KGgo=", "mimeType": "image/png"}]}
            }),
            json!({"type": "branch_summary", "id": "2", "parentId": "1", "fromId": "9", "summary": "Explored approach A"}),
            json!({
                "type": "custom_message", "id": "3", "parentId": "2", "customType": "reminder",
                "content": [{"type": "text", "text": "Visible extension note"}], "display": true
            }),
            json!({
                "type": "custom_message", "id": "4", "parentId": "3", "customType": "hidden",
                "content": "Hidden extension context", "display": false
            }),
        ];

        let snapshot =
            snapshot_from_records(&SessionRef::new(Provider::Pi, "session"), &records, 0)
                .expect("Pi snapshot");

        let conversation = omnis_core::import_conversation(&snapshot);
        assert_eq!(
            conversation
                .messages
                .iter()
                .map(|message| (message.role, message.text.as_str()))
                .collect::<Vec<_>>(),
            [
                (omnis_core::HandoffRole::User, "[1 image omitted]"),
                (
                    omnis_core::HandoffRole::Assistant,
                    "[Source branch summary]\nExplored approach A"
                ),
                (
                    omnis_core::HandoffRole::User,
                    "[Source extension message: reminder]\nVisible extension note"
                ),
            ]
        );
        let rendered = serde_json::to_string(&snapshot).expect("serialize Pi snapshot");
        assert!(!rendered.contains("Hidden extension context"));
        assert!(!rendered.contains("iVBORw0KGgo"));
    }

    #[test]
    fn indexed_read_matches_oracle_for_duplicate_ids_compaction_and_malformed_lines() {
        let temporary = tempfile::tempdir().expect("temporary Pi root");
        let root = temporary.path().join("sessions");
        let records = vec![
            json!({
                "type": "session", "version": 3, "id": "session", "cwd": "/workspace",
                "timestamp": "2026-01-01T00:00:00Z"
            }),
            json!({
                "type": "message", "id": "root", "parentId": null,
                "timestamp": "2026-01-01T00:00:01Z",
                "message": {"role": "user", "content": "retained root"}
            }),
            json!({
                "type": "message", "id": "duplicate", "parentId": "root",
                "timestamp": "2026-01-01T00:00:02Z",
                "message": {"role": "user", "content": "superseded duplicate"}
            }),
            json!({
                "type": "message", "id": "alternate", "parentId": "root",
                "timestamp": "2026-01-01T00:00:03Z",
                "message": {"role": "user", "content": "selected branch"}
            }),
            json!({
                "type": "branch_summary", "id": "summary", "parentId": "duplicate",
                "timestamp": "2026-01-01T00:00:04Z", "summary": "abandoned summary"
            }),
            json!({
                "type": "compaction", "id": "old-compact", "parentId": "summary",
                "timestamp": "2026-01-01T00:00:05Z", "firstKeptEntryId": "duplicate"
            }),
            json!({
                "type": "message", "id": "old-tail", "parentId": "old-compact",
                "timestamp": "2026-01-01T00:00:06Z",
                "message": {"role": "user", "content": "abandoned tail"}
            }),
            json!({
                "type": "message", "id": "duplicate", "parentId": "alternate",
                "timestamp": "2026-01-01T00:00:07Z",
                "message": {"role": "user", "content": "latest duplicate"}
            }),
            json!({
                "type": "compaction", "id": "latest-compact", "parentId": "duplicate",
                "timestamp": "2026-01-01T00:00:08Z", "firstKeptEntryId": "root",
                "summary": "current compacted context"
            }),
            json!({
                "type": "message", "id": "final", "parentId": "latest-compact",
                "timestamp": "2026-01-01T00:00:09Z",
                "message": {"role": "user", "content": "latest request"}
            }),
            json!({
                "type": "title_change", "title": "late title",
                "timestamp": "2026-01-01T00:00:10Z"
            }),
        ];
        let path = write_fixture(
            &root,
            "pi.jsonl",
            &format!(
                "leading malformed line\n{}trailing malformed line\n",
                serialized_records(&records)
            ),
        );
        let adapter = PiAdapter::with_root(&root);
        let session = SessionRef::new(Provider::Pi, "session");

        let snapshot = assert_matches_snapshot_oracle(&adapter, &session, &path, &records, 0);

        assert_eq!(snapshot.title.as_deref(), Some("late title"));
        assert_eq!(
            snapshot.captured_at.to_rfc3339(),
            "2026-01-01T00:00:10+00:00"
        );
        assert_eq!(
            snapshot
                .events
                .iter()
                .filter_map(|event| event.payload.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>(),
            [
                "retained root",
                "selected branch",
                "latest duplicate",
                "latest request"
            ]
        );
        assert_eq!(snapshot.events[0].kind, EventKind::CompactionCreated);
    }

    #[test]
    fn sampled_omp_rename_requires_a_known_path_to_the_leaf() {
        let session = SessionRef::new(Provider::OhMyPi, "session");
        let slot = json!({"type":"title","v":1,"title":"Original"});
        let header = json!({"type":"session","version":3,"id":"session","cwd":"/test"});
        let reset = json!({"type":"reset_boundary","id":"reset","parentId":null});
        let rename = json!({"type":"title_change","id":"name","parentId":"missing older entry","title":"Active"});
        let middle = json!({"type":"model_change","id":"middle","parentId":"name"});
        let leaf = json!({"type":"message","id":"leaf","parentId":"middle","message":{"role":"assistant","content":"Reply"}});
        let original = vec![slot, header, reset, rename, middle, leaf];
        for (name, kind, parent, tail, expected) in [
            ("Active", "model_change", "name", 3, true),
            ("Active", "model_change", "name", 2, false),
            ("Active", "model_change", "sibling", 3, false),
            ("Active", "future_entry", "name", 3, false),
            ("Active", "model_change", "name", 0, false),
            (" ", "model_change", "name", 3, false),
        ] {
            let mut records = original.clone();
            records[3]["title"] = name.into();
            records[4]["type"] = kind.into();
            records[4]["parentId"] = parent.into();
            let snapshot = snapshot_from_records_preview(&session, &records, tail).unwrap();
            let title = omnis_core::session_search_title(&snapshot);
            assert_eq!(
                title.as_deref(),
                Some(if expected { "Active" } else { "Original" })
            );
            assert_eq!(
                omnis_core::session_search_title_is_authoritative(&snapshot, false),
                expected
            );
            assert_eq!(
                omnis_core::session_preview(&snapshot).title
                    == omnis_core::SessionPreviewTitle::VerifiedRename("Active".into()),
                expected
            );
        }
        let mut unknown_leaf = original;
        unknown_leaf.push(json!({"type":"future_entry_without_id"}));
        let snapshot = snapshot_from_records_preview(&session, &unknown_leaf, 4).unwrap();
        assert!(!omnis_core::session_search_title_is_authoritative(
            &snapshot, false
        ));
        assert_eq!(
            omnis_core::session_search_title(&snapshot).as_deref(),
            Some("Original")
        );
    }

    #[test]
    fn omp_title_follows_the_selected_branch_for_full_and_sampled_reads() {
        let root = tempfile::tempdir().unwrap();
        let adapter = PiAdapter::oh_my_pi_with_root(root.path());
        let session = SessionRef::new(Provider::OhMyPi, "example");
        for (slot, branch_name, expected) in [
            ("File title", None, Some("File title")),
            ("File title", Some("Active rename"), Some("Active rename")),
            ("", None, None),
        ] {
            let mut rows = vec![
                json!({"type":"title","v":1,"title":slot}),
                json!({"type":"session","id":"example","version":3,"cwd":"/test"}),
                json!({"type":"reset_boundary","id":"clear","parentId":null}),
            ];
            if let Some(name) = branch_name {
                rows.push(
                    json!({"type":"title_change","id":"name","parentId":"clear","title":name}),
                );
            }
            rows.push(json!({"type":"title_change","id":"sibling","parentId":null,"title":"Other branch"}));
            rows.push(json!({"type":"message","id":"leaf","parentId":if branch_name.is_some() {"name"} else {"clear"},
                "message":{"role":"assistant","content":"Current reply"}}));
            let path = write_fixture(root.path(), "example.jsonl", &serialized_records(&rows));
            for snapshot in [
                adapter.read_session_at(&session, Some(&path)).unwrap(),
                adapter.preview_session_at(&session, Some(&path)).unwrap(),
            ] {
                assert_eq!(snapshot.title.as_deref(), expected);
                assert_eq!(
                    omnis_core::session_search_title(&snapshot).as_deref(),
                    expected
                );
            }
        }
    }

    #[test]
    fn indexed_read_keeps_omp_title_slot_out_of_the_entry_tree() {
        let temporary = tempfile::tempdir().expect("temporary OMP root");
        let root = temporary.path().join("sessions");
        let adapter = PiAdapter::oh_my_pi_with_root(&root);
        let title = json!({
            "type": "title", "id": "title-entry", "title": "OMP title slot",
            "timestamp": "2026-01-01T00:00:00Z"
        });
        let header = json!({
            "type": "session", "version": 3, "id": "omp-session", "cwd": "/workspace",
            "timestamp": "2026-01-01T00:00:01Z"
        });
        let records = vec![title.clone(), header.clone()];
        let path = write_fixture(&root, "title-only.jsonl", &serialized_records(&records));
        let session = SessionRef::new(Provider::OhMyPi, "omp-session");

        let snapshot = assert_matches_snapshot_oracle(&adapter, &session, &path, &records, 0);

        assert_eq!(snapshot.title.as_deref(), Some("OMP title slot"));
        assert!(snapshot.events.is_empty());

        let child = json!({
            "type": "message", "id": "child", "parentId": "title-entry",
            "timestamp": "2026-01-01T00:00:02Z",
            "message": {"role": "user", "content": "synthetic child"}
        });
        let child_records = vec![title, header, child];
        let child_path = write_fixture(
            &root,
            "title-parent.jsonl",
            &serialized_records(&child_records),
        );
        let actual_error = adapter
            .read_session_at(&session, Some(&child_path))
            .expect_err("title slot is not part of the entry tree");
        let expected_error = snapshot_from_records(&session, &child_records, 0)
            .expect_err("oracle excludes the first title slot");
        assert_eq!(actual_error.to_string(), expected_error.to_string());
        assert_eq!(
            actual_error.to_string(),
            "Pi session tree contains a missing leaf"
        );
    }

    #[test]
    fn indexed_read_matches_oracle_with_oversized_line_and_late_metadata() {
        let temporary = tempfile::tempdir().expect("temporary Pi root");
        let root = temporary.path().join("sessions");
        let records = vec![
            json!({
                "type": "session", "version": 3, "id": "session", "cwd": "/workspace",
                "timestamp": "2026-01-01T00:00:00Z"
            }),
            json!({
                "type": "message", "id": "user", "parentId": null,
                "timestamp": "2026-01-01T00:00:01Z",
                "message": {"role": "user", "content": "kept request"}
            }),
            json!({
                "type": "session_info", "name": "late metadata",
                "timestamp": "2026-01-01T00:00:03Z"
            }),
        ];
        let mut document = serialized_records(&records[..2]);
        document.push_str("{\"synthetic_padding\":\"");
        document.push_str(&"x".repeat(16 * 1024 * 1024));
        document.push_str("\"}\n");
        document.push_str(&records[2].to_string());
        document.push('\n');
        let path = write_fixture(&root, "oversized.jsonl", &document);
        let adapter = PiAdapter::with_root(&root);
        let session = SessionRef::new(Provider::Pi, "session");

        let snapshot = assert_matches_snapshot_oracle(&adapter, &session, &path, &records, 1);

        assert_eq!(snapshot.title.as_deref(), Some("late metadata"));
        assert_eq!(
            snapshot.captured_at.to_rfc3339(),
            "2026-01-01T00:00:03+00:00"
        );
        let omission_notice = snapshot
            .events
            .iter()
            .find(|event| {
                event.source.raw_record_type.as_deref() == Some("omnisession.record_size_limit")
            })
            .expect("oversized line notice");
        assert_eq!(omission_notice.payload["omitted_records"], 1);
    }

    #[test]
    fn indexed_read_preserves_strict_missing_parent_error() {
        let temporary = tempfile::tempdir().expect("temporary Pi root");
        let root = temporary.path().join("sessions");
        let records = vec![
            json!({
                "type": "session", "version": 3, "id": "session", "cwd": "/workspace"
            }),
            json!({
                "type": "message", "id": "leaf", "parentId": "missing",
                "message": {"role": "user", "content": "synthetic"}
            }),
        ];
        let path = write_fixture(&root, "missing-parent.jsonl", &serialized_records(&records));
        let adapter = PiAdapter::with_root(&root);
        let session = SessionRef::new(Provider::Pi, "session");

        let actual = adapter
            .read_session_at(&session, Some(&path))
            .expect_err("strict Pi read rejects a missing parent");
        let expected = snapshot_from_records(&session, &records, 0)
            .expect_err("snapshot oracle rejects a missing parent");
        assert_eq!(actual.to_string(), expected.to_string());

        let cycle_records = vec![
            json!({
                "type": "session", "version": 3, "id": "cycle", "cwd": "/workspace"
            }),
            json!({"type": "message", "id": "a", "parentId": "b"}),
            json!({"type": "message", "id": "b", "parentId": "a"}),
        ];
        let cycle_path = write_fixture(&root, "cycle.jsonl", &serialized_records(&cycle_records));
        let cycle_session = SessionRef::new(Provider::Pi, "cycle");
        let actual_cycle = adapter
            .read_session_at(&cycle_session, Some(&cycle_path))
            .expect_err("strict Pi read rejects a parent cycle");
        let expected_cycle = snapshot_from_records(&cycle_session, &cycle_records, 0)
            .expect_err("snapshot oracle rejects a parent cycle");
        assert_eq!(actual_cycle.to_string(), expected_cycle.to_string());
    }

    #[cfg(unix)]
    #[test]
    fn file_stamp_detects_same_length_in_place_updates() {
        let temporary = tempfile::tempdir().expect("temporary Pi root");
        let path = temporary.path().join("session.jsonl");
        fs::write(&path, b"first").expect("write synthetic session");
        let file = fs::File::open(&path).expect("open synthetic session");
        let original = PiFileStamp::read(&file).expect("initial file stamp");

        std::thread::sleep(std::time::Duration::from_secs(1));
        fs::write(&path, b"other").expect("rewrite synthetic session in place");

        assert_ne!(
            original,
            PiFileStamp::read(&file).expect("updated file stamp")
        );
        assert!(
            original
                .ensure_unchanged(&file)
                .expect_err("same-length content rewrite is detected")
                .to_string()
                .contains("Pi session changed during read")
        );
    }
}
