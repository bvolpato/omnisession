use std::{
    collections::{HashMap, HashSet},
    fs,
    io::{BufReader, Read},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, RwLock},
};

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use omnis_ir::{CanonicalSnapshot, EventKind, Provider, ReplayPolicy, SessionRef, SourceRevision};
use serde_json::{Value, json};
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
    ForeignOwner,
    Missing(String),
}

impl std::fmt::Display for GrokHistoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
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

struct PendingMessage {
    kind: EventKind,
    text: String,
    content: Vec<Value>,
    content_unreadable: bool,
    cumulative_text: Option<(usize, String)>,
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
        && [
            "num_chat_messages",
            "chat_format_version",
            "next_trace_turn",
        ]
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
    native_message_block(content)
        && !malformed_message_block(content)
        && message_body(update)
            .is_none_or(|body| std::ptr::eq(content, body) || cumulative_fallback(update).is_some())
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

#[derive(Default)]
struct NativeMessageGroup {
    kind: Option<EventKind>,
    message_id: Option<String>,
    has_chunk: bool,
    has_fallback: bool,
}

impl NativeMessageGroup {
    fn needs_fallback(&self) -> bool {
        self.has_fallback && !self.has_chunk
    }

    fn observe(&mut self, update: &Value) -> bool {
        let kind = update["sessionUpdate"].as_str().and_then(message_kind);
        let id = update["messageId"].as_str();
        if kind != self.kind || id != self.message_id.as_deref() {
            if self.needs_fallback() {
                return false;
            }
            *self = Self {
                kind,
                message_id: id.map(str::to_owned),
                ..Self::default()
            };
        }
        if self.kind.is_some() {
            self.has_fallback |= cumulative_fallback(update).is_some();
            self.has_chunk |= block_text(&update["content"]).is_some_and(|text| !text.is_empty());
        }
        true
    }
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
        // Invalidate cached projections when recovery changes which source records are visible.
        let checked = Uuid::new_v5(&file, b"grok.owned-update-count.v7");
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
    malformed_updates: usize,
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
        let message_id = string_at(
            update,
            &[&["messageId"], &["message_id"], &["message", "id"]],
        )
        .map(str::to_owned);
        let omitted_before = self.preservation.omitted_events;
        let cumulative_text = cumulative_fallback(update)
            .map(|blocks| blocks.iter().filter_map(block_text).collect::<String>());
        let (text, content, content_unreadable, has_source_text) = if cumulative_text.is_some() {
            (String::new(), Vec::new(), false, false)
        } else {
            message_content(update, &mut self.preservation)
        };
        if text.is_empty() && self.preservation.omitted_events > omitted_before {
            flush_message(builder, &mut self.pending);
            return;
        }
        if content_unreadable {
            self.malformed_updates = self.malformed_updates.saturating_add(1);
        }
        match &mut self.pending {
            Some(pending) if pending.kind == kind && pending.message_id == message_id => {
                if gap {
                    pending.text.push_str("\n[Source records omitted]\n");
                }
                if let Some(text) = cumulative_text {
                    pending.cumulative_text = Some((pending.text.len(), text));
                }
                pending.has_source_text |= has_source_text;
                pending.text.push_str(&text);
                pending.content.extend(content);
                pending.content_unreadable |= content_unreadable;
            }
            _ => {
                flush_message(builder, &mut self.pending);
                self.pending = Some(PendingMessage {
                    kind,
                    text,
                    content,
                    content_unreadable,
                    cumulative_text: cumulative_text.map(|text| (0, text)),
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

fn message_bodies(update: &Value) -> impl Iterator<Item = &Value> {
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

fn block_text(block: &Value) -> Option<&str> {
    block.as_str().or_else(|| {
        if block["type"].is_null() || block["type"] == "text" {
            block["text"].as_str()
        } else {
            None
        }
    })
}

fn attachment_block(block: &Value) -> bool {
    let present = |value: &Value| value.as_str().is_some_and(|text| !text.is_empty());
    match block["type"].as_str() {
        Some("image" | "audio") => present(&block["data"]),
        Some("resource_link") => present(&block["uri"]),
        Some("resource") => {
            let resource = &block["resource"];
            present(&resource["uri"])
                && (resource["text"].is_string() || resource["blob"].is_string())
        }
        _ => false,
    }
}

fn malformed_message_block(block: &Value) -> bool {
    if matches!(
        block["type"].as_str(),
        Some("image" | "audio" | "resource" | "resource_link")
    ) {
        return !attachment_block(block);
    }
    (block["type"] == "text" || (block["type"].is_null() && block.get("text").is_some()))
        && !block["text"].is_string()
}

fn message_body(update: &Value) -> Option<&Value> {
    let contributes = |block: &Value| {
        block_text(block).is_some_and(|text| !text.is_empty()) || attachment_block(block)
    };
    message_bodies(update).find(|value| message_blocks(value).iter().any(contributes))
}

fn cumulative_fallback(update: &Value) -> Option<&[Value]> {
    if update["content"]["type"] != "text" || update["content"]["text"] != "" {
        return None;
    }
    let blocks = message_blocks(message_body(update)?);
    blocks
        .iter()
        .all(|block| block_text(block).is_some())
        .then_some(blocks)
}

fn message_content(
    update: &Value,
    preservation: &mut GrokPreservationBudget,
) -> (String, Vec<Value>, bool, bool) {
    let mut has_source_text = false;
    let mut text = String::new();
    let mut historical_content = Vec::new();
    let mut content_unreadable = false;
    let body = message_body(update).or_else(|| {
        message_bodies(update).find(|value| {
            let canonical_empty = update.get("content").is_some_and(|block| {
                std::ptr::eq(block, *value) && block["type"] == "text" && block["text"] == ""
            });
            canonical_empty
                || message_blocks(value)
                    .iter()
                    .any(|block| !block.is_null() && block_text(block).is_none())
        })
    });
    if let Some(body) = body {
        for value in message_bodies(update).take_while(|value| !std::ptr::eq(*value, body)) {
            for block in message_blocks(value)
                .iter()
                .filter(|block| !block.is_null() && block_text(block).is_none())
            {
                if preservation.retain(block) {
                    historical_content.push(block.clone());
                }
            }
        }
    }
    let mut push = |block: &Value| {
        if let Some(chunk) = block_text(block) {
            has_source_text |= !chunk.is_empty();
            text.push_str(chunk);
        } else if block.is_null() || malformed_message_block(block) {
            text.push_str("\n[Source message content unreadable]\n");
            content_unreadable = true;
            if !block.is_null() && preservation.retain(block) {
                historical_content.push(block.clone());
            }
        } else if preservation.retain(block) {
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
    if let Some(content) = body {
        for block in message_blocks(content) {
            push(block);
        }
    } else {
        push(&Value::Null);
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

fn flush_message(builder: &mut EventBuilder, pending: &mut Option<PendingMessage>) {
    let Some(pending) = pending.take() else {
        return;
    };
    let mut text = pending.text;
    if !pending.has_source_text
        && let Some((offset, fallback)) = pending.cumulative_text
    {
        text.insert_str(offset, &fallback);
    }
    let mut payload = json!({ "text": text });
    if pending.content_unreadable {
        payload["content_unreadable"] = Value::Bool(true);
        if let Some(id) = pending.message_id {
            payload["message_id"] = Value::String(id);
        }
    }
    let nontext_blocks = pending
        .content
        .iter()
        .filter(|block| block["type"] != "text" && !malformed_message_block(block))
        .count();
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
    fn canonical_empty_chunk_keeps_the_contiguous_reply() {
        let root = tempfile::tempdir().unwrap();
        let mut updates = ["copper", "", "finch"].map(|text|
            json!({"method":"session/update","params":{"sessionId":SESSION_ID,"update":{
                "sessionUpdate":"agent_message_chunk","messageId":"reply","content":{"type":"text","text":text}
            }}})
        );
        updates[1]["params"]["update"]["message"] =
            json!({"content":[{"type":"text","text":"copper"},{"type":"text","text":"finch"}]});
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
        assert!(!omnis_core::import_conversation(&snapshot).truncated);
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

    #[test]
    fn native_fork_requires_the_fields_read_by_grok() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        let complete = json!({"info":{"id":SESSION_ID,"cwd":project},"session_summary":"Title","created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-02T00:00:00Z","current_model_id":"synthetic","num_messages":0});
        let mut titled = complete.clone();
        titled["generated_title"] = json!({});
        assert!(!can_fork_summary(SESSION_ID, &titled));
        titled["generated_title"] = json!("A title");
        assert!(can_fork_summary(SESSION_ID, &titled));
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
    fn historical_retention_budget_reports_loss_without_limiting_visible_text_or_tools() {
        let mut builder = EventBuilder::new(Provider::Grok, SESSION_ID);
        let mut processor = GrokUpdateProcessor {
            pending: None,
            unsupported_text_records: 0,
            malformed_updates: 0,
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
