use std::{
    collections::{HashMap, HashSet},
    fs,
    io::{BufReader, Read},
    path::{Path, PathBuf},
    rc::Rc,
    sync::{Arc, Mutex, RwLock},
};

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use omnis_ir::{CanonicalSnapshot, EventKind, Provider, ReplayPolicy, SessionRef, SourceRevision};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    CodexWorkspaceProjection, IndexRead, LaunchPlan, LaunchTarget, NativeResumeSupport,
    NativeSession, ProviderAdapter, ProviderInstallation, RollbackTitle,
    support::{
        BoundedLine, EventBuilder, JsonLineSkips, MAX_COLLECTED_TRANSCRIPT_FILE_SIZE,
        MAX_PREVIEW_TAIL_SIZE, MAX_STREAMED_TRANSCRIPT_LINE_SIZE, MAX_TRANSCRIPT_LINE_SIZE,
        SnapshotFileStamp, SqliteSnapshotCache, WorkspaceGap, encode_uri_component,
        json_lines_preview_with_omissions, nested_files, parse_timestamp, paths_match,
        percent_decode, provider_executable, provider_file, provider_root,
        read_bounded_line_with_budget, read_json, sort_sessions, string_at, validate_provider,
        value_at, visit_json_lines_with_omissions,
    },
};

#[derive(Clone, Debug)]
pub struct GrokAdapter {
    sessions_root: Option<PathBuf>,
    catalog: Arc<SqliteSnapshotCache>,
    unavailable_ids: Arc<RwLock<HashSet<String>>>,
    checked_updates: Arc<Mutex<Option<CheckedUpdates>>>,
    listed_catalog: Arc<RwLock<Option<HashMap<String, NativeSession>>>>,
}

/// The selected Grok source failed a checked read. A summary conflict identifies the provider root without selecting a log.
#[derive(Debug)]
pub struct GrokReadRejection {
    pub updates: Option<SourceRevision>,
    pub revision: SourceRevision,
    pub updated_at: Option<DateTime<Utc>>,
    /// Original read error, including any transient filesystem cause.
    pub cause: anyhow::Error,
}

#[derive(Debug)]
enum GrokHistoryError {
    ConflictingSummaries(String),
    ForeignOwner,
    Missing(String),
}

impl std::fmt::Display for GrokHistoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ConflictingSummaries(id) => write!(
                f,
                "multiple Grok summaries match session `{id}` in different directories"
            ),
            Self::ForeignOwner => f.write_str("Grok updates contain a different session ID"),
            Self::Missing(id) => write!(
                f,
                "Grok session `{id}` declares history but has no readable updates"
            ),
        }
    }
}

impl std::error::Error for GrokHistoryError {}

fn checked_history_error(
    cause: anyhow::Error,
    revision: SourceRevision,
    updates: Option<SourceRevision>,
    updated_at: Option<DateTime<Utc>>,
) -> anyhow::Error {
    let Some(issue) = cause.downcast_ref::<GrokHistoryError>() else {
        return cause;
    };
    let updates = matches!(issue, GrokHistoryError::ForeignOwner)
        .then_some(updates)
        .flatten();
    GrokReadRejection {
        updates,
        revision,
        updated_at,
        cause,
    }
    .into()
}

impl GrokReadRejection {
    #[must_use]
    pub fn has_conflicting_summaries(&self) -> bool {
        matches!(
            self.cause.downcast_ref::<GrokHistoryError>(),
            Some(GrokHistoryError::ConflictingSummaries(_))
        )
    }
}

impl std::fmt::Display for GrokReadRejection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.cause.fmt(formatter)
    }
}

impl std::error::Error for GrokReadRejection {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.cause.source()
    }
}

#[derive(Debug)]
struct CheckedUpdates {
    path: PathBuf,
    stamp: SnapshotFileStamp,
    support: NativeResumeSupport,
    expected_updates: u64,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum UpdateOwnership {
    Explicit,
    Implicit,
    Damaged,
}

fn update_ownership(record: &Value, session_id: &str) -> Result<UpdateOwnership> {
    let Some(object) = record.as_object() else {
        return Ok(UpdateOwnership::Damaged);
    };
    let Some(value) = record.pointer("/params/sessionId") else {
        let empty_params = object
            .get("params")
            .is_some_and(|params| params.as_object().is_none_or(serde_json::Map::is_empty));
        let empty_update = is_update_envelope(record)
            && record
                .pointer("/params/update")
                .and_then(Value::as_object)
                .is_none_or(serde_json::Map::is_empty);
        if object.is_empty() || (object.len() == 1 && empty_params) || empty_update {
            return Ok(UpdateOwnership::Damaged);
        }
        return Ok(UpdateOwnership::Implicit);
    };
    if value.as_str() == Some(session_id) {
        return Ok(UpdateOwnership::Explicit);
    }
    let Some(owner) = value.as_str().and_then(|owner| Uuid::parse_str(owner).ok()) else {
        return Ok(UpdateOwnership::Damaged);
    };
    if Uuid::parse_str(session_id).ok() == Some(owner) {
        Ok(UpdateOwnership::Explicit)
    } else {
        Err(GrokHistoryError::ForeignOwner.into())
    }
}

impl GrokAdapter {
    /// Maximum log size for refreshing a previously complete search document.
    pub const MAX_INDEX_REFRESH_BYTES: u64 = MAX_COLLECTED_TRANSCRIPT_FILE_SIZE;

