//! Standalone Antigravity IDE logs. The provider documents the path, but not the JSONL
//! schema. Known Antigravity steps are decoded conservatively and other records remain
//! historical metadata. Logs are exportable history, not native continuation state.

use std::{
    collections::HashMap,
    env, fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use anyhow::{Result, anyhow};
use base64::{Engine, engine::general_purpose::STANDARD};
use chrono::{DateTime, Utc};
use omnis_ir::{CanonicalSnapshot, EventKind, Provider, ReplayPolicy, SessionRef};
use prost::Message;
use rusqlite::OptionalExtension;
use serde_json::{Value, json};

use crate::{
    AntigravityDesktopAdapter, LaunchPlan, LaunchTarget, NativeSession, ProviderAdapter,
    ProviderInstallation,
    antigravity::{antigravity_user_request, proto_timestamp, validate_id},
    antigravity_desktop::ProtoTrajectorySummary,
    support::{
        EventBuilder, MAX_STREAMED_TRANSCRIPT_FILE_SIZE, estimated_event_bytes, json_lines_preview,
        nested_files_matching, parse_timestamp, paths_match, provider_file, provider_root,
        sort_sessions, sqlite_snapshot_bounded, validate_provider, visit_json_lines,
    },
};

const PREVIEW_RECORDS: usize = 128;
const SUMMARY_KEY: &str = "antigravityUnifiedStateSync.trajectorySummaries";
const MAX_SUMMARIES_BYTES: i64 = 64 * 1024 * 1024;
const MAX_HISTORY_BYTES: usize = 64 * 1024 * 1024;
const MAX_HISTORY_RECORDS: usize = 100_000;

#[derive(Clone, Debug)]
pub struct AntigravityIdeAdapter {
    root: Option<PathBuf>,
    legacy_desktop: bool,
    legacy_root: Option<PathBuf>,
    metadata_root: Option<PathBuf>,
    metadata_unavailable: Arc<AtomicBool>,
}

impl AntigravityIdeAdapter {
    #[must_use]
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        Self {
            root: Some(root.into()),
            legacy_desktop: false,
            legacy_root: None,
            metadata_root: None,
            metadata_unavailable: Arc::default(),
        }
    }

    /// Uses a specific IDE user-data directory without consulting other profiles.
    #[must_use]
    pub fn with_roots(root: impl Into<PathBuf>, user_data_root: impl Into<PathBuf>) -> Self {
        Self {
            metadata_root: Some(user_data_root.into()),
            ..Self::with_root(root)
        }
    }

    fn summaries(&self) -> HashMap<String, ProtoTrajectorySummary> {
        self.metadata_unavailable.store(false, Ordering::Relaxed);
        if let Ok(summaries) = self.read_summaries() {
            summaries
        } else {
            self.metadata_unavailable.store(true, Ordering::Relaxed);
            HashMap::new()
        }
    }

    fn read_summaries(&self) -> Result<HashMap<String, ProtoTrajectorySummary>> {
        let Some(root) = self.metadata_root.as_deref() else {
            return Ok(HashMap::new());
        };
        let Some(path) = provider_file(root, &root.join("User/globalStorage/state.vscdb")) else {
            return Ok(HashMap::new());
        };
        // SQLite opens only the private copy so it cannot create WAL sidecars in the provider
        // store. Only this metadata key is queried; other state rows remain opaque.
        let snapshot = sqlite_snapshot_bounded(root, &path, u64::try_from(MAX_SUMMARIES_BYTES)?)?;
        let encoded = snapshot
            .connection
            .query_row(
                "SELECT value FROM ItemTable WHERE key = ?1 AND length(value) <= ?2",
                rusqlite::params![SUMMARY_KEY, MAX_SUMMARIES_BYTES],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        let Some(encoded) = encoded else {
            return Ok(HashMap::new());
        };
        let topic = ProtoTopic::decode(STANDARD.decode(encoded)?.as_slice())?;
        let mut summaries = HashMap::new();
        let mut has_unreadable_rows = false;
        for (id, row) in topic.data {
            if validate_id(&id).is_err() {
                has_unreadable_rows = true;
                continue;
            }
            let Ok(encoded_summary) = STANDARD.decode(row.value) else {
                has_unreadable_rows = true;
                continue;
            };
            let Ok(summary) = ProtoTrajectorySummary::decode(encoded_summary.as_slice()) else {
                has_unreadable_rows = true;
                continue;
            };
            summaries.insert(id, summary);
        }
        if has_unreadable_rows {
            self.metadata_unavailable.store(true, Ordering::Relaxed);
        }
        Ok(summaries)
    }

    fn transcript(&self, id: &str) -> Option<PathBuf> {
        validate_id(id).ok()?;
        let root = self.root.as_deref()?;
        provider_file(
            root,
            &root
                .join("brain")
                .join(id)
                .join(".system_generated/logs/transcript.jsonl"),
        )
    }

    fn desktop(&self) -> Option<AntigravityDesktopAdapter> {
        self.root
            .as_ref()
            .filter(|_| self.legacy_desktop)
            .map(AntigravityDesktopAdapter::with_root)
    }

    fn read(&self, session: &SessionRef, preview: bool) -> Result<CanonicalSnapshot> {
        validate_provider(session, Provider::AntigravityIde)?;
        validate_id(&session.id)?;
        let path = self.transcript(&session.id);
        let legacy_root = if self.legacy_desktop {
            self.root.as_ref()
        } else {
            self.legacy_root.as_ref()
        }
        .filter(|root| {
            provider_file(
                root,
                &root
                    .join("conversations")
                    .join(format!("{}.db", session.id)),
            )
            .is_some()
        });
        if !self.legacy_desktop && path.is_some() && legacy_root.is_some() {
            return Err(anyhow!(
                "Antigravity conversation `{}` exists in both IDE and Desktop stores; select `antigravity-desktop:{}` or set ANTIGRAVITY_IDE_HOME explicitly",
                session.id,
                session.id
            ));
        }
        if let Some(root) = legacy_root {
            let desktop = AntigravityDesktopAdapter::with_root(root);
            let mut snapshot = desktop
                .read_session(&SessionRef::new(Provider::AntigravityDesktop, &session.id))?;
            // Preserve old references while recording the actual product in event provenance.
            snapshot.session = session.clone();
            return Ok(snapshot);
        }
        let path = path.ok_or_else(|| {
            anyhow!(
                "Antigravity IDE session `{}` has no readable conversation log",
                session.id
            )
        })?;
        let mut metadata = LogMetadata::default();
        let mut builder = EventBuilder::new(Provider::AntigravityIde, &session.id);
        let mut retention = HistoryRetention::new(MAX_HISTORY_BYTES, MAX_HISTORY_RECORDS);
        if preview {
            for record in json_lines_preview(&path, PREVIEW_RECORDS)? {
                metadata.observe(&record);
                retention.push_record(&mut builder, &record);
            }
            builder.retain_preview_events(
                PREVIEW_RECORDS,
                metadata.updated_at.unwrap_or_else(Utc::now),
            );
        } else {
            let oversized = visit_json_lines(&path, MAX_STREAMED_TRANSCRIPT_FILE_SIZE, |record| {
                metadata.observe(&record);
                retention.push_record(&mut builder, &record);
                Ok(())
            })?;
            builder.push_oversized_record_notice(oversized, metadata.updated_at);
        }
        metadata.merge_summary(self.summaries().get(&session.id));
        let captured_at = metadata
            .updated_at
            .or_else(|| file_time(&path))
            .unwrap_or_else(Utc::now);
        retention.finish(&mut builder, Some(captured_at));
        Ok(builder.snapshot(
            session.clone(),
            metadata.title,
            metadata.project_path,
            metadata.git_branch,
            captured_at,
        ))
    }
}

impl Default for AntigravityIdeAdapter {
    fn default() -> Self {
        let root = provider_root("ANTIGRAVITY_IDE_HOME", &[".gemini", "antigravity-ide"]);
        let explicitly_set =
            env::var_os("ANTIGRAVITY_IDE_HOME").is_some_and(|value| !value.is_empty());
        // Keep explicit old desktop roots usable. Default discovery never crosses products.
        let legacy_desktop = explicitly_set
            && root.as_ref().is_some_and(|path| {
                path.join("conversations").is_dir()
                    && (path.join("agyhub_summaries_proto.pb").is_file()
                        || !path.join("brain").is_dir())
            });
        Self {
            root,
            legacy_desktop,
            legacy_root: (!explicitly_set)
                .then(|| provider_root("ANTIGRAVITY_DESKTOP_HOME", &[".gemini", "antigravity"]))
                .flatten(),
            metadata_root: env::var_os("ANTIGRAVITY_IDE_USER_DATA_DIR")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .or_else(|| (!explicitly_set).then(default_user_data_root).flatten()),
            metadata_unavailable: Arc::default(),
        }
    }
}

impl ProviderAdapter for AntigravityIdeAdapter {
    fn provider(&self) -> Provider {
        Provider::AntigravityIde
    }

    fn probe(&self) -> ProviderInstallation {
        let data_root = self.root.clone().filter(|root| {
            root.join("brain").is_dir()
                || self.legacy_desktop && root.join("conversations").is_dir()
        });
        ProviderInstallation {
            provider: self.provider(),
            installed: data_root.is_some(),
            executable: None,
            data_root,
        }
    }

    fn list_sessions(&self, project: Option<&Path>) -> Result<Vec<NativeSession>> {
        if let Some(desktop) = self.desktop() {
            let mut sessions = desktop.list_sessions(project)?;
            for session in &mut sessions {
                session.session.provider = Provider::AntigravityIde;
            }
            return Ok(sessions);
        }
        let Some(root) = self.root.as_deref() else {
            return Ok(Vec::new());
        };
        let brain = root.join("brain");
        let summaries = self.summaries();
        let mut sessions = Vec::new();
        for candidate in nested_files_matching(&brain, 3, &|path, is_dir| {
            if is_dir {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| {
                        validate_id(name).is_ok() || matches!(name, ".system_generated" | "logs")
                    })
            } else {
                path.file_name()
                    .is_some_and(|name| name == "transcript.jsonl")
            }
        }) {
            let Some(id) = candidate
                .strip_prefix(&brain)
                .ok()
                .and_then(|path| path.components().next())
                .and_then(|part| part.as_os_str().to_str())
            else {
                continue;
            };
            let Some(path) = self.transcript(id) else {
                continue;
            };
            let mut metadata = LogMetadata::default();
            for record in json_lines_preview(&path, PREVIEW_RECORDS)? {
                metadata.observe(&record);
            }
            metadata.merge_summary(summaries.get(id));
            if project.is_some_and(|requested| {
                metadata
                    .project_path
                    .as_deref()
                    .is_none_or(|recorded| !paths_match(recorded, requested))
            }) {
                continue;
            }
            sessions.push(NativeSession {
                session: SessionRef::new(self.provider(), id),
                title: metadata.title,
                project_path: metadata.project_path,
                git_branch: metadata.git_branch,
                created_at: metadata.created_at,
                updated_at: metadata.updated_at.or_else(|| file_time(&path)),
                updated_at_approximate: metadata.updated_at.is_none(),
                // A sampled log does not establish an exact native step count.
                event_count: summaries.get(id).map_or(0, |summary| {
                    usize::try_from(summary.step_count).unwrap_or(usize::MAX)
                }),
                source_path: Some(path),
            });
        }
        sort_sessions(&mut sessions);
        Ok(sessions)
    }

    fn read_session(&self, session: &SessionRef) -> Result<CanonicalSnapshot> {
        self.read(session, false)
    }

    fn discovery_notes(&self) -> Vec<String> {
        if self.legacy_desktop
            || self
                .root
                .as_ref()
                .is_none_or(|root| !root.join("brain").is_dir())
        {
            return Vec::new();
        }
        let mut notes = vec!["Antigravity IDE log schema is not published; unrecognized records remain historical metadata. Sessions without recorded workspace metadata require --all-projects.".to_owned()];
        if self.metadata_unavailable.load(Ordering::Relaxed) {
            notes.push("Antigravity IDE summary metadata is incomplete or unreadable; discovery uses valid summaries and available conversation logs.".to_owned());
        }
        notes
    }
    fn preview_session(&self, session: &SessionRef) -> Result<CanonicalSnapshot> {
        self.read(session, true)
    }
    fn new_session_plan(&self, _target: &LaunchTarget) -> Result<LaunchPlan> {
        Err(anyhow!(
            "Antigravity IDE has no supported native continuation API; continue the history in Antigravity CLI"
        ))
    }
    fn launch_plan(&self, session: &SessionRef, target: &LaunchTarget) -> Result<LaunchPlan> {
        validate_provider(session, self.provider())?;
        self.new_session_plan(target)
    }
}

