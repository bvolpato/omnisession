use std::{
    collections::{HashMap, HashSet},
    fs,
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, Ordering},
    },
};

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use omnis_ir::{CanonicalSnapshot, EventKind, Provider, ReplayPolicy, SessionRef, SourceRevision};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    IndexRead, LaunchPlan, LaunchTarget, NativeResumeSupport, NativeSession, ProviderAdapter,
    ProviderInstallation,
    support::{
        EventBuilder, JsonLineSkips, MAX_COLLECTED_TRANSCRIPT_FILE_SIZE, MAX_PREVIEW_TAIL_SIZE,
        MAX_STREAMED_TRANSCRIPT_LINE_SIZE, MAX_TRANSCRIPT_LINE_SIZE, SnapshotFileStamp,
        SqliteSnapshotCache, encode_uri_component, json_lines_preview_bounded, nested_files,
        parse_timestamp, paths_match, percent_decode, provider_executable, provider_file,
        provider_root, read_json, sort_sessions, string_at, validate_provider, value_at,
        visit_json_lines_with_omissions,
    },
};

#[derive(Clone, Debug)]
pub struct GrokAdapter {
    sessions_root: Option<PathBuf>,
    catalog: Arc<SqliteSnapshotCache>,
    has_ambiguous_ids: Arc<AtomicBool>,
    checked_updates: Arc<Mutex<Option<CheckedUpdates>>>,
    listed_catalog: Arc<RwLock<Option<HashMap<String, NativeSession>>>>,
}

#[derive(Debug)]
struct CheckedUpdates {
    path: PathBuf,
    stamp: SnapshotFileStamp,
    support: NativeResumeSupport,
    expected_updates: u64,
}

#[derive(Deserialize)]
struct UpdateHeader {
    params: Option<UpdateParams>,
    #[serde(rename = "sessionUpdate", alias = "type", alias = "event")]
    legacy_kind: Option<String>,
}

#[derive(Deserialize)]
struct UpdateParams {
    update: Option<serde::de::IgnoredAny>,
    #[serde(default, rename = "sessionId", deserialize_with = "present_owner")]
    session_id: Option<String>,
}

fn present_owner<'de, D: serde::Deserializer<'de>>(
    de: D,
) -> std::result::Result<Option<String>, D::Error> {
    String::deserialize(de).map(Some)
}

impl GrokAdapter {
    #[must_use]
    pub fn with_root(sessions_root: impl Into<PathBuf>) -> Self {
        Self {
            sessions_root: Some(sessions_root.into()),
            catalog: Arc::default(),
            has_ambiguous_ids: Arc::default(),
            checked_updates: Arc::default(),
            listed_catalog: Arc::default(),
        }
    }

    /// Size of the update log that supplies searchable conversation text.
    #[must_use]
    pub fn updates_size(summary: &Path) -> Option<u64> {
        let directory = summary.parent()?;
        let file = provider_file(directory, &directory.join("updates.jsonl"))?;
        Some(fs::metadata(file).ok()?.len())
    }

    fn summaries(&self) -> Vec<PathBuf> {
        self.sessions_root
            .as_deref()
            .and_then(|root| root.canonicalize().ok())
            .map(|root| nested_files(&root, 2, Some("summary.json")))
            .unwrap_or_default()
    }

    fn find_summary(&self, id: &str) -> Result<Option<(Value, PathBuf)>> {
        if let Some(file) = self.direct_summary(id)? {
            let summary = read_json(&file).with_context(|| {
                format!("cannot verify the Grok summary in session directory `{id}`")
            })?;
            anyhow::ensure!(
                session_id(&summary, &file).as_deref() == Some(id),
                "the Grok summary in session directory `{id}` identifies a different session"
            );
            return Ok(Some((summary, file)));
        }
        unique_summary(id, self.summaries())
    }

    fn direct_summary(&self, id: &str) -> Result<Option<PathBuf>> {
        let Some(root) = self.sessions_root.as_deref() else {
            return Ok(None);
        };
        if uuid::Uuid::parse_str(id).is_err() {
            return Ok(None);
        }
        let Ok(entries) = fs::read_dir(root) else {
            return Ok(None);
        };
        let mut candidates = entries.flatten().filter_map(|entry| {
            entry.file_type().ok()?.is_dir().then_some(())?;
            let directory = entry.path().join(id);
            directory.symlink_metadata().ok()?.is_dir().then_some(())?;
            provider_file(root, &directory.join("summary.json"))
        });
        let first = candidates.next();
        if candidates.next().is_some() {
            anyhow::bail!("multiple Grok summaries match session `{id}` in different directories");
        }
        Ok(first)
    }

    fn native_location_matches(&self, id: &str, summary: &Value, path: &Path) -> bool {
        let Some(root) = self.sessions_root.as_deref() else {
            return false;
        };
        let stored = stored_workspace(path);
        let cwd = summary
            .pointer("/info/cwd")
            .and_then(Value::as_str)
            .filter(|cwd| !cwd.is_empty())
            .or_else(|| stored.as_deref()?.to_str());
        cwd.filter(|cwd| Path::new(cwd).is_absolute())
            .is_some_and(|cwd| {
                let expected = root
                    .join(encode_uri_component(cwd))
                    .join(id)
                    .join("summary.json");
                has_native_directory(path, id)
                    && provider_file(root, &expected).as_deref() == Some(path)
            })
    }