    #[must_use]
    pub fn with_root(sessions_root: impl Into<PathBuf>) -> Self {
        Self {
            sessions_root: Some(sessions_root.into()),
            catalog: Arc::default(),
            unavailable_ids: Arc::default(),
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

    /// Identifies the physical update log independently of catalog and summary metadata.
    #[must_use]
    pub fn updates_revision(summary: &Path) -> Option<SourceRevision> {
        let directory = summary.parent()?;
        let updates = provider_file(directory, &directory.join("updates.jsonl"))?;
        let file = path_id(summary);
        Some(SourceRevision {
            file: Some(file),
            contents: Some(SnapshotFileStamp::read(&updates).ok()?.revision(file)),
            ..SourceRevision::default()
        })
    }

    /// Checks every update owner before making a changed or relocated log searchable again.
    ///
    /// # Errors
    ///
    /// Returns an error when the selected file changed or any update owner cannot be verified.
    pub fn verify_update_owners(
        session: &SessionRef,
        summary: &Path,
        log: SourceRevision,
        selection: SourceRevision,
        updated_at: Option<DateTime<Utc>>,
    ) -> Result<()> {
        let stale = "selected Grok source changed during ownership verification";
        anyhow::ensure!(
            session.provider == Provider::Grok && Self::updates_revision(summary) == Some(log),
            stale
        );
        let directory = summary.parent().context(stale)?;
        let path = provider_file(directory, &directory.join("updates.jsonl")).context(stale)?;
        let ownership = scan_update_headers(&path, |line| {
            let Some(line) = line else {
                return Ok(true);
            };
            let Ok(record) = serde_json::from_slice::<Value>(line) else {
                return Ok(true);
            };
            Ok(update_ownership(&record, &session.id)? != UpdateOwnership::Implicit)
        });
        anyhow::ensure!(Self::updates_revision(summary) == Some(log), stale);
        let result = ownership
            .map_err(|cause| checked_history_error(cause, selection, Some(log), updated_at))?;
        anyhow::ensure!(
            result.is_some(),
            "cannot verify the owner of every Grok update"
        );
        Ok(())
    }

    fn summaries(&self) -> Vec<PathBuf> {
        self.sessions_root
            .as_deref()
            .and_then(|root| root.canonicalize().ok())
            .map(|root| nested_files(&root, 2, Some("summary.json")))
            .unwrap_or_default()
    }

    fn unavailable(&self, cause: anyhow::Error) -> anyhow::Error {
        GrokReadRejection {
            updates: None,
            revision: SourceRevision {
                file: self.sessions_root.as_deref().map(path_id),
                ..SourceRevision::default()
            },
            updated_at: None,
            cause,
        }
        .into()
    }

    fn find_summary(&self, id: &str) -> Result<Option<(Value, PathBuf)>> {
        if let Some(file) = self
            .direct_summary(id)
            .map_err(|cause| self.unavailable(cause))?
        {
            let summary = read_json(&file)
                .with_context(|| {
                    format!("cannot verify the Grok summary in session directory `{id}`")
                })
                .map_err(|cause| self.unavailable(cause))?;
            if session_id(&summary, &file).as_deref() != Some(id) {
                return Err(self.unavailable(anyhow!(
                    "the Grok summary in session directory `{id}` identifies a different session"
                )));
            }
            return Ok(Some((summary, file)));
        }
        unique_summary(id, self.summaries()).map_err(|cause| self.unavailable(cause))
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
            return Err(GrokHistoryError::ConflictingSummaries(id.to_owned()).into());
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
        let support = native_update_headers(&updates, id, expected_updates);
        anyhow::ensure!(
            SnapshotFileStamp::read(&updates)? == before,
            "selected Grok source changed during native ownership verification"
        );
        let support = support.map_err(|cause| {
            let metadata = self.resolved_metadata(id, summary, path, None);
            let file = path_id(path);
            let log = SourceRevision {
                file: Some(file),
                contents: Some(before.revision(file)),
                ..SourceRevision::default()
            };
            checked_history_error(
                cause,
                metadata.source_revision(path),
                Some(log),
                metadata.updated_at,
            )
        })?;
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
    ) -> Result<(CanonicalSnapshot, bool)> {
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
        let log = provider_file(root, &directory.join("updates.jsonl"));
        if let Some(file) = &log {
            let mut previous_owner_damaged = false;
            let mut push = |record: Value, gap: bool| -> Result<()> {
                read_any = true;
                let ownership = update_ownership(&record, &session.id)?;
                if ownership == UpdateOwnership::Damaged || is_update_record(&record) {
                    observed += 1;
                }
                if ownership == UpdateOwnership::Damaged {
                    updates.malformed_updates += 1;
                    previous_owner_damaged = true;
                } else {
                    updates.push_record(&mut builder, record, gap || previous_owner_damaged);
                    previous_owner_damaged = false;
                }
                Ok(())
            };
            if full {
                omissions = visit_json_lines_with_omissions(
                    file,
                    MAX_COLLECTED_TRANSCRIPT_FILE_SIZE,
                    push,
                )?;
            } else {
                let preview = json_lines_preview_with_omissions(
                    file,
                    SAMPLE_RECORDS,
                    MAX_PREVIEW_TAIL_SIZE,
                    MAX_TRANSCRIPT_LINE_SIZE,
                    |_, _| WorkspaceGap::Other,
                )?;
                sampled = preview.sampled;
                omissions = preview.omissions;
                for (index, record) in preview.records.into_iter().enumerate() {
                    push(record, preview.gap_before.contains(&index))?;
                }
            }
        }
        if !read_any
            && declared_message_count(summary) > 0
            && (full || (!sampled && omissions.total() == 0))
        {
            return Err(GrokHistoryError::Missing(session.id.clone()).into());
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
        push_grok_state(&mut builder, directory);
        let line_limit = if full {
            MAX_STREAMED_TRANSCRIPT_LINE_SIZE
        } else {
            MAX_TRANSCRIPT_LINE_SIZE
        };
        builder.push_json_line_omissions(omissions, line_limit, None, metadata.updated_at);
        let snapshot = builder.snapshot(
            session.clone(),
            metadata.title.clone(),
            metadata.project_path.clone(),
            metadata.git_branch.clone(),
            metadata
                .updated_at
                .or_else(|| {
                    fs::metadata(log.as_deref().unwrap_or(path))
                        .ok()?
                        .modified()
                        .ok()
                        .map(Into::into)
                })
                .unwrap_or_else(Utc::now),
        );
        Ok((snapshot, full || (!sampled && omissions.total() == 0)))
    }
}

impl Default for GrokAdapter {
    fn default() -> Self {
        Self {
            sessions_root: provider_root("GROK_HOME", &[".grok"]).map(|root| root.join("sessions")),
            catalog: Arc::default(),
            unavailable_ids: Arc::default(),
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

struct CumulativeText {
    offset: usize,
    marker_index: usize,
    text: String,
    markers: Vec<(usize, Rc<Value>)>,
}

impl CumulativeText {
    fn carry_missing_markers(mut self, next: &mut Self) {
        if self.markers.is_empty() {
            return;
        }
        let mut repeated = HashMap::new();
        for (_, block) in &next.markers {
            *repeated.entry(Rc::as_ptr(block)).or_insert(0_usize) += 1;
        }
        self.markers.retain(|(_, block)| {
            let count = repeated.entry(Rc::as_ptr(block)).or_default();
            if *count == 0 {
                true
            } else {
                *count -= 1;
                false
            }
        });
        self.offset = 0;
        self.marker_index = next.markers.len();
        self.place_markers(&next.text, &mut next.markers);
    }

    fn place_markers(self, text: &str, markers: &mut Vec<(usize, Rc<Value>)>) {
        if self.markers.is_empty() {
            return;
        }
        let start = text.find(&self.text);
        markers.splice(
            self.marker_index..self.marker_index,
            self.markers
                .into_iter()
                .map(|(offset, block)| (start.map_or(self.offset, |start| start + offset), block)),
        );
    }
}

struct PendingMessage {
    kind: EventKind,
    text: String,
    content: Vec<Rc<Value>>,
    alternate_content: Vec<(usize, Vec<Rc<Value>>)>,
    alternate_markers: Vec<(usize, Rc<Value>)>,
    retained_blocks: HashMap<Rc<Value>, BlockOccurrences>,
    content_unreadable: bool,
    content_limited: bool,
    cumulative_text: Option<CumulativeText>,
    has_source_text: bool,
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
        && summary["num_messages"].is_u64()
        && [
            "generated_title",
            "parent_session_id",
            "grok_home",
            "sandbox_profile",
            "agent_id",
            "attempt_id",
            "session_kind",
        ]
        .iter()
        .all(|key| optional_native_field(summary, key, Value::is_string))
        && optional_native_field(summary, "forked_at", |value| {
            value
                .as_str()
                .is_some_and(|time| DateTime::parse_from_rfc3339(time).is_ok())
        })
        && summary
            .get("chat_format_version")
            .is_none_or(|version| version.as_u64().is_some_and(|v| u8::try_from(v).is_ok()))
        && ["num_chat_messages", "next_trace_turn"]
            .iter()
            .all(|key| summary.get(key).is_none_or(Value::is_u64))
        && summary.get("git_remotes").is_none_or(|value| {
            value
                .as_array()
                .is_some_and(|remotes| remotes.iter().all(Value::is_string))
        })
}

fn optional_native_field(object: &Value, key: &str, valid: impl FnOnce(&Value) -> bool) -> bool {
    object
        .get(key)
        .is_none_or(|value| value.is_null() || valid(value))
}

fn native_message_block(content: &Value) -> bool {
    let annotations = &content["annotations"];
    let valid_annotations = annotations.is_null()
        || (annotations.is_object()
            && optional_native_field(annotations, "audience", |value| {
                value.as_array().is_some_and(|roles| {
                    roles
                        .iter()
                        .all(|role| matches!(role.as_str(), Some("user" | "assistant")))
                })
            })
            && optional_native_field(annotations, "priority", Value::is_number)
            && optional_native_field(annotations, "lastModified", Value::is_string));
    if !valid_annotations || !optional_native_field(content, "_meta", Value::is_object) {
        return false;
    }
    match content["type"].as_str() {
        Some("text") => content["text"].is_string(),
        Some("image") => {
            content["data"].is_string()
                && content["mimeType"].is_string()
                && optional_native_field(content, "uri", Value::is_string)
        }
        Some("audio") => content["data"].is_string() && content["mimeType"].is_string(),
        Some("resource_link") => {
            content["uri"].is_string()
                && content["name"].is_string()
                && ["title", "description", "mimeType"]
                    .iter()
                    .all(|key| optional_native_field(content, key, Value::is_string))
                && optional_native_field(content, "size", Value::is_u64)
        }
        Some("resource") => {
            let resource = &content["resource"];
            resource["uri"].is_string()
                && (resource["text"].is_string() || resource["blob"].is_string())
                && optional_native_field(resource, "mimeType", Value::is_string)
                && optional_native_field(resource, "_meta", Value::is_object)
        }
        _ => false,
    }
}

fn native_message_content(update: &Value) -> bool {
    let content = &update["content"];
    if !native_message_block(content) || malformed_message_block(content) {
        return false;
    }
    let body = message_body(update);
    if body.is_some_and(|body| std::ptr::eq(content, body)) {
        return true;
    }
    body.is_none() || (content["type"] == "text" && content["text"] == "")
}

fn native_tool_content(value: &Value) -> bool {
    value.as_array().is_some_and(|items| {
        items.iter().all(|item| {
            optional_native_field(item, "_meta", Value::is_object)
                && match item["type"].as_str() {
                    Some("content") => native_message_block(&item["content"]),
                    Some("diff") => {
                        item["path"].is_string()
                            && item["newText"].is_string()
                            && optional_native_field(item, "oldText", Value::is_string)
                    }
                    Some("terminal") => item["terminalId"].is_string(),
                    _ => false,
                }
        })
    })
}

fn native_tool_locations(value: &Value) -> bool {
    value.as_array().is_some_and(|locations| {
        locations.iter().all(|location| {
            location["path"].is_string()
                && optional_native_field(location, "line", |line| {
                    line.as_u64().is_some_and(|n| u32::try_from(n).is_ok())
                })
                && optional_native_field(location, "_meta", Value::is_object)
        })
    })
}

fn native_tool_status(value: &Value) -> bool {
    matches!(
        value.as_str(),
        Some("pending" | "in_progress" | "completed" | "failed")
    )
}

fn grok_message_id(update: &Value) -> Option<&str> {
    string_at(
        update,
        &[&["messageId"], &["message_id"], &["message", "id"]],
    )
}

#[derive(Default)]
struct NativeMessageGroup {
    kind: Option<EventKind>,
    message_id: Option<String>,
    native_id: Option<String>,
    attachments: HashMap<[u8; 32], (usize, usize)>,
    canonical_text: Sha256,
    copied_text: HashSet<[u8; 32]>,
    required_text: HashSet<[u8; 32]>,
    has_chunk: bool,
    has_fallback: bool,
}

fn redundant_damaged_text(body: &Value, canonical: &Value) -> bool {
    let mut remaining = if let Some(text) = block_text(canonical).filter(|text| !text.is_empty()) {
        text
    } else if attachment_block(canonical) && message_blocks(body).contains(canonical) {
        ""
    } else {
        return false;
    };
    for block in message_blocks(body) {
        if let MessageBlock::Text(text) = classify_message_block(block) {
            let Some(rest) = remaining.strip_prefix(text) else {
                return false;
            };
            remaining = rest;
        }
    }
    remaining.is_empty()
}

fn empty_text_descriptor(block: &Value) -> bool {
    block["type"] == "text"
        && block["text"].is_null()
        && block.as_object().is_some_and(|fields| {
            fields
                .keys()
                .all(|key| matches!(key.as_str(), "type" | "text"))
        })
}

impl NativeMessageGroup {
    fn needs_fallback(&self) -> bool {
        (self.has_fallback && !self.has_chunk)
            || !self.required_text.is_empty()
            || self
                .attachments
                .values()
                .any(|(copied, needed)| copied < needed)
    }

    fn observe(&mut self, update: &Value) -> bool {
        let kind = update["sessionUpdate"].as_str().and_then(message_kind);
        let id = grok_message_id(update);
        let native_id = update["messageId"].as_str();
        let changed = id != self.message_id.as_deref();
        if kind.is_some()
            && kind == self.kind
            && changed != (native_id != self.native_id.as_deref())
        {
            return false;
        }
        if kind != self.kind || changed {
            if self.needs_fallback() {
                return false;
            }
            *self = Self {
                kind,
                message_id: id.map(str::to_owned),
                native_id: native_id.map(str::to_owned),
                ..Self::default()
            };
        }
        if self.kind.is_some() {
            let canonical = &update["content"];
            if let Some(text) = block_text(canonical).filter(|text| !text.is_empty()) {
                self.canonical_text.update(text.as_bytes());
                let prefix = self.canonical_text.clone().finalize().into();
                let chunk = Sha256::digest(text.as_bytes()).into();
                for digest in [prefix, chunk] {
                    self.required_text.remove(&digest);
                    if self.copied_text.len() < MAX_GROK_PRESERVED_RECORDS {
                        self.copied_text.insert(digest);
                    }
                }
            }
            if attachment_block(canonical) {
                self.attachments
                    .entry(block_digest(canonical))
                    .or_default()
                    .0 += 1;
            }
            for body in message_bodies(update).filter(|body| !std::ptr::eq(*body, canonical)) {
                let mut occurrences = HashMap::<_, usize>::new();
                let mut damaged_text = false;
                for block in message_blocks(body) {
                    if block.is_null()
                        || matches!(classify_message_block(block), MessageBlock::Text(_))
                    {
                        continue;
                    }
                    if empty_text_descriptor(block) {
                        damaged_text = true;
                        continue;
                    }
                    if !attachment_block(block) {
                        return false;
                    }
                    let digest = block_digest(block);
                    let count = occurrences.entry(digest).or_default();
                    *count += 1;
                    let (_, needed) = self.attachments.entry(digest).or_default();
                    *needed = (*needed).max(*count);
                }
                if damaged_text && !redundant_damaged_text(body, canonical) {
                    let mut alternate = Sha256::new();
                    let mut has_text = false;
                    for block in message_blocks(body) {
                        if let Some(text) = block_text(block).filter(|text| !text.is_empty()) {
                            alternate.update(text.as_bytes());
                            has_text = true;
                        }
                    }
                    if has_text {
                        let digest = alternate.finalize().into();
                        if !self.copied_text.contains(&digest) {
                            self.required_text.insert(digest);
                        }
                    } else if occurrences.is_empty() {
                        return false;
                    }
                }
            }
            if !self.required_text.is_empty() {
                let digest: [u8; 32] = self.canonical_text.clone().finalize().into();
                self.required_text.remove(&digest);
            }
            self.has_fallback |= cumulative_fallback(update).is_some();
            self.has_chunk |= block_text(&update["content"]).is_some_and(|text| !text.is_empty());
        }
        self.attachments.len() + self.required_text.len() <= MAX_GROK_PRESERVED_RECORDS
    }
}

fn block_digest(block: &Value) -> [u8; 32] {
    Sha256::digest(serde_json::to_vec(block).expect("JSON values serialize")).into()
}

// A native fork rekeys every update it copies. Check the source owners before handing it to Grok.
fn native_update_headers(
    path: &Path,
    id: &str,
    expected_updates: u64,
) -> Result<NativeResumeSupport> {
    let mut group = NativeMessageGroup::default();
    let Some(observed) = scan_update_headers(path, |line| {
        let Some(line) = line else {
            return Ok(false);
        };
        let Ok(record) = serde_json::from_slice::<Value>(line) else {
            return Ok(false);
        };
        if update_ownership(&record, id)? != UpdateOwnership::Explicit {
            return Ok(false);
        }
        let update = &record["params"]["update"];
        if !group.observe(update)
            || !optional_native_field(&record["params"], "_meta", Value::is_object)
            || !optional_native_field(update, "_meta", Value::is_object)
        {
            return Ok(false);
        }
        Ok(match update["sessionUpdate"].as_str() {
            Some("user_message_chunk" | "agent_message_chunk" | "agent_thought_chunk") => {
                optional_native_field(update, "messageId", Value::is_string)
                    && native_message_content(update)
            }
            Some("tool_call") => {
                update["toolCallId"].is_string()
                    && update["title"].is_string()
                    && update.get("kind").is_none_or(Value::is_string)
                    && update.get("status").is_none_or(native_tool_status)
                    && update.get("locations").is_none_or(native_tool_locations)
                    && update.get("content").is_none_or(native_tool_content)
            }
            Some("tool_call_update") => {
                update["toolCallId"].is_string()
                    && optional_native_field(update, "kind", Value::is_string)
                    && optional_native_field(update, "status", native_tool_status)
                    && optional_native_field(update, "locations", native_tool_locations)
                    && optional_native_field(update, "content", native_tool_content)
                    && optional_native_field(update, "title", Value::is_string)
            }
            Some("session_info_update") => {
                optional_native_field(update, "title", Value::is_string)
                    && optional_native_field(update, "updatedAt", Value::is_string)
            }
            Some("plan") => update["entries"].as_array().is_some_and(|entries| {
                entries.iter().all(|entry| {
                    entry["content"].is_string()
                        && matches!(entry["priority"].as_str(), Some("low" | "medium" | "high"))
                        && matches!(
                            entry["status"].as_str(),
                            Some("pending" | "in_progress" | "completed")
                        )
                        && optional_native_field(entry, "_meta", Value::is_object)
                })
            }),
            Some("current_mode_update") => update["currentModeId"].is_string(),
            Some("available_commands_update") => update["availableCommands"].is_array(),
            Some("config_option_update") => update["configOptions"].is_array(),
            Some("usage_update") => update["used"].is_u64() && update["size"].is_u64(),
            _ => false,
        })
    })?
    else {
        return Ok(NativeResumeSupport::ImportOnly);
    };
    anyhow::ensure!(
        observed > 0 || expected_updates == 0,
        "Grok session `{id}` declares history but has no readable updates"
    );
    Ok(if observed < expected_updates || group.needs_fallback() {
        NativeResumeSupport::ImportOnly
    } else {
        NativeResumeSupport::Direct
    })
}

fn scan_update_headers(
    path: &Path,
    mut accepts: impl FnMut(Option<&[u8]>) -> Result<bool>,
) -> Result<Option<u64>> {
    let file = fs::File::open(path)?;
    let bytes = file.metadata()?.len();
    let mut reader = BufReader::new(file.take(bytes));
    let mut line = Vec::new();
    let mut observed = 0_u64;
    while let Some((kind, _)) = read_bounded_line_with_budget(
        &mut reader,
        MAX_STREAMED_TRANSCRIPT_LINE_SIZE,
        u64::MAX,
        &mut line,
    )? {
        let record = match kind {
            BoundedLine::Complete if line.iter().all(u8::is_ascii_whitespace) => continue,
            BoundedLine::Complete => Some(line.as_slice()),
            BoundedLine::Oversized => None,
        };
        if !accepts(record)? {
            return Ok(None);
        }
        observed += 1;
    }
    Ok(Some(observed))
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
        return Err(GrokHistoryError::ConflictingSummaries(id.to_owned()).into());
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
        // Invalidate cached projections when recovery changes which source records are visible.
        let checked = Uuid::new_v5(&file, b"grok.owned-update-count.v14");
        let checked = Uuid::new_v5(
            &checked,
            &self.expected_updates.unwrap_or_default().to_le_bytes(),
        );
        let scope = Uuid::new_v5(&checked, project.as_os_str().as_encoded_bytes());
        let metadata = Uuid::new_v5(&scope, self.title.as_deref().unwrap_or_default().as_bytes());
        let mut contents = metadata;
        if let Some(directory) = path.parent() {
            let stamp = |name: &str| {
                let path = provider_file(directory, &directory.join(name))?;
                SnapshotFileStamp::read(&path).ok()
            };
            if let Some(updates) = stamp("updates.jsonl") {
                contents = updates.revision(contents);
            }
            if let Some(plan) = stamp("plan.json") {
                contents = plan.revision(Uuid::new_v5(&contents, b"plan.json"));
            }
        }
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
    possible_message_records: usize,
    malformed_updates: usize,
}

#[derive(Default)]
struct BlockOccurrences {
    canonical: usize,
    retained: usize,
}

struct GrokPreservationBudget {
    bytes_left: usize,
    records_left: usize,
    omitted_events: usize,
}

const MAX_GROK_PRESERVED_RECORDS: usize = 100_000;

impl Default for GrokPreservationBudget {
    fn default() -> Self {
        Self {
            bytes_left: 64 * 1024 * 1024,
            records_left: MAX_GROK_PRESERVED_RECORDS,
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

fn is_update_envelope(record: &Value) -> bool {
    record["method"] == "session/update" || record.pointer("/params/update").is_some()
}

fn is_update_record(record: &Value) -> bool {
    is_update_envelope(record)
        || string_at(record, &[&["sessionUpdate"], &["type"], &["event"]]).is_some()
}

impl GrokUpdateProcessor {
    fn push_record(&mut self, builder: &mut EventBuilder, mut record: Value, gap: bool) {
        let timestamp = parse_timestamp(value_at(
            &record,
            &[&["timestamp"], &["created_at"], &["createdAt"]],
        ));
        let is_update_envelope = is_update_envelope(&record);
        let nested_update = record
            .as_object_mut()
            .and_then(|object| object.get_mut("params"))
            .and_then(Value::as_object_mut)
            .and_then(|params| params.remove("update"));
        let missing_update = is_update_envelope && nested_update.is_none();
        let update = nested_update.unwrap_or(record);
        let raw_type = if missing_update {
            None
        } else {
            string_at(&update, &[&["sessionUpdate"], &["type"], &["event"]]).map(str::to_owned)
        };
        let update_type = raw_type.as_deref().unwrap_or("");
        if let Some(kind) = message_kind(update_type) {
            self.push_message(builder, &update, kind, raw_type, timestamp, gap);
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
            if is_update_envelope && update_type.is_empty() {
                self.malformed_updates = self.malformed_updates.saturating_add(1);
            } else if kind == EventKind::ProviderEvent && update_type != "tool_call_update" {
                self.unsupported_text_records = self.unsupported_text_records.saturating_add(1);
                if !matches!(update_type, "session_info_update" | "usage_update") {
                    self.possible_message_records = self.possible_message_records.saturating_add(1);
                }
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

    fn push_message(
        &mut self,
        builder: &mut EventBuilder,
        update: &Value,
        kind: EventKind,
        raw_type: Option<String>,
        timestamp: Option<DateTime<Utc>>,
        gap: bool,
    ) {
        if kind == EventKind::ReasoningSummary && !self.preservation.retain(update) {
            flush_message(builder, &mut self.pending);
            return;
        }
        let message_id = grok_message_id(update).map(str::to_owned);
        let mut retained_blocks = self
            .pending
            .as_mut()
            .filter(|pending| pending.kind == kind && pending.message_id == message_id)
            .map(|pending| std::mem::take(&mut pending.retained_blocks))
            .unwrap_or_default();
        let omitted_before = self.preservation.omitted_events;
        let mut alternate_markers = Vec::new();
        let (mut text, content, content_unreadable, mut has_source_text) = read_message_content(
            update,
            &mut self.preservation,
            &mut alternate_markers,
            &mut retained_blocks,
        );
        let cumulative_text = if has_source_text && cumulative_fallback(update).is_some() {
            has_source_text = false;
            Some(CumulativeText {
                offset: 0,
                marker_index: 0,
                text: std::mem::take(&mut text),
                markers: std::mem::take(&mut alternate_markers),
            })
        } else {
            None
        };
        let content_limited = self.preservation.omitted_events > omitted_before;
        if content_unreadable {
            self.malformed_updates = self.malformed_updates.saturating_add(1);
        }
        let (content, mut alternate) = split_message_evidence(update, content);
        match &mut self.pending {
            Some(pending) if pending.kind == kind && pending.message_id == message_id => {
                if gap {
                    pending.text.push_str("\n[Source records omitted]\n");
                }
                if let Some(mut fallback) = cumulative_text {
                    if let Some(previous) = pending.cumulative_text.take() {
                        if !pending.has_source_text && previous.offset == pending.text.len() {
                            previous.carry_missing_markers(&mut fallback);
                        } else {
                            previous.place_markers(&pending.text, &mut pending.alternate_markers);
                        }
                    }
                    fallback.offset = pending.text.len();
                    fallback.marker_index = pending.alternate_markers.len();
                    pending.cumulative_text = Some(fallback);
                }
                pending.has_source_text |= has_source_text;
                let offset = pending.text.len();
                pending.alternate_markers.extend(
                    alternate_markers
                        .into_iter()
                        .map(|(pos, block)| (pos + offset, block)),
                );
                pending.text.push_str(&text);
                pending.content.extend(content);
                pending.retained_blocks = retained_blocks;
                pending.alternate_content.extend(
                    alternate
                        .drain(..)
                        .map(|view| (pending.content.len(), view)),
                );
                pending.content_unreadable |= content_unreadable;
                pending.content_limited |= content_limited;
            }
            _ => {
                flush_message(builder, &mut self.pending);
                self.pending = Some(PendingMessage {
                    kind,
                    text,
                    alternate_markers,
                    retained_blocks,
                    alternate_content: alternate
                        .into_iter()
                        .map(|view| (content.len(), view))
                        .collect(),
                    content,
                    content_unreadable,
                    content_limited,
                    cumulative_text,
                    has_source_text,
                    timestamp,
                    raw_type,
                    message_id,
                });
            }
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
        let malformed_updates = std::mem::take(&mut self.malformed_updates);
        if malformed_updates > 0 {
            builder.push(EventKind::ProviderEvent, json!({"omitted_events": malformed_updates, "malformed_updates": malformed_updates}), None, ReplayPolicy::HistoricalOnly, Some("omnisession.grok_malformed_updates".to_owned()), None);
        }
        let unsupported_text_records = std::mem::take(&mut self.unsupported_text_records);
        if unsupported_text_records > 0 {
            builder.push(EventKind::ProviderEvent,
                json!({"unsupported_text_records": unsupported_text_records,
                    "possible_message_records": std::mem::take(&mut self.possible_message_records),
                    "raw_records_preserved": true,
                    "reason": "Unknown Grok updates remain historical metadata; their text cannot be transferred safely"}),
                None, ReplayPolicy::HistoricalOnly,
                Some("omnisession.unsupported_history".to_owned()), None);
        }
    }
}

fn message_kind(update_type: &str) -> Option<EventKind> {
    match update_type {
        "user_message_chunk" | "user" | "user_message" => Some(EventKind::MessageUser),
        "agent_message_chunk" | "assistant" | "assistant_message" => {
            Some(EventKind::MessageAssistant)
        }
        "agent_thought_chunk" => Some(EventKind::ReasoningSummary),
        _ => None,
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

fn message_bodies(update: &Value) -> impl Iterator<Item = &Value> + Clone {
    [
        update.get("content"),
        update.get("text"),
        update.pointer("/message/content"),
        update.pointer("/message/text"),
    ]
    .into_iter()
    .flatten()
}

fn message_blocks(value: &Value) -> &[Value] {
    value
        .as_array()
        .map_or_else(|| std::slice::from_ref(value), Vec::as_slice)
}

enum MessageBlock<'a> {
    Text(&'a str),
    Attachment,
    Malformed(Option<&'a str>),
    Opaque,
}

fn classify_message_block(block: &Value) -> MessageBlock<'_> {
    if let Some(text) = block.as_str() {
        return MessageBlock::Text(text);
    }
    let text = block["text"].as_str();
    match block["type"].as_str() {
        Some("text") => text.map_or(MessageBlock::Malformed(None), MessageBlock::Text),
        Some("image" | "audio" | "resource_link" | "resource") => {
            let has_image_uri = block["type"] == "image"
                && block["uri"].as_str().is_some_and(|uri| !uri.is_empty());
            let empty_media = block["type"] == "image" && block["data"] == "" && !has_image_uri;
            if native_message_block(block) && !empty_media {
                MessageBlock::Attachment
            } else {
                MessageBlock::Malformed(text)
            }
        }
        Some(_) => MessageBlock::Opaque,
        None if block.get("type").is_none() => {
            text.map_or(MessageBlock::Malformed(None), MessageBlock::Text)
        }
        None => MessageBlock::Malformed(text),
    }
}

fn block_text(block: &Value) -> Option<&str> {
    match classify_message_block(block) {
        MessageBlock::Text(text) | MessageBlock::Malformed(Some(text)) => Some(text),
        _ => None,
    }
}

fn attachment_block(block: &Value) -> bool {
    matches!(classify_message_block(block), MessageBlock::Attachment)
}

fn partial_attachment_block(block: &Value) -> bool {
    if !matches!(classify_message_block(block), MessageBlock::Malformed(_)) {
        return false;
    }
    let has_field = |value: &Value, keys: &[&str]| {
        keys.iter()
            .any(|key| value[*key].as_str().is_some_and(|text| !text.is_empty()))
    };
    match block["type"].as_str() {
        Some("image") => has_field(block, &["data", "mimeType", "uri"]),
        Some("audio") => has_field(block, &["data", "mimeType"]),
        Some("resource_link") => {
            has_field(block, &["uri", "name", "title", "description", "mimeType"])
                || block["size"].is_u64()
        }
        Some("resource") => has_field(&block["resource"], &["uri", "text", "blob", "mimeType"]),
        _ => false,
    }
}

fn malformed_message_block(block: &Value) -> bool {
    matches!(
        classify_message_block(block),
        MessageBlock::Malformed(_) | MessageBlock::Opaque
    )
}

fn message_body(update: &Value) -> Option<&Value> {
    let contributes = |block: &Value| {
        block_text(block).is_some_and(|text| !text.is_empty()) || attachment_block(block)
    };
    message_bodies(update).find(|value| message_blocks(value).iter().any(contributes))
}

fn cumulative_fallback(update: &Value) -> Option<&Value> {
    if update["content"]["type"] != "text" || update["content"]["text"] != "" {
        return None;
    }
    message_body(update).filter(|body| {
        message_blocks(body)
            .iter()
            .any(|block| block_text(block).is_some_and(|text| !text.is_empty()))
    })
}

fn readable_body_bytes(body: &Value) -> impl Iterator<Item = u8> + '_ {
    message_blocks(body)
        .iter()
        .filter_map(block_text)
        .flat_map(str::bytes)
}

fn uncertain_alternate_block(block: &Value) -> bool {
    if !matches!(classify_message_block(block), MessageBlock::Malformed(_)) {
        return false;
    }
    partial_attachment_block(block)
        || block
            .get("text")
            .is_some_and(|value| !value.is_null() && value != "")
        || block.as_object().is_some_and(|object| {
            !object.is_empty() && !object.contains_key("type") && !object.contains_key("text")
        })
}

struct UpdateBlockRetention<'a, 'b> {
    budget: &'b mut GrokPreservationBudget,
    retained: &'b mut HashMap<Rc<Value>, BlockOccurrences>,
    primary: HashMap<&'a Value, usize>,
    occurrences: HashMap<&'a Value, usize>,
}

impl<'a> UpdateBlockRetention<'a, '_> {
    fn retain(&mut self, block: &'a Value) -> Option<Rc<Value>> {
        let occurrence = self.occurrences.entry(block).or_default();
        *occurrence += 1;
        let canonical = *occurrence <= self.primary.get(block).copied().unwrap_or(0);
        if let Some((value, counts)) = self.retained.get_key_value(block) {
            let value = Rc::clone(value);
            let required = if canonical {
                counts.canonical + 1
            } else {
                *occurrence
            };
            if required > counts.retained && !self.budget.retain(block) {
                return None;
            }
            let counts = self.retained.get_mut(block).expect("retained block");
            counts.canonical += usize::from(canonical);
            counts.retained = counts.retained.max(required);
            return Some(value);
        }
        if !self.budget.retain(block) {
            return None;
        }
        let value = Rc::new(block.clone());
        self.retained.insert(
            Rc::clone(&value),
            BlockOccurrences {
                canonical: usize::from(canonical),
                retained: 1,
            },
        );
        Some(value)
    }
}

#[cfg(test)]
fn message_content(
    update: &Value,
    preservation: &mut GrokPreservationBudget,
    markers: &mut Vec<(usize, Rc<Value>)>,
) -> (String, Vec<Value>, bool, bool) {
    let (text, blocks, partial, has_text) =
        read_message_content(update, preservation, markers, &mut HashMap::new());
    (
        text,
        blocks.into_iter().map(Rc::unwrap_or_clone).collect(),
        partial,
        has_text,
    )
}

fn historical_block(block: &Value) -> bool {
    !block.is_null() && !matches!(classify_message_block(block), MessageBlock::Text(_))
}

fn retain_alternate_blocks<'a>(
    update: &'a Value,
    content: &'a Value,
    retention: &mut UpdateBlockRetention<'a, '_>,
    historical_content: &mut Vec<Rc<Value>>,
) -> bool {
    let mut content_unreadable = false;
    let mut seen = HashMap::new();
    for block in message_blocks(content)
        .iter()
        .filter(|block| historical_block(block))
    {
        *seen.entry(block).or_insert(0usize) += 1;
    }
    for alternate in message_bodies(update).filter(|value| !std::ptr::eq(*value, content)) {
        let blocks = message_blocks(alternate);
        let conflicting_text = blocks.iter().any(malformed_message_block)
            && blocks
                .iter()
                .any(|block| block_text(block).is_some_and(|text| !text.is_empty()))
            && readable_body_bytes(alternate).ne(readable_body_bytes(content));
        content_unreadable |= conflicting_text;
        let mut occurrences = HashMap::new();
        for block in blocks.iter().filter(|block| {
            historical_block(block)
                || (conflicting_text && block_text(block).is_some_and(|text| !text.is_empty()))
        }) {
            let count = occurrences.entry(block).or_insert(0usize);
            *count += 1;
            let retained = seen.entry(block).or_default();
            if *count > *retained {
                *retained = *count;
                content_unreadable |= uncertain_alternate_block(block);
                if let Some(retained) = retention.retain(block) {
                    historical_content.push(retained);
                }
            }
        }
    }
    content_unreadable
}

fn read_message_content(
    update: &Value,
    preservation: &mut GrokPreservationBudget,
    alternate_markers: &mut Vec<(usize, Rc<Value>)>,
    retained: &mut HashMap<Rc<Value>, BlockOccurrences>,
) -> (String, Vec<Rc<Value>>, bool, bool) {
    let mut primary = HashMap::new();
    if let Some(body) = message_bodies(update).next() {
        for block in message_blocks(body) {
            *primary.entry(block).or_default() += 1;
        }
    }
    let mut retention = UpdateBlockRetention {
        budget: preservation,
        retained,
        primary,
        occurrences: HashMap::new(),
    };
    let mut has_source_text = false;
    let mut text = String::new();
    let mut historical_content = Vec::new();
    let mut content_unreadable = false;
    let body = message_body(update)
        .or_else(|| {
            message_bodies(update).find(|value| message_blocks(value).iter().any(historical_block))
        })
        .or_else(|| {
            update
                .get("content")
                .filter(|block| block["type"] == "text" && block["text"] == "")
        });
    if let Some(content) = body {
        let deferred = message_bodies(update)
            .next()
            .is_some_and(|primary| !std::ptr::eq(primary, content));
        for block in message_blocks(content) {
            match classify_message_block(block) {
                MessageBlock::Text(chunk) => {
                    has_source_text |= !chunk.is_empty();
                    text.push_str(chunk);
                    continue;
                }
                MessageBlock::Malformed(Some(chunk)) if !chunk.is_empty() => {
                    content_unreadable = true;
                    has_source_text = true;
                    text.push_str(chunk);
                }
                MessageBlock::Malformed(_) | MessageBlock::Opaque => {
                    content_unreadable = true;
                    text.push_str("\n[Source message content unreadable]\n");
                }
                MessageBlock::Attachment => {
                    let Some(retained) = retention.retain(block) else {
                        const OMITTED: &str = "\n[Source attachment omitted by collection limit]\n";
                        if !text.ends_with(OMITTED) {
                            text.push_str(OMITTED);
                        }
                        continue;
                    };
                    if deferred {
                        alternate_markers.push((text.len(), Rc::clone(&retained)));
                    } else {
                        if !text.is_empty() {
                            text.push('\n');
                        }
                        text.push_str(attachment_notice(block));
                    }
                    historical_content.push(retained);
                    continue;
                }
            }
            if let Some(retained) = retention.retain(block) {
                historical_content.push(retained);
            }
        }
        content_unreadable |=
            retain_alternate_blocks(update, content, &mut retention, &mut historical_content);
    } else {
        content_unreadable = true;
        text.push_str("\n[Source message content unreadable]\n");
    }
    (
        text,
        historical_content,
        content_unreadable,
        has_source_text,
    )
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

const GROK_STATE_FILES: [(&str, EventKind, &str, &str); 3] = [
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
];

fn push_grok_state(builder: &mut EventBuilder, directory: &Path) {
    for (filename, kind, record_type, field) in GROK_STATE_FILES {
        let Some(path) = provider_file(directory, &directory.join(filename)) else {
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

fn split_message_evidence(
    update: &Value,
    content: Vec<Rc<Value>>,
) -> (Vec<Rc<Value>>, Vec<Vec<Rc<Value>>>) {
    if content.is_empty() {
        return (content, Vec::new());
    }
    let mut bodies = message_bodies(update);
    let Some(primary) = bodies.next() else {
        return (content, Vec::new());
    };
    if bodies.clone().next().is_none() {
        return (content, Vec::new());
    }
    let mut retained = HashMap::new();
    for block in &content {
        let (_, count) = retained.entry(block.as_ref()).or_insert((block, 0usize));
        *count += 1;
    }
    let collect = |body: &Value| {
        let mut occurrences = HashMap::new();
        message_blocks(body)
            .iter()
            .filter_map(|block| {
                let (value, limit) = retained.get(block)?;
                let count = occurrences.entry(block).or_insert(0usize);
                *count += 1;
                (*count <= *limit).then(|| Rc::clone(value))
            })
            .collect::<Vec<_>>()
    };
    (
        collect(primary),
        bodies
            .map(collect)
            .filter(|view| !view.is_empty())
            .collect(),
    )
}

fn reconcile_message_evidence<T: Eq + std::hash::Hash + Clone>(
    chunks: Vec<T>,
    views: &[(usize, Vec<T>)],
) -> Vec<T> {
    if views.is_empty() {
        return chunks;
    }
    let mut positions = HashMap::<&T, Vec<usize>>::new();
    for (index, block) in chunks.iter().enumerate() {
        positions.entry(block).or_default().push(index);
    }
    let mut extras = HashSet::new();
    let mut insertions = Vec::new();
    for (offset, view) in views {
        let mut occurrences = HashMap::new();
        let mut pending = Vec::new();
        let mut after_last_match = 0;
        for block in view {
            let index = occurrences.entry(block).or_insert(0usize);
            if let Some(position) = positions.get(block).and_then(|items| items.get(*index)) {
                insertions.extend(pending.drain(..).map(|extra| (*position, extra)));
                after_last_match = *position + 1;
            } else if extras.insert((block, *index)) {
                pending.push(block);
            }
            *index += 1;
        }
        insertions.extend(
            pending
                .into_iter()
                .map(|extra| ((*offset).max(after_last_match), extra)),
        );
    }
    insertions.sort_by_key(|(offset, _)| *offset);
    let mut insertions = insertions.into_iter().peekable();
    let mut result = Vec::with_capacity(chunks.len() + insertions.len());
    for (index, block) in chunks.into_iter().enumerate() {
        while insertions
            .peek()
            .is_some_and(|(offset, _)| *offset == index)
        {
            result.push(insertions.next().unwrap().1.clone());
        }
        result.push(block);
    }
    result.extend(insertions.map(|(_, block)| block.clone()));
    result
}

fn attachment_notice(block: &Value) -> &'static str {
    match block["type"].as_str() {
        Some("image") => "[Source image attachment preserved as historical metadata]",
        Some("audio") => "[Source audio attachment preserved as historical metadata]",
        _ => "[Source nontext content preserved as historical metadata]",
    }
}

fn flush_message(builder: &mut EventBuilder, pending: &mut Option<PendingMessage>) {
    let Some(pending) = pending.take() else {
        return;
    };
    let mut text = pending.text;
    let mut additions = Vec::new();
    let mut markers = pending.alternate_markers;
    if let Some(fallback) = pending.cumulative_text {
        if pending.has_source_text {
            fallback.place_markers(&text, &mut markers);
        } else {
            text.insert_str(fallback.offset, &fallback.text);
            for (offset, _) in &mut markers[fallback.marker_index..] {
                *offset += fallback.text.len();
            }
            markers.splice(
                fallback.marker_index..fallback.marker_index,
                fallback
                    .markers
                    .into_iter()
                    .map(|(offset, block)| (offset + fallback.offset, block)),
            );
        }
    }
    let mut canonical = HashMap::new();
    if !markers.is_empty() {
        for block in &pending.content {
            *canonical.entry(block.clone()).or_insert(0usize) += 1;
        }
    }
    let content = reconcile_message_evidence(pending.content, &pending.alternate_content);
    if !markers.is_empty() {
        let mut available = HashMap::new();
        for block in &content {
            *available.entry(block).or_insert(0usize) += 1;
        }
        for (offset, block) in &markers {
            let used = canonical.entry(block.clone()).or_default();
            if *used < available.get(block).copied().unwrap_or_default() {
                additions.push((*offset, attachment_notice(block).to_owned()));
                *used += 1;
            }
        }
    }
    if !additions.is_empty() {
        additions.sort_by_key(|(offset, _)| *offset);
        let mut rendered = String::with_capacity(text.len());
        let mut start = 0;
        for (offset, addition) in additions {
            rendered.push_str(&text[start..offset]);
            rendered.push_str(&addition);
            start = offset;
        }
        rendered.push_str(&text[start..]);
        text = rendered;
    }
    let mut payload = json!({ "text": text });
    if pending.content_unreadable {
        payload["content_unreadable"] = Value::Bool(true);
    }
    if pending.content_limited {
        payload["content_limited"] = Value::Bool(true);
    }
    if (pending.content_unreadable || pending.content_limited)
        && let Some(id) = pending.message_id
    {
        payload["message_id"] = Value::String(id);
    }
    let opaque = content
        .iter()
        .filter(|block| matches!(classify_message_block(block), MessageBlock::Opaque))
        .count();
    let nontext_blocks = content
        .iter()
        .filter(|block| attachment_block(block))
        .count();
    if !content.is_empty() {
        payload["content"] = Value::Array(content.into_iter().map(Rc::unwrap_or_clone).collect());
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
    report_message_omissions(builder, pending.timestamp, opaque, nontext_blocks);
}

fn report_message_omissions(
    builder: &mut EventBuilder,
    timestamp: Option<DateTime<Utc>>,
    opaque: usize,
    nontext_blocks: usize,
) {
    if opaque > 0 {
        builder.push(
            EventKind::ProviderEvent,
            json!({"unsupported_text_records":opaque,"raw_records_preserved":true}),
            timestamp,
            ReplayPolicy::HistoricalOnly,
            Some("omnisession.unsupported_history".to_owned()),
            None,
        );
    }
    if nontext_blocks > 0 {
        builder.push(
                EventKind::ProviderEvent,
                json!({
                    "omitted_events": nontext_blocks,
                    "event_kind": "attachment",
                    "reason": "Attachments remain in canonical historical metadata; native transfer carries documentary text only",
                }),
                timestamp,
                ReplayPolicy::HistoricalOnly,
                Some("omnisession.unsupported_content".to_owned()),
                None,
            );
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
        *self
            .unavailable_ids
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = ambiguous;
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

    fn unavailable_session_ids(&self) -> Vec<String> {
        self.unavailable_ids
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .cloned()
            .collect()
    }

    fn discovery_notes(&self) -> Vec<String> {
        if self
            .unavailable_ids
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
        {
            Vec::new()
        } else {
            vec![
                "Grok skipped sessions with conflicting or unreadable native summaries.".to_owned(),
            ]
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
        let ownership_revision = Self::updates_revision(&path);
        let full = Self::updates_size(&path).unwrap_or(0) <= full_read_bytes;
        let mut snapshot = self.snapshot(session, &summary, &path, &metadata, full);
        let provider_limited = full_read_bytes != u64::MAX
            && full
            && snapshot
                .as_ref()
                .is_err_and(anyhow::Error::is::<crate::support::TranscriptReadLimit>);
        if provider_limited {
            snapshot = self.snapshot(session, &summary, &path, &metadata, false);
        }
        anyhow::ensure!(
            Self::updates_revision(&path) == ownership_revision,
            "selected Grok source changed during its read"
        );
        let native = metadata.into_session(session.id.clone(), path);
        anyhow::ensure!(
            native.source_revision == Some(source_revision),
            "selected Grok source changed during its read"
        );
        let (snapshot, source_complete) = snapshot.map_err(|cause| {
            checked_history_error(
                cause,
                source_revision,
                ownership_revision,
                native.updated_at,
            )
        })?;
        Ok(IndexRead {
            database_aliases: Arc::default(),
            verified_prefix_end: None,
            search_record_groups: Vec::new(),
            provider_limited,
            ownership_revision,
            snapshot,
            workspace_projection: CodexWorkspaceProjection::Unchecked,
            rollback_title: RollbackTitle::Unknown,
            source_complete: source_complete && !provider_limited,
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
    fn unreadable_update_envelopes_count_as_present_but_incomplete() {
        let root = tempfile::tempdir().expect("synthetic Grok root");
        write_session(
            root.path(),
            SESSION_ID,
            &json!({"id": SESSION_ID, "num_messages": 6}),
            &[
                json!({"sessionUpdate": "user_message_chunk", "content": {"text": "question"}}),
                json!({"params": {"sessionId": SESSION_ID, "update": 17}}),
                json!({"sessionUpdate": "agent_message_chunk", "messageId": "reply"}),
                json!({"method": "session/update", "params": {"sessionId": SESSION_ID}}),
                json!({"method": "session/update", "params": {"sessionId": SESSION_ID, "update": null}}),
                json!({"method": "session/update", "params": {"sessionId": SESSION_ID, "update": {"sessionUpdate": "session_info_update", "title": "Example"}}}),
            ],
        );
        let snapshot = GrokAdapter::with_root(root.path())
            .read_session(&SessionRef::new(Provider::Grok, SESSION_ID))
            .expect("partially readable history");
        assert_eq!(
            snapshot
                .events
                .iter()
                .find_map(|event| event.payload["malformed_updates"].as_u64()),
            Some(4)
        );
        let assistant = snapshot
            .events
            .iter()
            .find(|event| event.kind == EventKind::MessageAssistant)
            .expect("recorded assistant chunk");
        assert_eq!(assistant.payload["message_id"], "reply");
        assert_eq!(assistant.payload["content_unreadable"], true);
        assert!(omnis_core::import_conversation(&snapshot).truncated);
        assert!(
            !snapshot
                .events
                .iter()
                .any(|event| event.source.raw_record_type.as_deref()
                    == Some("omnisession.grok_missing_updates"))
        );
        assert!(
            snapshot
                .events
                .iter()
                .any(|event| event.kind == EventKind::MessageUser)
        );
    }

    #[test]
    fn empty_nested_message_content_preserves_the_assistant_occurrence() {
        let root = tempfile::tempdir().unwrap();
        let update = json!({"params":{"sessionId":SESSION_ID,"update":{
            "sessionUpdate":"agent_message_chunk","messageId":"reply","message":{"content":[]}
        }}});
        write_session(
            root.path(),
            SESSION_ID,
            &json!({"id": SESSION_ID, "num_messages":1}),
            &[update],
        );
        let session = SessionRef::new(Provider::Grok, SESSION_ID);
        let snapshot = GrokAdapter::with_root(root.path())
            .read_session(&session)
            .unwrap();
        let assistant = &snapshot.events[0];
        assert_eq!(assistant.kind, EventKind::MessageAssistant);
        assert_eq!(assistant.payload["message_id"], "reply");
        assert_eq!(assistant.payload["content_unreadable"], true);
        assert!(omnis_core::import_conversation(&snapshot).truncated);
    }

    #[test]
    fn fallback_keeps_partial_attachment_fields() {
        let descriptor = json!({"type":"image","data":"YWJj"});
        let empty_text = json!({"type":"text","text":""});
        let body = json!({"content":empty_text,"message":{"content":[descriptor]}});
        let (text, retained, partial, _) = message_content(
            &body,
            &mut GrokPreservationBudget::default(),
            &mut Vec::new(),
        );
        assert!(partial);
        assert!(text.contains("content unreadable"));
        assert_eq!(retained, [descriptor]);
        let descriptor = json!({"type":"image","mimeType":"image/png","text":""});
        let body = json!({"content":descriptor,"message":{"text":"recovered reply"}});
        let (text, retained, partial, _) = message_content(
            &body,
            &mut GrokPreservationBudget::default(),
            &mut Vec::new(),
        );
        assert!(partial);
        assert_eq!(text, "recovered reply");
        assert_eq!(retained, [descriptor]);
        let image = json!({"type":"image","data":"","mimeType":"image/png","uri":"file:///synthetic/image"});
        assert!(native_message_content(&json!({"content":image})));
    }

    #[test]
    fn repeated_attachment_view_has_one_rendered_notice() {
        let resource = json!({"type":"resource","resource":{"uri":"fixture:r","text":"data"}});
        let chunk = |content: Value| json!({"sessionUpdate":"agent_message_chunk","messageId":"reply","content":content});
        let mut cumulative = chunk(json!({"type":"text","text":""}));
        cumulative["message"] = json!({"content":[resource]});
        let mut caption = cumulative.clone();
        caption["message"]["content"] = json!([resource, {"type":"text","text":"caption"}]);
        let mut trailing = caption.clone();
        trailing["message"]["content"] = json!([{"type":"text","text":"caption"}, resource]);
        let notice = attachment_notice(&resource);
        for (updates, expected) in [
            (
                vec![chunk(resource.clone()), cumulative.clone()],
                notice.to_owned(),
            ),
            (
                vec![cumulative.clone(), chunk(resource.clone())],
                notice.to_owned(),
            ),
            (vec![cumulative], notice.to_owned()),
            (
                vec![chunk(resource.clone()), caption.clone()],
                format!("{notice}caption"),
            ),
            (
                vec![
                    chunk(json!({"type":"text","text":"caption"})),
                    caption.clone(),
                ],
                format!("{notice}caption"),
            ),
            (
                vec![
                    caption.clone(),
                    chunk(json!({"type":"text","text":"caption"})),
                ],
                format!("{notice}caption"),
            ),
            (
                vec![
                    chunk(json!({"type":"text","text":"caption"})),
                    trailing.clone(),
                ],
                format!("caption{notice}"),
            ),
            (vec![trailing.clone(), trailing], format!("caption{notice}")),
            (vec![caption], format!("{notice}caption")),
        ] {
            let root = tempfile::tempdir().unwrap();
            write_session(root.path(), SESSION_ID, &json!({"id":SESSION_ID}), &updates);
            let snapshot = GrokAdapter::with_root(root.path())
                .read_session(&SessionRef::new(Provider::Grok, SESSION_ID))
                .unwrap();
            let message = &snapshot.events[0].payload;
            assert_eq!(message["text"], expected);
            assert_eq!(message["content"], json!([resource]));
        }
    }

    #[test]
    fn cumulative_evidence_matches_ordered_chunk_occurrences() {
        let a = json!({"type":"resource_link","uri":"fixture:a","name":"a"});
        let b = json!({"type":"resource_link","uri":"fixture:b","name":"b"});
        let c = json!({"type":"resource_link","uri":"fixture:c","name":"c"});
        let view = vec![a.clone(), b.clone(), b.clone(), c.clone()];
        for offset in [0, 2] {
            assert_eq!(
                reconcile_message_evidence(
                    vec![a.clone(), c.clone()],
                    &[(offset, view.clone()), (offset, view.clone())]
                ),
                view
            );
        }
    }

    #[test]
    fn damaged_alternate_keeps_conflicting_raw_text() {
        let raw = json!({"type":"text","text":"other","annotations":{"priority":0.5}});
        let damaged = json!({"type":"text"});
        let update =
            json!({"content":{"type":"text","text":"answer"},"message":{"content":[raw,damaged]}});
        let (visible, retained, partial, _) = message_content(
            &update,
            &mut GrokPreservationBudget::default(),
            &mut Vec::new(),
        );
        assert_eq!(visible, "answer");
        assert_eq!(retained, [raw, damaged]);
        assert!(partial);
        let unknown = json!({"parts":["other"]});
        let update = json!({"content":"answer","message":{"text":unknown}});
        let (visible, retained, partial, _) = message_content(
            &update,
            &mut GrokPreservationBudget::default(),
            &mut Vec::new(),
        );
        assert_eq!(visible, "answer");
        assert_eq!(retained, [unknown]);
        assert!(partial);
    }

    #[test]
    fn alternate_bodies_keep_distinct_historical_blocks() {
        let mime = json!({"type":"image","mimeType":"image/png","text":""});
        let data = json!({"type":"image","data":"YWJj"});
        let update = json!({"content":mime,"message":{"content":[mime,data]}});
        let (_, retained, partial, _) = message_content(
            &update,
            &mut GrokPreservationBudget::default(),
            &mut Vec::new(),
        );
        assert!(partial);
        assert_eq!(retained, [mime, data]);
        let resource =
            json!({"type":"resource","resource":{"uri":"fixture:item","text":"embedded"}});
        let update = json!({"content":{"type":"text","text":""},"text":"reply",
            "message":{"content":[{"type":"text","text":"reply"},resource]}});
        let (text, retained, _, _) = message_content(
            &update,
            &mut GrokPreservationBudget::default(),
            &mut Vec::new(),
        );
        assert_eq!(text, "reply");
        assert_eq!(retained, [resource]);
    }

    #[test]
    fn attachment_evidence_is_distinct_from_recovered_text() {
        let incomplete = json!({"type":"image","data":"YWJj"});
        assert!(partial_attachment_block(&incomplete));
        assert!(!partial_attachment_block(&json!({"type":"image"})));
        let body = json!({"content":incomplete, "message":{"text":"recovered reply"}});
        assert_eq!(message_body(&body), body.pointer("/message/text"));
        assert!(matches!(
            classify_message_block(&json!({})),
            MessageBlock::Malformed(None)
        ));
        assert!(matches!(
            classify_message_block(&json!({"type":17,"text":"readable"})),
            MessageBlock::Malformed(Some("readable"))
        ));
        assert!(matches!(
            classify_message_block(&json!({"type":"unknown","body":1})),
            MessageBlock::Opaque
        ));
        let embedded =
            json!({"content":{"type":"resource","resource":{"uri":"","text":"embedded text"}}});
        assert!(attachment_block(&embedded["content"]));
        assert!(native_message_content(&embedded));
    }

    #[test]
    fn empty_text_blocks_use_the_readable_message_body() {
        let root = tempfile::tempdir().unwrap();
        let update = |message: Value| json!({"sessionUpdate":"agent_message_chunk", "messageId":"reply", "message":message});
        let empty = json!([{"type":"text","text":""}]);
        for (message, expected, partial) in [
            (
                json!({"content":[{"type":"image"}],"text":"restored marigold"}),
                "restored marigold",
                false,
            ),
            (
                json!({"content":[{"type":"image"}]}),
                "\n[Source message content unreadable]\n",
                true,
            ),
            (
                json!({"content":[{"text":null}],"text":"untyped recovery"}),
                "untyped recovery",
                false,
            ),
            (
                json!({"content":[{"type":"text"}],"text":"recovered reply"}),
                "recovered reply",
                false,
            ),
            (
                json!({"content":empty,"text":"fallback reply"}),
                "fallback reply",
                false,
            ),
            (
                json!({"content":empty}),
                "\n[Source message content unreadable]\n",
                true,
            ),
            (
                json!({"content":[{"text":""},{"text":"primary reply"}],"text":"fallback"}),
                "primary reply",
                false,
            ),
        ] {
            write_session(
                root.path(),
                SESSION_ID,
                &json!({"id":SESSION_ID}),
                &[json!({"params":{"sessionId":SESSION_ID,"update":update(message)}})],
            );
            let snapshot = GrokAdapter::with_root(root.path())
                .read_session(&SessionRef::new(Provider::Grok, SESSION_ID))
                .unwrap();
            let assistant = &snapshot.events[0];
            assert_eq!(assistant.kind, EventKind::MessageAssistant);
            assert_eq!(assistant.payload["text"], expected);
            assert_eq!(
                omnis_core::import_conversation(&snapshot).truncated,
                partial
            );
        }
    }

    #[test]
    fn delayed_cumulative_text_matches_copied_chunks_and_prefixes() {
        let chunk = |text| json!({"sessionUpdate":"agent_message_chunk","messageId":"a","content":{"type":"text","text":text}});
        for (view, direct) in [
            ("copper", true),
            ("finch", true),
            ("copperfinch", true),
            ("copp", false),
            ("unmatched", false),
        ] {
            let mut group = NativeMessageGroup::default();
            let mut alias = chunk("");
            alias["message"] = json!({"content":[view, {"type":"text"}]});
            for update in [chunk("copper"), chunk("finch"), chunk("end"), alias] {
                assert!(group.observe(&update));
            }
            assert_eq!(!group.needs_fallback(), direct);
        }
    }

    #[test]
    fn native_message_group_matches_cumulative_evidence_across_chunks() {
        let resource =
            json!({"type":"resource","resource":{"uri":"file:///sample","text":"contents"}});
        let chunk = |content| json!({"sessionUpdate":"agent_message_chunk","messageId":"turn","content":content});
        let alias = |body| {
            let mut update = chunk(json!({"type":"text","text":""}));
            update["message"] = json!({"content":body});
            update
        };
        for (body, direct) in [
            (json!([resource, {"type":"text"}]), true),
            (json!([resource, "copperfinch", {"type":"text"}]), true),
            (json!([resource, "new text", {"type":"text"}]), false),
            (json!([resource, resource, {"type":"text"}]), false),
        ] {
            let mut group = NativeMessageGroup::default();
            for update in [
                chunk(resource.clone()),
                chunk(json!({"type":"text","text":"copper"})),
                alias(body),
                chunk(json!({"type":"text","text":"finch"})),
            ] {
                assert!(group.observe(&update));
            }
            assert_eq!(!group.needs_fallback(), direct);
        }
    }

    #[test]
    fn native_fork_keeps_repeated_resources_and_empty_audio() {
        let resource =
            json!({"type":"resource","resource":{"uri":"file:///sample","text":"contents"}});
        let audio = json!({"type":"audio","data":"","mimeType":"audio/wav"});
        assert!(attachment_block(&audio));
        assert!(!attachment_block(
            &json!({"type":"audio","data":7,"mimeType":"audio/wav"})
        ));
        let repeated = json!([resource, {"type":"text"}]);
        assert!(redundant_damaged_text(&repeated, &resource));
        let extra_text = json!([resource, {"type":"text"}, "additional"]);
        assert!(!redundant_damaged_text(&extra_text, &resource));
    }

    #[test]
    fn native_fork_accepts_only_redundant_empty_text_descriptors() {
        let root = tempfile::tempdir().unwrap();
        let log = root.path().join("updates.jsonl");
        for (alias, descriptor, direct) in [
            ("answer", json!({"type":"text"}), true),
            ("answer", json!({"type":"text","text":null}), true),
            ("different", json!({"type":"text"}), false),
            ("answer", json!({"type":"text","note":"evidence"}), false),
            ("answer", json!({"type":"future","value":7}), false),
        ] {
            let record = json!({"params":{"sessionId":SESSION_ID,"update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"answer"},"message":{"content":[{"type":"text","text":alias},descriptor]}}}});
            fs::write(&log, record.to_string()).unwrap();
            assert_eq!(
                native_update_headers(&log, SESSION_ID, 1).unwrap() == NativeResumeSupport::Direct,
                direct
            );
        }
    }

    #[test]
    fn native_message_groups_compare_copied_attachments_and_message_boundaries() {
        let root = tempfile::tempdir().unwrap();
        let log = root.path().join("updates.jsonl");
        let chunk = |content| json!({"sessionUpdate":"agent_message_chunk","messageId":"a","content":content});
        let resource =
            json!({"type":"resource","resource":{"uri":"file:///sample","text":"contents"}});
        let text = chunk(json!({"type":"text","text":"first"}));
        let fallback = |blocks| {
            let mut update = chunk(json!({"type":"text","text":""}));
            update["text"] = json!("first");
            update["message"] = json!({"content":blocks});
            update
        };
        let borrowed = fallback(json!(["first", resource]));
        let mut nested = text.clone();
        nested.as_object_mut().unwrap().remove("messageId");
        nested["message"] = json!({"id":"a"});
        for (updates, direct) in [
            (
                vec![chunk(resource.clone()), text.clone(), borrowed.clone()],
                true,
            ),
            (
                vec![text.clone(), borrowed.clone(), chunk(resource.clone())],
                true,
            ),
            (vec![text.clone(), borrowed], false),
            (
                vec![
                    chunk(resource.clone()),
                    text.clone(),
                    fallback(json!(["first", resource, resource])),
                ],
                false,
            ),
            (vec![text, nested], false),
        ] {
            let lines = updates
                .iter()
                .map(|update| {
                    json!({"params":{"sessionId":SESSION_ID,"update":update}}).to_string()
                })
                .collect::<Vec<_>>()
                .join("\n");
            fs::write(&log, lines).unwrap();
            assert_eq!(
                native_update_headers(&log, SESSION_ID, updates.len() as u64).unwrap()
                    == NativeResumeSupport::Direct,
                direct
            );
        }
    }

    #[test]
    fn native_fork_checks_nested_message_ids_and_cumulative_resources() {
        let root = tempfile::tempdir().unwrap();
        let log = root.path().join("updates.jsonl");
        let chunk = |id, text, body| json!({"params":{"sessionId":SESSION_ID,"update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":text},"message":{"id":id,"content":body}}}});
        for (id, body, support) in [
            ("a", json!(["silver"]), NativeResumeSupport::Direct),
            ("b", json!(["silver"]), NativeResumeSupport::ImportOnly),
            (
                "a",
                json!(["silver", {"type":"resource","resource":{"uri":"file:///sample","text":"document"}}]),
                NativeResumeSupport::ImportOnly,
            ),
        ] {
            fs::write(
                &log,
                format!(
                    "{}\n{}",
                    chunk("a", "first", Value::Null),
                    chunk(id, "", body)
                ),
            )
            .unwrap();
            assert_eq!(native_update_headers(&log, SESSION_ID, 2).unwrap(), support);
        }
    }

    #[test]
    fn canonical_empty_chunk_keeps_the_contiguous_reply() {
        let root = tempfile::tempdir().unwrap();
        let mut updates = ["copper", "", "finch"].map(|text|
            json!({"method":"session/update","params":{"sessionId":SESSION_ID,"update":{
                "sessionUpdate":"agent_message_chunk","messageId":"reply","content":{"type":"text","text":text}
            }}})
        );
        updates[1]["params"]["update"]["message"] = json!({"content":[{"type":"text","text":"copper"},{"type":"text","text":"finch"}, null]});
        write_session(
            root.path(),
            SESSION_ID,
            &json!({"info":{"id":SESSION_ID}, "num_messages":3}),
            &updates,
        );
        let adapter = GrokAdapter::with_root(root.path());
        let snapshot = adapter
            .read_session(&SessionRef::new(Provider::Grok, SESSION_ID))
            .unwrap();
        assert_eq!(snapshot.events[0].payload["text"], "copperfinch");
        assert_eq!(snapshot.events[0].payload["content"], json!([null]));
    }

    #[test]
    fn damage_preserves_the_only_readable_fallback_at_its_source_position() {
        let root = tempfile::tempdir().unwrap();
        let updates = [
            json!({"content":{"type":"text","text":""},"message":{"text":"marigold fallback"}}),
            json!({"content":{"type":"text","text":null}}),
        ]
        .map(|mut update| {
            update["sessionUpdate"] = json!("agent_message_chunk");
            update["messageId"] = json!("same-reply");
            json!({"params":{"sessionId":SESSION_ID,"update":update}})
        });
        let adapter = GrokAdapter::with_root(root.path());
        for (records, expected) in [
            (
                updates.clone(),
                "marigold fallback\n[Source message content unreadable]\n",
            ),
            (
                [updates[1].clone(), updates[0].clone()],
                "\n[Source message content unreadable]\nmarigold fallback",
            ),
        ] {
            write_session(root.path(), SESSION_ID, &json!({"id":SESSION_ID}), &records);
            let snapshot = adapter
                .read_session(&SessionRef::new(Provider::Grok, SESSION_ID))
                .unwrap();
            assert_eq!(snapshot.events[0].payload["text"], expected);
            assert!(omnis_core::import_conversation(&snapshot).truncated);
        }
    }

    #[test]
    fn malformed_blocks_preserve_the_gap_and_opaque_evidence() {
        let root = tempfile::tempdir().expect("synthetic Grok root");
        let block = json!({"type": "text", "text": {"part": "opaque"}, "annotations": {"audience": ["user"]}});
        let records = ["Release ticket A".into(), block.clone(), "B is ready".into()]
            .map(|content: Value| json!({"sessionUpdate": "agent_message_chunk", "messageId": "reply", "content": content}));
        write_session(
            root.path(),
            SESSION_ID,
            &json!({"id": SESSION_ID}),
            &records,
        );
        let snapshot = GrokAdapter::with_root(root.path())
            .read_session(&SessionRef::new(Provider::Grok, SESSION_ID))
            .expect("partial message");
        let payload = &snapshot.events[0].payload;
        assert_eq!(
            payload["text"],
            "Release ticket A\n[Source message content unreadable]\nB is ready"
        );
        assert_eq!(payload["content"][0], block);
        assert!(omnis_core::import_conversation(&snapshot).truncated);
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
    fn torn_records_separate_same_id_chunks_in_full_and_bounded_reads() {
        let root = tempfile::tempdir().unwrap();
        let directory = write_session(root.path(), SESSION_ID, &json!({"id": SESSION_ID}), &[]);
        let chunk = |text| json!({"sessionUpdate":"agent_message_chunk","messageId":"r","content":{"type":"text","text":text}});
        fs::write(
            directory.join("updates.jsonl"),
            format!("{}\n{{\n{}\n", chunk("copper"), chunk("finch")),
        )
        .unwrap();
        let adapter = GrokAdapter::with_root(root.path());
        for budget in [0, u64::MAX] {
            let read = adapter
                .index_session(&SessionRef::new(Provider::Grok, SESSION_ID), None, budget)
                .unwrap();
            assert_eq!(
                read.snapshot.events[0].payload["text"],
                "copper\n[Source records omitted]\nfinch"
            );
        }
    }

    #[test]
    fn grok_preview_covers_a_metadata_only_log_read_to_eof() {
        let root = tempfile::tempdir().unwrap();
        write_session(
            root.path(),
            SESSION_ID,
            &json!({"id":SESSION_ID,"num_messages":1}),
            &[
                json!({"params":{"sessionId":SESSION_ID,"update":{"sessionUpdate":"available_commands_update"}}}),
            ],
        );
        let adapter = GrokAdapter::with_root(root.path());
        let session = SessionRef::new(Provider::Grok, SESSION_ID);
        assert!(
            adapter
                .index_session(&session, None, 0)
                .unwrap()
                .source_complete
        );
    }

    #[test]
    fn damaged_owners_leave_a_gap_between_verified_chunks() {
        let id = "abcdefab-1234-4123-8123-abcdefabcdef";
        let root = tempfile::tempdir().unwrap();
        let chunk = |owner: Value, text| {
            json!({"params":{"sessionId":owner,"update":{
                "sessionUpdate":"user_message_chunk","messageId":"ask","content":{"text":text}
            }}})
        };
        write_session(
            root.path(),
            id,
            &json!({"id":id,"num_messages":11}),
            &[
                chunk(json!(id), "north"),
                chunk(Value::Null, "hidden"),
                chunk(json!(7), "hidden"),
                chunk(json!("broken-id"), "hidden"),
                Value::Null,
                json!({}),
                json!({"params":7}),
                json!({"method":"session/update","params":null}),
                json!({"method":"session/update","params":{"update":{}}}),
                json!({"params":{"update":{}}}),
                chunk(json!(id.to_ascii_uppercase()), "star"),
            ],
        );
        let adapter = GrokAdapter::with_root(root.path());
        let session = SessionRef::new(Provider::Grok, id);
        for budget in [0, u64::MAX] {
            let read = adapter.index_session(&session, None, budget).unwrap();
            assert_eq!(
                read.snapshot.events[0].payload["text"],
                "north\n[Source records omitted]\nstar"
            );
            assert!(
                read.snapshot
                    .events
                    .iter()
                    .any(|e| e.payload["malformed_updates"] == 9)
            );
            GrokAdapter::verify_update_owners(
                &session,
                read.native_session
                    .as_ref()
                    .unwrap()
                    .source_path
                    .as_deref()
                    .unwrap(),
                read.ownership_revision.unwrap(),
                read.source_revision,
                None,
            )
            .unwrap();
        }
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
        let captured_at = DateTime::<Utc>::from(log.metadata().unwrap().modified().unwrap());
        assert_eq!(
            adapter.preview_session(&session).unwrap().captured_at,
            captured_at
        );
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
    fn native_fork_preserves_resource_blocks_and_recovers_invalid_message_headers() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("updates.jsonl");
        let mut update = json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "visible reply"}});
        for (message_id, content, expected) in [
            (
                json!(17),
                update["content"].clone(),
                NativeResumeSupport::ImportOnly,
            ),
            (
                Value::Null,
                update["content"].clone(),
                NativeResumeSupport::Direct,
            ),
            (
                json!("reply"),
                json!({"type":"resource_link","uri":"file:///synthetic/item","name":"sample"}),
                NativeResumeSupport::Direct,
            ),
            (
                json!("reply"),
                json!({"type":"resource","resource":{"uri":"file:///synthetic/item","text":"embedded"}}),
                NativeResumeSupport::Direct,
            ),
            (
                json!("reply"),
                json!({"type":"resource_link","uri":"file:///synthetic/item"}),
                NativeResumeSupport::ImportOnly,
            ),
        ] {
            update["messageId"] = message_id;
            update["content"] = content;
            fs::write(
                &log,
                json!({"params": {"sessionId": SESSION_ID, "update": update}}).to_string(),
            )
            .unwrap();
            assert_eq!(
                native_update_headers(&log, SESSION_ID, 1).unwrap(),
                expected
            );
        }
    }

    #[test]
    fn native_fork_checks_tool_kind_on_calls_and_updates() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("updates.jsonl");
        for (tag, kind, expected) in [
            ("tool_call", json!("custom"), NativeResumeSupport::Direct),
            ("tool_call", json!(4), NativeResumeSupport::ImportOnly),
            ("tool_call", Value::Null, NativeResumeSupport::ImportOnly),
            (
                "tool_call_update",
                json!(4),
                NativeResumeSupport::ImportOnly,
            ),
            ("tool_call_update", Value::Null, NativeResumeSupport::Direct),
        ] {
            let update =
                json!({"sessionUpdate": tag, "toolCallId": "call", "title": "Read", "kind": kind});
            fs::write(
                &log,
                json!({"params": {"sessionId": SESSION_ID, "update": update}}).to_string(),
            )
            .unwrap();
            assert_eq!(
                native_update_headers(&log, SESSION_ID, 1).unwrap(),
                expected
            );
        }
    }

    #[test]
    fn native_fork_checks_nested_tool_result_content() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("updates.jsonl");
        for tag in ["tool_call", "tool_call_update"] {
            for (content, direct) in [
                (Value::Null, tag == "tool_call_update"),
                (
                    json!([{"type":"content","content":{"type":"text","text":""}}]),
                    true,
                ),
                (json!([{"type":"content","content":{"type":"text"}}]), false),
                (
                    json!([{"type":"diff","path":"file","newText":"changed"}]),
                    true,
                ),
                (json!([{"type":"terminal","terminalId":"term"}]), true),
                (json!([{"type":"terminal","terminalId":7}]), false),
            ] {
                let update = json!({"sessionUpdate":tag,"toolCallId":"call","title":"Read","content":content,"rawOutput":"visible"});
                fs::write(
                    &log,
                    json!({"params":{"sessionId":SESSION_ID,"update":update}}).to_string(),
                )
                .unwrap();
                assert_eq!(
                    native_update_headers(&log, SESSION_ID, 1).unwrap()
                        == NativeResumeSupport::Direct,
                    direct
                );
            }
        }
    }

    #[test]
    fn native_fork_checks_locations_on_tool_calls_and_updates() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("updates.jsonl");
        for tag in ["tool_call", "tool_call_update"] {
            for (locations, direct) in [
                (Value::Null, tag == "tool_call_update"),
                (json!([{"path":"file","line":1}]), true),
                (json!([{"path":"file","line":null}]), true),
                (json!([{"path":"file","line":"broken"}]), false),
                (json!([{"path":"file","line":4_294_967_296_u64}]), false),
                (json!([{"line":1}]), false),
            ] {
                let update = json!({"sessionUpdate":tag,"toolCallId":"call","title":"Read","locations":locations});
                fs::write(
                    &log,
                    json!({"params":{"sessionId":SESSION_ID,"update":update}}).to_string(),
                )
                .unwrap();
                assert_eq!(
                    native_update_headers(&log, SESSION_ID, 1).unwrap()
                        == NativeResumeSupport::Direct,
                    direct
                );
            }
        }
    }

    #[test]
    fn native_fork_checks_envelope_metadata_and_history_status() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("updates.jsonl");
        let plan = |status| json!({"sessionUpdate": "plan", "entries": [{"content": "inspect", "priority": "high", "status": status}]});
        let call = |status| json!({"sessionUpdate": "tool_call", "toolCallId": "call", "title": "Read", "status": status});
        for (meta, update, expected) in [
            (
                json!({}),
                call(json!("failed")),
                NativeResumeSupport::Direct,
            ),
            (
                json!(7),
                call(json!("failed")),
                NativeResumeSupport::ImportOnly,
            ),
            (Value::Null, call(json!(7)), NativeResumeSupport::ImportOnly),
            (
                Value::Null,
                json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": ""}, "message": {"text": "recovered"}}),
                NativeResumeSupport::ImportOnly,
            ),
            (Value::Null, plan(json!(7)), NativeResumeSupport::ImportOnly),
            (
                Value::Null,
                plan(json!("completed")),
                NativeResumeSupport::Direct,
            ),
        ] {
            fs::write(
                &log,
                json!({"params": {"sessionId": SESSION_ID, "_meta": meta, "update": update}})
                    .to_string(),
            )
            .unwrap();
            assert_eq!(
                native_update_headers(&log, SESSION_ID, 1).unwrap(),
                expected
            );
        }
    }

    fn check_native_summary_fields(complete: &Value) {
        for (version, valid) in [(255, true), (256, false)] {
            let mut summary = complete.clone();
            summary["chat_format_version"] = json!(version);
            assert_eq!(can_fork_summary(SESSION_ID, &summary), valid);
        }
        let mut titled = complete.clone();
        titled["generated_title"] = json!({});
        assert!(!can_fork_summary(SESSION_ID, &titled));
        titled["generated_title"] = json!("A title");
        assert!(can_fork_summary(SESSION_ID, &titled));
    }

    #[test]
    fn native_fork_requires_the_fields_read_by_grok() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        let complete = json!({"info":{"id":SESSION_ID,"cwd":project},"session_summary":"Title","created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-02T00:00:00Z","current_model_id":"synthetic","num_messages":0});
        check_native_summary_fields(&complete);
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
        let incomplete = json!({"params":{"sessionId":SESSION_ID,"update":{"sessionUpdate":"agent_message_chunk","messageId":"reply"}}});
        fs::write(&updates, incomplete.to_string()).unwrap();
        assert_eq!(
            adapter.native_resume_support(&session).unwrap(),
            NativeResumeSupport::ImportOnly
        );
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
    fn cumulative_views_charge_the_maximum_retained_occurrences() {
        let mut builder = EventBuilder::new(Provider::Grok, SESSION_ID);
        let mut processor = GrokUpdateProcessor::default();
        processor.preservation.records_left = 3;
        let block = json!({"type":"image","mimeType":"image/png","data":"YQ=="});
        let primary = |id: &str| json!({"sessionUpdate":"agent_message_chunk","messageId":id,"content":block});
        let view = json!({"sessionUpdate":"agent_message_chunk","messageId":"a","content":{"type":"text","text":""},"message":{"content":[block,block]}});
        for record in [
            primary("a"),
            view.clone(),
            view,
            primary("a"),
            primary("b"),
            primary("c"),
        ] {
            processor.push_record(&mut builder, record, false);
        }
        processor.finish(&mut builder);
        let snapshot = builder.snapshot(
            SessionRef::new(Provider::Grok, SESSION_ID),
            None,
            None,
            None,
            Utc::now(),
        );
        let messages: Vec<_> = snapshot
            .events
            .iter()
            .filter(|event| event.kind == EventKind::MessageAssistant)
            .map(|event| &event.payload)
            .collect();
        assert_eq!(messages[0]["content"], json!([block, block]));
        assert_eq!(messages[1]["content"], json!([block]));
        assert_eq!(messages[2]["content_limited"], true);
        assert_eq!(snapshot.events.last().unwrap().payload["omitted_events"], 1);
    }

    #[test]
    fn historical_retention_budget_reports_loss_without_limiting_visible_text_or_tools() {
        let mut builder = EventBuilder::new(Provider::Grok, SESSION_ID);
        let mut processor = GrokUpdateProcessor {
            pending: None,
            unsupported_text_records: 0,
            possible_message_records: 0,
            malformed_updates: 0,
            preservation: GrokPreservationBudget {
                bytes_left: 2_048,
                records_left: 2,
                omitted_events: 0,
            },
        };
        for record in [
            json!({"sessionUpdate": "future_record", "text": "Small opaque history"}),
            json!({"sessionUpdate": "user_message_chunk", "content": {"type": "image", "mimeType": "image/png", "data": "x".repeat(8_192)}}),
            json!({"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": "x".repeat(8_192)}}),
            json!({"sessionUpdate": "future_record", "text": "x".repeat(8_192)}),
            json!({"sessionUpdate": "user_message_chunk", "content": {"type": "text", "text": "Unchanged visible question"}}),
            json!({"sessionUpdate": "tool_call", "toolCallId": "synthetic", "rawInput": "x".repeat(8_192)}),
            json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "Unchanged visible answer"}}),
        ] {
            processor.push_record(&mut builder, record, false);
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
                &EventKind::MessageUser,
                &EventKind::ToolCalled,
                &EventKind::MessageAssistant,
                &EventKind::ProviderEvent,
                &EventKind::ProviderEvent
            ]
        );
        assert_eq!(snapshot.events[1].payload["content_limited"], true);
        assert!(snapshot.events[1].payload.get("content").is_none());
        assert_eq!(
            snapshot.events[2].payload["text"],
            "Unchanged visible question"
        );
        assert_eq!(
            snapshot.events[4].payload["text"],
            "Unchanged visible answer"
        );
        assert_eq!(snapshot.events[5].payload["omitted_events"], 3);
        assert_eq!(
            snapshot.events[5].source.raw_record_type.as_deref(),
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
            possible_message_records: 0,
            malformed_updates: 0,
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
                false,
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