#[derive(Default)]
struct LogMetadata {
    title: Option<String>,
    project_path: Option<PathBuf>,
    workspace_ambiguous: bool,
    git_branch: Option<String>,
    created_at: Option<DateTime<Utc>>,
    updated_at: Option<DateTime<Utc>>,
}

fn sole_workspace(value: &Value) -> Option<PathBuf> {
    let mut workspace: Option<PathBuf> = None;
    for value in value.as_array()? {
        let path = Path::new(value.as_str()?);
        if !path.is_absolute() {
            return None;
        }
        let canonical = fs::canonicalize(path).ok().filter(|path| path.is_dir())?;
        if workspace
            .as_ref()
            .is_some_and(|workspace| workspace != &canonical)
        {
            return None;
        }
        workspace = Some(canonical);
    }
    workspace
}

impl LogMetadata {
    fn merge_summary(&mut self, summary: Option<&ProtoTrajectorySummary>) {
        let Some(summary) = summary else {
            return;
        };
        if !summary.summary.is_empty() {
            self.title = Some(summary.summary.clone());
        }
        if summary.has_workspaces() {
            self.project_path = summary.project_path();
        }
        self.git_branch = self
            .project_path
            .as_ref()
            .and_then(|_| summary.git_branch());
        if let Some(created) = summary.created_time.as_ref().and_then(proto_timestamp) {
            self.created_at = Some(
                self.created_at
                    .map_or(created, |current| current.min(created)),
            );
        }
        if let Some(updated) = summary
            .last_modified_time
            .as_ref()
            .and_then(proto_timestamp)
        {
            self.updated_at = Some(
                self.updated_at
                    .map_or(updated, |current| current.max(updated)),
            );
        }
    }
    fn observe(&mut self, record: &Value) {
        if self.title.is_none()
            && let Some((EventKind::MessageUser, text)) = message(record)
        {
            let title = text
                .lines()
                .next()
                .unwrap_or_default()
                .chars()
                .take(160)
                .collect::<String>();
            if !title.trim().is_empty() {
                self.title = Some(title);
            }
        }
        // Hook workspaces are mounted roots, not a primary-task ordering. A single verified
        // canonical workspace is a safe fallback; multiple or changing roots stay unbound.
        if let Some(paths) = record.get("workspacePaths") {
            let workspace = sole_workspace(paths);
            if self.workspace_ambiguous
                || workspace.as_ref().is_none_or(|workspace| {
                    self.project_path
                        .as_ref()
                        .is_some_and(|current| !paths_match(current, workspace))
                })
            {
                self.project_path = None;
                self.workspace_ambiguous = true;
            } else {
                self.project_path = workspace;
            }
        }
        if let Some(timestamp) = record_time(record) {
            self.created_at = Some(
                self.created_at
                    .map_or(timestamp, |current| current.min(timestamp)),
            );
            self.updated_at = Some(
                self.updated_at
                    .map_or(timestamp, |current| current.max(timestamp)),
            );
        }
    }
}