    fn native_updates_support(
        &self,
        id: &str,
        path: &Path,
        summary: &Value,
    ) -> Result<NativeResumeSupport> {
        let root = self
            .sessions_root
            .as_deref()
            .context("Grok sessions root is unavailable")?;
        let directory = path
            .parent()
            .context("Grok summary has no parent directory")?;
        let expected_updates = declared_message_count(summary);
        let candidate = directory.join("updates.jsonl");
        let Some(updates) = provider_file(root, &candidate) else {
            anyhow::ensure!(
                candidate
                    .symlink_metadata()
                    .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound),
                "Grok updates are not a readable file in the selected session directory"
            );
            anyhow::ensure!(
                expected_updates == 0,
                "Grok session `{id}` declares history but has no readable updates"
            );
            return Ok(NativeResumeSupport::Direct);
        };
        let before = SnapshotFileStamp::read(&updates)?;
        if let Some(checked) = self
            .checked_updates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            && checked.path == updates
            && checked.stamp == before
            && checked.expected_updates == expected_updates
        {
            return Ok(checked.support);
        }
        let support = native_update_headers(&updates, id, expected_updates)?;
        anyhow::ensure!(
            SnapshotFileStamp::read(&updates)? == before,
            "selected Grok source changed during native ownership verification"
        );
        *self
            .checked_updates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(CheckedUpdates {
            path: updates,
            stamp: before,
            support,
            expected_updates,
        });
        Ok(support)
    }

    fn catalog_sessions(&self, id: Option<&str>) -> Vec<NativeSession> {
        let Some(root) = self.sessions_root.as_deref() else {
            return Vec::new();
        };
        let path = root.join("session_search.sqlite");
        let revision = SourceRevision {
            file: Some(path_id(&path)),
            ..SourceRevision::default()
        };
        self.catalog
            .read(root, &path, |connection| {
                let all = "SELECT session_id, cwd, updated_at, title FROM session_docs";
                let sql = id.map_or_else(
                    || all.to_owned(),
                    |_| format!("{all} WHERE session_id = ?1"),
                );
                let mut statement = connection.prepare(&sql)?;
                let rows = statement.query_map(rusqlite::params_from_iter(id), |row| {
                    Ok(NativeSession {
                        session: SessionRef::new(Provider::Grok, row.get::<_, String>(0)?),
                        project_path: Some(PathBuf::from(row.get::<_, String>(1)?)),
                        updated_at: DateTime::from_timestamp(row.get(2)?, 0),
                        title: row.get(3)?,
                        git_branch: None,
                        created_at: None,
                        updated_at_approximate: false,
                        event_count: 0,
                        source_path: None,
                        source_revision: Some(revision),
                    })
                })?;
                Ok(rows
                    .flatten()
                    .filter(|row| uuid::Uuid::parse_str(&row.session.id).is_ok())
                    .collect())
            })
            .unwrap_or_default()
    }

    fn resolved_summary(
        &self,
        id: &str,
        listed_path: Option<&Path>,
    ) -> Result<(Value, PathBuf, GrokMetadata)> {
        let (summary, path) = self
            .find_summary(id)?
            .ok_or_else(|| anyhow!("Grok session `{id}` was not found"))?;
        let metadata = self.resolved_metadata(id, &summary, &path, listed_path);
        Ok((summary, path, metadata))
    }

    fn resolved_metadata(
        &self,
        id: &str,
        summary: &Value,
        path: &Path,
        listed_path: Option<&Path>,
    ) -> GrokMetadata {
        let mut data = metadata(summary);
        if data.needs_metadata(summary) {
            // Index workers share the catalog that discovered this source.
            let listed = listed_path.filter(|hint| *hint == path).and_then(|_| {
                self.listed_catalog
                    .read()
                    .ok()?
                    .as_ref()
                    .map(|rows| rows.get(id).cloned())
            });
            let catalog = listed.unwrap_or_else(|| self.catalog_sessions(Some(id)).pop());
            data.fill_missing(summary, path, catalog.as_ref());
        }
        data
    }

    fn snapshot(
        &self,
        session: &SessionRef,
        summary: &Value,
        path: &Path,
        metadata: &GrokMetadata,
        full: bool,
    ) -> Result<CanonicalSnapshot> {
        const SAMPLE_RECORDS: usize = 1_024;
        let root = self
            .sessions_root
            .as_deref()
            .context("Grok sessions root is unavailable")?;
        let directory = path
            .parent()
            .context("Grok summary has no parent directory")?;
        let mut builder = EventBuilder::new(Provider::Grok, &session.id);
        push_grok_session_metadata(&mut builder, summary);
        let mut updates = GrokUpdateProcessor::default();
        let mut read_any = false;
        let mut observed = 0_u64;
        let mut sampled = false;
        let mut omissions = JsonLineSkips::default();
        if let Some(file) = provider_file(root, &directory.join("updates.jsonl")) {
            let mut push = |record: Value| -> Result<()> {
                // Grok rekeys inherited ACP updates when it creates a native fork.
                anyhow::ensure!(
                    record
                        .pointer("/params/sessionId")
                        .is_none_or(|owner| owner.as_str() == Some(&session.id)),
                    "Grok updates contain a different session ID"
                );
                read_any = true;
                if record
                    .pointer("/params/update")
                    .is_some_and(|update| !update.is_null())
                    || string_at(&record, &[&["sessionUpdate"], &["type"], &["event"]]).is_some()
                {
                    observed += 1;
                }
                updates.push_record(&mut builder, record);
                Ok(())
            };
            if full {
                omissions = visit_json_lines_with_omissions(
                    &file,
                    MAX_COLLECTED_TRANSCRIPT_FILE_SIZE,
                    push,
                )?;
            } else {
                let preview =
                    json_lines_preview_bounded(&file, SAMPLE_RECORDS, MAX_PREVIEW_TAIL_SIZE)?;
                sampled = preview.sampled;
                omissions = preview.omissions;
                for record in preview.records {
                    push(record)?;
                }
            }
        }
        if !read_any && declared_message_count(summary) > 0 {
            anyhow::bail!(
                "Grok session `{}` declares history but has no readable updates",
                session.id
            );
        }
        updates.finish(&mut builder);
        if !sampled && let Some(expected) = metadata.expected_updates {
            let records = observed.saturating_add(omissions.total() as u64);
            if expected > records {
                builder.push(
                    EventKind::ProviderEvent,
                    json!({"omitted_events": expected - records, "expected_updates": expected, "found_updates": records}),
                    metadata.updated_at,
                    ReplayPolicy::HistoricalOnly,
                    Some("omnisession.grok_missing_updates".to_owned()),
                    None,
                );
            }
        }
        push_grok_state(&mut builder, root, path);
        let line_limit = if full {
            MAX_STREAMED_TRANSCRIPT_LINE_SIZE
        } else {
            MAX_TRANSCRIPT_LINE_SIZE
        };
        builder.push_json_line_omissions(omissions, line_limit, None, metadata.updated_at);
        Ok(builder.snapshot(
            session.clone(),
            metadata.title.clone(),
            metadata.project_path.clone(),
            metadata.git_branch.clone(),
            metadata.updated_at.unwrap_or_else(Utc::now),
        ))
    }
}