fn file_time(path: &Path) -> Option<DateTime<Utc>> {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .map(DateTime::<Utc>::from)
}

#[cfg(target_os = "windows")]
fn default_user_data_root() -> Option<PathBuf> {
    env::var_os("APPDATA")
        .map(PathBuf::from)
        .map(|root| root.join("Antigravity"))
}

#[cfg(target_os = "macos")]
fn default_user_data_root() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|dirs| {
        dirs.home_dir()
            .join("Library/Application Support/Antigravity")
    })
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn default_user_data_root() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|dirs| dirs.config_dir().join("Antigravity"))
}

/// Installed IDE `exa.unified_state_sync_pb.Topic` and `Row` descriptors.
#[derive(Clone, PartialEq, Message)]
struct ProtoTopic {
    #[prost(map = "string, message", tag = "1")]
    data: HashMap<String, ProtoRow>,
}

#[derive(Clone, PartialEq, Message)]
struct ProtoRow {
    #[prost(string, tag = "1")]
    value: String,
}

fn record_time(record: &Value) -> Option<DateTime<Utc>> {
    parse_timestamp(
        record
            .get("created_at")
            .or_else(|| record.pointer("/metadata/createdAt")),
    )
}

fn message(record: &Value) -> Option<(EventKind, &str)> {
    let kind = match record.get("type")?.as_str()? {
        "USER_INPUT" | "CORTEX_STEP_TYPE_USER_INPUT" => EventKind::MessageUser,
        "PLANNER_RESPONSE" | "CORTEX_STEP_TYPE_PLANNER_RESPONSE" => EventKind::MessageAssistant,
        _ => return None,
    };
    let text = record
        .get("content")
        .and_then(Value::as_str)
        .or_else(|| match kind {
            EventKind::MessageUser => record
                .pointer("/userInput/query")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
                .or_else(|| {
                    record
                        .pointer("/userInput/userResponse")
                        .and_then(Value::as_str)
                }),
            _ => record
                .pointer("/plannerResponse/modifiedResponse")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
                .or_else(|| {
                    record
                        .pointer("/plannerResponse/response")
                        .and_then(Value::as_str)
                }),
        })?;
    let text = if kind == EventKind::MessageUser {
        antigravity_user_request(text)
    } else {
        text
    };
    (!text.is_empty()).then_some((kind, text))
}

struct HistoryRetention {
    remaining_bytes: usize,
    max_records: usize,
    records: usize,
    omitted_events: usize,
    omitted_raw_records: usize,
    unsupported_records: usize,
}

impl HistoryRetention {
    fn new(max_bytes: usize, max_records: usize) -> Self {
        Self {
            remaining_bytes: max_bytes,
            max_records,
            records: 0,
            omitted_events: 0,
            omitted_raw_records: 0,
            unsupported_records: 0,
        }
    }

    fn retain(&mut self, payload: &Value) -> bool {
        let bytes = estimated_event_bytes(payload);
        if bytes > self.remaining_bytes {
            self.omitted_events = self.omitted_events.saturating_add(1);
            false
        } else {
            self.remaining_bytes -= bytes;
            true
        }
    }

    fn push_record(&mut self, builder: &mut EventBuilder, record: &Value) {
        let visible = message(record);
        // Undocumented future records may put visible text anywhere in their payload.
        if visible.is_none() || record.get("tool_calls").is_some() {
            self.unsupported_records = self.unsupported_records.saturating_add(1);
        }
        if self.records >= self.max_records {
            self.omitted_events = self
                .omitted_events
                .saturating_add(1 + usize::from(visible.is_some()));
            self.omitted_raw_records = self.omitted_raw_records.saturating_add(1);
            return;
        }
        self.records += 1;
        let record_type = record
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .chars()
            .take(160)
            .collect::<String>();
        let timestamp = record_time(record);
        // Retain the visible projection first. Its raw duplicate shares the same budget.
        if let Some((kind, text)) = visible {
            let payload = json!({"text": text});
            if self.retain(&payload) {
                builder.push(
                    kind,
                    payload,
                    timestamp,
                    ReplayPolicy::Contextual,
                    Some(record_type.clone()),
                    None,
                );
            }
        }
        if self.retain(record) {
            builder.push(
                EventKind::ProviderEvent,
                record.clone(),
                timestamp,
                ReplayPolicy::HistoricalOnly,
                Some(format!("{record_type}.raw")),
                None,
            );
        } else {
            self.omitted_raw_records = self.omitted_raw_records.saturating_add(1);
        }
    }