impl Default for GrokAdapter {
    fn default() -> Self {
        Self {
            sessions_root: provider_root("GROK_HOME", &[".grok"]).map(|root| root.join("sessions")),
            catalog: Arc::default(),
            has_ambiguous_ids: Arc::default(),
            checked_updates: Arc::default(),
            listed_catalog: Arc::default(),
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
    expected_updates: Option<u64>,
}

struct PendingMessage {
    kind: EventKind,
    text: String,
    content: Vec<Value>,
    timestamp: Option<DateTime<Utc>>,
    raw_type: Option<String>,
    message_id: Option<String>,
}

fn stored_workspace(path: &Path) -> Option<PathBuf> {
    let group = path.parent()?.parent()?.file_name()?.to_str()?;
    Some(PathBuf::from(percent_decode(group)?)).filter(|cwd| cwd.is_absolute())
}

fn has_native_directory(path: &Path, id: &str) -> bool {
    uuid::Uuid::parse_str(id).is_ok()
        && path
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|name| name == id)
}

fn can_fork_summary(id: &str, summary: &Value) -> bool {
    summary.pointer("/info/id").and_then(Value::as_str) == Some(id)
        && ["/info/cwd", "/session_summary", "/current_model_id"]
            .iter()
            .all(|field| summary.pointer(field).is_some_and(Value::is_string))
        && ["created_at", "updated_at"].iter().all(|field| {
            summary
                .get(field)
                .and_then(Value::as_str)
                .is_some_and(|value| DateTime::parse_from_rfc3339(value).is_ok())
        })
        && summary
            .get("num_messages")
            .and_then(Value::as_u64)
            .is_some()
}

// A native fork rekeys every update it copies. Check the source owners before handing it to Grok.
fn native_update_headers(
    path: &Path,
    id: &str,
    expected_updates: u64,
) -> Result<NativeResumeSupport> {
    let file = fs::File::open(path)?;
    let bytes = file.metadata()?.len();
    let mut reader = BufReader::new(file.take(bytes));
    let mut line = Vec::new();
    let mut observed = 0_u64;
    loop {
        line.clear();
        let size = reader
            .by_ref()
            .take(MAX_STREAMED_TRANSCRIPT_LINE_SIZE + 1)
            .read_until(b'\n', &mut line)?;
        if size == 0 {
            break;
        }
        if size as u64 > MAX_STREAMED_TRANSCRIPT_LINE_SIZE {
            return Ok(NativeResumeSupport::ImportOnly);
        }
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let Ok(header) = serde_json::from_slice::<UpdateHeader>(&line) else {
            return Ok(NativeResumeSupport::ImportOnly);
        };
        anyhow::ensure!(
            header
                .params
                .as_ref()
                .and_then(|params| params.session_id.as_deref())
                .is_none_or(|owner| owner == id),
            "Grok updates contain a different session ID"
        );
        if header.legacy_kind.is_none()
            && header.params.is_none_or(|params| params.update.is_none())
        {
            return Ok(NativeResumeSupport::ImportOnly);
        }
        observed += 1;
    }
    anyhow::ensure!(
        observed > 0 || expected_updates == 0,
        "Grok session `{id}` declares history but has no readable updates"
    );
    Ok(if observed < expected_updates {
        NativeResumeSupport::ImportOnly
    } else {
        NativeResumeSupport::Direct
    })
}

fn path_id(path: &Path) -> Uuid {
    Uuid::new_v5(&Uuid::NAMESPACE_URL, path.as_os_str().as_encoded_bytes())
}

fn unique_summary(id: &str, paths: Vec<PathBuf>) -> Result<Option<(Value, PathBuf)>> {
    let mut matching = paths
        .into_iter()
        .filter_map(|path| matching_summary(id, path));
    let first = matching.next();
    if matching.next().is_some() {
        anyhow::bail!("multiple Grok summaries match session `{id}` in different directories");
    }
    Ok(first)
}

fn matching_summary(id: &str, path: PathBuf) -> Option<(Value, PathBuf)> {
    let value = read_json(&path).ok()?;
    (session_id(&value, &path).as_deref() == Some(id)).then_some((value, path))
}

fn session_id(summary: &Value, path: &Path) -> Option<String> {
    string_at(
        summary,
        &[
            &["info", "id"],
            &["id"],
            &["session_id"],
            &["sessionId"],
            &["session", "id"],
        ],
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

const TITLES: &[&[&str]] = &[
    &["generated_title"],
    &["session_summary"],
    &["title"],
    &["name"],
    &["session", "title"],
    &["metadata", "title"],
];
const WORKSPACES: &[&[&str]] = &[
    &["info", "cwd"],
    &["cwd"],
    &["project_path"],
    &["projectPath"],
    &["session", "cwd"],
    &["metadata", "cwd"],
];

impl GrokMetadata {
    fn needs_metadata(&self, summary: &Value) -> bool {
        (self.title.is_none() && value_at(summary, TITLES).is_none()) || self.project_path.is_none()
    }

    fn fill_missing(&mut self, summary: &Value, path: &Path, catalog: Option<&NativeSession>) {
        if !self.needs_metadata(summary) {
            return;
        }
        // Grok stores ordinary sessions under a percent-encoded absolute workspace path.
        let stored_cwd = stored_workspace(path);
        let catalog_cwd = catalog
            .and_then(|row| row.project_path.as_deref())
            .filter(|cwd| cwd.is_absolute());
        if let Some(catalog_cwd) = catalog_cwd
            && [self.project_path.as_deref(), stored_cwd.as_deref()]
                .into_iter()
                .flatten()
                .any(|cwd| !paths_match(cwd, catalog_cwd))
        {
            return;
        }
        if self.project_path.is_none() {
            self.project_path = stored_cwd.or_else(|| catalog_cwd.map(Path::to_path_buf));
        }
        if self.title.is_none() && value_at(summary, TITLES).is_none() && catalog_cwd.is_some() {
            self.title = catalog
                .and_then(|row| row.title.clone())
                .filter(|title| !title.is_empty());
        }
    }

    fn source_revision(&self, path: &Path) -> SourceRevision {
        let file = path_id(path);
        let project = self.project_path.as_deref().unwrap_or(Path::new(""));
        // Older projections did not check ownership and the native update count.
        let checked = Uuid::new_v5(&file, b"grok.owned-update-count.v1");
        let checked = Uuid::new_v5(
            &checked,
            &self.expected_updates.unwrap_or_default().to_le_bytes(),
        );
        let scope = Uuid::new_v5(&checked, project.as_os_str().as_encoded_bytes());
        let metadata = Uuid::new_v5(&scope, self.title.as_deref().unwrap_or_default().as_bytes());
        let contents = path
            .parent()
            .and_then(|directory| {
                let updates = provider_file(directory, &directory.join("updates.jsonl"))?;
                Some(SnapshotFileStamp::read(&updates).ok()?.revision(metadata))
            })
            .unwrap_or(metadata);
        SourceRevision {
            file: Some(file),
            contents: Some(contents),
            ..SourceRevision::default()
        }
    }

    fn into_session(self, id: String, source: PathBuf) -> NativeSession {
        let revision = self.source_revision(&source);
        NativeSession {
            session: SessionRef::new(Provider::Grok, id),
            title: self.title,
            project_path: self.project_path,
            git_branch: self.git_branch,
            created_at: self.created_at,
            updated_at: self.updated_at,
            updated_at_approximate: false,
            event_count: 0,
            source_revision: Some(revision),
            source_path: Some(source),
        }
    }
}

fn metadata(summary: &Value) -> GrokMetadata {
    GrokMetadata {
        expected_updates: summary.get("num_messages").and_then(Value::as_u64),
        title: string_at(summary, TITLES).map(str::to_owned),
        project_path: string_at(summary, WORKSPACES).map(PathBuf::from),
        git_branch: string_at(
            summary,
            &[
                &["git_branch"],
                &["gitBranch"],
                &["head_branch"],
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

fn declared_message_count(summary: &Value) -> u64 {
    // Native chat counts include system prompts and reminders before any visible ACP updates.
    summary
        .get("num_messages")
        .and_then(Value::as_u64)
        .or_else(|| {
            value_at(
                summary,
                &[
                    &["num_chat_messages"],
                    &["message_count"],
                    &["session", "num_messages"],
                ],
            )
            .and_then(Value::as_u64)
        })
        .unwrap_or_default()
}

#[derive(Default)]
struct GrokUpdateProcessor {
    pending: Option<PendingMessage>,
    preservation: GrokPreservationBudget,
    unsupported_text_records: usize,
}

struct GrokPreservationBudget {
    bytes_left: usize,
    records_left: usize,
    omitted_events: usize,
}

impl Default for GrokPreservationBudget {
    fn default() -> Self {
        Self {
            bytes_left: 64 * 1024 * 1024,
            records_left: 100_000,
            omitted_events: 0,
        }
    }
}

impl GrokPreservationBudget {
    fn retain(&mut self, value: &Value) -> bool {
        let bytes = estimated_value_size(value, self.bytes_left);
        if self.records_left == 0 || bytes > self.bytes_left {
            self.omitted_events = self.omitted_events.saturating_add(1);
            return false;
        }
        self.records_left -= 1;
        self.bytes_left -= bytes;
        true
    }
}

fn estimated_value_size(value: &Value, limit: usize) -> usize {
    // Charge JSON allocations conservatively without serializing large historical payloads.
    let mut size = 256_usize;
    match value {
        Value::String(text) => size = size.saturating_add(text.len()),
        Value::Array(values) => {
            for value in values {
                size = size.saturating_add(estimated_value_size(value, limit.saturating_sub(size)));
                if size > limit {
                    break;
                }
            }
        }
        Value::Object(values) => {
            for (key, value) in values {
                size = size
                    .saturating_add(key.len())
                    .saturating_add(estimated_value_size(value, limit.saturating_sub(size)));
                if size > limit {
                    break;
                }
            }
        }
        _ => {}
    }
    size
}

impl GrokUpdateProcessor {
    fn push_record(&mut self, builder: &mut EventBuilder, mut record: Value) {
        let timestamp = parse_timestamp(value_at(
            &record,
            &[&["timestamp"], &["created_at"], &["createdAt"]],
        ));
        let nested_update = record
            .as_object_mut()
            .and_then(|object| object.get_mut("params"))
            .and_then(Value::as_object_mut)
            .and_then(|params| params.remove("update"));
        let update = nested_update.unwrap_or(record);
        let raw_type =
            string_at(&update, &[&["sessionUpdate"], &["type"], &["event"]]).map(str::to_owned);
        let update_type = raw_type.as_deref().unwrap_or("");
        let message_kind = match update_type {
            "user_message_chunk" | "user" | "user_message" => Some(EventKind::MessageUser),
            "agent_message_chunk" | "assistant" | "assistant_message" => {
                Some(EventKind::MessageAssistant)
            }
            "agent_thought_chunk" => Some(EventKind::ReasoningSummary),
            _ => None,
        };
        if let Some(kind) = message_kind {
            if kind == EventKind::ReasoningSummary && !self.preservation.retain(&update) {
                flush_message(builder, &mut self.pending);
                return;
            }
            let message_id = string_at(
                &update,
                &[&["messageId"], &["message_id"], &["message", "id"]],
            )
            .map(str::to_owned);
            let (text, content) = message_content(&update, &mut self.preservation);
            match &mut self.pending {
                Some(pending) if pending.kind == kind && pending.message_id == message_id => {
                    pending.text.push_str(&text);
                    pending.content.extend(content);
                }
                _ => {
                    flush_message(builder, &mut self.pending);
                    self.pending = Some(PendingMessage {
                        kind,
                        text,
                        content,
                        timestamp,
                        raw_type,
                        message_id,
                    });
                }
            }
            return;
        }

        flush_message(builder, &mut self.pending);
        let kind = nonmessage_kind(update_type, string_at(&update, &[&["status"]]));
        if let Some(kind) = kind {
            if matches!(kind, EventKind::ProviderEvent | EventKind::PlanUpdated)
                && update_type != "tool_call_update"
                && !self.preservation.retain(&update)
            {
                return;
            }
            if kind == EventKind::ProviderEvent && update_type != "tool_call_update" {
                self.unsupported_text_records = self.unsupported_text_records.saturating_add(1);
            }
            let mut update = update;
            if kind == EventKind::PlanUpdated
                && let Some(entries) = update
                    .as_object_mut()
                    .and_then(|object| object.remove("entries"))
            {
                update["plan"] = entries;
            }
            builder.push(
                kind,
                update,
                timestamp,
                ReplayPolicy::HistoricalOnly,
                raw_type,
                None,
            );
        }
    }

    fn finish(&mut self, builder: &mut EventBuilder) {
        flush_message(builder, &mut self.pending);
        let omitted_events = std::mem::take(&mut self.preservation.omitted_events);
        if omitted_events > 0 {
            builder.push(EventKind::ProviderEvent,
                json!({"omitted_events": omitted_events,
                    "reason": "Grok nontext, reasoning, plan, and opaque historical records exceeded the collection budget"}),
                None, ReplayPolicy::HistoricalOnly,
                Some("omnisession.collection_limit".to_owned()), None);
        }
        let unsupported_text_records = std::mem::take(&mut self.unsupported_text_records);
        if unsupported_text_records > 0 {
            builder.push(EventKind::ProviderEvent,
                json!({"unsupported_text_records": unsupported_text_records,
                    "raw_records_preserved": true,
                    "reason": "Unknown Grok updates remain historical metadata; their text cannot be transferred safely"}),
                None, ReplayPolicy::HistoricalOnly,
                Some("omnisession.unsupported_history".to_owned()), None);
        }
    }
}

fn nonmessage_kind(update_type: &str, status: Option<&str>) -> Option<EventKind> {
    match update_type {
        "tool_call" | "tool_use" => Some(EventKind::ToolCalled),
        "tool_result" | "tool_completed" => {
            let failed = status.is_some_and(|status| matches!(status, "failed" | "error"));
            Some(if failed {
                EventKind::ToolFailed
            } else {
                EventKind::ToolCompleted
            })
        }
        "tool_call_update" => Some(match status {
            Some("failed" | "error") => EventKind::ToolFailed,
            Some("completed" | "complete" | "success" | "succeeded" | "done") => {
                EventKind::ToolCompleted
            }
            _ => EventKind::ProviderEvent,
        }),
        "tool_error" | "tool_failed" => Some(EventKind::ToolFailed),
        "command" | "command_executed" => Some(EventKind::CommandExecuted),
        "plan" => Some(EventKind::PlanUpdated),
        "available_commands_update" | "current_mode_update" | "config_option_update" => None,
        _ => Some(EventKind::ProviderEvent),
    }
}

fn message_content(
    update: &Value,
    preservation: &mut GrokPreservationBudget,
) -> (String, Vec<Value>) {
    let mut text = String::new();
    let mut historical_content = Vec::new();
    let mut push = |block: &Value| {
        if let Some(chunk) = block
            .as_str()
            .or_else(|| block.get("text").and_then(Value::as_str))
        {
            text.push_str(chunk);
        } else if !block.is_null() && preservation.retain(block) {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(match block.get("type").and_then(Value::as_str) {
                Some("image") => "[Source image attachment preserved as historical metadata]",
                Some("audio") => "[Source audio attachment preserved as historical metadata]",
                _ => "[Source nontext content preserved as historical metadata]",
            });
            historical_content.push(block.clone());
        }
    };
    if let Some(content) = update.get("content") {
        if let Some(blocks) = content.as_array() {
            for block in blocks {
                push(block);
            }
        } else {
            push(content);
        }
    } else if let Some(fallback) = value_at(
        update,
        &[&["text"], &["message", "content"], &["message", "text"]],
    ) {
        push(fallback);
    }
    (text, historical_content)
}

fn push_grok_session_metadata(builder: &mut EventBuilder, summary: &Value) {
    let model = string_at(
        summary,
        &[
            &["current_model_id"],
            &["model"],
            &["model_id"],
            &["metadata", "model"],
        ],
    );
    let reasoning_mode = string_at(
        summary,
        &[
            &["reasoning_effort"],
            &["reasoning_mode"],
            &["metadata", "reasoning_effort"],
        ],
    );
    let total_tokens = value_at(
        summary,
        &[
            &["total_tokens"],
            &["usage", "total_tokens"],
            &["metadata", "total_tokens"],
        ],
    )
    .and_then(Value::as_u64);
    let parent_session_id = summary.get("parent_session_id").and_then(Value::as_str);
    let session_kind = summary.get("session_kind").and_then(Value::as_str);
    if model.is_none()
        && reasoning_mode.is_none()
        && total_tokens.is_none()
        && parent_session_id.is_none()
        && session_kind.is_none()
    {
        return;
    }
    builder.push(
        EventKind::ProviderEvent,
        json!({
            "model": model,
            "reasoning_mode": reasoning_mode,
            "total_tokens": total_tokens,
            "token_usage": "cumulative",
            "parent_session_id": parent_session_id,
            "session_kind": session_kind,
        }),
        None,
        ReplayPolicy::HistoricalOnly,
        Some("omnisession.session_metadata".to_owned()),
        None,
    );
}

fn push_grok_state(builder: &mut EventBuilder, root: &Path, summary_path: &Path) {
    let Some(directory) = summary_path.parent() else {
        return;
    };
    for (filename, kind, record_type, field) in [
        (
            "plan.json",
            EventKind::PlanUpdated,
            "grok.plan_state",
            "plan",
        ),
        (
            "goal/state.json",
            EventKind::ProviderEvent,
            "grok.goal_state",
            "goal",
        ),
        (
            "usage.json",
            EventKind::ProviderEvent,
            "grok.usage_state",
            "usage",
        ),
    ] {
        let Some(path) = provider_file(root, &directory.join(filename)) else {
            continue;
        };
        let Ok(value) = read_json(&path) else {
            continue;
        };
        builder.push(
            kind,
            json!({field: value}),
            None,
            ReplayPolicy::HistoricalOnly,
            Some(record_type.to_owned()),
            None,
        );
    }
}

fn flush_message(builder: &mut EventBuilder, pending: &mut Option<PendingMessage>) {
    let Some(pending) = pending.take() else {
        return;
    };
    if !pending.text.is_empty() {
        let mut payload = json!({ "text": pending.text });
        let nontext_blocks = pending.content.len();
        if !pending.content.is_empty() {
            payload["content"] = Value::Array(pending.content);
        }
        let replay_policy = if pending.kind == EventKind::ReasoningSummary {
            ReplayPolicy::HistoricalOnly
        } else {
            ReplayPolicy::Contextual
        };
        builder.push(
            pending.kind,
            payload,
            pending.timestamp,
            replay_policy,
            pending.raw_type,
            None,
        );
        if nontext_blocks > 0 {
            builder.push(
                EventKind::ProviderEvent,
                json!({
                    "omitted_events": nontext_blocks,
                    "event_kind": "attachment",
                    "reason": "Attachments remain in canonical historical metadata; native transfer carries documentary text only",
                }),
                pending.timestamp,
                ReplayPolicy::HistoricalOnly,
                Some("omnisession.unsupported_content".to_owned()),
                None,
            );
        }
    }
}

impl ProviderAdapter for GrokAdapter {
    fn provider(&self) -> Provider {
        Provider::Grok
    }

    fn probe(&self) -> ProviderInstallation {
        let executable = provider_executable(Provider::Grok);
        ProviderInstallation {
            provider: Provider::Grok,
            installed: executable.is_some()
                || self.sessions_root.as_deref().is_some_and(Path::is_dir),
            executable,
            data_root: self.sessions_root.clone(),
        }
    }

    fn list_sessions(&self, project: Option<&Path>) -> Result<Vec<NativeSession>> {
        let catalog: HashMap<_, _> = self
            .catalog_sessions(None)
            .into_iter()
            .map(|session| (session.session.id.clone(), session))
            .collect();
        let mut sessions: HashMap<String, NativeSession> = HashMap::new();
        let mut ambiguous = HashSet::new();
        let mut native_ids = HashSet::new();
        let mut unavailable_native_ids = HashSet::new();
        for path in self.summaries() {
            let physical_id = path
                .parent()
                .and_then(Path::file_name)
                .and_then(|name| name.to_str())
                .filter(|id| Uuid::parse_str(id).is_ok());
            if let Some(id) = physical_id
                && !native_ids.insert(id.to_owned())
            {
                unavailable_native_ids.insert(id.to_owned());
            }
            let Ok(summary) = read_json(&path) else {
                unavailable_native_ids.extend(physical_id.map(str::to_owned));
                continue;
            };
            let Some(id) = session_id(&summary, &path) else {
                continue;
            };
            if let Some(physical_id) = physical_id.filter(|candidate| *candidate != id) {
                unavailable_native_ids.insert(physical_id.to_owned());
            }
            if let Some(previous) = sessions.get(&id).and_then(|row| row.source_path.as_deref()) {
                match (
                    has_native_directory(previous, &id),
                    has_native_directory(&path, &id),
                ) {
                    (true, false) => continue,
                    (false, true) => {
                        ambiguous.remove(&id);
                    }
                    _ => {
                        ambiguous.insert(id);
                        continue;
                    }
                }
            }
            let mut data = metadata(&summary);
            data.fill_missing(&summary, &path, catalog.get(&id));
            sessions.insert(id.clone(), data.into_session(id, path));
        }
        for (id, row) in &catalog {
            sessions.entry(id.clone()).or_insert_with(|| row.clone());
        }
        *self
            .listed_catalog
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(catalog);
        ambiguous.extend(unavailable_native_ids);
        self.has_ambiguous_ids
            .store(!ambiguous.is_empty(), Ordering::Relaxed);
        let mut sessions: Vec<_> = sessions
            .into_values()
            .filter(|row| !ambiguous.contains(&row.session.id))
            .collect();
        sessions.retain(|session| {
            project.is_none_or(|project| {
                session
                    .project_path
                    .as_deref()
                    .is_some_and(|recorded| paths_match(recorded, project))
            })
        });
        sort_sessions(&mut sessions);
        Ok(sessions)
    }

    fn native_resume_support(&self, session: &SessionRef) -> Result<NativeResumeSupport> {
        validate_provider(session, Provider::Grok)?;
        let Some((summary, path)) = self.find_summary(&session.id)? else {
            anyhow::ensure!(
                self.catalog_sessions(Some(&session.id)).is_empty(),
                "Grok session `{}` has a catalog entry but no verified native summary",
                session.id
            );
            return Ok(NativeResumeSupport::Direct);
        };
        if !can_fork_summary(&session.id, &summary)
            || !self.native_location_matches(&session.id, &summary, &path)
        {
            return Ok(NativeResumeSupport::ImportOnly);
        }
        self.native_updates_support(&session.id, &path, &summary)
    }

    fn discovery_notes(&self) -> Vec<String> {
        if self.has_ambiguous_ids.load(Ordering::Relaxed) {
            vec![
                "Grok skipped sessions with conflicting or unreadable native summaries.".to_owned(),
            ]
        } else {
            Vec::new()
        }
    }

    fn find_native_session(&self, id: &str) -> Result<Option<NativeSession>> {
        if let Some((summary, path)) = self.find_summary(id)? {
            let metadata = self.resolved_metadata(id, &summary, &path, None);
            return Ok(Some(metadata.into_session(id.to_owned(), path)));
        }
        Ok(self.catalog_sessions(Some(id)).pop())
    }

    fn fork_candidates_created_since(
        &self,
        source: &SessionRef,
        project: &Path,
        started_at: DateTime<Utc>,
    ) -> Result<Vec<SessionRef>> {
        validate_provider(source, Provider::Grok)?;
        uuid::Uuid::parse_str(&source.id).context("Grok fork source is not a UUID")?;
        let mut candidates = Vec::new();
        for path in self.summaries() {
            let Ok(summary) = read_json(&path) else {
                continue;
            };
            if summary.get("parent_session_id").and_then(Value::as_str) != Some(source.id.as_str())
            {
                continue;
            }
            let Some(id) = session_id(&summary, &path) else {
                continue;
            };
            if id == source.id || uuid::Uuid::parse_str(&id).is_err() {
                continue;
            }
            let metadata = metadata(&summary);
            if metadata
                .created_at
                .is_none_or(|created_at| created_at < started_at)
                || metadata
                    .project_path
                    .as_deref()
                    .is_none_or(|cwd| !paths_match(cwd, project))
            {
                continue;
            }
            candidates.push(SessionRef::new(Provider::Grok, id));
        }
        Ok(candidates)
    }

    fn read_session(&self, session: &SessionRef) -> Result<CanonicalSnapshot> {
        Ok(self.index_session(session, None, u64::MAX)?.snapshot)
    }

    fn preview_session(&self, session: &SessionRef) -> Result<CanonicalSnapshot> {
        Ok(self.index_session(session, None, 0)?.snapshot)
    }

    fn index_session(
        &self,
        session: &SessionRef,
        listed_path: Option<&Path>,
        full_read_bytes: u64,
    ) -> Result<IndexRead> {
        validate_provider(session, Provider::Grok)?;
        let (summary, path, metadata) = self.resolved_summary(&session.id, listed_path)?;
        let source_revision = metadata.source_revision(&path);
        let source_complete = Self::updates_size(&path).unwrap_or(0) <= full_read_bytes;
        let snapshot = self.snapshot(session, &summary, &path, &metadata, source_complete)?;
        let native = metadata.into_session(session.id.clone(), path);
        anyhow::ensure!(
            native.source_revision == Some(source_revision),
            "selected Grok source changed during its read"
        );
        Ok(IndexRead {
            snapshot,
            source_complete,
            source_revision,
            native_session: Some(native),
        })
    }

    fn new_session_plan(&self, target: &LaunchTarget) -> Result<LaunchPlan> {
        Ok(LaunchPlan {
            program: "grok".to_owned(),
            args: target.prompt.iter().cloned().collect(),
            cwd: target.cwd.clone(),
            env: Vec::new(),
        })
    }

    fn launch_plan(&self, session: &SessionRef, target: &LaunchTarget) -> Result<LaunchPlan> {
        validate_provider(session, Provider::Grok)?;
        anyhow::ensure!(
            self.native_resume_support(session)? != NativeResumeSupport::ImportOnly,
            "Grok cannot resume this source in place. Create a verified copy with omni fork."
        );
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
            env: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SESSION_ID: &str = "77777777-7777-4777-8777-777777777777";

    fn write_session(root: &Path, id: &str, summary: &Value, updates: &[Value]) -> PathBuf {
        let directory = root.join("workspace-hash").join(id);
        fs::create_dir_all(&directory).expect("synthetic session directory");
        fs::write(directory.join("summary.json"), summary.to_string()).expect("synthetic summary");
        fs::write(
            directory.join("updates.jsonl"),
            updates
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .expect("synthetic updates");
        directory
    }

    #[test]
    fn bootstrap_chat_records_do_not_require_visible_update_history() {
        for missing_updates in [false, true] {
            let root = tempfile::tempdir().expect("synthetic Grok root");
            let directory = write_session(
                root.path(),
                SESSION_ID,
                &json!({
                    "info": {"id": SESSION_ID, "cwd": "/workspace/demo"},
                    "num_messages": 0,
                    "num_chat_messages": 2,
                    "chat_format_version": 1,
                    "current_model_id": "synthetic-model"
                }),
                &[],
            );
            if missing_updates {
                fs::remove_file(directory.join("updates.jsonl")).expect("absent update history");
            }
            let chat_history = concat!(
                "{\"type\":\"system\",\"content\":\"Synthetic system context\"}\n",
                "{\"type\":\"user\",\"synthetic_reason\":\"system_reminder\",",
                "\"content\":[{\"type\":\"text\",\"text\":\"Synthetic environment reminder\"}]}\n"
            );
            fs::write(directory.join("chat_history.jsonl"), chat_history)
                .expect("bootstrap chat history");
            let adapter = GrokAdapter::with_root(root.path());
            let session = SessionRef::new(Provider::Grok, SESSION_ID);

            for snapshot in [
                adapter
                    .read_session(&session)
                    .expect("bootstrap-only session"),
                adapter
                    .preview_session(&session)
                    .expect("bootstrap-only preview"),
            ] {
                assert!(snapshot.events.iter().all(|event| {
                    event.kind == EventKind::ProviderEvent
                        && event.replay_policy == ReplayPolicy::HistoricalOnly
                }));
                assert_eq!(snapshot.events[0].payload["model"], "synthetic-model");
            }
            assert_eq!(
                fs::read_to_string(directory.join("chat_history.jsonl"))
                    .expect("unchanged bootstrap history"),
                chat_history
            );
        }
    }

    #[test]
    fn explicit_update_count_and_legacy_chat_count_still_require_history() {
        for counts in [
            json!({"num_messages": 1, "num_chat_messages": 0}),
            json!({"num_chat_messages": 1}),
            json!({"num_messages": null, "num_chat_messages": 1}),
        ] {
            let root = tempfile::tempdir().expect("synthetic Grok root");
            let mut summary = counts;
            summary["id"] = json!(SESSION_ID);
            write_session(root.path(), SESSION_ID, &summary, &[]);
            let adapter = GrokAdapter::with_root(root.path());
            let session = SessionRef::new(Provider::Grok, SESSION_ID);
            for error in [
                adapter
                    .read_session(&session)
                    .expect_err("missing declared history"),
                adapter
                    .preview_session(&session)
                    .expect_err("missing declared preview history"),
            ] {
                assert!(
                    error
                        .to_string()
                        .contains("declares history but has no readable updates")
                );
            }
        }
    }

    #[test]
    fn incomplete_search_catalog_does_not_hide_native_summaries() {
        for has_indexed_session in [false, true] {
            let root = tempfile::tempdir().expect("synthetic Grok root");
            let database = root.path().join("session_search.sqlite");
            let connection = rusqlite::Connection::open(&database).expect("synthetic catalog");
            connection
                .execute_batch(
                    "CREATE TABLE session_docs (session_id TEXT PRIMARY KEY, cwd TEXT NOT NULL,
                 updated_at INTEGER NOT NULL, title TEXT);",
                )
                .expect("synthetic catalog schema");
            if has_indexed_session {
                connection
                    .execute(
                        "INSERT INTO session_docs VALUES (?1, ?2, ?3, ?4)",
                        rusqlite::params![
                            SESSION_ID,
                            "/workspace/demo",
                            1_785_000_000_i64,
                            "Indexed"
                        ],
                    )
                    .expect("indexed session");
            }
            write_session(
                root.path(),
                SESSION_ID,
                &json!({"id": SESSION_ID, "cwd": "/workspace/demo", "title": "Summary"}),
                &[],
            );
            let missing = "88888888-8888-4888-8888-888888888888";
            write_session(
                root.path(),
                missing,
                &json!({"id": missing, "cwd": "/workspace/demo", "title": "Missing from catalog"}),
                &[],
            );

            let sessions = GrokAdapter::with_root(root.path())
                .list_sessions(Some(Path::new("/workspace/demo")))
                .expect("combined Grok discovery");
            assert_eq!(sessions.len(), 2, "catalog complete: {has_indexed_session}");
            assert_eq!(
                sessions
                    .iter()
                    .filter(|session| session.session.id == SESSION_ID)
                    .count(),
                1
            );
            assert!(sessions.iter().any(|session| session.session.id == missing));
        }
    }

    #[test]
    fn nontext_messages_and_historical_updates_survive_readback() {
        let root = tempfile::tempdir().expect("synthetic Grok root");
        let records = [
            json!({"sessionUpdate": "user_message_chunk", "messageId": "image-turn",
                "content": {"type": "image", "mimeType": "image/png", "data": "synthetic-image"}}),
            json!({"sessionUpdate": "plan", "entries": [{"content": "Synthetic next action", "status": "pending", "priority": "high"}]}),
            json!({"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": "Synthetic reasoning"}}),
            json!({"sessionUpdate": "future_historical_record", "historical": "Synthetic opaque event"}),
        ];
        write_session(
            root.path(),
            SESSION_ID,
            &json!({"id": SESSION_ID, "cwd": "/workspace/demo"}),
            &records,
        );
        let snapshot = GrokAdapter::with_root(root.path())
            .read_session(&SessionRef::new(Provider::Grok, SESSION_ID))
            .expect("Grok snapshot");
        assert_eq!(
            snapshot
                .events
                .iter()
                .map(|event| &event.kind)
                .collect::<Vec<_>>(),
            [
                &EventKind::MessageUser,
                &EventKind::ProviderEvent,
                &EventKind::PlanUpdated,
                &EventKind::ReasoningSummary,
                &EventKind::ProviderEvent,
                &EventKind::ProviderEvent
            ]
        );
        let image = &snapshot.events[0];
        assert!(
            image.payload["text"]
                .as_str()
                .expect("visible attachment marker")
                .contains("image")
        );
        assert_eq!(image.payload["content"][0]["data"], "synthetic-image");
        assert_eq!(image.replay_policy, ReplayPolicy::Contextual);
        for event in &snapshot.events[1..] {
            assert_eq!(event.replay_policy, ReplayPolicy::HistoricalOnly);
        }
        assert!(omnis_core::import_trajectory(&snapshot).truncated);
        assert_eq!(snapshot.events[1].payload["omitted_events"], 1);
    }

    #[test]
    fn unknown_text_updates_are_preserved_and_report_unsupported_transfer() {
        let root = tempfile::tempdir().expect("synthetic Grok root");
        let records = [
            json!({"sessionUpdate": "user_message_chunk", "content": {"type": "text", "text": "Synthetic known question"}}),
            json!({"sessionUpdate": "future_message_chunk", "data": {"text": "Synthetic future text"}}),
            json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "Synthetic known answer"}}),
        ];
        write_session(
            root.path(),
            SESSION_ID,
            &json!({"id": SESSION_ID, "cwd": "/workspace/demo"}),
            &records,
        );
        let snapshot = GrokAdapter::with_root(root.path())
            .read_session(&SessionRef::new(Provider::Grok, SESSION_ID))
            .expect("unknown text read-back");
        assert_eq!(
            snapshot.events[1].payload["data"]["text"],
            "Synthetic future text"
        );
        assert_eq!(
            snapshot.events[1].replay_policy,
            ReplayPolicy::HistoricalOnly
        );
        assert_eq!(
            snapshot
                .events
                .last()
                .expect("unsupported history notice")
                .payload["unsupported_text_records"],
            1
        );
        assert!(omnis_core::import_trajectory(&snapshot).truncated);
        assert!(omnis_core::import_conversation(&snapshot).truncated);
    }

    #[test]
    fn native_summary_overrides_stale_indexed_workspace() {
        let root = tempfile::tempdir().expect("synthetic Grok root");
        let connection = rusqlite::Connection::open(root.path().join("session_search.sqlite"))
            .expect("synthetic catalog");
        connection.execute_batch("CREATE TABLE session_docs (session_id TEXT PRIMARY KEY, cwd TEXT NOT NULL, updated_at INTEGER NOT NULL, title TEXT);")
            .expect("catalog schema");
        connection
            .execute(
                "INSERT INTO session_docs VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![
                    SESSION_ID,
                    "/workspace/old",
                    1_785_000_000_i64,
                    "Stale title"
                ],
            )
            .expect("stale catalog row");
        write_session(
            root.path(),
            SESSION_ID,
            &json!({"id": SESSION_ID, "info": {"cwd": "/workspace/current"}, "title": "Current title"}),
            &[],
        );
        let adapter = GrokAdapter::with_root(root.path());
        assert!(
            adapter
                .list_sessions(Some(Path::new("/workspace/old")))
                .expect("old workspace filter")
                .is_empty()
        );
        let current = adapter
            .list_sessions(Some(Path::new("/workspace/current")))
            .expect("current workspace filter");
        assert_eq!(current.len(), 1);
        assert_eq!(current[0].title.as_deref(), Some("Current title"));
        assert!(current[0].source_path.is_some());
    }

    #[test]
    fn grok_checks_update_ownership_and_reports_incomplete_json() {
        let root = tempfile::tempdir().unwrap();
        let update = |owner: &str| {
            json!({"params":{"sessionId":owner,"update":{
                "sessionUpdate":"user_message_chunk","content":{"type":"text","text":"question"}
            }}})
        };
        let directory = write_session(
            root.path(),
            SESSION_ID,
            &json!({"info":{"id":SESSION_ID},"num_messages":2}),
            &[update(SESSION_ID)],
        );
        let log = directory.join("updates.jsonl");
        let adapter = GrokAdapter::with_root(root.path());
        let session = SessionRef::new(Provider::Grok, SESSION_ID);
        let snapshot = adapter.read_session(&session).unwrap();
        assert!(
            snapshot
                .events
                .iter()
                .any(|event| event.payload["omitted_events"] == 1
                    && event.payload["expected_updates"] == 2)
        );
        fs::write(&log, format!("{}\n{{", update(SESSION_ID))).unwrap();
        for budget in [0, u64::MAX] {
            let read = adapter.index_session(&session, None, budget).unwrap();
            assert!(
                read.snapshot
                    .events
                    .iter()
                    .any(|event| event.payload["malformed_records"] == 1)
            );
        }
        fs::write(
            log,
            update("88888888-8888-4888-8888-888888888888").to_string(),
        )
        .unwrap();
        for budget in [0, u64::MAX] {
            let read = adapter.index_session(&session, None, budget);
            assert!(
                read.err()
                    .unwrap()
                    .to_string()
                    .contains("different session ID")
            );
        }
    }

    #[test]
    fn partial_summary_recovers_only_matching_catalog_metadata() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        let other = root.path().join("other");
        let connection =
            rusqlite::Connection::open(root.path().join("session_search.sqlite")).unwrap();
        connection.execute_batch("CREATE TABLE session_docs (session_id TEXT PRIMARY KEY, cwd TEXT NOT NULL, updated_at INTEGER NOT NULL, title TEXT);").unwrap();
        connection
            .execute(
                "INSERT INTO session_docs VALUES (?1, ?2, 1, 'Catalog title')",
                rusqlite::params![SESSION_ID, project.to_str().unwrap()],
            )
            .unwrap();
        let partial = json!({"info":{"id": SESSION_ID}, "num_messages":1});
        let update = json!({"sessionUpdate":"user_message_chunk", "content":{"type":"text","text":"kept question"}});
        let directory = write_session(root.path(), SESSION_ID, &partial, &[update]);
        let adapter = GrokAdapter::with_root(root.path());
        let session = SessionRef::new(Provider::Grok, SESSION_ID);
        let listed = adapter.list_sessions(Some(&project)).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(
            adapter.native_resume_support(&session).unwrap(),
            NativeResumeSupport::ImportOnly
        );
        assert_eq!(
            adapter.find_native_session(SESSION_ID).unwrap().as_ref(),
            listed.first()
        );
        for snapshot in [
            adapter.read_session(&session).unwrap(),
            adapter.preview_session(&session).unwrap(),
        ] {
            assert_eq!(snapshot.title.as_deref(), Some("Catalog title"));
            assert_eq!(snapshot.workspace.current_dir, project);
            assert_eq!(snapshot.events[0].payload["text"], "kept question");
        }
        connection
            .execute("UPDATE session_docs SET title = 'New title'", [])
            .unwrap();
        let indexed = adapter
            .index_session(&session, listed[0].source_path.as_deref(), u64::MAX)
            .unwrap();
        assert_eq!(indexed.snapshot.title.as_deref(), Some("Catalog title"));
        assert_eq!(
            adapter.read_session(&session).unwrap().title.as_deref(),
            Some("New title")
        );

        for (body, expected_cwd) in [
            (
                json!({"info":{"id":SESSION_ID,"cwd":project},"session_summary": null}),
                &project,
            ),
            (json!({"info":{"id":SESSION_ID,"cwd":other}}), &other),
        ] {
            fs::write(directory.join("summary.json"), body.to_string()).unwrap();
            let snapshot = adapter.read_session(&session).unwrap();
            assert_eq!(snapshot.workspace.current_dir, *expected_cwd);
            assert_eq!(snapshot.title, None);
        }
        let encoded = project
            .to_str()
            .unwrap()
            .replace('%', "%25")
            .replace('/', "%2F")
            .replace('\\', "%5C")
            .replace(':', "%3A");
        let original = directory.parent().unwrap();
        let new_dir = root.path().join(encoded);
        fs::rename(original, &new_dir).unwrap();
        fs::write(
            new_dir.join(SESSION_ID).join("summary.json"),
            partial.to_string(),
        )
        .unwrap();
        connection
            .execute(
                "UPDATE session_docs SET cwd = ?1",
                [other.to_str().unwrap()],
            )
            .unwrap();
        let conflicting = adapter.read_session(&session).unwrap();
        assert!(conflicting.workspace.current_dir.as_os_str().is_empty());
        assert_eq!(conflicting.title, None);
        connection.execute("DELETE FROM session_docs", []).unwrap();
        let without_catalog = adapter.read_session(&session).unwrap();
        assert_eq!(without_catalog.workspace.current_dir, project);
        assert_eq!(without_catalog.title, None);
    }

    #[test]
    fn native_fork_requires_the_fields_read_by_grok() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        let complete = json!({"info":{"id":SESSION_ID,"cwd":project},"session_summary":"Title","created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-02T00:00:00Z","current_model_id":"synthetic","num_messages":0});
        let directory = write_session(root.path(), SESSION_ID, &complete, &[]);
        let native_group = root
            .path()
            .join(encode_uri_component(project.to_str().unwrap()));
        fs::rename(directory.parent().unwrap(), &native_group).unwrap();
        let directory = native_group.join(SESSION_ID);
        let adapter = GrokAdapter::with_root(root.path());
        let session = SessionRef::new(Provider::Grok, SESSION_ID);
        assert_eq!(
            adapter.native_resume_support(&session).unwrap(),
            NativeResumeSupport::Direct
        );
        let summaries = directory.join("summary.json");
        let updates = directory.join("updates.jsonl");
        let mut with_history = complete.clone();
        with_history["num_messages"] = json!(1);
        fs::write(&summaries, with_history.to_string()).unwrap();
        assert!(adapter.native_resume_support(&session).is_err());
        let owned = json!({"params":{"sessionId":SESSION_ID,"update":{"sessionUpdate":"session_info_update"}}});
        fs::write(&updates, format!("{owned}\n")).unwrap();
        assert_eq!(
            adapter.native_resume_support(&session).unwrap(),
            NativeResumeSupport::Direct
        );
        with_history["num_messages"] = json!(2);
        fs::write(&summaries, with_history.to_string()).unwrap();
        assert_eq!(
            adapter.native_resume_support(&session).unwrap(),
            NativeResumeSupport::ImportOnly
        );
        let unrelated = json!({"params":{"sessionId":"22222222-2222-4222-8222-222222222222"}});
        fs::write(&updates, format!("{owned}\n{unrelated}\n")).unwrap();
        let error = adapter
            .launch_plan(&session, &LaunchTarget::default())
            .unwrap_err();
        assert!(error.to_string().contains("different session ID"));
        fs::write(&updates, format!("{owned}\n{{")).unwrap();
        assert_eq!(
            adapter.native_resume_support(&session).unwrap(),
            NativeResumeSupport::ImportOnly
        );
        let mut wrong_owner = complete.clone();
        wrong_owner["info"]["id"] = json!("22222222-2222-4222-8222-222222222222");
        fs::write(&summaries, wrong_owner.to_string()).unwrap();
        assert!(
            adapter
                .find_native_session(SESSION_ID)
                .unwrap_err()
                .to_string()
                .contains("different session")
        );
        assert!(adapter.native_resume_support(&session).is_err());
        assert!(
            adapter
                .list_sessions(None)
                .unwrap()
                .iter()
                .all(|row| row.session != session)
        );
        fs::write(&updates, []).unwrap();
        fs::write(&summaries, complete.to_string()).unwrap();
        let restored = root.path().join("restored");
        fs::rename(&native_group, &restored).unwrap();
        assert_eq!(
            adapter.native_resume_support(&session).unwrap(),
            NativeResumeSupport::ImportOnly
        );
        fs::rename(restored, native_group).unwrap();
        for field in [
            "/info/id",
            "/created_at",
            "/updated_at",
            "/num_messages",
            "/current_model_id",
        ] {
            let mut damaged = complete.clone();
            *damaged.pointer_mut(field).unwrap() = Value::Null;
            fs::write(directory.join("summary.json"), damaged.to_string()).unwrap();
            assert_eq!(
                adapter.native_resume_support(&session).unwrap(),
                NativeResumeSupport::ImportOnly,
                "{field}"
            );
        }
    }

    #[test]
    fn ambiguous_restored_id_is_excluded_without_hiding_other_sessions() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("one");
        let summary = json!({"info":{"id": SESSION_ID,"cwd": project},"session_summary":"First","created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-02T00:00:00Z","current_model_id":"synthetic","num_messages":0});
        let original = write_session(root.path(), SESSION_ID, &summary, &[]);
        let native_group = root
            .path()
            .join(encode_uri_component(project.to_str().unwrap()));
        fs::rename(original.parent().unwrap(), &native_group).unwrap();
        let original = native_group.join(SESSION_ID);
        let other = root.path().join("restored-workspace").join(SESSION_ID);
        fs::create_dir_all(&other).unwrap();
        fs::write(
            other.join("summary.json"),
            json!({"info":{"id":SESSION_ID,"cwd":"/workspace/two"},"session_summary":"Second"})
                .to_string(),
        )
        .unwrap();
        let sibling = "88888888-8888-4888-8888-888888888888";
        write_session(root.path(), sibling, &json!({"id": sibling}), &[]);
        let adapter = GrokAdapter::with_root(root.path());
        let listed = adapter.list_sessions(None).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].session.id, sibling);
        assert!(!adapter.discovery_notes().is_empty());
        let session = SessionRef::new(Provider::Grok, SESSION_ID);
        for error in [
            adapter.find_native_session(SESSION_ID).unwrap_err(),
            adapter
                .launch_plan(&session, &LaunchTarget::default())
                .unwrap_err(),
            adapter.read_session(&session).unwrap_err(),
        ] {
            assert!(error.to_string().contains("multiple Grok summaries"));
        }
        fs::write(other.join("summary.json"), "{").unwrap();
        assert!(
            adapter
                .list_sessions(None)
                .unwrap()
                .iter()
                .all(|row| row.session != session)
        );
        assert!(
            adapter
                .native_resume_support(&session)
                .unwrap_err()
                .to_string()
                .contains("multiple Grok summaries")
        );
        fs::write(
            other.join("summary.json"),
            json!({"info":{"id":SESSION_ID,"cwd":"/workspace/two"},"session_summary":"Second"})
                .to_string(),
        )
        .unwrap();
        let backup = other.with_file_name(format!("{SESSION_ID}.backup"));
        fs::rename(&other, backup).unwrap();
        assert!(
            adapter
                .list_sessions(None)
                .unwrap()
                .iter()
                .any(|row| row.title.as_deref() == Some("First"))
        );
        assert_eq!(
            adapter.native_resume_support(&session).unwrap(),
            NativeResumeSupport::Direct
        );
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&original, &other).unwrap();
            assert_eq!(
                adapter.read_session(&session).unwrap().title.as_deref(),
                Some("First")
            );
            fs::remove_file(&other).unwrap();
        }
        fs::remove_dir_all(original).unwrap();
        assert_eq!(
            adapter.read_session(&session).unwrap().title.as_deref(),
            Some("Second")
        );
        assert_eq!(
            adapter.native_resume_support(&session).unwrap(),
            NativeResumeSupport::ImportOnly
        );
    }

    #[test]
    fn fork_candidates_require_exact_parent_workspace_and_creation_time() {
        let root = tempfile::tempdir().expect("synthetic Grok root");
        let project = root.path().join("workspace");
        let other_project = root.path().join("other-workspace");
        fs::create_dir_all(&project).expect("workspace");
        fs::create_dir_all(&other_project).expect("other workspace");
        let started_at = DateTime::parse_from_rfc3339("2026-01-01T00:00:01Z")
            .expect("start time")
            .with_timezone(&Utc);
        let children = [
            (
                "11111111-1111-4111-8111-111111111111",
                SESSION_ID,
                &project,
                "2026-01-01T00:00:02Z",
            ),
            (
                "22222222-2222-4222-8222-222222222222",
                SESSION_ID,
                &other_project,
                "2026-01-01T00:00:02Z",
            ),
            (
                "33333333-3333-4333-8333-333333333333",
                SESSION_ID,
                &project,
                "2026-01-01T00:00:00Z",
            ),
            (
                "44444444-4444-4444-8444-444444444444",
                "unrelated-parent",
                &project,
                "2026-01-01T00:00:02Z",
            ),
        ];
        for (id, parent, cwd, created) in children {
            write_session(
                root.path(),
                id,
                &json!({"id": id, "parent_session_id": parent, "info": {"cwd": cwd}, "created_at": created}),
                &[],
            );
        }
        let adapter = GrokAdapter::with_root(root.path());
        let source = SessionRef::new(Provider::Grok, SESSION_ID);
        let candidates = adapter
            .fork_candidates_created_since(&source, &project, started_at)
            .expect("native fork candidates");
        assert_eq!(candidates, [SessionRef::new(Provider::Grok, children[0].0)]);
        let ambiguous = "55555555-5555-4555-8555-555555555555";
        write_session(
            root.path(),
            ambiguous,
            &json!({"id": ambiguous, "parent_session_id": SESSION_ID, "info": {"cwd": project}, "created_at": "2026-01-01T00:00:03Z"}),
            &[],
        );
        assert_eq!(
            adapter
                .fork_candidates_created_since(&source, &project, started_at)
                .expect("ambiguous candidates remain explicit")
                .len(),
            2
        );
    }

    #[test]
    fn known_state_sidecars_remain_historical_and_preview_matches() {
        let root = tempfile::tempdir().expect("synthetic Grok root");
        let directory = write_session(
            root.path(),
            SESSION_ID,
            &json!({"id": SESSION_ID, "cwd": "/workspace/demo"}),
            &[],
        );
        fs::create_dir_all(directory.join("goal")).expect("goal directory");
        for (filename, value) in [
            (
                "plan.json",
                json!([{"content": "Synthetic next action", "status": "pending"}]),
            ),
            (
                "goal/state.json",
                json!({"objective": "Synthetic objective"}),
            ),
            ("usage.json", json!({"total_tokens": 123})),
        ] {
            fs::write(directory.join(filename), value.to_string()).expect("synthetic sidecar");
        }
        let adapter = GrokAdapter::with_root(root.path());
        let session = SessionRef::new(Provider::Grok, SESSION_ID);
        let snapshot = adapter.read_session(&session).expect("state read-back");
        assert_eq!(snapshot.events.len(), 3);
        assert_eq!(snapshot.events[0].kind, EventKind::PlanUpdated);
        assert_eq!(
            snapshot.events[0].payload["plan"][0]["content"],
            "Synthetic next action"
        );
        assert_eq!(
            snapshot.events[1].payload["goal"]["objective"],
            "Synthetic objective"
        );
        assert_eq!(snapshot.events[2].payload["usage"]["total_tokens"], 123);
        assert!(
            snapshot
                .events
                .iter()
                .all(|event| event.replay_policy == ReplayPolicy::HistoricalOnly)
        );
        assert_eq!(
            snapshot.events,
            adapter
                .preview_session(&session)
                .expect("state preview")
                .events
        );
    }

    #[test]
    fn historical_retention_budget_reports_loss_without_limiting_visible_text_or_tools() {
        let mut builder = EventBuilder::new(Provider::Grok, SESSION_ID);
        let mut processor = GrokUpdateProcessor {
            pending: None,
            unsupported_text_records: 0,
            preservation: GrokPreservationBudget {
                bytes_left: 2_048,
                records_left: 2,
                omitted_events: 0,
            },
        };
        for record in [
            json!({"sessionUpdate": "future_record", "text": "Small opaque history"}),
            json!({"sessionUpdate": "user_message_chunk", "content": {"type": "image", "data": "x".repeat(8_192)}}),
            json!({"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": "x".repeat(8_192)}}),
            json!({"sessionUpdate": "future_record", "text": "x".repeat(8_192)}),
            json!({"sessionUpdate": "user_message_chunk", "content": {"type": "text", "text": "Unchanged visible question"}}),
            json!({"sessionUpdate": "tool_call", "toolCallId": "synthetic", "rawInput": "x".repeat(8_192)}),
            json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "Unchanged visible answer"}}),
        ] {
            processor.push_record(&mut builder, record);
        }
        processor.finish(&mut builder);
        let snapshot = builder.snapshot(
            SessionRef::new(Provider::Grok, SESSION_ID),
            None,
            None,
            None,
            Utc::now(),
        );
        assert_eq!(
            snapshot
                .events
                .iter()
                .map(|event| &event.kind)
                .collect::<Vec<_>>(),
            [
                &EventKind::ProviderEvent,
                &EventKind::MessageUser,
                &EventKind::ToolCalled,
                &EventKind::MessageAssistant,
                &EventKind::ProviderEvent,
                &EventKind::ProviderEvent
            ]
        );
        assert_eq!(
            snapshot.events[1].payload["text"],
            "Unchanged visible question"
        );
        assert_eq!(
            snapshot.events[3].payload["text"],
            "Unchanged visible answer"
        );
        assert_eq!(snapshot.events[4].payload["omitted_events"], 3);
        assert_eq!(
            snapshot.events[4].source.raw_record_type.as_deref(),
            Some("omnisession.collection_limit")
        );
        assert!(omnis_core::import_trajectory(&snapshot).truncated);
    }

    #[test]
    fn historical_record_count_budget_emits_one_omission_notice() {
        let mut builder = EventBuilder::new(Provider::Grok, SESSION_ID);
        let mut processor = GrokUpdateProcessor {
            pending: None,
            unsupported_text_records: 0,
            preservation: GrokPreservationBudget {
                bytes_left: 64 * 1024,
                records_left: 1,
                omitted_events: 0,
            },
        };
        for index in 0..3 {
            processor.push_record(
                &mut builder,
                json!({"sessionUpdate": "future_record", "index": index}),
            );
        }
        processor.finish(&mut builder);
        let snapshot = builder.snapshot(
            SessionRef::new(Provider::Grok, SESSION_ID),
            None,
            None,
            None,
            Utc::now(),
        );
        assert_eq!(snapshot.events.len(), 3);
        assert_eq!(snapshot.events[0].payload["index"], 0);
        assert_eq!(snapshot.events[1].payload["omitted_events"], 2);
        assert_eq!(snapshot.events[2].payload["unsupported_text_records"], 1);
    }
}