    fn finish(self, builder: &mut EventBuilder, timestamp: Option<DateTime<Utc>>) {
        // The provider documents the path, not the record schema. Even recognized records
        // cannot prove full native history, and the tolerant JSONL visitor may skip corruption.
        builder.push(
            EventKind::ProviderEvent,
            json!({
                "native_history_unverified": true,
                "unsupported_text_records": self.unsupported_records,
                "omitted_events": self.omitted_events,
                "raw_record_preserved": self.omitted_raw_records == 0,
            }),
            timestamp,
            ReplayPolicy::HistoricalOnly,
            Some("omnisession.antigravity_ide_unverified_history".to_owned()),
            None,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::AntigravityIdeAdapter;
    use crate::{
        AntigravityDesktopAdapter, ProviderAdapter,
        antigravity::{ProtoUserInput, proto_step, synthetic_step, write_steps_database},
    };
    use base64::{Engine, engine::general_purpose::STANDARD};
    use omnis_ir::{EventKind, Provider, ReplayPolicy, SessionRef};
    use prost::Message;
    use serde_json::json;
    use std::collections::HashMap;
    use std::{
        fs,
        path::{Path, PathBuf},
    };
    const ID: &str = "11111111-1111-4111-8111-111111111111";

    #[derive(Clone, PartialEq, Message)]
    struct SummaryFixture {
        #[prost(string, tag = "1")]
        title: String,
        #[prost(uint32, tag = "2")]
        steps: u32,
        #[prost(message, optional, tag = "3")]
        updated: Option<crate::antigravity::ProtoTimestamp>,
        #[prost(message, repeated, tag = "9")]
        workspaces: Vec<WorkspaceFixture>,
    }

    #[derive(Clone, PartialEq, Message)]
    struct WorkspaceFixture {
        #[prost(string, tag = "1")]
        uri: String,
        #[prost(string, tag = "4")]
        branch: String,
    }

    fn write_summary_metadata(global: &Path, project: &Path) -> PathBuf {
        let mut uri_path = project.to_string_lossy().replace('\\', "/");
        if cfg!(windows) {
            uri_path.insert(0, '/');
        }
        let summary = SummaryFixture {
            title: "Synthetic native title".to_owned(),
            steps: 1,
            updated: Some(crate::antigravity::ProtoTimestamp {
                seconds: 1_785_240_060,
                nanos: 0,
            }),
            workspaces: vec![WorkspaceFixture {
                uri: format!("file://{uri_path}").replace(' ', "%20"),
                branch: "synthetic-branch".to_owned(),
            }],
        };
        let encoded = STANDARD.encode(summary.encode_to_vec());
        assert!(STANDARD.decode("not-base64").is_err());
        let malformed_protobuf = STANDARD.encode([0x80]);
        assert!(
            super::ProtoTrajectorySummary::decode(
                STANDARD.decode(&malformed_protobuf).unwrap().as_slice()
            )
            .is_err()
        );
        let topic = super::ProtoTopic {
            data: HashMap::from([
                (
                    ID.to_owned(),
                    super::ProtoRow {
                        value: encoded.clone(),
                    },
                ),
                (
                    "22222222-2222-4222-8222-222222222222".to_owned(),
                    super::ProtoRow { value: encoded },
                ),
                (
                    "33333333-3333-4333-8333-333333333333".to_owned(),
                    super::ProtoRow {
                        value: "not-base64".to_owned(),
                    },
                ),
                (
                    "44444444-4444-4444-8444-444444444444".to_owned(),
                    super::ProtoRow {
                        value: malformed_protobuf,
                    },
                ),
            ]),
        };
        let database = global.join("state.vscdb");
        let connection = rusqlite::Connection::open(&database).unwrap();
        connection
            .execute_batch("CREATE TABLE ItemTable (key TEXT PRIMARY KEY, value TEXT);")
            .unwrap();
        connection
            .execute(
                "INSERT INTO ItemTable VALUES (?1, ?2)",
                rusqlite::params![super::SUMMARY_KEY, STANDARD.encode(topic.encode_to_vec())],
            )
            .unwrap();
        // Unrelated state stays opaque even when its value is not valid base64.
        connection
            .execute(
                "INSERT INTO ItemTable VALUES ('unrelated.state', 'not-base64')",
                [],
            )
            .unwrap();
        drop(connection);
        database
    }

    #[test]
    fn metadata_reads_do_not_create_wal_sidecars_in_provider_store() {
        let temporary = tempfile::tempdir().unwrap();
        let global = temporary.path().join("User/globalStorage");
        fs::create_dir_all(&global).unwrap();
        let database = global.join("state.vscdb");
        let connection = rusqlite::Connection::open(&database).unwrap();
        connection.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE ItemTable (key TEXT PRIMARY KEY, value TEXT);").unwrap();
        connection
            .execute(
                "INSERT INTO ItemTable VALUES (?1, '')",
                [super::SUMMARY_KEY],
            )
            .unwrap();
        drop(connection);
        let files = || {
            fs::read_dir(&global)
                .unwrap()
                .map(|entry| {
                    let entry = entry.unwrap();
                    (entry.file_name(), fs::read(entry.path()).unwrap())
                })
                .collect::<std::collections::BTreeMap<_, _>>()
        };
        let before = files();
        let adapter = AntigravityIdeAdapter::with_roots(
            temporary.path().join("brain-root"),
            temporary.path(),
        );
        assert!(adapter.read_summaries().unwrap().is_empty());
        assert!(
            files() == before,
            "metadata reads must not modify provider files or create sidecars"
        );
    }

    #[test]
    fn malformed_summary_rows_do_not_hide_valid_workspace_metadata_or_logs() {
        let temporary = tempfile::tempdir().unwrap();
        let ide = temporary.path().join("ide");
        let metadata_root = temporary.path().join("userdata");
        let global = metadata_root.join("User/globalStorage");
        fs::create_dir_all(&global).unwrap();
        let project = temporary.path().join("project space");
        let mounted = temporary.path().join("other mounted workspace");
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(&mounted).unwrap();
        let canonical_project = fs::canonicalize(&project).unwrap();
        let logs = ide.join("brain").join(ID).join(".system_generated/logs");
        fs::create_dir_all(&logs).unwrap();
        let path = logs.join("transcript.jsonl");
        let transcript = json!({"type":"USER_INPUT", "content":"Synthetic IDE request", "created_at":"2026-10-12T10:00:00Z", "workspacePaths":[mounted]}).to_string();
        fs::write(&path, &transcript).unwrap();
        let database = write_summary_metadata(&global, &project);
        let before = fs::read(&database).unwrap();
        let adapter = AntigravityIdeAdapter::with_roots(&ide, &metadata_root);
        let sessions = adapter.list_sessions(Some(&project)).unwrap();
        assert_eq!(sessions.len(), 1, "a stale summary cannot invent a session");
        assert_eq!(sessions[0].title.as_deref(), Some("Synthetic native title"));
        assert_eq!(sessions[0].project_path.as_ref(), Some(&canonical_project));
        assert_eq!(sessions[0].git_branch.as_deref(), Some("synthetic-branch"));
        assert_eq!(
            sessions[0].updated_at.unwrap().to_rfc3339(),
            "2026-10-12T10:00:00+00:00",
            "the log wins over a stale summary timestamp"
        );
        let snapshot = adapter.read_session(&sessions[0].session).unwrap();
        assert_eq!(snapshot.workspace.current_dir, canonical_project);
        assert_eq!(snapshot.title.as_deref(), Some("Synthetic native title"));
        assert_eq!(fs::read(&database).unwrap(), before);
        assert_eq!(fs::read_to_string(&path).unwrap(), transcript);
        assert!(adapter.discovery_notes().iter().any(|note| {
            note.contains("incomplete or unreadable")
                && note.contains("valid summaries")
                && note.contains("conversation logs")
        }));
        let connection = rusqlite::Connection::open(&database).unwrap();
        connection
            .execute(
                "UPDATE ItemTable SET value = 'invalid' WHERE key = ?1",
                [super::SUMMARY_KEY],
            )
            .unwrap();
        drop(connection);
        assert_eq!(
            adapter.list_sessions(None).unwrap().len(),
            1,
            "an unreadable cache cannot hide a log"
        );
        assert!(
            adapter
                .discovery_notes()
                .iter()
                .any(|note| note.contains("unreadable"))
        );
    }

    #[test]
    fn legacy_references_use_exact_desktop_id_and_reject_product_collisions() {
        let temporary = tempfile::tempdir().unwrap();
        let ide = temporary.path().join("ide");
        let desktop = temporary.path().join("desktop");
        fs::create_dir_all(desktop.join("conversations")).unwrap();
        let database = desktop.join("conversations").join(format!("{ID}.db"));
        write_steps_database(
            &database,
            &[synthetic_step(
                14,
                Some(proto_step::Step::UserInput(ProtoUserInput {
                    user_response: "Synthetic legacy desktop history".to_owned(),
                    ..ProtoUserInput::default()
                })),
            )],
        );
        let before = fs::read(&database).unwrap();
        let legacy = AntigravityIdeAdapter {
            root: Some(ide.clone()),
            legacy_desktop: false,
            legacy_root: Some(desktop),
            metadata_root: None,
            metadata_unavailable: std::sync::Arc::default(),
        };
        let reference = SessionRef::new(Provider::AntigravityIde, ID);
        let snapshot = legacy.read_session(&reference).unwrap();
        assert_eq!(snapshot.session, reference);
        assert_eq!(
            snapshot.events[0].source.provider,
            Provider::AntigravityDesktop
        );
        assert_eq!(
            snapshot.events[0].payload["text"],
            "Synthetic legacy desktop history"
        );
        assert!(legacy.list_sessions(None).unwrap().is_empty());
        assert!(
            AntigravityIdeAdapter::with_root(&ide)
                .read_session(&reference)
                .is_err(),
            "explicit IDE roots do not fall back"
        );
        let logs = ide.join("brain").join(ID).join(".system_generated/logs");
        fs::create_dir_all(&logs).unwrap();
        fs::write(
            logs.join("transcript.jsonl"),
            json!({"type":"USER_INPUT", "content":"Synthetic standalone history"}).to_string(),
        )
        .unwrap();
        assert!(
            legacy
                .read_session(&reference)
                .unwrap_err()
                .to_string()
                .contains("both IDE and Desktop")
        );
        assert_eq!(fs::read(&database).unwrap(), before);
    }

    #[test]
    fn standalone_logs_are_distinct_from_desktop_and_preserve_unknown_history() {
        let temporary = tempfile::tempdir().unwrap();
        let ide = temporary.path().join("antigravity-ide");
        let desktop = temporary.path().join("antigravity");
        let workspace = temporary.path().join("project");
        fs::create_dir_all(&workspace).unwrap();
        let canonical_workspace = fs::canonicalize(&workspace).unwrap();
        fs::create_dir_all(desktop.join("conversations")).unwrap();
        write_steps_database(
            &desktop.join("conversations").join(format!("{ID}.db")),
            &[synthetic_step(
                14,
                Some(proto_step::Step::UserInput(ProtoUserInput {
                    user_response: "Synthetic separate desktop request".to_owned(),
                    ..ProtoUserInput::default()
                })),
            )],
        );
        let logs = ide.join("brain").join(ID).join(".system_generated/logs");
        fs::create_dir_all(&logs).unwrap();
        let path = logs.join("transcript.jsonl");
        let records = [
            json!({"type":"USER_INPUT", "content":"<USER_REQUEST>Synthetic IDE request</USER_REQUEST>\nHistorical provider metadata", "created_at":"2026-10-12T10:00:00Z", "workspacePaths":[workspace]}),
            json!({"type":"PLANNER_RESPONSE", "content":"Synthetic IDE response", "created_at":"2026-10-12T10:01:00Z"}),
            json!({"type":"FUTURE_STEP", "content":"Historical output", "futureField":{"value":42}}),
        ];
        let bytes = records
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(&path, &bytes).unwrap();
        let adapter = AntigravityIdeAdapter::with_root(&ide);
        let sessions = adapter.list_sessions(None).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(
            sessions[0].session,
            SessionRef::new(Provider::AntigravityIde, ID)
        );
        assert_eq!(sessions[0].title.as_deref(), Some("Synthetic IDE request"));
        assert_eq!(
            sessions[0].project_path.as_ref(),
            Some(&canonical_workspace)
        );
        assert!(!sessions[0].updated_at_approximate);
        assert_eq!(adapter.list_sessions(Some(&workspace)).unwrap().len(), 1);
        assert!(
            AntigravityDesktopAdapter::with_root(&ide)
                .list_sessions(None)
                .unwrap()
                .is_empty()
        );
        assert!(
            AntigravityIdeAdapter::with_root(&desktop)
                .list_sessions(None)
                .unwrap()
                .is_empty()
        );
        let desktop_sessions = AntigravityDesktopAdapter::with_root(&desktop)
            .list_sessions(None)
            .unwrap();
        assert_eq!(desktop_sessions.len(), 1);
        assert_eq!(
            desktop_sessions[0].session,
            SessionRef::new(Provider::AntigravityDesktop, ID)
        );
        let snapshot = adapter.read_session(&sessions[0].session).unwrap();
        assert_eq!(snapshot.events[0].kind, EventKind::MessageUser);
        assert_eq!(snapshot.events[0].payload["text"], "Synthetic IDE request");
        assert_eq!(snapshot.events[1].kind, EventKind::ProviderEvent);
        assert_eq!(snapshot.events[2].kind, EventKind::MessageAssistant);
        assert_eq!(
            snapshot.events[4].replay_policy,
            ReplayPolicy::HistoricalOnly
        );
        assert_eq!(snapshot.events[4].payload["futureField"]["value"], 42);
        assert_eq!(snapshot.events[5].payload["unsupported_text_records"], 1);
        assert!(omnis_core::import_conversation(&snapshot).truncated);
        assert_eq!(fs::read_to_string(&path).unwrap(), bytes);
    }

    #[test]
    fn missing_metadata_does_not_guess_workspace_or_promote_unknown_messages() {
        let temporary = tempfile::tempdir().unwrap();
        let logs = temporary
            .path()
            .join("brain")
            .join(ID)
            .join(".system_generated/logs");
        fs::create_dir_all(&logs).unwrap();
        fs::write(
            logs.join("transcript.jsonl"),
            json!({"type":"FUTURE_SCHEMA", "content":"Opaque history"}).to_string(),
        )
        .unwrap();
        let adapter = AntigravityIdeAdapter::with_root(temporary.path());
        let sessions = adapter.list_sessions(None).unwrap();
        assert_eq!(sessions.len(), 1);
        assert!(sessions[0].title.is_none() && sessions[0].project_path.is_none());
        assert!(sessions[0].updated_at_approximate);
        assert!(
            adapter
                .list_sessions(Some(temporary.path()))
                .unwrap()
                .is_empty()
        );
        let snapshot = adapter.read_session(&sessions[0].session).unwrap();
        assert_eq!(snapshot.events[0].kind, EventKind::ProviderEvent);
        assert_eq!(snapshot.events[0].payload["content"], "Opaque history");
    }

    #[test]
    fn unknown_nested_history_and_malformed_lines_cannot_claim_complete_history() {
        let temporary = tempfile::tempdir().unwrap();
        let logs = temporary
            .path()
            .join("brain")
            .join(ID)
            .join(".system_generated/logs");
        fs::create_dir_all(&logs).unwrap();
        let records = [
            json!({"type":"FUTURE_STEP", "data":{"text":"Synthetic nested text"}}),
            json!({"type":"FUTURE_STEP", "plannerResponse":{"response":"Synthetic response"}}),
            json!({"type":"FUTURE_STEP", "userInput":{"query":"Synthetic request"}}),
        ];
        let transcript = format!(
            "{}\nmalformed complete line\n",
            records
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n")
        );
        let path = logs.join("transcript.jsonl");
        fs::write(&path, &transcript).unwrap();
        let snapshot = AntigravityIdeAdapter::with_root(temporary.path())
            .read_session(&SessionRef::new(Provider::AntigravityIde, ID))
            .unwrap();
        assert_eq!(snapshot.events.len(), 4);
        for (event, record) in snapshot.events.iter().zip(&records) {
            assert_eq!(&event.payload, record);
            assert_eq!(event.replay_policy, ReplayPolicy::HistoricalOnly);
        }
        let notice = &snapshot.events.last().unwrap().payload;
        assert_eq!(notice["unsupported_text_records"], 3);
        assert_eq!(notice["native_history_unverified"], true);
        assert!(omnis_core::import_conversation(&snapshot).truncated);
        assert_eq!(fs::read_to_string(path).unwrap(), transcript);
    }

    #[test]
    fn history_budget_prioritizes_visible_text_and_reports_raw_or_record_omissions() {
        let record = json!({"type":"USER_INPUT", "content":"Synthetic visible history", "extra":"x".repeat(4096)});
        let visible_bytes =
            crate::support::estimated_event_bytes(&json!({"text":"Synthetic visible history"}));
        let mut builder = crate::support::EventBuilder::new(Provider::AntigravityIde, ID);
        let mut retention = super::HistoryRetention::new(visible_bytes, 1);
        retention.push_record(&mut builder, &record);
        retention.push_record(&mut builder, &record);
        retention.finish(&mut builder, None);
        let snapshot = builder.snapshot(
            SessionRef::new(Provider::AntigravityIde, ID),
            None,
            None,
            None,
            chrono::Utc::now(),
        );
        assert_eq!(snapshot.events.len(), 2);
        assert_eq!(snapshot.events[0].kind, EventKind::MessageUser);
        assert_eq!(
            snapshot.events[0].payload["text"],
            "Synthetic visible history"
        );
        assert_eq!(snapshot.events[1].payload["omitted_events"], 3);
        assert_eq!(snapshot.events[1].payload["raw_record_preserved"], false);

        let mut builder = crate::support::EventBuilder::new(Provider::AntigravityIde, ID);
        let mut retention = super::HistoryRetention::new(visible_bytes - 1, 1);
        retention.push_record(&mut builder, &record);
        retention.finish(&mut builder, None);
        let snapshot = builder.snapshot(
            SessionRef::new(Provider::AntigravityIde, ID),
            None,
            None,
            None,
            chrono::Utc::now(),
        );
        assert_eq!(
            snapshot.events.len(),
            1,
            "visible history must also obey the budget"
        );
        assert_eq!(snapshot.events[0].payload["omitted_events"], 2);
    }

    #[test]
    fn mounted_workspaces_do_not_route_a_multiroot_history_by_array_order() {
        let temporary = tempfile::tempdir().unwrap();
        let first = temporary.path().join("first");
        let second = temporary.path().join("second");
        fs::create_dir_all(&first).unwrap();
        fs::create_dir_all(&second).unwrap();
        let logs = temporary
            .path()
            .join("brain")
            .join(ID)
            .join(".system_generated/logs");
        fs::create_dir_all(&logs).unwrap();
        let path = logs.join("transcript.jsonl");
        fs::write(&path, json!({"type":"USER_INPUT", "content":"Synthetic multiroot history", "workspacePaths":[first,second]}).to_string()).unwrap();
        let adapter = AntigravityIdeAdapter::with_root(temporary.path());
        assert!(
            adapter.list_sessions(None).unwrap()[0]
                .project_path
                .is_none()
        );
        assert!(adapter.list_sessions(Some(&first)).unwrap().is_empty());
        assert!(adapter.list_sessions(Some(&second)).unwrap().is_empty());
        fs::write(&path, json!({"type":"USER_INPUT", "content":"Synthetic sole workspace", "workspacePaths":[first, first.join(".")]}).to_string()).unwrap();
        assert_eq!(adapter.list_sessions(Some(&first)).unwrap().len(), 1);
        let records = [
            json!({"type":"USER_INPUT", "content":"Synthetic changing workspaces", "workspacePaths":[first]}),
            json!({"type":"PLANNER_RESPONSE", "content":"Synthetic response", "workspacePaths":[second]}),
        ];
        fs::write(
            &path,
            records
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();
        assert!(
            adapter.list_sessions(None).unwrap()[0]
                .project_path
                .is_none()
        );
    }

    #[test]
    fn ambiguous_summary_workspaces_override_a_single_mounted_fallback() {
        let temporary = tempfile::tempdir().unwrap();
        let first = temporary.path().join("first");
        let second = temporary.path().join("second");
        fs::create_dir_all(&first).unwrap();
        fs::create_dir_all(&second).unwrap();
        let canonical_first = fs::canonicalize(&first).unwrap();
        let canonical_second = fs::canonicalize(&second).unwrap();
        let workspace = |path: &std::path::Path| {
            let mut uri_path = path.to_string_lossy().replace('\\', "/");
            if cfg!(windows) {
                uri_path.insert(0, '/');
            }
            WorkspaceFixture {
                uri: format!("file://{uri_path}"),
                branch: "synthetic-branch".to_owned(),
            }
        };
        let mut metadata = super::LogMetadata::default();
        metadata.observe(&json!({"workspacePaths":[first]}));
        assert_eq!(metadata.project_path.as_ref(), Some(&canonical_first));
        let fixture = SummaryFixture {
            title: String::new(),
            steps: 0,
            updated: None,
            workspaces: vec![workspace(&first), workspace(&second)],
        };
        let summary = crate::antigravity_desktop::ProtoTrajectorySummary::decode(
            fixture.encode_to_vec().as_slice(),
        )
        .unwrap();
        metadata.merge_summary(Some(&summary));
        assert!(
            metadata.project_path.is_none(),
            "multiple summary roots must clear a single hook root"
        );
        assert!(metadata.git_branch.is_none());
        let fixture = SummaryFixture {
            workspaces: vec![workspace(&second), workspace(&second.join("."))],
            ..fixture
        };
        let summary = crate::antigravity_desktop::ProtoTrajectorySummary::decode(
            fixture.encode_to_vec().as_slice(),
        )
        .unwrap();
        metadata.merge_summary(Some(&summary));
        assert_eq!(metadata.project_path.as_ref(), Some(&canonical_second));
        assert_eq!(metadata.git_branch.as_deref(), Some("synthetic-branch"));
    }
}
