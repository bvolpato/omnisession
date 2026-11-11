use sha2::Digest as _;
use std::{
    borrow::Cow,
    cmp::Reverse,
    collections::{BinaryHeap, HashMap, HashSet, VecDeque},
    ffi::OsString,
    fs,
    io::{BufRead, ErrorKind, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
};

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, TimeDelta, Utc};
use omnis_ir::{CanonicalSnapshot, EventKind, GitState, Provider, ReplayPolicy, SessionRef};
use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::{
    LaunchPlan, LaunchTarget, NativeSession, NativeWorkspace, ProviderAdapter,
    ProviderInstallation,
    support::{
        EventBuilder, JsonLine, JsonlRecord, MAX_PREVIEW_TAIL_SIZE,
        MAX_STREAMED_TRANSCRIPT_FILE_SIZE, json_lines_prefix, json_lines_preview,
        omitted_images_text, parse_timestamp, paths_match, provider_executable, provider_file,
        provider_root, sort_sessions, string_at, validate_provider, value_at,
        visit_filtered_streamed_json_lines, visit_index_json_lines, visit_json_lines_tail,
    },
};

mod attachments;
mod checkpoint_wire;
mod rollout_lines;
use rollout_lines::{Admission, RolloutLines};
mod completed_item;
mod display_text;
mod message_mirrors;
mod model_cut;
use message_mirrors::{MessageIdentity, MessageMatch, MessageMirrors};
mod paginated_answers;
mod paginated_checkpoint;
mod paginated_compaction;
mod paginated_preview;
mod paginated_rollback;
mod paginated_workspace;
mod rollout_cache;
mod tool_history;
mod tool_revisions;
mod turn_aliases;
mod workspace_catalog;
mod workspace_damage;
mod workspace_records;
mod workspace_settings;

use paginated_answers::{PaginatedAnswers, ToolId};
use paginated_compaction::{
    DisplayEvidence, PaginatedCompactions, ProjectedReplacement, WindowKind,
};
use paginated_rollback::{InputPosition, LegacyPrivateInputs, PaginatedRollback, Withdrawal};
use rollout_cache::{RolloutCache, RolloutHeader, RolloutStamp};
use tool_history::{NativeCalls, PendingOutcomes};
use tool_revisions::{DisplayRevisions, PreviousDisplay};
use turn_aliases::{MessageIds, TurnAliases};
use workspace_catalog::WorkspaceCatalog;
use workspace_records::WorkspaceRecords;

const SCAN_LIMIT: usize = 10_000;
const MAX_CANONICAL_TOOL_EVENTS: usize = 256;
// Rollouts stream without a record budget, so retained events and turn checkpoints keep the newest.
const MAX_CANONICAL_EVENTS: usize = 100_000;
const MAX_TOOL_STRING_CHARACTERS: usize = 32 * 1024;
const MAX_TOOL_ARRAY_ITEMS: usize = 256;
const TOOL_COMPACTION_RECORD_INTERVAL: usize = 1_024;
const COMPACT_USER_TOKEN_LIMIT: usize = 20_000;
// Codex recognizes unannotated local summaries by this complete prompt line.
// Source: codex-rs/prompts/templates/compact/summary_prefix.md.
const LOCAL_SUMMARY_PREFIX: &str = concat!(
    "Another language model started to solve this problem and produced a summary of its ",
    "thinking process. You also have access to the state of the tools that were used by ",
    "that language model. Use this to build on the work that has already been done and ",
    "avoid duplicating work. Here is the summary produced by the other language model, ",
    "use the information in this summary to assist with your own analysis:",
);

#[derive(Clone, Debug, Default)]
struct CodexFileScan {
    files: Vec<PathBuf>,
    notes: Vec<String>,
}

#[derive(Debug)]
struct CodexListing {
    sessions: Vec<CodexSession>,
    notes: Vec<String>,
}

#[derive(Debug, Default)]
struct TitleIndex {
    titles: HashMap<String, String>,
    notes: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct CodexAdapter {
    codex_home: Option<PathBuf>,
    /// Rollout header cache file. Explicit roots leave it unset.
    rollout_cache: Option<PathBuf>,
    workspace_catalog: Option<Arc<WorkspaceCatalog>>,
    session_scan: Arc<OnceLock<CodexFileScan>>,
    listing: Arc<OnceLock<CodexListing>>,
    titles: Arc<OnceLock<TitleIndex>>,
}

impl CodexAdapter {
    #[must_use]
    pub fn with_root(codex_home: impl Into<PathBuf>) -> Self {
        Self {
            codex_home: Some(codex_home.into()),
            rollout_cache: None,
            workspace_catalog: None,
            session_scan: Arc::default(),
            listing: Arc::default(),
            titles: Arc::default(),
        }
    }

    fn discover_session_scan(&self) -> CodexFileScan {
        let Some(home) = self.codex_home.as_deref() else {
            return CodexFileScan::default();
        };
        scan_session_files(home, SCAN_LIMIT)
    }

    fn session_files(&self) -> &[PathBuf] {
        &self.session_scan().files
    }

    fn session_scan(&self) -> &CodexFileScan {
        self.session_scan
            .get_or_init(|| self.discover_session_scan())
    }

    fn title_index(&self) -> &TitleIndex {
        self.titles.get_or_init(|| {
            let Some(home) = self.codex_home.as_deref() else {
                return TitleIndex::default();
            };
            let Some(path) = provider_file(home, &home.join("session_index.jsonl")) else {
                return TitleIndex::default();
            };
            let mut rows: HashMap<String, (Option<DateTime<Utc>>, usize, String)> = HashMap::new();
            let mut position = 0_usize;
            let scan = visit_index_json_lines(&path, |row| {
                position += 1;
                let Some(id) = string_at(&row, &[&["id"]]).filter(|id| Uuid::parse_str(id).is_ok())
                else {
                    return;
                };
                let Some(title) = string_at(&row, &[&["thread_name"]]) else {
                    return;
                };
                let updated_at = parse_timestamp(row.get("updated_at"));
                let replace = rows
                    .get(id)
                    .is_none_or(|(current_time, current_position, _)| {
                        updated_at > *current_time
                            || (updated_at == *current_time && position > *current_position)
                    });
                if replace {
                    rows.insert(id.to_owned(), (updated_at, position, title.to_owned()));
                }
            });
            let notes = match scan {
                Ok(scan) => scan.notes("Codex session_index.jsonl"),
                Err(error) => vec![format!(
                    "Codex session_index.jsonl could not be read: {}.",
                    compact_discovery_error(&error)
                )],
            };
            TitleIndex {
                titles: rows
                    .into_iter()
                    .map(|(id, (_, _, title))| (id, title))
                    .collect(),
                notes,
            }
        })
    }

    fn listing(&self) -> &CodexListing {
        self.listing.get_or_init(|| self.build_listing())
    }

    fn sessions(&self) -> &[CodexSession] {
        &self.listing().sessions
    }

    fn build_listing(&self) -> CodexListing {
        let scan = self.session_scan();
        let title_index = self.title_index();
        let mut cache = self
            .rollout_cache
            .as_deref()
            .zip(self.codex_home.as_deref())
            .and_then(|(path, home)| RolloutCache::load(path, home));
        let mut sessions: HashMap<String, CodexSession> = HashMap::new();
        let mut subagents = 0_usize;
        let mut unreadable_files = 0_usize;
        let mut skipped_metadata = 0_usize;
        let mut first_unreadable: Option<String> = None;
        for path in &scan.files {
            match listed_session(path, cache.as_mut()) {
                Ok(Some(mut session)) => {
                    if session.is_subagent {
                        subagents += 1;
                        continue;
                    }
                    if let Some(title) = title_index.titles.get(&session.id) {
                        session.title = Some(title.clone());
                    }
                    let replace = sessions
                        .get(&session.id)
                        .is_none_or(|current| session.updated_at > current.updated_at);
                    if replace {
                        sessions.insert(session.id.clone(), session);
                    }
                }
                Ok(None) => skipped_metadata += 1,
                Err(error) => {
                    unreadable_files += 1;
                    if first_unreadable.is_none() {
                        first_unreadable = Some(compact_discovery_error(&error));
                    }
                }
            }
        }
        if let Some(cache) = cache {
            cache.save();
        }
        let sessions = sessions.into_values().collect::<Vec<_>>();
        let mut notes = listing_notes(
            scan,
            sessions.len(),
            subagents,
            unreadable_files,
            skipped_metadata,
            first_unreadable.as_deref(),
        );
        notes.extend(title_index.notes.iter().cloned());
        CodexListing { sessions, notes }
    }

    fn find_session(&self, id: &str) -> Result<CodexSession> {
        let path = self.find_session_path(id)?;
        let mut session = CodexSession::parse_metadata_path_result(&path)?
            .ok_or_else(|| anyhow!("Codex session `{id}` could not be parsed"))?;
        if let Some(title) = self.title_index().titles.get(id) {
            session.title = Some(title.clone());
        }
        Ok(session)
    }

    fn find_session_metadata(&self, id: &str) -> Result<CodexSession> {
        let path = self.find_session_path(id)?;
        let mut session = CodexSession::parse_metadata_path(&path)
            .ok_or_else(|| anyhow!("Codex session `{id}` metadata could not be parsed"))?;
        if let Some(title) = self.title_index().titles.get(id) {
            session.title = Some(title.clone());
        }
        Ok(session)
    }

    fn find_session_path(&self, id: &str) -> Result<PathBuf> {
        Uuid::parse_str(id).context("Codex session ID must be a UUID")?;
        self.session_path(id)
            .ok_or_else(|| anyhow!("Codex session `{id}` was not found"))
    }

    /// Reads the model provider that a session records.
    ///
    /// Codex resumes a session only with the provider that created it.
    ///
    /// # Errors
    ///
    /// Returns an error when the rollout cannot be read. A missing session or a rollout that
    /// records no provider is `Ok(None)`.
    pub fn recorded_model_provider(&self, id: &str) -> Result<Option<String>> {
        if Uuid::parse_str(id).is_err() {
            return Ok(None);
        }
        let Some(path) = self.session_path(id) else {
            return Ok(None);
        };
        let records = json_lines_prefix(&path, 1)?;
        Ok(records
            .first()
            .filter(|record| record.get("type").and_then(Value::as_str) == Some("session_meta"))
            .and_then(|record| record.get("payload"))
            .and_then(|payload| string_at(payload, &[&["model_provider"]]))
            .map(str::to_owned))
    }

    fn session_path(&self, id: &str) -> Option<PathBuf> {
        self.session_files()
            .iter()
            .find(|path| path_uuid(path).as_deref() == Some(id))
            .cloned()
            .or_else(|| {
                self.discover_session_scan()
                    .files
                    .into_iter()
                    .find(|path| path_uuid(path).as_deref() == Some(id))
            })
    }

    /// Finds user sessions created after a native fork command started.
    ///
    /// A candidate must record the source as its fork parent in the launch workspace.
    ///
    /// # Errors
    ///
    /// Returns an error for a non-Codex source reference or unreadable metadata.
    pub fn fork_candidates_created_since(
        &self,
        source: &SessionRef,
        project: &Path,
        started_at: DateTime<Utc>,
    ) -> Result<Vec<SessionRef>> {
        validate_provider(source, Provider::Codex)?;
        let parent = Uuid::parse_str(&source.id).context("Codex fork parent must be a UUID")?;
        let Some(home) = self.codex_home.as_deref() else {
            return Ok(Vec::new());
        };

        // Codex files rollouts by local date, which can differ from the UTC date by one day.
        let mut directories = HashSet::new();
        for timestamp in [started_at, Utc::now()] {
            for days in [-1, 0, 1] {
                directories.insert(
                    home.join("sessions").join(
                        (timestamp + TimeDelta::days(days))
                            .format("%Y/%m/%d")
                            .to_string(),
                    ),
                );
            }
        }

        let mut candidates = Vec::new();
        for directory in directories {
            let mut paths = Vec::new();
            let mut unreadable_dirs = 0;
            let mut depth_limited_dirs = 0;
            collect_jsonl(
                &directory,
                0,
                &mut paths,
                SCAN_LIMIT,
                &mut unreadable_dirs,
                &mut depth_limited_dirs,
            );
            for path in paths {
                let Some(session) = CodexSession::parse_metadata_path_result(&path)? else {
                    continue;
                };
                if session.id == source.id
                    || session.forked_from_id != Some(parent)
                    || session.is_subagent
                    || session
                        .created_at
                        .is_none_or(|created_at| created_at < started_at)
                    || session
                        .project_path
                        .as_deref()
                        .is_none_or(|recorded| !paths_match(recorded, project))
                {
                    continue;
                }
                candidates.push(session);
            }
        }
        candidates.sort_by_key(|session| session.created_at);
        candidates.dedup_by(|left, right| left.id == right.id);
        Ok(candidates
            .into_iter()
            .map(|session| SessionRef::new(Provider::Codex, session.id))
            .collect())
    }
}

impl Default for CodexAdapter {
    fn default() -> Self {
        let codex_home = provider_root("CODEX_HOME", &[".codex"]);
        Self {
            rollout_cache: codex_home.as_deref().and_then(rollout_cache::default_path),
            workspace_catalog: codex_home
                .as_deref()
                .and_then(WorkspaceCatalog::new)
                .map(Arc::new),
            codex_home,
            session_scan: Arc::default(),
            listing: Arc::default(),
            titles: Arc::default(),
        }
    }
}

/// Reads a rollout's listing metadata, reusing the cached header while the rollout is unchanged.
fn listed_session(path: &Path, cache: Option<&mut RolloutCache>) -> Result<Option<CodexSession>> {
    let Some(cache) = cache else {
        return CodexSession::parse_metadata_path_result(path);
    };
    // Stat before reading, so a rollout written in between is parsed again next time.
    let Some((metadata, stamp)) = fs::metadata(path)
        .ok()
        .and_then(|metadata| RolloutStamp::new(&metadata).map(|stamp| (metadata, stamp)))
    else {
        return CodexSession::parse_metadata_path_result(path);
    };
    if let Some(cached) = cache.get(path, stamp) {
        let updated_at = metadata.modified().ok().map(DateTime::<Utc>::from);
        return Ok(cached
            .header
            .clone()
            .map(|header| CodexSession::from_header(path, header, updated_at, stamp)));
    }
    let session = CodexSession::parse_metadata_path_result(path)?;
    cache.insert(path, stamp, session.as_ref().map(CodexSession::header));
    Ok(session)
}

fn scan_session_files(home: &Path, limit: usize) -> CodexFileScan {
    let mut files = Vec::new();
    let mut unreadable_dirs = 0_usize;
    let mut depth_limited_dirs = 0_usize;
    let collect_limit = limit.saturating_add(1);
    collect_jsonl(
        &home.join("sessions"),
        5,
        &mut files,
        collect_limit,
        &mut unreadable_dirs,
        &mut depth_limited_dirs,
    );
    if files.len() <= limit {
        collect_jsonl(
            &home.join("archived_sessions"),
            5,
            &mut files,
            collect_limit,
            &mut unreadable_dirs,
            &mut depth_limited_dirs,
        );
    }
    let truncated = files.len() > limit;
    files.truncate(limit);
    let mut notes = Vec::new();
    if unreadable_dirs > 0 {
        notes.push(format!(
            "Codex encountered {unreadable_dirs} errors while reading session directories."
        ));
    }
    if depth_limited_dirs > 0 {
        notes.push(format!(
            "Codex skipped {depth_limited_dirs} session directories beyond the scan depth limit."
        ));
    }
    if truncated {
        notes.push(format!(
            "Codex scan stopped after {limit} newest jsonl files; older sessions are omitted."
        ));
    }
    CodexFileScan { files, notes }
}

fn listing_notes(
    scan: &CodexFileScan,
    listed: usize,
    subagents: usize,
    unreadable_files: usize,
    skipped_metadata: usize,
    first_unreadable: Option<&str>,
) -> Vec<String> {
    let mut notes = scan.notes.clone();
    if unreadable_files > 0 {
        notes.push(match first_unreadable {
            Some(error) if !error.is_empty() => {
                format!("Codex skipped {unreadable_files} unreadable session file(s): {error}.")
            }
            _ => format!("Codex skipped {unreadable_files} unreadable session file(s)."),
        });
    }
    if skipped_metadata > 0 {
        notes.push(format!(
            "Codex skipped {skipped_metadata} session file(s) without readable session metadata."
        ));
    }
    if !scan.files.is_empty() && listed == 0 {
        notes.push(format!(
            "Found {} Codex jsonl file(s) but listed 0 user sessions ({subagents} subagent(s) skipped, {unreadable_files} unreadable, {skipped_metadata} without session metadata).",
            scan.files.len()
        ));
    }
    notes
}

fn compact_discovery_error(error: &anyhow::Error) -> String {
    error
        .to_string()
        .lines()
        .next()
        .unwrap_or("unreadable")
        .chars()
        .take(160)
        .collect()
}

fn collect_jsonl(
    root: &Path,
    depth: usize,
    output: &mut Vec<PathBuf>,
    limit: usize,
    unreadable_dirs: &mut usize,
    depth_limited_dirs: &mut usize,
) {
    if output.len() >= limit {
        return;
    }
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return,
        Err(_) => {
            *unreadable_dirs = unreadable_dirs.saturating_add(1);
            return;
        }
    };
    // Retain every directory because even a lexically early directory may be empty, while
    // keeping only the newest file names this branch could contribute to the bounded result.
    // This preserves the old reverse-lexical DFS selection without storing and sorting every
    // path in a wide session directory.
    let file_limit = limit.saturating_sub(output.len());
    let mut directories = Vec::new();
    let mut files = BinaryHeap::<Reverse<OsString>>::new();
    for entry in entries {
        let Ok(entry) = entry else {
            *unreadable_dirs = unreadable_dirs.saturating_add(1);
            continue;
        };
        let Ok(file_type) = entry.file_type() else {
            *unreadable_dirs = unreadable_dirs.saturating_add(1);
            continue;
        };
        if file_type.is_symlink() {
            continue;
        }

        let name = entry.file_name();
        if file_type.is_dir() {
            directories.push(name);
        } else if file_type.is_file()
            && Path::new(&name)
                .extension()
                .is_some_and(|extension| extension == "jsonl")
        {
            if files.len() < file_limit {
                files.push(Reverse(name));
            } else if files
                .peek()
                .is_some_and(|Reverse(smallest)| name.as_os_str() > smallest.as_os_str())
            {
                files.pop();
                files.push(Reverse(name));
            }
        }
    }

    directories.sort_unstable_by(|left, right| right.cmp(left));
    let mut files = files
        .into_iter()
        .map(|Reverse(name)| name)
        .collect::<Vec<_>>();
    files.sort_unstable_by(|left, right| right.cmp(left));
    let mut directories = directories.into_iter().peekable();
    let mut files = files.into_iter().peekable();
    while output.len() < limit {
        let next_is_directory = match (directories.peek(), files.peek()) {
            (Some(directory), Some(file)) => directory > file,
            (Some(_), None) => true,
            _ => false,
        };
        if next_is_directory {
            let directory = root.join(directories.next().expect("peeked directory"));
            if depth > 0 {
                collect_jsonl(
                    &directory,
                    depth - 1,
                    output,
                    limit,
                    unreadable_dirs,
                    depth_limited_dirs,
                );
            } else {
                *depth_limited_dirs = depth_limited_dirs.saturating_add(1);
            }
        } else if let Some(file) = files.next() {
            output.push(root.join(file));
        } else {
            break;
        }
    }
}

fn path_uuid(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let start = stem.len().checked_sub(36)?;
    let id = stem.get(start..)?;
    Uuid::parse_str(id).ok().map(|_| id.to_owned())
}

#[derive(Default)]
struct RolloutWorkspace {
    directory: Option<PathBuf>,
    settings: WorkspaceSettings,
    rolled_back: bool,
    recent_records: Option<VecDeque<JsonlRecord>>,
}

#[derive(Default)]
enum OwnedWorkspace {
    #[default]
    Absent,
    Known(PathBuf),
    Unknown,
}

#[derive(Default)]
struct WorkspaceSettings {
    owned: OwnedWorkspace,
    startup: Option<PathBuf>,
}

impl WorkspaceSettings {
    fn from_header(directory: Option<PathBuf>) -> Self {
        Self {
            owned: OwnedWorkspace::Absent,
            startup: directory,
        }
    }

    fn observe(&mut self, session: &str, record: &Value) {
        let payload = &record["payload"];
        if record["type"] != "event_msg" || payload["type"] != "thread_settings_applied" {
            return;
        }
        let settings = &payload["thread_settings"];
        let cwd = settings["cwd"].as_str().map(Path::new);
        // Codex can offer the directory of a copied settings record when the TUI starts.
        if self.startup.as_deref() != cwd {
            self.startup = None;
        }
        if workspace_settings::belongs_to(payload, session) {
            self.owned = workspace_settings::saved_directory(settings)
                .map_or(OwnedWorkspace::Unknown, OwnedWorkspace::Known);
        }
    }

    fn has_owned(&self) -> bool {
        !matches!(self.owned, OwnedWorkspace::Absent)
    }

    fn directory(&self, fallback: Option<PathBuf>) -> Option<PathBuf> {
        match &self.owned {
            OwnedWorkspace::Absent => fallback,
            OwnedWorkspace::Known(path) => Some(path.clone()),
            OwnedWorkspace::Unknown => None,
        }
    }
}

#[derive(Debug)]
struct CodexSession {
    stamp: Option<RolloutStamp>,
    id: String,
    title: Option<String>,
    path: PathBuf,
    project_path: Option<PathBuf>,
    git: GitState,
    forked_from_id: Option<Uuid>,
    cli_version: Option<String>,
    is_subagent: bool,
    paginated: bool,
    created_at: Option<DateTime<Utc>>,
    updated_at: Option<DateTime<Utc>>,
}

impl CodexSession {
    fn parse_metadata_path(path: &Path) -> Option<Self> {
        Self::parse_metadata_path_result(path).ok().flatten()
    }

    fn parse_metadata_path_result(path: &Path) -> Result<Option<Self>> {
        let metadata = fs::metadata(path)?;
        let records = json_lines_prefix(path, 1)?;
        Ok(records
            .first()
            .and_then(|record| Self::parse_metadata_record(path.to_path_buf(), record, &metadata)))
    }

    fn parse_metadata_record(
        path: PathBuf,
        record: &Value,
        metadata: &fs::Metadata,
    ) -> Option<Self> {
        if record.get("type").and_then(Value::as_str) != Some("session_meta") {
            return None;
        }
        let payload = record.get("payload")?;
        let id = string_at(payload, &[&["id"], &["session_id"]])?;
        Uuid::parse_str(id).ok()?;
        if path_uuid(&path).as_deref() != Some(id) {
            return None;
        }
        let updated_at = metadata.modified().ok().map(DateTime::<Utc>::from);
        Some(Self {
            stamp: RolloutStamp::new(metadata),
            id: id.to_owned(),
            title: None,
            path,
            project_path: string_at(payload, &[&["cwd"]]).map(PathBuf::from),
            git: GitState {
                branch: string_at(payload, &[&["git", "branch"], &["git", "branch_name"]])
                    .map(str::to_owned),
                head: string_at(payload, &[&["git", "commit_hash"]])
                    .filter(|head| {
                        matches!(head.len(), 40 | 64)
                            && head.bytes().all(|byte| byte.is_ascii_hexdigit())
                    })
                    .map(str::to_ascii_lowercase),
                remote_fingerprint: string_at(payload, &[&["git", "repository_url"]])
                    .filter(|url| !url.is_empty())
                    .map(|url| omnis_core::fingerprint(url.as_bytes())),
                ..GitState::default()
            },
            forked_from_id: string_at(payload, &[&["forked_from_id"]])
                .and_then(|parent| Uuid::parse_str(parent).ok()),
            cli_version: string_at(payload, &[&["cli_version"]]).map(str::to_owned),
            paginated: payload["history_mode"] == "paginated",
            is_subagent: string_at(
                payload,
                &[
                    &["agent_role"],
                    &["agent_path"],
                    &["thread_source", "agent_role"],
                ],
            )
            .is_some()
                || value_at(payload, &[&["source", "subagent"]]).is_some(),
            created_at: parse_timestamp(record.get("timestamp")),
            updated_at,
        })
    }

    fn from_header(
        path: &Path,
        header: RolloutHeader,
        updated_at: Option<DateTime<Utc>>,
        stamp: RolloutStamp,
    ) -> Self {
        Self {
            stamp: Some(stamp),
            id: header.id,
            title: None,
            path: path.to_path_buf(),
            project_path: header.project_path,
            git: header.git,
            forked_from_id: header.forked_from_id,
            cli_version: header.cli_version,
            is_subagent: header.is_subagent,
            paginated: header.paginated,
            created_at: header.created_at,
            updated_at,
        }
    }

    fn header(&self) -> RolloutHeader {
        RolloutHeader {
            id: self.id.clone(),
            project_path: self.project_path.clone(),
            git: self.git.clone(),
            forked_from_id: self.forked_from_id,
            cli_version: self.cli_version.clone(),
            is_subagent: self.is_subagent,
            paginated: self.paginated,
            created_at: self.created_at,
        }
    }

    fn native_session(&self) -> NativeSession {
        NativeSession {
            session: SessionRef::new(Provider::Codex, self.id.clone()),
            title: self.title.clone(),
            project_path: self.project_path.clone(),
            git_branch: self.git.branch.clone(),
            created_at: self.created_at,
            updated_at: self.updated_at,
            updated_at_approximate: false,
            event_count: 0,
            source_path: Some(self.path.clone()),
        }
    }

    fn canonical_events(&self) -> Result<(EventBuilder, Option<PathBuf>)> {
        let mut builder = self.event_builder();
        let mut history = CodexHistory::new(self.project_path.clone());
        let mut records_seen = 0_usize;
        let mut workspace =
            WorkspaceRecords::from_header(self.project_path.clone(), self.paginated);
        let mut has_history_base = false;
        let oversized_records = visit_filtered_streamed_json_lines(
            &self.path,
            MAX_STREAMED_TRANSCRIPT_FILE_SIZE,
            |_| true,
            |line| {
                if workspace.observe(&self.id, &line) {
                    history.clear_workspace_before_gap(builder.checkpoint());
                }
                let (mut record, terminated) = match line {
                    JsonLine::Record(record) => (record, true),
                    JsonLine::Unterminated(record) => (record, false),
                    JsonLine::Malformed(_) => {
                        history.rollout_lines.malformed();
                        return Ok(());
                    }
                    JsonLine::Oversized(..) => return Ok(()),
                };
                if records_seen == 0 {
                    has_history_base = declares_history_base(&record);
                }
                history.push(&mut builder, &mut record, terminated)?;
                records_seen += 1;
                if records_seen.checked_rem(TOOL_COMPACTION_RECORD_INTERVAL) == Some(0) {
                    history.compact(&mut builder);
                }
                Ok(())
            },
        )?;
        history.compact(&mut builder);
        if history.omitted_events > 0 {
            builder.push(
                EventKind::ProviderEvent,
                json!({
                    "omitted_events": history.omitted_events,
                    "retained_latest_events": MAX_CANONICAL_EVENTS,
                }),
                self.updated_at,
                ReplayPolicy::HistoricalOnly,
                Some("omnisession.codex_event_limit".to_owned()),
                None,
            );
        }
        let omitted_tool_events = history.omitted_tool_events;
        if omitted_tool_events > 0 {
            builder.push(
                EventKind::ProviderEvent,
                json!({
                    "omitted_events": omitted_tool_events,
                    "omitted_tool_events": omitted_tool_events,
                    "event_kind": "tool",
                    "retained_latest_tool_events": MAX_CANONICAL_TOOL_EVENTS,
                }),
                self.updated_at,
                ReplayPolicy::HistoricalOnly,
                Some("omnisession.codex_tool_limit".to_owned()),
                None,
            );
        }
        if has_history_base {
            self.report_unread_history_base(&mut builder);
        }
        history.finish(&mut builder);
        let restored = history.paginated || history.restored_workspace();
        workspace.use_history(history.project_path, restored);
        let damaged = workspace.had_gap();
        let malformed = workspace.malformed;
        let directory = workspace.finish(&self.path)?.directory;
        let unknown = damaged && directory.is_none();
        let sequence = builder.checkpoint();
        builder.push_oversized_record_notice(oversized_records, self.updated_at);
        if unknown && let Some(event) = builder.event_at_mut(sequence) {
            event.payload["workspace_unknown"] = true.into();
        }
        if malformed > 0 {
            builder.push(
                EventKind::ProviderEvent,
                json!({"omitted_events":malformed,"malformed_records":malformed,"workspace_unknown":unknown}),
                self.updated_at,
                ReplayPolicy::HistoricalOnly,
                Some("omnisession.codex_malformed_records".to_owned()),
                None,
            );
        }
        Ok((builder, directory))
    }

    fn workspace_metadata(&self, for_preview: bool) -> Result<RolloutWorkspace> {
        let size = fs::metadata(&self.path)?.len();
        if size > MAX_STREAMED_TRANSCRIPT_FILE_SIZE {
            let mut recent = self.recent_workspace(for_preview)?;
            if !recent.settings.has_owned() {
                recent.directory = None;
            }
            return Ok(recent);
        }
        if !for_preview && size > MAX_PREVIEW_TAIL_SIZE {
            let mut recent = self.recent_workspace(false)?;
            if recent.settings.has_owned() {
                return Ok(recent);
            }
            if recent.directory.is_some() && !contains_settings_marker(&self.path)? {
                recent.settings.startup.clone_from(&self.project_path);
                return Ok(recent);
            }
        }
        let mut records = WorkspaceRecords::from_header(self.project_path.clone(), self.paginated);
        let oversized = visit_filtered_streamed_json_lines(
            &self.path,
            MAX_STREAMED_TRANSCRIPT_FILE_SIZE,
            workspace_record_filter(),
            |line| {
                records.observe(&self.id, &line);
                Ok(())
            },
        )?;
        let mut workspace = records.finish(&self.path)?;
        if for_preview && oversized > 0 && !self.paginated {
            workspace.recent_records = self.recent_workspace(true)?.recent_records;
        }
        Ok(workspace)
    }

    fn recent_workspace(&self, keep_preview: bool) -> Result<RolloutWorkspace> {
        let mut workspace = WorkspaceRecords::from_header(None, self.paginated);
        let mut records = VecDeque::new();
        let is_workspace_record = workspace_record_filter();
        visit_json_lines_tail(
            &self.path,
            MAX_PREVIEW_TAIL_SIZE,
            |line| keep_preview || is_workspace_record(line),
            |_, line| {
                workspace.observe(&self.id, &line);
                if !keep_preview {
                    return;
                }
                if !self.paginated
                    && let JsonLine::Record(record) | JsonLine::Unterminated(record) = &line
                    && record["type"] == "event_msg"
                    && record["payload"]["type"] == "thread_rolled_back"
                {
                    if record["payload"]["num_turns"].as_u64() != Some(0) {
                        records.clear();
                    }
                } else if let Some(record) = line.into_record() {
                    if records.len() == 1_024 {
                        records.pop_front();
                    }
                    records.push_back(record);
                }
            },
        )?;
        let mut workspace = workspace.finish(&self.path)?;
        workspace.rolled_back = false;
        workspace.recent_records = keep_preview.then_some(records);
        Ok(workspace)
    }

    fn recent_preview(&self, records: VecDeque<JsonlRecord>) -> Result<EventBuilder> {
        let mut events = self.event_builder();
        let mut history = CodexHistory::new(None);
        history.paginated = self.paginated;
        for line in records {
            let (mut record, terminated) = match line {
                JsonlRecord::Complete(record) => (record, true),
                JsonlRecord::Unterminated(record) => (record, false),
                JsonlRecord::Malformed => {
                    history.rollout_lines.skipped += usize::from(self.paginated);
                    continue;
                }
            };
            if self.paginated {
                // An unread prefix can contain a higher display ordinal than any row in the suffix.
                if history
                    .rollout_lines
                    .prepare_wire(&mut record, true, Some(true))
                {
                    history.push_admitted(&mut events, &record, Admission::ModelOnly)?;
                }
            } else {
                history.push(&mut events, &mut record, terminated)?;
            }
        }
        history.finish(&mut events);
        let reason = if self.paginated {
            "The source prefix was unavailable, so the public display ordinals in its suffix could not be verified."
        } else {
            "Only the recent Codex records were available."
        };
        events.push(
            EventKind::ProviderEvent,
            json!({"omitted_events":1, "reason":reason}),
            self.updated_at,
            ReplayPolicy::HistoricalOnly,
            Some("omnisession.preview_limit".to_owned()),
            None,
        );
        Ok(events)
    }

    fn git_at(&self, directory: Option<&Path>) -> GitState {
        // Codex records Git only for the creation directory.
        if directory.is_some()
            && directory.map(Path::as_os_str) == self.project_path.as_deref().map(Path::as_os_str)
        {
            self.git.clone()
        } else {
            GitState::default()
        }
    }

    fn snapshot(
        &self,
        catalog: Option<&WorkspaceCatalog>,
        session: &SessionRef,
        events: EventBuilder,
        project_path: Option<PathBuf>,
        captured_at: DateTime<Utc>,
    ) -> CanonicalSnapshot {
        let git = self.git_at(project_path.as_deref());
        if let Some(catalog) = catalog {
            catalog.record(self, project_path.as_deref(), git.branch.as_deref());
        }
        let mut snapshot = events.snapshot(
            session.clone(),
            self.title.clone(),
            project_path,
            None,
            captured_at,
        );
        snapshot.workspace.git = git;
        snapshot
    }

    fn preview_events(&self) -> Result<EventBuilder> {
        const SAMPLE_RECORDS: usize = 1_024;
        let records = if self.paginated {
            let Some(records) = crate::support::complete_small_json_records(&self.path)? else {
                return paginated_preview::read(self, SAMPLE_RECORDS);
            };
            records
        } else {
            json_lines_preview(&self.path, SAMPLE_RECORDS)?
                .into_iter()
                .map(JsonlRecord::Complete)
                .collect()
        };
        let mut history = CodexHistory::new(self.project_path.clone());
        let mut builder = self.event_builder();
        let mut has_history_base = false;
        for (index, line) in records.into_iter().enumerate() {
            let (mut record, terminated) = match line {
                JsonlRecord::Complete(record) => (record, true),
                JsonlRecord::Unterminated(record) => (record, false),
                JsonlRecord::Malformed => {
                    history.rollout_lines.malformed();
                    continue;
                }
            };
            if index == 0 {
                has_history_base = declares_history_base(&record);
            }
            history.push(&mut builder, &mut record, terminated)?;
        }
        if has_history_base {
            self.report_unread_history_base(&mut builder);
        }
        history.finish(&mut builder);
        Ok(builder)
    }

    fn report_unread_history_base(&self, builder: &mut EventBuilder) {
        builder.push(
            EventKind::ProviderEvent,
            json!({
                "omitted_events": 1,
                "native_history_unverified": true,
                "reason": "The child rollout references an inherited prefix that is unavailable in this snapshot."
            }),
            self.updated_at,
            ReplayPolicy::HistoricalOnly,
            Some("omnisession.codex_history_base".to_owned()),
            None,
        );
    }

    fn event_builder(&self) -> EventBuilder {
        let mut builder = EventBuilder::new(Provider::Codex, &self.id);
        builder.set_provider_version(self.cli_version.clone());
        builder
    }
}

fn declares_history_base(record: &Value) -> bool {
    record["type"] == "session_meta" && !record["payload"]["history_base"].is_null()
}

struct TurnCheckpoint {
    id: Option<String>,
    sequence: u64,
    admission: Option<InputPosition>,
    project_path: Option<PathBuf>,
    tool_events: usize,
}

struct ReplacementRequest<'a> {
    sequence: u64,
    checkpoint: TurnCheckpoint,
    item: &'a Value,
}

impl TurnCheckpoint {
    fn admitted_sequence(&self) -> Option<u64> {
        // A completed display alone does not establish a native instruction boundary.
        self.admission.map(|_| self.sequence)
    }
}

fn revision_owner<'a>(
    late: &'a HashMap<u64, u64>,
    original: &'a HashMap<u64, u64>,
) -> impl Fn(u64) -> u64 + 'a {
    |seq| {
        late.get(&seq)
            .and_then(|id| original.get(id))
            .copied()
            .unwrap_or(seq)
    }
}

fn confirm_admission(
    checkpoints: &mut [(u64, TurnCheckpoint)],
    requests: &[(u64, u64)],
    sequence: Option<u64>,
    position: InputPosition,
) {
    if let Some(seq) = sequence
        && let Ok(i) = requests.binary_search_by_key(&seq, |(s, _)| *s)
    {
        let (sequence, owner) = requests[i];
        let i = checkpoints.partition_point(|(_, cp)| cp.sequence <= sequence);
        if let Some((_, cp)) = checkpoints[..i]
            .iter_mut()
            .rev()
            .find(|(id, _)| *id == owner)
        {
            cp.admission = Some(position);
        }
    }
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
enum CompletedKind {
    User,
    Assistant,
    AsyncAssistant,
    Plan,
    Tool,
}

impl CompletedKind {
    fn for_item(payload: &Value) -> Self {
        match payload["item"]["type"].as_str() {
            Some("UserMessage") => Self::User,
            Some("AgentMessage")
                if completed_item::assistant::is_async(&payload["item"]["delivery"]) =>
            {
                Self::AsyncAssistant
            }
            Some("AgentMessage") => Self::Assistant,
            Some("Plan") => Self::Plan,
            _ => Self::Tool,
        }
    }
}

struct CodexHistory {
    project_path: Option<PathBuf>,
    first_workspace_turn: Option<u64>,
    restored_turn: Option<u64>,
    items: CodexItems,
    // Logical order can differ from file order when a checkpoint supplies missing requests.
    turns: Vec<u64>,
    next_turn: u64,
    checkpoints: Vec<(u64, TurnCheckpoint)>,
    // Each entry identifies a genuine user message in the canonical event stream.
    requests: Vec<(u64, u64)>,
    original_requests: HashMap<u64, u64>,
    pending_turn: Option<TurnCheckpoint>,
    active_turn_id: Option<String>,
    source_ordinal: u64,
    paginated: bool,
    paginated_inputs: PaginatedRollback,
    paginated_workspace: paginated_workspace::PaginatedWorkspace,
    paginated_answers: PaginatedAnswers,
    compactions: PaginatedCompactions,
    checkpoint_turns: paginated_checkpoint::CheckpointTurns,
    withdrawn: HashMap<u64, (Withdrawal, u64)>,
    explicit_display_turn: bool,
    legacy_display: MessageIdentity,
    legacy_request: Option<u64>,
    uncertain_legacy_rollback: bool,
    legacy_private: LegacyPrivateInputs,
    rollout_lines: RolloutLines,
    // Codex updates a completed item in place using its turn and item IDs.
    completed: HashMap<String, HashMap<(CompletedKind, String), u64>>,
    // Native turn names and delayed events refer to logical turns, including their replacement copies.
    named_turns: HashMap<String, u64>,
    plan_turns: HashSet<String>,
    plan_receipts: display_text::PlanReceipts,
    message_ids: MessageIds,
    aliases: TurnAliases,
    calls: NativeCalls,
    display_revisions: DisplayRevisions,
    late_owners: HashMap<u64, u64>,
    late_tool_counts: HashMap<u64, usize>,
    display_positions: HashMap<u64, (u64, u64)>,
    omitted_tool_events: usize,
    omitted_events: usize,
}

impl CodexHistory {
    fn new(project_path: Option<PathBuf>) -> Self {
        Self {
            paginated_workspace: paginated_workspace::PaginatedWorkspace::new(project_path.clone()),
            project_path,
            first_workspace_turn: None,
            restored_turn: None,
            items: CodexItems::default(),
            turns: Vec::new(),
            next_turn: 0,
            checkpoints: Vec::new(),
            requests: Vec::new(),
            original_requests: HashMap::new(),
            pending_turn: None,
            active_turn_id: None,
            source_ordinal: 0,
            paginated: false,
            paginated_inputs: PaginatedRollback::default(),
            paginated_answers: PaginatedAnswers::default(),
            compactions: PaginatedCompactions::default(),
            checkpoint_turns: paginated_checkpoint::CheckpointTurns::default(),
            withdrawn: HashMap::new(),
            explicit_display_turn: false,
            legacy_display: MessageIdentity::default(),
            legacy_request: None,
            uncertain_legacy_rollback: false,
            legacy_private: LegacyPrivateInputs::default(),
            rollout_lines: RolloutLines::default(),
            completed: HashMap::new(),
            named_turns: HashMap::new(),
            plan_turns: HashSet::new(),
            plan_receipts: display_text::PlanReceipts::default(),
            message_ids: MessageIds::default(),
            aliases: TurnAliases::default(),
            calls: NativeCalls::default(),
            display_revisions: DisplayRevisions::default(),
            late_owners: HashMap::new(),
            late_tool_counts: HashMap::new(),
            display_positions: HashMap::new(),
            omitted_tool_events: 0,
            omitted_events: 0,
        }
    }

    /// Applies the retained tool event, event, and turn checkpoint caps.
    fn compact(&mut self, builder: &mut EventBuilder) {
        let tools_removed = builder.retain_latest_tool_events(MAX_CANONICAL_TOOL_EVENTS);
        let events_removed = builder.retain_latest_events(MAX_CANONICAL_EVENTS);
        self.omitted_tool_events += tools_removed;
        self.omitted_events += events_removed;
        if tools_removed > 0 || events_removed > 0 {
            self.prune_completed(builder);
            let first = builder.first_sequence();
            self.items.mirrors.discard_before(first);
            self.paginated_inputs.discard_before(first);
            self.paginated_answers.discard_before(first);
            self.compactions.discard_before(first);
            self.withdrawn.retain(|seq, _| *seq >= first);
        }
        let excess = self.turns.len().saturating_sub(MAX_CANONICAL_EVENTS);
        if excess > 0 {
            let evicted: HashSet<_> = self.turns.drain(..excess).collect();
            self.requests.retain(|(_, id)| !evicted.contains(id));
            self.named_turns.retain(|_, id| !evicted.contains(id));
            self.original_requests.retain(|id, _| !evicted.contains(id));
            self.aliases.retain(&self.named_turns);
            self.late_tool_counts.retain(|id, _| !evicted.contains(id));
        }
        if excess > 0 || self.plan_turns.len() > MAX_CANONICAL_EVENTS {
            self.plan_turns.retain(|id| {
                self.named_turns.contains_key(id) || self.active_turn_id.as_deref() == Some(id)
            });
        }
        let first = builder.first_sequence();
        let stale = self
            .requests
            .partition_point(|(sequence, _)| *sequence < first);
        self.requests.drain(..stale);
        let stale = self
            .checkpoints
            .partition_point(|(_, point)| point.sequence < first);
        let excess = self.checkpoints.len().saturating_sub(MAX_CANONICAL_EVENTS);
        self.checkpoints
            .drain(..excess.max(stale.saturating_sub(1)));
        if tools_removed > 0 || events_removed > 0 || excess > 0 {
            let oldest = self
                .checkpoints
                .first()
                .map_or(0, |(_, turn)| turn.sequence);
            self.display_revisions.prune(builder, oldest);
        }
    }

    fn new_turn(&mut self) -> u64 {
        let id = self.next_turn;
        self.next_turn += 1;
        id
    }

    fn clear_workspace_before_gap(&mut self, sequence: u64) {
        self.project_path = None;
        self.first_workspace_turn = Some(self.next_turn);
        self.restored_turn = None;
        if let Some(turn) = &mut self.pending_turn {
            turn.project_path = None;
        }
        self.paginated_workspace.new_segment(sequence);
    }

    fn restored_workspace(&self) -> bool {
        self.restored_turn
            .is_some_and(|id| self.first_workspace_turn.is_none_or(|first| id >= first))
    }

    fn verify_legacy_rollback(&mut self, first_removed: u64, count: u64) {
        // A replacement can give the same logical request another physical checkpoint.
        let mut admitted = HashMap::new();
        for (id, point) in &self.checkpoints {
            if let Some(position) = point.admission {
                admitted.entry(*id).or_insert(position);
            }
        }
        self.uncertain_legacy_rollback |= self.turns.iter().any(|id| !admitted.contains_key(id));
        if let Some(&cut) = admitted.get(&first_removed) {
            let excess = count.saturating_sub(self.turns.len() as u64);
            self.uncertain_legacy_rollback |= self.legacy_private.crosses(cut, excess);
        }
    }

    fn rollback(&mut self, builder: &mut EventBuilder, count: u64) {
        if count > 0 {
            self.restored_turn = None;
        }
        let keep = self
            .turns
            .len()
            .saturating_sub(usize::try_from(count).unwrap_or(usize::MAX));
        let Some(&first_removed) = self.turns.get(keep) else {
            if count > 0 && self.first_workspace_turn.is_some() {
                self.project_path = None;
            }
            return;
        };
        self.verify_legacy_rollback(first_removed, count);
        let discarded: HashSet<_> = self.turns.drain(keep..).collect();
        let mut ranges = Vec::new();
        let mut removed_tools = 0;
        let mut points = std::mem::take(&mut self.checkpoints).into_iter().peekable();
        while let Some((id, mut point)) = points.next() {
            if discarded.contains(&id) {
                if ranges.is_empty() {
                    self.restored_turn =
                        (first_removed == id && point.admission.is_some()).then_some(id);
                    self.project_path =
                        if self.first_workspace_turn.is_none() || self.restored_workspace() {
                            point.project_path
                        } else {
                            None
                        };
                }
                let (end, tools) = points.peek().map_or(
                    (builder.checkpoint(), self.items.tool_events),
                    |(_, next)| (next.sequence, next.tool_events),
                );
                ranges.push(point.sequence..end);
                removed_tools += tools - point.tool_events;
            } else {
                point.tool_events -= removed_tools;
                self.checkpoints.push((id, point));
            }
        }
        let survives = |sequence: u64| {
            let index = ranges.partition_point(|r| r.end <= sequence);
            ranges.get(index).is_none_or(|r| sequence < r.start)
        };
        builder.retain_events(|event| {
            self.late_owners.get(&event.sequence).map_or_else(
                || survives(event.sequence),
                |owner| !discarded.contains(owner),
            )
        });
        self.named_turns.retain(|_, id| !discarded.contains(id));
        self.original_requests
            .retain(|id, _| !discarded.contains(id));
        self.plan_receipts
            .rollback(|owner| discarded.contains(&owner));
        self.aliases.retain(&self.named_turns);
        self.late_tool_counts
            .retain(|id, _| !discarded.contains(id));
        self.items.tool_events -= removed_tools;
        if self.omitted_tool_events > 0 {
            let total_tools =
                self.items.tool_events + self.late_tool_counts.values().sum::<usize>();
            let visible_tools = builder
                .events()
                .iter()
                .filter(|event| {
                    matches!(
                        event.kind,
                        EventKind::CommandExecuted
                            | EventKind::ToolCalled
                            | EventKind::ToolCompleted
                            | EventKind::ToolFailed
                    )
                })
                .count();
            self.omitted_tool_events = self
                .omitted_tool_events
                .min(total_tools.saturating_sub(visible_tools));
        }
        self.display_revisions
            .rollback(builder, |sequence| !survives(sequence));
        self.prune_completed(builder);
        self.requests.retain(|(sequence, _)| survives(*sequence));
    }

    fn checkpoint(&self, builder: &EventBuilder) -> TurnCheckpoint {
        TurnCheckpoint {
            id: self.active_turn_id.clone(),
            sequence: builder.checkpoint(),
            admission: None,
            project_path: self.project_path.clone(),
            tool_events: self.items.tool_events,
        }
    }

    fn remember_turn(&mut self, mut checkpoint: TurnCheckpoint, sequence: Option<u64>) {
        let id = self.new_turn();
        self.turns.push(id);
        self.original_requests
            .insert(id, sequence.unwrap_or(checkpoint.sequence));
        if let Some(name) = &checkpoint.id {
            let name = self.aliases.resolve(name).to_owned();
            self.named_turns.insert(name.clone(), id);
            checkpoint.id = Some(name);
        }
        if let Some(sequence) = sequence {
            self.requests.push((sequence, id));
        }
        self.checkpoints.push((id, checkpoint));
    }

    fn prune_completed(&mut self, builder: &mut EventBuilder) {
        self.items
            .outcomes
            .retain(|seq| builder.event_at_mut(seq).is_some());
        self.calls.retain(|seq| builder.event_at_mut(seq).is_some());
        self.message_ids
            .retain(|seq| builder.event_at_mut(seq).is_some());
        self.completed.retain(|_, items| {
            items.retain(|_, sequence| builder.event_at_mut(*sequence).is_some());
            !items.is_empty()
        });
        self.late_owners
            .retain(|seq, _| builder.event_at_mut(*seq).is_some());
        self.display_positions
            .retain(|seq, _| builder.event_at_mut(*seq).is_some());
    }

    fn noncurrent_owner(&self, turn: &str) -> Option<u64> {
        let turn = self.aliases.resolve(turn);
        self.named_turns.get(turn).copied().filter(|id| {
            self.turns.last() != Some(id)
                || self
                    .active_turn_id
                    .as_deref()
                    .is_some_and(|active| self.aliases.resolve(active) != turn)
        })
    }

    fn model_plan_mode(&self, item: &Value) -> bool {
        let turn = item["internal_chat_message_metadata_passthrough"]["turn_id"]
            .as_str()
            .filter(|turn| !turn.is_empty())
            .map(|turn| self.aliases.resolve(turn));
        turn.map_or(self.items.plan_mode, |turn| {
            self.plan_turns.contains(turn)
                || (self.items.plan_mode
                    && self.current_turn_id().is_none_or(|active| active == turn))
        })
    }

    fn set_plan_mode(&mut self, turn: Option<&str>, plan: bool) {
        self.items.plan_mode = plan;
        if let Some(turn) = turn {
            let turn = self.aliases.resolve(turn);
            if plan {
                self.plan_turns.insert(turn.to_owned());
            } else {
                self.plan_turns.remove(turn);
            }
        }
    }

    fn current_turn_id(&self) -> Option<&str> {
        self.active_turn_id
            .as_deref()
            .or_else(|| {
                let (owner, checkpoint) = self.checkpoints.last()?;
                (Some(owner) == self.turns.last()).then_some(())?;
                checkpoint.id.as_deref()
            })
            .map(|turn| self.aliases.resolve(turn))
    }

    fn observe_context_turn(&mut self, turn: &str, sequence: u64) {
        if self
            .active_turn_id
            .as_deref()
            .is_some_and(|active| self.aliases.resolve(active) == self.aliases.resolve(turn))
        {
            return;
        }
        // Some older rollouts name the turn in its context after the first request.
        if self.active_turn_id.is_none()
            && self
                .items
                .message
                .as_ref()
                .is_some_and(|message| message.kind == EventKind::MessageUser)
            && let Some((owner, checkpoint)) = self.checkpoints.last_mut()
            && Some(&*owner) == self.turns.last()
            && checkpoint.id.as_deref().is_none_or(|id| id == turn)
        {
            checkpoint.id = Some(turn.to_owned());
            self.named_turns.insert(turn.to_owned(), *owner);
        } else {
            if is_implicit_turn(turn) {
                self.items.turn_boundary(Some(turn));
            } else {
                self.items.message = None;
                self.items.mirrors.start_context(sequence);
                self.items.outcomes.retain_models();
            }
            self.pending_turn = None;
        }
        self.active_turn_id = Some(turn.to_owned());
        self.items.in_turn = true;
    }

    fn end_display_turn(&mut self) {
        self.legacy_request = None;
        self.explicit_display_turn = false;
        self.legacy_display = MessageIdentity::default();
        self.items.plan_mode = false;
        self.items.in_turn = false;
        self.pending_turn = None;
        self.active_turn_id = None;
    }

    fn push(
        &mut self,
        builder: &mut EventBuilder,
        record: &mut Value,
        terminated: bool,
    ) -> Result<()> {
        if self.source_ordinal == 0 {
            self.rollout_lines.start_segment(record);
            self.paginated = self.rollout_lines.paginated();
        }
        let disposition = self
            .rollout_lines
            .prepare(record, terminated, Some(self.paginated));
        self.push_admitted(builder, record, disposition)
    }

    fn push_admitted(
        &mut self,
        builder: &mut EventBuilder,
        record: &Value,
        disposition: Admission,
    ) -> Result<()> {
        let index = self.source_ordinal;
        self.source_ordinal += 1;
        match disposition {
            Admission::Complete => self.push_verified(builder, record, index),
            Admission::ModelOnly => {
                if record["type"] == "turn_context" {
                    self.observe_turn_directory(record, record["payload"]["turn_id"].as_str());
                }
                let event_type = (record["type"] == "event_msg")
                    .then(|| record["payload"]["type"].as_str())
                    .flatten();
                self.consume_model_record(record, event_type, builder)?;
                Ok(())
            }
            Admission::Skip => Ok(()),
        }
    }

    fn push_verified(
        &mut self,
        builder: &mut EventBuilder,
        record: &Value,
        index: u64,
    ) -> Result<()> {
        let record_type = record.get("type").and_then(Value::as_str);
        let payload = &record["payload"];
        let event_type = (record_type == Some("event_msg"))
            .then(|| payload["type"].as_str())
            .flatten();
        if record_type == Some("session_meta") {
            self.observe_history_mode(payload, index, builder);
        }
        if self.consume_model_record(record, event_type, builder)? {
            return Ok(());
        }
        let turn_id = payload["turn_id"].as_str();
        if event_type == Some("thread_rolled_back") {
            let count = payload["num_turns"]
                .as_u64()
                .context("Codex rollback marker has no valid num_turns")?;
            if count > 0 {
                self.rollback(builder, count);
                self.end_display_turn();
                self.plan_turns
                    .retain(|id| self.named_turns.contains_key(id));
                self.items.clear_mirrors();
            }
            return Ok(());
        }
        if matches!(event_type, Some("task_started" | "turn_started")) {
            if self.paginated {
                self.paginated_inputs
                    .turn_started(turn_id, self.source_ordinal);
            }
            self.explicit_display_turn = true;
            self.legacy_request = None;
            // The migrator writes a default mode when it synthesizes a turn after its context.
            let generated = turn_id.is_some_and(is_implicit_turn);
            let plan =
                payload["collaboration_mode_kind"] == "plan" || (generated && self.items.plan_mode);
            self.set_plan_mode(turn_id, plan);
            self.active_turn_id = turn_id.map(str::to_owned);
            self.pending_turn = Some(self.checkpoint(builder));
            self.items.turn_boundary(turn_id);
            self.items.in_turn = true;
        } else if matches!(
            event_type,
            Some("task_complete" | "turn_complete" | "turn_aborted")
        ) && turn_id
            .zip(self.current_turn_id())
            .is_none_or(|(target, current)| self.aliases.resolve(target) == current)
        {
            self.end_display_turn();
            self.items.turn_boundary(turn_id);
        }
        if record_type == Some("turn_context") {
            self.push_turn_context(builder, record, turn_id);
        }
        // A legacy request can start a generated display turn at its source ordinal.
        if event_type == Some("user_message") && self.items.in_turn && !self.explicit_display_turn {
            self.legacy_display =
                MessageIdentity::new(Some(&format!("rollout-{index}")), None, true);
        }
        if event_type == Some("item_completed")
            && let (Some(turn), Some(id)) = (turn_id, payload["item"]["id"].as_str())
        {
            self.push_identified_item(
                builder,
                payload,
                turn,
                id,
                parse_timestamp(record.get("timestamp")),
            );
        } else if !self.push_model_tool(builder, record) {
            self.push_message_or_metadata(builder, record, event_type);
        }
        Ok(())
    }

    fn push_turn_context(&mut self, builder: &EventBuilder, record: &Value, turn_id: Option<&str>) {
        let payload = &record["payload"];
        if let Some(mode) = payload["collaboration_mode"]["mode"].as_str() {
            self.set_plan_mode(turn_id, mode == "plan");
        } else if turn_id.is_some()
            && turn_id.map(|id| self.aliases.resolve(id)) != self.current_turn_id()
        {
            self.items.plan_mode =
                turn_id.is_some_and(|turn| self.plan_turns.contains(self.aliases.resolve(turn)));
        }
        if let Some(turn) = turn_id {
            self.observe_context_turn(turn, builder.checkpoint());
        }
        if self.pending_turn.is_none()
            && self
                .items
                .message
                .as_ref()
                .is_none_or(|message| message.kind != EventKind::MessageUser)
        {
            self.pending_turn = Some(self.checkpoint(builder));
        }
        self.observe_turn_directory(record, turn_id);
    }

    fn observe_turn_directory(&mut self, record: &Value, turn_id: Option<&str>) {
        if let Some(cwd) = string_at(record, &[&["payload", "cwd"]]) {
            if self.paginated {
                self.paginated_workspace
                    .observe(turn_id.or(self.active_turn_id.as_deref()), Path::new(&cwd));
            }
            self.project_path = Some(PathBuf::from(cwd));
        }
    }

    fn observe_history_mode(&mut self, payload: &Value, index: u64, builder: &mut EventBuilder) {
        if self.paginated && index > 0 {
            if let Some((ordinal, end)) = self.compactions.latest() {
                self.paginated_answers
                    .resolve_turns(self.paginated_inputs.resolved_turns());
                self.paginated_answers.resolve_positions(builder, ordinal);
                self.paginated_answers.finish_answers();
                self.paginated_answers.project_revisions(
                    builder,
                    self.compactions.public_history(),
                    &self.paginated_inputs,
                    ordinal,
                    end,
                );
            }
            self.paginated_inputs.segment();
            self.paginated_workspace.new_segment(builder.checkpoint());
            self.paginated_answers.segment();
        }
        self.paginated |= payload["history_mode"] == "paginated";
    }

    fn consume_model_record(
        &mut self,
        record: &Value,
        event_type: Option<&str>,
        builder: &mut EventBuilder,
    ) -> Result<bool> {
        let payload = &record["payload"];
        if self.paginated {
            self.checkpoint_turns.observe(record, self.source_ordinal);
        }
        if record["type"] == "compacted" {
            self.push_compaction(builder, record);
            return Ok(true);
        }
        if !self.paginated {
            return Ok(self.legacy_private.observe(record, self.source_ordinal));
        }
        if let Some(turn) = payload["turn_id"].as_str() {
            let finished = matches!(event_type, Some("task_complete" | "turn_complete"));
            if finished && self.plan_turns.contains(self.aliases.resolve(turn)) {
                self.plan_receipts.finish_turn(turn, self.source_ordinal);
            }
            if finished
                || (record["type"] == "token_usage_record" && payload["response_id"].is_string())
            {
                self.paginated_answers.end_response(turn);
            }
        }
        if record["type"] == "inter_agent_communication"
            || (record["type"] == "response_item"
                && matches!(payload["type"].as_str(), Some("message" | "agent_message")))
        {
            if !self
                .paginated_inputs
                .model_user(record, self.source_ordinal, builder.checkpoint())
                && payload["type"] == "message"
                && payload["role"] == "assistant"
            {
                let plan_mode = self.model_plan_mode(payload);
                let reserved = self.compactions.has_model_window().then(|| {
                    let start = builder.reserve_sequence();
                    if plan_mode {
                        builder.reserve_sequence();
                    }
                    start
                });
                let active = self
                    .active_turn_id
                    .as_deref()
                    .filter(|turn| !is_implicit_turn(turn));
                self.paginated_answers.model(
                    record,
                    self.source_ordinal,
                    reserved,
                    plan_mode,
                    active,
                );
            }
            return Ok(true);
        }
        if event_type == Some("thread_rolled_back") {
            let count = payload["num_turns"]
                .as_u64()
                .context("Codex rollback marker has no valid num_turns")?;
            self.withdraw_displayed_inputs(builder, usize::try_from(count).unwrap_or(usize::MAX));
            return Ok(true);
        }
        Ok(matches!(event_type, Some("user_message" | "agent_message")))
    }

    fn withdraw_displayed_inputs(&mut self, builder: &mut EventBuilder, count: usize) {
        let effective = self.compactions.apply_rollback(
            &self.paginated_inputs,
            count,
            self.checkpoint_turns.selected(),
        );
        let aliases = self.paginated_inputs.resolved_turns();
        let floor = self
            .compactions
            .selected_checkpoint()
            .map_or(0, paginated_checkpoint::PaginatedCheckpoint::source_ordinal);
        let mut owners = self.paginated_inputs.withdraw(
            effective.input_count,
            &aliases,
            effective.boundary,
            floor,
        );
        if effective.changed {
            self.project_path = self.paginated_workspace.surviving(
                &self.aliases,
                &aliases,
                self.checkpoints.iter().filter_map(|(id, turn)| {
                    let seq = *self.original_requests.get(id)?;
                    let baseline = owners
                        .get(&seq)
                        .copied()
                        .or_else(|| self.withdrawn.get(&seq).map(|&(s, _)| s));
                    let status = self.compactions.public_status(seq, Some(seq), baseline);
                    if turn.id.is_some()
                        && self
                            .compactions
                            .selected_checkpoint()
                            .is_some_and(|w| w.applies_to(seq))
                        && let Some(status @ (Withdrawal::Confirmed | Withdrawal::Unverified)) =
                            status
                    {
                        owners.insert(seq, status);
                    }
                    Some((seq, turn.id.as_deref(), status))
                }),
            );
        }
        self.paginated_answers.resolve_turns(aliases);
        let replies = effective
            .boundary
            .map_or_else(HashMap::new, |(ordinal, order)| {
                self.paginated_answers.rollback(ordinal, order)
            });
        if owners.is_empty() && replies.is_empty() {
            return;
        }
        builder.visit_events(|event| {
            if self.compactions.contains(event.sequence) {
                return;
            }
            if let Some(&withdrawal) = replies.get(&event.sequence) {
                if let Some(status) = withdrawal {
                    self.withdrawn
                        .entry(event.sequence)
                        .or_insert((status, self.source_ordinal));
                }
                return;
            }
            if let Some(status) = self
                .event_request(event.sequence)
                .and_then(|seq| owners.get(&seq))
            {
                self.withdrawn
                    .entry(event.sequence)
                    .or_insert((*status, self.source_ordinal));
            }
        });
    }

    fn event_request(&self, sequence: u64) -> Option<u64> {
        let owner = self.late_owners.get(&sequence).copied().or_else(|| {
            let point = self
                .checkpoints
                .partition_point(|(_, checkpoint)| checkpoint.sequence <= sequence)
                .checked_sub(1)
                .map(|i| (self.checkpoints[i].1.sequence, self.checkpoints[i].0));
            let request = self
                .requests
                .partition_point(|(seq, _)| *seq <= sequence)
                .checked_sub(1)
                .map(|i| self.requests[i]);
            point.max(request).map(|(_, id)| id)
        });
        owner.and_then(|id| self.original_requests.get(&id).copied())
    }

    fn finish(&mut self, builder: &mut EventBuilder) {
        self.compactions.finish(builder);
        let answers = {
            self.paginated_answers
                .resolve_turns(self.paginated_inputs.resolved_turns());
            self.paginated_answers.finish_answers()
        };
        if let Some((ordinal, end)) = self.compactions.latest() {
            self.paginated_answers
                .finish_checkpoint(builder, ordinal, end);
            self.paginated_answers.project_revisions(
                builder,
                self.compactions.public_history(),
                &self.paginated_inputs,
                ordinal,
                end,
            );
        }
        let missing = self
            .plan_receipts
            .missing_paginated(self.paginated_answers.surviving_plans(), || {
                self.paginated_inputs.resolved_turns()
            });
        self.plan_receipts.report(builder, missing);
        self.display_revisions.finish(builder);
        if self.uncertain_legacy_rollback {
            builder.push(
                EventKind::ProviderEvent,
                json!({"native_history_unverified": true, "reason": "The legacy display and model records do not establish the same rollback boundary."}),
                None,
                ReplayPolicy::HistoricalOnly,
                Some("omnisession.codex_legacy_rollback".into()),
                None,
            );
        }
        for (count, record, reason) in [
            (
                self.rollout_lines.invalid_checkpoints,
                "omnisession.codex_invalid_checkpoint",
                "Codex recorded an invalid compaction summary or replacement. The handoff uses the last verified history.",
            ),
            (
                self.rollout_lines.invalid_messages,
                "omnisession.codex_invalid_message",
                "A completed Codex message could not be verified. The handoff uses the last verified message.",
            ),
        ] {
            if count > 0 {
                builder.push(
                    EventKind::ProviderEvent,
                    json!({"omitted_events": count, "native_history_unverified": true, "reason": reason}),
                    None,
                    ReplayPolicy::HistoricalOnly,
                    Some(record.into()),
                    None,
                );
            }
        }
        if self.rollout_lines.skipped > 0 || self.rollout_lines.gap {
            builder.push(
                EventKind::ProviderEvent,
                json!({
                    "omitted_events": self.rollout_lines.skipped,
                    "ordinal_gap": self.rollout_lines.gap,
                    "native_history_unverified": true,
                    "reason": "The paginated Codex rollout contains missing or unreadable display records. The handoff uses the last verified version of each item."
                }),
                None,
                ReplayPolicy::HistoricalOnly,
                Some("omnisession.codex_rollout_line".into()),
                None,
            );
        }
        if self.paginated {
            self.annotate_model_window(builder, &answers);
        }
        self.restore_legacy_users(builder);
        builder.order_delayed_messages(&self.display_positions);
    }

    fn annotate_model_window(
        &self,
        builder: &mut EventBuilder,
        answers: &HashMap<u64, Option<Withdrawal>>,
    ) {
        builder.visit_events_mut(|event| {
            if !matches!(
                event.kind,
                EventKind::MessageUser
                    | EventKind::MessageAssistant
                    | EventKind::ToolCalled
                    | EventKind::ToolCompleted
                    | EventKind::ToolFailed
                    | EventKind::CommandExecuted
            ) {
                return;
            }
            let seq = event.sequence;
            if self.compactions.contains(seq) {
                return;
            }
            let baseline = answers
                .get(&seq)
                .copied()
                .unwrap_or_else(|| self.withdrawn.get(&seq).map(|&(status, _)| status));
            let status = self
                .compactions
                .public_status(seq, self.event_request(seq), baseline);
            let (status, prior) = if event.kind == EventKind::MessageAssistant {
                let projection = self.paginated_answers.project_answer(
                    seq,
                    self.compactions.public_history(),
                    status,
                );
                (projection.status, projection.prior)
            } else {
                (status, None)
            };
            if let Some(status) = status {
                let field = if status == Withdrawal::OutsideCheckpoint {
                    "omnisession_codex_checkpoint"
                } else {
                    "omnisession_codex_rollback"
                };
                event.payload[field] = status.as_str().into();
                if let Some(prior) = prior {
                    event.payload["omnisession_codex_prior_display"] = prior.into();
                }
            }
        });
    }

    fn push_message_or_metadata(
        &mut self,
        builder: &mut EventBuilder,
        record: &Value,
        event_type: Option<&str>,
    ) {
        let payload = &record["payload"];
        let checkpoint = self.checkpoint(builder);
        self.items.projected = None;
        if self.push_record(builder, record) {
            let mut checkpoint = self.pending_turn.take().unwrap_or(checkpoint);
            checkpoint.admission = InputPosition::user(record, self.source_ordinal);
            // The migrator can emit the next raw request before closing a synthesized turn.
            if let Some(stamped) =
                payload["internal_chat_message_metadata_passthrough"]["turn_id"].as_str()
            {
                checkpoint.id = Some(stamped.to_owned());
            } else if checkpoint.id.as_deref().is_some_and(|id| {
                is_implicit_turn(id) && self.named_turns.contains_key(self.aliases.resolve(id))
            }) {
                checkpoint.id = None;
            }
            self.remember_turn(checkpoint, self.items.projected);
        }
        if event_type == Some("user_message") {
            self.legacy_request = self.items.projected;
        } else if event_type == Some("agent_message")
            && let Some(sequence) = self.items.projected
            && let Some(request) = self.legacy_request
            && let Some(owner) = self.request_id(request)
        {
            self.place_early_answer(builder, owner, sequence);
        }
    }

    fn push_compaction(&mut self, builder: &mut EventBuilder, record: &Value) {
        let timestamp = parse_timestamp(record.get("timestamp"));
        let payload = &record["payload"];
        let history = payload["replacement_history"].as_array();
        let legacy = payload["replacement_history"].is_null() && payload["message"].is_string();
        let summary = match payload["message"].as_str() {
            Some("") if legacy => Some("(no summary available)"),
            text => text.filter(|text| !text.trim().is_empty()),
        };
        let items = history.map_or(&[][..], Vec::as_slice);
        let (public, scoped_sources) = self
            .paginated
            .then(|| paginated_checkpoint::PaginatedCheckpoint::read(self, builder, payload))
            .flatten()
            .unzip();
        let start = self.checkpoint(builder);
        let positioned_summary =
            push_compaction_boundary(builder, history, legacy, summary, self.paginated, timestamp);
        let evidence =
            (self.paginated && !items.is_empty()).then(|| DisplayEvidence::collect(builder));
        let replacement_start = builder.checkpoint();
        let mut projection = ProjectedReplacement::default();
        let mut replacement = Vec::new();
        self.items.identity = MessageIdentity::default();
        self.items.clear_mirrors();
        for (index, item) in items.iter().enumerate() {
            if index != 0 && index.is_multiple_of(TOOL_COMPACTION_RECORD_INTERVAL) {
                self.compact(builder);
            }
            if self.paginated {
                projection.record_start(index, builder);
            }
            if paginated_rollback::is_private_instruction(item) || opaque_checkpoint_tool(item) {
                continue;
            }
            let selected = positioned_summary == Some(index);
            if let Some(text) = replacement_summary(item, summary.filter(|_| selected)) {
                push_summary_message(builder, text, timestamp, selected, self.paginated);
                continue;
            }
            if let Some(sources) = scoped_sources.as_ref()
                && self.project_checkpoint_plan(builder, index, item, sources, timestamp)
            {
                continue;
            }
            if self.paginated
                && item["type"]
                    .as_str()
                    .is_some_and(|kind| model_tool_kind(kind, item).is_some())
            {
                self.project_checkpoint_tool(
                    builder,
                    &mut projection,
                    index,
                    scoped_sources.as_ref(),
                    timestamp,
                );
                continue;
            }
            let source = scoped_sources
                .as_ref()
                .and_then(|sources| sources.items.get(&index).copied());
            if let Some(request) = self.project_replacement_item(
                builder,
                item,
                &payload["replacement_history_metadata"][index],
                evidence.as_ref(),
                source,
                timestamp,
            ) {
                replacement.push(request);
            }
        }
        if self.paginated {
            projection.record_start(items.len(), builder);
            self.track_pending_plan_window(items);
        }
        let first = start.sequence;
        self.register_replacement(builder, start, replacement);
        let summary_sequence = summary.filter(|_| legacy).map(|_| builder.checkpoint());
        if let Some(text) = summary.filter(|_| legacy) {
            push_summary_message(builder, text, timestamp, true, self.paginated);
        }
        let empty = builder.checkpoint() == replacement_start;
        let missing_context = history.map_or(!legacy || summary.is_none(), |items| {
            missing_replacement_context(items, empty, self.paginated)
        });
        if missing_context {
            report_unavailable_compaction(builder, timestamp);
        }
        let kind = WindowKind::from_checkpoint(legacy, history.is_some(), summary_sequence);
        self.record_compaction_window(
            first..builder.checkpoint(),
            kind,
            public,
            projection,
            payload["message"].as_str().filter(|_| legacy),
        );
        self.items.clear_mirrors();
    }

    fn record_compaction_window(
        &mut self,
        range: std::ops::Range<u64>,
        kind: WindowKind,
        evidence: Option<paginated_checkpoint::PaginatedCheckpoint>,
        projection: ProjectedReplacement,
        legacy_message: Option<&str>,
    ) {
        if self.paginated {
            let window = paginated_compaction::Window {
                range,
                ordinal: self.source_ordinal,
                kind,
                projection,
            };
            self.compactions.record(
                window,
                evidence,
                legacy_message,
                &self.paginated_inputs,
                self.checkpoint_turns.selected(),
            );
        }
    }

    fn project_checkpoint_tool(
        &self,
        builder: &mut EventBuilder,
        projection: &mut ProjectedReplacement,
        index: usize,
        sources: Option<&paginated_checkpoint::MatchedDisplays>,
        timestamp: Option<DateTime<Utc>>,
    ) {
        let source = sources
            .and_then(|matched| matched.items.get(&index))
            .map(|(seq, _)| *seq);
        projection.project_tool(builder, index, source, timestamp);
        if let Some(&(seq, spelling)) =
            sources.and_then(|matched| matched.async_questions.get(&index))
            && let Some(text) = self.replacement_text(builder, Some((seq, Some(spelling))))
        {
            projection.project_question(builder, seq, text, timestamp);
        }
    }

    fn track_pending_plan_window(&mut self, items: &[Value]) {
        let pending =
            self.paginated_answers
                .pending_plan_window(self.source_ordinal, items, |item| {
                    self.model_plan_mode(item)
                });
        self.paginated_answers.store_plan_window(pending);
    }

    fn project_replacement_item<'a>(
        &mut self,
        builder: &mut EventBuilder,
        item: &'a Value,
        metadata: &Value,
        evidence: Option<&DisplayEvidence>,
        source: Option<paginated_checkpoint::PublicCopy>,
        timestamp: Option<DateTime<Utc>>,
    ) -> Option<ReplacementRequest<'a>> {
        let mut checkpoint = self.checkpoint(builder);
        checkpoint.admission = InputPosition::user_item(item, metadata, self.source_ordinal);
        self.items.projected = None;
        let live_plan_mode = std::mem::take(&mut self.items.plan_mode);
        let user_turn = push_response_item(builder, item, timestamp, &mut self.items);
        self.items.plan_mode = live_plan_mode;
        if let Some(evidence) = evidence
            && let Some(seq) = self.items.projected
            && item["type"] == "message"
            && matches!(item["role"].as_str(), Some("user" | "assistant"))
            && !self.confirm_replacement_message(builder, seq, item, evidence, source)
        {
            return None;
        }
        (user_turn && builder.checkpoint() > checkpoint.sequence).then(|| ReplacementRequest {
            sequence: builder.checkpoint() - 1,
            checkpoint,
            item,
        })
    }

    fn replacement_text(
        &self,
        builder: &mut EventBuilder,
        source: Option<paginated_checkpoint::PublicCopy>,
    ) -> Option<String> {
        source.and_then(|(seq, spelling)| {
            let current = builder.event_at_mut(seq)?.payload["text"].as_str()?;
            match spelling {
                None => Some(current.to_owned()),
                Some(digest)
                    if <[u8; 32]>::from(sha2::Sha256::digest(current.as_bytes())) == digest =>
                {
                    Some(current.to_owned())
                }
                Some(digest) => self
                    .paginated_answers
                    .public_spelling(seq, digest)
                    .map(str::to_owned),
            }
        })
    }

    fn project_checkpoint_plan(
        &mut self,
        builder: &mut EventBuilder,
        index: usize,
        item: &Value,
        sources: &paginated_checkpoint::MatchedDisplays,
        timestamp: Option<DateTime<Utc>>,
    ) -> bool {
        if !self.model_plan_mode(item) {
            return false;
        }
        let Some((narrative, _)) = display_text::model_plan(item) else {
            return false;
        };
        for is_plan in [false, true] {
            if !is_plan && narrative.is_none() {
                continue;
            }
            let source = if is_plan {
                sources
                    .plans
                    .get(&index)
                    .map(|&(seq, digest)| (seq, Some(digest)))
            } else {
                sources.items.get(&index).copied()
            };
            if let Some(text) = self.replacement_text(builder, source) {
                builder.push(
                    EventKind::MessageAssistant,
                    json!({"text":text,"omnisession_codex_replacement":true}),
                    timestamp,
                    ReplayPolicy::Contextual,
                    Some("response_item.message".to_owned()),
                    None,
                );
            } else {
                let seq = builder.checkpoint();
                builder.push(
                    EventKind::ProviderEvent,
                    json!({"omitted_events": 1, "reason": "The Codex checkpoint has no matching published reply for this part of the proposal."}),
                    timestamp,
                    ReplayPolicy::HistoricalOnly,
                    Some("omnisession.codex_compaction_unavailable".into()),
                    None,
                );
                self.paginated_answers.pending_replacement_plan(
                    seq,
                    index,
                    item,
                    self.source_ordinal,
                    is_plan,
                );
            }
        }
        true
    }

    fn confirm_replacement_message(
        &mut self,
        builder: &mut EventBuilder,
        sequence: u64,
        item: &Value,
        evidence: &DisplayEvidence,
        scoped_source: Option<(u64, Option<[u8; 32]>)>,
    ) -> bool {
        let source_turn = item["internal_chat_message_metadata_passthrough"]["turn_id"].as_str();
        let source_id = item["id"].as_str();
        let public_text = self.replacement_text(builder, scoped_source);
        let confirmed = public_text.is_some()
            || (scoped_source.is_none()
                && source_turn.zip(source_id).is_none()
                && (item["role"] != "user" || item["id"].is_null())
                && evidence
                    .unique_unscoped(builder, sequence)
                    .is_some_and(|shown| {
                        item["role"] != "user"
                            || (self.paginated_inputs.display_is_plain_text(shown)
                                && item["content"].as_array().is_some_and(|parts| {
                                    parts.iter().all(|part| part["type"] == "input_text")
                                }))
                    }));
        if let Some(e) = builder.event_at_mut(sequence) {
            if confirmed {
                if let Some(text) = public_text {
                    e.payload["text"] = text.into();
                }
                e.payload["omnisession_codex_replacement"] = true.into();
            } else {
                if e.kind == EventKind::MessageAssistant
                    && let Some(text) = e.payload["text"].as_str()
                {
                    self.paginated_answers.pending_replacement(
                        sequence,
                        item,
                        text,
                        self.source_ordinal,
                    );
                }
                e.kind = EventKind::ProviderEvent;
                e.payload = json!({"omitted_events":1,"reason":"The Codex replacement contains a message without a matching display item."});
                e.source.raw_record_type =
                    Some("omnisession.codex_compaction_unavailable".to_owned());
                e.replay_policy = ReplayPolicy::HistoricalOnly;
            }
        }
        confirmed
    }

    fn legacy_revision_input(&self, owner: Option<u64>) -> Option<&TurnCheckpoint> {
        if self.paginated {
            return None;
        }
        let id = owner.or_else(|| self.turns.last().copied())?;
        let original = self.original_requests.get(&id)?;
        let i = self
            .checkpoints
            .partition_point(|(_, point)| point.sequence <= *original);
        self.checkpoints[..i]
            .iter()
            .rev()
            .find(|(owner, _)| *owner == id)
            .map(|(_, point)| point)
    }

    fn push_unscoped_record(&mut self, builder: &mut EventBuilder, record: &Value) -> bool {
        let payload = &record["payload"];
        self.items.projected = None;
        self.items.identity = if record["type"] == "event_msg"
            && !self.explicit_display_turn
            && matches!(
                payload["type"].as_str(),
                Some("user_message" | "agent_message")
            ) {
            self.legacy_display
        } else {
            MessageIdentity::default()
        };
        self.items.cited_display =
            payload["type"] == "agent_message" && payload["memory_citation"].is_object();
        let input = self
            .legacy_revision_input(None)
            .and_then(TurnCheckpoint::admitted_sequence);
        let new_turn = push_record(builder, record, &mut self.items);
        self.display_revisions.observe_message(
            builder,
            self.items.previous_message.take(),
            input,
            revision_owner(&self.late_owners, &self.original_requests),
        );
        if record["type"] == "response_item" {
            self.observe_model_tool(self.items.projected, record, None, None);
        }
        new_turn
    }

    fn push_record(&mut self, builder: &mut EventBuilder, record: &Value) -> bool {
        let payload = &record["payload"];
        if record["type"] != "response_item" || payload["type"] != "message" {
            return self.push_unscoped_record(builder, record);
        }
        let role = payload["role"].as_str();
        if !matches!(role, Some("assistant" | "user")) {
            return false;
        }
        let stamped = payload["internal_chat_message_metadata_passthrough"]["turn_id"]
            .as_str()
            .filter(|turn| !turn.is_empty())
            .map(|turn| self.aliases.resolve(turn));
        // A generated turn can close after the next raw message has already arrived.
        let turn = stamped.or_else(|| {
            self.active_turn_id
                .as_deref()
                .filter(|id| !is_implicit_turn(id))
                .map(|id| self.aliases.resolve(id))
        });
        let mut identity = MessageIdentity::new(turn, payload["id"].as_str(), false);
        identity.ordered_user_input =
            role == Some("user") && model_cut::input_order(&record["metadata"]).is_some();
        let key = identity.scoped(role == Some("assistant"));
        let owner = stamped.and_then(|turn| self.noncurrent_owner(turn));
        let input = self
            .legacy_revision_input(owner)
            .and_then(TurnCheckpoint::admitted_sequence);
        let plan_mode = stamped.map_or(self.items.plan_mode, |turn| {
            self.plan_turns.contains(turn)
                || (self.items.plan_mode
                    && self.current_turn_id().is_none_or(|active| active == turn))
        });
        let mut late = CodexItems::default();
        let items = if owner.is_some() {
            &mut late
        } else {
            &mut self.items
        };
        let plan_scope = turn.or(self.active_turn_id.as_deref());
        let previous_plan_mode = std::mem::replace(&mut items.plan_mode, plan_mode);
        items.identity = identity;
        items.cited_display = false;
        items.update = key.and_then(|key| self.message_ids.get(&key).copied());
        items.projected = None;
        let new_turn = push_record(builder, record, items);
        if let Some(proposal) = items.raw_proposal.take() {
            let owner = owner.or_else(|| self.turns.last().copied());
            self.plan_receipts.proposal(plan_scope, proposal, owner);
        }
        if !new_turn && let Some(position) = InputPosition::user(record, self.source_ordinal) {
            confirm_admission(
                &mut self.checkpoints,
                &self.requests,
                items.projected,
                position,
            );
        }
        self.display_revisions.observe_message(
            builder,
            items.previous_message.take(),
            input,
            revision_owner(&self.late_owners, &self.original_requests),
        );
        if role == Some("assistant")
            && let Some(copy) = items
                .mirrors
                .confirm_ordered_copy(items.projected, builder.checkpoint())
        {
            // The raw admission remains a rollback checkpoint even when both displayed messages match.
            builder.truncate_from(copy.accepted);
            if let Some(key) = copy.scoped_id {
                self.message_ids.insert(key, copy.original);
            }
        }
        items.plan_mode = previous_plan_mode;
        if let Some(seq) = items.projected {
            if let Some(key) = key {
                self.message_ids.insert(key, seq);
            }
            if let Some(owner) = owner {
                self.late_owners.entry(seq).or_insert(owner);
            }
        }
        items.update = None;
        items.identity = MessageIdentity::default();
        new_turn && owner.is_none()
    }

    fn push_model_tool(&mut self, builder: &mut EventBuilder, record: &Value) -> bool {
        if record["type"] != "response_item" {
            return false;
        }
        let payload = &record["payload"];
        let stamp = parse_timestamp(record.get("timestamp"));
        let stamped = payload["internal_chat_message_metadata_passthrough"]["turn_id"]
            .as_str()
            .filter(|turn| !turn.is_empty())
            .map(|turn| self.aliases.resolve(turn));
        let current = self.active_turn_id.as_deref();
        let active = current
            .filter(|turn| !is_implicit_turn(turn))
            .map(|turn| self.aliases.resolve(turn));
        if let Some(id) = model_call_id(payload) {
            // A generated turn can remain open after the next raw input has arrived.
            // Use it only while its confirmed request is the most recent checkpoint.
            let turn = stamped.or(active).or_else(|| {
                let named = self.aliases.resolve(current?);
                let owner = self.named_turns.get(named)?;
                let completed = self
                    .completed
                    .get(named)
                    .is_some_and(|items| items.contains_key(&(CompletedKind::Tool, id.to_owned())));
                (self.turns.last()? == owner && !completed).then_some(named)
            });
            let sequence = builder.checkpoint();
            let owner = turn.and_then(|turn| self.named_turns.get(turn).copied());
            if let Some(owner) = turn.and_then(|turn| self.noncurrent_owner(turn)) {
                self.late_owners.insert(sequence, owner);
                let mut late = CodexItems::default();
                push_response_item(builder, payload, stamp, &mut late);
                *self.late_tool_counts.entry(owner).or_default() += late.tool_events;
            } else {
                push_response_item(builder, payload, stamp, &mut self.items);
            }
            self.calls
                .push(id, turn.map(str::to_owned), sequence, owner);
            let key = self.paginated_tool_id(payload, turn, id);
            self.observe_model_tool(Some(sequence), record, None, key);
            return true;
        }
        let Some(id) = model_result_id(payload) else {
            return false;
        };
        let call = self.calls.take(id, stamped, active);
        let turn = stamped
            .or_else(|| call.as_ref()?.turn.as_deref())
            .or(active)
            .map(str::to_owned);
        let sequence = if let Some(turn) = &turn {
            self.push_identified_item(builder, payload, turn, id, stamp)
        } else {
            self.items.projected = None;
            self.items.tool_owner = self.turns.last().copied().filter(|&id| {
                self.legacy_revision_input(Some(id))
                    .is_some_and(|input| input.admission.is_some())
                    && self
                        .original_requests
                        .get(&id)
                        .zip(call.as_ref())
                        .is_some_and(|(&request, call)| call.sequence >= request)
            });
            push_response_item(builder, payload, stamp, &mut self.items);
            self.items.tool_owner = None;
            self.items.projected
        };
        let key = self.paginated_tool_id(payload, turn.as_deref(), id);
        self.observe_model_tool(
            sequence,
            record,
            call.as_ref().map(|call| call.sequence),
            key,
        );
        if let Some(sequence) = sequence
            && let Some(call) = call
        {
            if let Some(owner) = call.owner.or_else(|| {
                turn.as_ref()
                    .and_then(|turn| self.named_turns.get(turn).copied())
            }) {
                self.claim_tool(owner, sequence);
                self.claim_tool(owner, call.sequence);
            } else if call.turn.is_none() {
                self.calls.linked.insert(sequence, call.sequence);
            }
        }
        true
    }

    fn paginated_tool_id(&self, payload: &Value, turn: Option<&str>, id: &str) -> Option<ToolId> {
        if !self.paginated {
            return None;
        }
        ToolId::new(
            payload["internal_chat_message_metadata_passthrough"]["turn_id"]
                .as_str()
                .or(turn),
            id,
        )
    }

    fn observe_model_tool(
        &mut self,
        sequence: Option<u64>,
        record: &Value,
        call: Option<u64>,
        key: Option<ToolId>,
    ) {
        if self.paginated
            && let Some(sequence) = sequence
            && let Some(status) =
                self.paginated_answers
                    .model_tool(sequence, record, self.source_ordinal, call, key)
        {
            self.withdrawn
                .entry(sequence)
                .or_insert((status, self.source_ordinal));
        }
    }

    fn push_identified_item(
        &mut self,
        builder: &mut EventBuilder,
        payload: &Value,
        turn: &str,
        id: &str,
        timestamp: Option<DateTime<Utc>>,
    ) -> Option<u64> {
        let display = payload["type"] == "item_completed";
        if display {
            push_codex_session_metadata(builder, payload, timestamp, true);
        }
        let native_turn = self.aliases.resolve(turn).to_owned();
        // An asynchronous assistant message and its tool receipt share the call ID.
        let key = (CompletedKind::for_item(payload), id.to_owned());
        let original = self
            .completed
            .get(&native_turn)
            .and_then(|items| items.get(&key))
            .copied();
        let owner = self.noncurrent_owner(&native_turn);
        let input = self.legacy_revision_input(owner).filter(|_| display);
        let message_input = input.and_then(TurnCheckpoint::admitted_sequence);
        let revision_input = input.map(|turn| turn.sequence);
        let previous_display = revision_input
            .filter(|_| owner.is_none())
            .and_then(|input| self.display_revisions.before(builder, original, input));
        // A late result cannot act as a message or tool mirror in the current turn.
        let mut late = CodexItems::default();
        let checkpoint = self.checkpoint(builder);
        // A synthesized display turn can still mirror earlier model items, even if the
        // last logical request in the file is newer. Its native turn owns the result.
        let current_mirrors =
            owner.is_none() || self.current_turn_id() == Some(native_turn.as_str());
        let items = if current_mirrors {
            &mut self.items
        } else {
            &mut late
        };
        let item = &payload["item"];
        let item_type = item["type"].as_str();
        let user = item_type == Some("UserMessage");
        items.identity = MessageIdentity::completed(turn, id, user);
        let scoped = MessageIdentity::completed(&native_turn, id, user);
        items.tool_turn = Some(native_turn.clone());
        items.tool_owner = self.named_turns.get(&native_turn).copied();
        items.cited_display = matches!(item["memory_citation"], Value::Object(_) | Value::Array(_));
        let message_id = match item_type {
            Some("AgentMessage") => scoped.scoped(true),
            Some("UserMessage") => scoped.scoped(false),
            _ => None,
        };
        items.update =
            original.or_else(|| message_id.and_then(|key| self.message_ids.get(&key).copied()));
        items.projected = None;
        let new_turn = items.project_identified(builder, payload, turn, id, timestamp);
        let previous_message = items.previous_message.take();
        let projected = items.finish_identified_item();
        let was_displayed = self.observe_reply_revision(builder, previous_message, message_input);
        if let Some(owner) = owner {
            self.count_late_outcome(
                owner,
                projected,
                current_mirrors,
                checkpoint.tool_events,
                late.tool_events,
            );
        }
        let sequence = projected?;
        if let Some(input) = revision_input {
            self.display_revisions
                .observe(builder, sequence, input, previous_display);
        }
        if display {
            self.observe_display_sequence(builder, sequence, turn, id, item);
        }
        if let Some(key) = message_id {
            self.message_ids.insert(key, sequence);
        }
        if display && !matches!(item_type, Some("AgentMessage" | "UserMessage" | "Plan")) {
            self.claim_display_outcome(&native_turn, sequence, checkpoint.sequence);
        }
        let item_owner = (!matches!(item_type, Some("UserMessage" | "AgentMessage")))
            .then(|| self.named_turns.get(&native_turn).copied())
            .flatten();
        if let Some(owner) = owner.or(item_owner).filter(|_| original.is_none()) {
            if message_id.is_some() || item_type == Some("Plan") {
                self.late_owners.entry(sequence).or_insert(owner);
            } else {
                self.claim_tool(owner, sequence);
            }
        } else if new_turn {
            let mut checkpoint = self.pending_turn.take().unwrap_or(checkpoint);
            checkpoint.id = Some(turn.to_owned());
            self.remember_turn(checkpoint, Some(sequence));
        } else if item_type == Some("UserMessage") && original.is_none() {
            self.name_request_turn(turn, sequence);
        }
        if display
            && original.is_none()
            && !was_displayed
            && item_type == Some("AgentMessage")
            && let Some(&owner) = self.named_turns.get(&native_turn)
        {
            self.place_early_answer(builder, owner, sequence);
        }
        self.completed
            .entry(self.aliases.resolve(turn).to_owned())
            .or_default()
            .insert(key, sequence);
        Some(sequence)
    }

    fn count_late_outcome(
        &mut self,
        owner: u64,
        projected: Option<u64>,
        current_mirrors: bool,
        initial_tools: usize,
        late_tools: usize,
    ) {
        let added = if current_mirrors {
            let added = self.items.tool_events - initial_tools;
            self.items.tool_events = initial_tools;
            added
        } else {
            late_tools
        };
        if added > 0
            && let Some(seq) = projected
        {
            self.late_owners.entry(seq).or_insert(owner);
            *self.late_tool_counts.entry(owner).or_default() += added;
        }
    }

    fn observe_reply_revision(
        &mut self,
        builder: &mut EventBuilder,
        previous: Option<(u64, PreviousDisplay)>,
        input: Option<u64>,
    ) -> bool {
        if self.paginated {
            self.paginated_answers
                .archive_changed_reply(builder, previous.as_ref());
        }
        self.display_revisions.observe_message(
            builder,
            previous,
            input,
            revision_owner(&self.late_owners, &self.original_requests),
        )
    }

    fn observe_display_sequence(
        &mut self,
        builder: &mut EventBuilder,
        sequence: u64,
        turn: &str,
        id: &str,
        item: &Value,
    ) {
        if item["type"] == "Plan"
            && let Some(text) = item["text"].as_str()
        {
            let name = self.aliases.resolve(turn);
            let owner = self
                .named_turns
                .get(name)
                .copied()
                .or_else(|| self.turns.last().copied());
            self.plan_receipts.completed(name, text, owner);
        }
        if !self.paginated {
            return;
        }
        if matches!(item["type"].as_str(), Some("Plan" | "AgentMessage")) {
            if let Some(text) = builder
                .event_at_mut(sequence)
                .and_then(|event| event.payload["text"].as_str())
            {
                self.paginated_answers.display(
                    sequence,
                    turn,
                    id,
                    text,
                    self.source_ordinal,
                    item["type"] == "Plan",
                );
            }
            if !completed_item::assistant::is_async(&item["delivery"]) {
                return;
            }
        }
        if self.withdrawn.contains_key(&sequence) {
            return;
        }
        if item["type"] == "UserMessage" {
            self.paginated_inputs
                .display_user(sequence, turn, item, self.source_ordinal);
        } else {
            let withdrawal = match item["type"].as_str() {
                Some("Plan") | None => None,
                _ => self
                    .paginated_answers
                    .display_tool(sequence, turn, id, item),
            };
            if let Some(status) = withdrawal {
                self.withdrawn
                    .entry(sequence)
                    .or_insert((status, self.source_ordinal));
            }
        }
    }

    fn place_early_answer(&mut self, builder: &EventBuilder, owner: u64, sequence: u64) {
        let Some(&request) = self.original_requests.get(&owner) else {
            return;
        };
        if sequence >= request || self.display_positions.contains_key(&sequence) {
            return;
        }
        let next = self
            .checkpoints
            .partition_point(|(_, point)| point.sequence <= request);
        let boundary = self
            .checkpoints
            .get(next)
            .map_or_else(|| builder.checkpoint(), |(_, point)| point.sequence);
        self.display_positions
            .insert(sequence, (boundary, self.source_ordinal));
        self.late_owners.entry(sequence).or_insert(owner);
    }

    /// A migrated request can precede its generated display turn.
    fn name_request_turn(&mut self, turn: &str, sequence: u64) {
        let Some(owner) = self.request_id(sequence) else {
            return;
        };
        let Some(&source_request) = self.original_requests.get(&owner) else {
            return;
        };
        let Some(index) = self
            .checkpoints
            .partition_point(|(_, c)| c.sequence <= source_request)
            .checked_sub(1)
        else {
            return;
        };
        let (id, checkpoint) = &mut self.checkpoints[index];
        if *id != owner {
            return;
        }
        if let Some(original) = &checkpoint.id
            && self.aliases.bind(turn, original)
        {
            self.message_ids.bind(turn, original);
            if let Some(known) = self.completed.remove(turn) {
                self.completed
                    .entry(original.clone())
                    .or_default()
                    .extend(known);
            }
            if self.plan_turns.remove(turn) {
                self.plan_turns.insert(original.clone());
            }
        }
        let canonical = self.aliases.resolve(turn);
        checkpoint.id = Some(canonical.to_owned());
        self.named_turns.insert(canonical.to_owned(), owner);
        if self
            .pending_turn
            .as_ref()
            .is_some_and(|pending| sequence < pending.sequence)
        {
            self.pending_turn = None;
        }
    }

    fn claim_display_outcome(&mut self, turn: &str, sequence: u64, checkpoint: u64) {
        if let Some(&owner) = self.named_turns.get(turn) {
            if let Some(call) = self.calls.linked.remove(&sequence) {
                self.claim_tool(owner, call);
            }
            if sequence < checkpoint {
                self.claim_tool(owner, sequence);
            }
        }
    }

    fn claim_tool(&mut self, owner: u64, sequence: u64) {
        if self.late_owners.contains_key(&sequence) {
            return;
        }
        let later = self
            .checkpoints
            .partition_point(|(_, point)| point.sequence <= sequence);
        // A display item can identify the native owner of an earlier model call and result.
        // Count the record with that owner, including after the visible tool window evicts it.
        self.late_owners.insert(sequence, owner);
        *self.late_tool_counts.entry(owner).or_default() += 1;
        self.items.tool_events -= 1;
        for (_, point) in &mut self.checkpoints[later..] {
            point.tool_events -= 1;
        }
        if let Some(point) = self
            .pending_turn
            .as_mut()
            .filter(|point| point.sequence > sequence)
        {
            point.tool_events -= 1;
        }
    }

    fn request_id(&self, sequence: u64) -> Option<u64> {
        let index = self
            .requests
            .binary_search_by_key(&sequence, |(seq, _)| *seq)
            .ok()?;
        Some(self.requests[index].1)
    }

    fn restore_legacy_users(&mut self, builder: &mut EventBuilder) {
        let events = builder.events();
        let Some(end) = events.iter().rposition(|e| {
            e.kind == EventKind::CompactionCreated
                && e.payload["omnisession_codex_rollback"] != "withdrawn"
        }) else {
            return;
        };
        if events[end].source.raw_record_type.as_deref() != Some("compacted.legacy") {
            return;
        }
        let retained = if self.paginated {
            self.compactions
                .legacy_users(events[end].sequence)
                .filter_map(|(seq, budget)| {
                    let event = &events[events.binary_search_by_key(&seq, |e| e.sequence).ok()?];
                    let text = event.payload["text"].as_str()?;
                    let text = budget.map_or_else(
                        || text.to_owned(),
                        |budget| shorten_legacy_user(text, budget).into_owned(),
                    );
                    legacy_message_source(event).map(|summary| (event.event_id, summary, text))
                })
                .collect()
        } else {
            self.unpaginated_legacy_users(&events[..=end])
        };
        let timestamp = events[end].timestamp;
        let boundary = events[end].sequence;
        let inserted = retained.len() as u64;
        // Older checkpoints retain their summaries and original requests for recovery.
        // Emit copies only for the last surviving window after all rollbacks have been applied.
        builder.insert_after(end, |builder| {
            for (id, summary, text) in retained {
                let (kind, wire) = if summary {
                    (EventKind::MessageAssistant, "compacted.summary")
                } else {
                    (EventKind::MessageUser, "compacted.user")
                };
                builder.push(
                    kind,
                    replacement_payload(&text, self.paginated),
                    timestamp,
                    ReplayPolicy::Contextual,
                    Some(wire.to_owned()),
                    Some(id),
                );
            }
        });
        if inserted > 0 {
            self.shift_legacy_positions(builder, boundary, inserted);
        }
    }

    fn shift_legacy_positions(&mut self, builder: &mut EventBuilder, boundary: u64, inserted: u64) {
        let adjust = |seq| if seq > boundary { seq + inserted } else { seq };
        if !self.display_positions.is_empty() {
            self.display_positions = self
                .display_positions
                .drain()
                .map(|(seq, (before, ordinal))| (adjust(seq), (adjust(before), ordinal)))
                .collect();
        }
        if self.paginated {
            builder.visit_events_mut(|event| {
                if matches!(
                    event.kind,
                    EventKind::MessageUser | EventKind::MessageAssistant
                ) && event.source.raw_record_type.as_deref() == Some("event_msg.item_completed")
                    && let Some(anchor) = event
                        .payload
                        .pointer_mut("/omnisession_codex_checkpoint_position/0")
                    && let Some(before) = anchor.as_u64()
                {
                    *anchor = adjust(before).into();
                }
            });
        }
    }

    fn unpaginated_legacy_users(
        &self,
        events: &[omnis_ir::OmniEvent],
    ) -> Vec<(Uuid, bool, String)> {
        let start = events
            .iter()
            .rposition(explicit_compaction)
            .map_or(0, |i| i + 1);
        let mut users = VecDeque::new();
        let mut tokens = 0;
        for e in &events[start..] {
            if e.kind == EventKind::CompactionCreated {
                trim_legacy_users(&mut users, &mut tokens);
            } else if let Some(summary) = legacy_message_source(e)
                && let Some(text) = e.payload["text"].as_str()
                && (self.request_id(e.sequence).is_some()
                    || (summary && !paginated_rollback::contextual_fragment(text)))
            {
                users.push_back((e.event_id, summary, Cow::Borrowed(text)));
                tokens += text.len().div_ceil(4);
            }
        }
        users
            .into_iter()
            .map(|(id, summary, text)| (id, summary, text.into_owned()))
            .collect()
    }

    fn register_replacement(
        &mut self,
        builder: &EventBuilder,
        start: TurnCheckpoint,
        replacement: Vec<ReplacementRequest<'_>>,
    ) {
        if replacement.is_empty() {
            return;
        }
        let events = builder.events();
        let before = events.partition_point(|e| e.sequence < start.sequence);
        let mut originals: HashMap<&str, Vec<(u64, u64)>> = HashMap::new();
        for e in events[..before]
            .iter()
            .rev()
            .take_while(|e| !explicit_compaction(e))
        {
            if let Some(id) = self.request_id(e.sequence)
                && let Some(text) = e.payload["text"].as_str()
            {
                originals.entry(text).or_default().push((e.sequence, id));
            }
        }
        // Work backward so the newest repeated requests match the newest surviving turns.
        let mut before = start.sequence;
        let mut owners = Vec::with_capacity(replacement.len());
        for request in replacement.iter().rev() {
            let matched = events
                .binary_search_by_key(&request.sequence, |e| e.sequence)
                .ok()
                .and_then(|i| events[i].payload["text"].as_str())
                .and_then(|text| {
                    if let Some(point) = request_origin(text, request.item, before, &originals) {
                        return Some(point);
                    }
                    (request.sequence == replacement[0].sequence)
                        .then(|| shortened_request_origin(text, before, &originals))
                        .flatten()
                });
            if let Some((sequence, _)) = matched {
                before = sequence;
            }
            owners.push(matched.map(|(_, id)| id));
        }
        owners.reverse();
        if owners.iter().all(Option::is_some) {
            self.requests.extend(
                replacement
                    .into_iter()
                    .zip(owners)
                    .map(|(request, id)| (request.sequence, id.unwrap())),
            );
            return;
        }

        // Some original turns are missing. Use the replacement for their boundaries and
        // associate copies of known requests with the same turn in both parts of the file.
        let mut previous = std::mem::take(&mut self.turns).into_iter();
        let mut missing = Vec::new();
        let mut window_start = Some(self.pending_turn.take().unwrap_or(start));
        for (request, matched) in replacement.into_iter().zip(owners) {
            let id = if let Some(id) = matched {
                self.turns
                    .extend(previous.by_ref().take_while(|old| *old != id));
                self.turns.append(&mut missing);
                self.turns.push(id);
                id
            } else {
                let id = self.new_turn();
                missing.push(id);
                id
            };
            self.requests.push((request.sequence, id));
            self.original_requests.entry(id).or_insert(request.sequence);
            let admission = request.checkpoint.admission;
            let mut point = window_start.take().unwrap_or(request.checkpoint);
            point.admission = admission;
            self.checkpoints.push((id, point));
        }
        self.turns.extend(previous);
        self.turns.append(&mut missing);
    }
}

fn request_origin(
    text: &str,
    item: &Value,
    before: u64,
    originals: &HashMap<&str, Vec<(u64, u64)>>,
) -> Option<(u64, u64)> {
    let previous = |text: &str| {
        let positions = originals.get(text)?;
        let skip = positions.partition_point(|(seq, _)| *seq >= before);
        positions.get(skip).copied()
    };
    let parts = item["content"]
        .as_array()
        .filter(|parts| parts.len() > 1)
        .map(|parts| {
            message_parts(parts, |i, _| {
                hook_attribution(&item["internal_chat_message_metadata_passthrough"], i)
                    != Some(true)
            })
        });
    // Display items concatenate the same parts that a model response joins with newlines.
    let display = parts
        .filter(|parts| parts.len() > 1)
        .and_then(|parts| previous(&parts.concat()));
    previous(text).max(display)
}

fn explicit_compaction(event: &omnis_ir::OmniEvent) -> bool {
    event.kind == EventKind::CompactionCreated
        && event.payload["omnisession_codex_rollback"] != "withdrawn"
        && event.source.raw_record_type.as_deref() != Some("compacted.legacy")
}

fn legacy_message_source(event: &omnis_ir::OmniEvent) -> Option<bool> {
    if event.kind == EventKind::MessageUser {
        Some(false)
    } else {
        (event.kind == EventKind::MessageAssistant
            && event.source.raw_record_type.as_deref() == Some("compacted.summary"))
        .then_some(true)
    }
}

fn trim_legacy_users(users: &mut VecDeque<(Uuid, bool, Cow<'_, str>)>, tokens: &mut usize) {
    // Codex retains 20,000 approximate tokens and replaces the middle of the oldest partial request.
    while *tokens > COMPACT_USER_TOKEN_LIMIT {
        let Some((_, _, text)) = users.front_mut() else {
            break;
        };
        let size = text.len().div_ceil(4);
        let other = *tokens - size;
        if other >= COMPACT_USER_TOKEN_LIMIT {
            users.pop_front();
            *tokens = other;
            continue;
        }
        if let Cow::Owned(shortened) = shorten_legacy_user(text, COMPACT_USER_TOKEN_LIMIT - other) {
            *text = Cow::Owned(shortened);
        }
        *tokens = other + text.len().div_ceil(4);
        break;
    }
}

fn shorten_legacy_user(text: &str, budget: usize) -> Cow<'_, str> {
    let omitted = text.len().div_ceil(4).saturating_sub(budget);
    if omitted > 0 {
        let (prefix, suffix) = retained_user_edges(text, budget);
        let marker = format!("…{omitted} tokens truncated…");
        if text[prefix.len()..text.len() - suffix.len()] != marker {
            return Cow::Owned(format!("{prefix}{marker}{suffix}"));
        }
    }
    Cow::Borrowed(text)
}

fn retained_user_edges(text: &str, tokens: usize) -> (&str, &str) {
    let half = 2 * tokens;
    let mut left = half;
    let mut right = text.len() - half;
    while !text.is_char_boundary(left) {
        left -= 1;
    }
    while !text.is_char_boundary(right) {
        right += 1;
    }
    (&text[..left], &text[right..])
}

fn shortened_request_origin(
    text: &str,
    before: u64,
    originals: &HashMap<&str, Vec<(u64, u64)>>,
) -> Option<(u64, u64)> {
    let (prefix, count, suffix) =
        text.match_indices(" tokens truncated…")
            .find_map(|(offset, end)| {
                let (prefix, count) = text[..offset].rsplit_once('…')?;
                let suffix = &text[offset + end.len()..];
                let count: usize = count.parse().ok()?;
                // The two retained halves have the same byte budget. UTF-8 boundaries can differ by three bytes.
                (count > 0 && prefix.len().abs_diff(suffix.len()) <= 3)
                    .then_some((prefix, count, suffix))
            })?;
    // Match the exact head and tail produced by Codex. Newer rollouts store the notice
    // in its own text block, which the visible projection joins with a newline on each side.
    originals
        .iter()
        .filter_map(|(source, positions)| {
            let kept = source.len().div_ceil(4).checked_sub(count)?;
            if !(1..=COMPACT_USER_TOKEN_LIMIT).contains(&kept) {
                return None;
            }
            let (left, right) = retained_user_edges(source, kept);
            if left.is_empty() && right.is_empty() {
                return None;
            }
            if (prefix == left && suffix == right)
                || (prefix.strip_suffix('\n') == Some(left)
                    && suffix.strip_prefix('\n') == Some(right))
            {
                let skip = positions.partition_point(|(seq, _)| *seq >= before);
                return positions.get(skip).copied();
            }
            None
        })
        .max()
}

fn replacement_summary_index(items: &[Value], summary: Option<&str>) -> Option<usize> {
    items
        .iter()
        .rposition(|item| replacement_summary(item, None).is_some())
        .or_else(|| {
            summary.and_then(|text| {
                items
                    .iter()
                    .rposition(|item| replacement_summary(item, Some(text)) == Some(text))
            })
        })
}

fn report_unavailable_compaction(builder: &mut EventBuilder, timestamp: Option<DateTime<Utc>>) {
    builder.push(
        EventKind::ProviderEvent,
        json!({"omitted_events": 1, "reason":
                "Codex compaction context is unavailable"}),
        timestamp,
        ReplayPolicy::HistoricalOnly,
        Some("omnisession.codex_compaction_unavailable".to_owned()),
        None,
    );
}

fn opaque_checkpoint_tool(item: &Value) -> bool {
    matches!(
        item["type"].as_str(),
        Some("computer_call" | "computer_call_output" | "local_shell_call_output")
    )
}

fn native_replacement_items(items: &[Value], paginated: bool) -> impl Iterator<Item = &Value> {
    items
        .iter()
        .filter(move |item| paginated || !checkpoint_wire::legacy_ghost(item))
}

fn missing_replacement_context(items: &[Value], empty_projection: bool, paginated: bool) -> bool {
    native_replacement_items(items, paginated).any(|item| {
        matches!(
            item["type"].as_str(),
            Some("compaction" | "context_compaction" | "compaction_summary")
        ) || (empty_projection && !opaque_checkpoint_tool(item))
    })
}

fn push_compaction_boundary(
    builder: &mut EventBuilder,
    history: Option<&Vec<Value>>,
    legacy: bool,
    summary: Option<&str>,
    paginated: bool,
    timestamp: Option<DateTime<Utc>>,
) -> Option<usize> {
    let start = builder.checkpoint();
    let items = history.map_or(&[][..], Vec::as_slice);
    let positioned_summary = replacement_summary_index(items, summary);
    if history.is_some() {
        // The model receives only the replacement items. A separate summary is documentary.
        let count = if paginated {
            items.len()
        } else {
            native_replacement_items(items, false).count()
        };
        let mut boundary = json!({"replacement_items":count});
        if let Some(text) = summary
            && positioned_summary.and_then(|i| replacement_summary(&items[i], summary))
                != Some(text)
        {
            boundary["summary"] = text.into();
        }
        builder.push(
            EventKind::CompactionCreated,
            boundary,
            timestamp,
            ReplayPolicy::HistoricalOnly,
            Some("compacted".to_owned()),
            None,
        );
    } else {
        builder.push(EventKind::CompactionCreated,
                json!({"summary":"Codex replaced the preceding conversation with a compacted context window."}),
                timestamp, ReplayPolicy::Contextual, Some(if legacy {"compacted.legacy"} else {"compacted"}.to_owned()), None);
    }
    if paginated && let Some(fence) = builder.event_at_mut(start) {
        fence.payload["omnisession_codex_paginated"] = true.into();
    }
    positioned_summary
}

fn replacement_payload(text: &str, paginated: bool) -> Value {
    let mut payload = json!({"text":text});
    if paginated {
        payload["omnisession_codex_replacement"] = true.into();
    }
    payload
}

fn push_summary_message(
    builder: &mut EventBuilder,
    text: &str,
    timestamp: Option<DateTime<Utc>>,
    latest: bool,
    paginated: bool,
) {
    let kind = if latest {
        "compacted.summary"
    } else {
        "response_item.message"
    };
    builder.push(
        EventKind::MessageAssistant,
        replacement_payload(text, paginated),
        timestamp,
        ReplayPolicy::Contextual,
        Some(kind.to_owned()),
        None,
    );
}

/// Local compaction stores its summary as a user message at its position in the replacement.
fn replacement_summary<'a>(item: &'a Value, summary: Option<&str>) -> Option<&'a str> {
    if item["type"] != "message" || item["role"] != "user" {
        return None;
    }
    let text = if let Some(text) = item["content"].as_str() {
        text
    } else {
        let [part] = item["content"].as_array()?.as_slice() else {
            return None;
        };
        if !matches!(part["type"].as_str(), Some("input_text" | "text")) {
            return None;
        }
        part["text"].as_str()?
    };
    let metadata = &item["internal_chat_message_metadata_passthrough"];
    if hook_attribution(metadata, 0).is_some() {
        return None;
    }
    let kinds = &metadata["content_item_kinds"];
    let legacy_summary = kinds[0].is_null()
        && Some(text) == summary
        && text
            .split_once('\n')
            .is_some_and(|(prefix, _)| prefix == LOCAL_SUMMARY_PREFIX);
    (!text.trim().is_empty() && (legacy_summary || kinds[0] == "compaction.summary"))
        .then_some(text)
}

fn workspace_record_filter() -> impl Fn(&[u8]) -> bool {
    let cwd = memchr::memmem::Finder::new(b"turn_context");
    let settings = memchr::memmem::Finder::new(b"thread_settings_applied");
    let rollback = memchr::memmem::Finder::new(b"thread_rolled_back");
    let escape = memchr::memmem::Finder::new(b"\\u00");
    move |line| {
        cwd.find(line).is_some()
            || settings.find(line).is_some()
            || rollback.find(line).is_some()
            || escape
                .find_iter(line)
                .any(|index| escapes_ascii_letter(line.get(index + 4..index + 6)))
            || workspace_damage::damaged_workspace_record(line).is_some()
    }
}

#[cfg(test)]
fn contains_rollback_marker(path: &Path) -> Result<bool> {
    contains_workspace_marker(path, b"thread_rolled_back", 1024 * 1024)
}

fn contains_settings_marker(path: &Path) -> Result<bool> {
    contains_workspace_marker(path, b"thread_settings_applied", 1024 * 1024)
}

fn contains_workspace_marker(path: &Path, name: &[u8], chunk: usize) -> Result<bool> {
    reader_contains_workspace_marker(fs::File::open(path)?, name, chunk)
}

fn oversized_settings_marker(
    path: &Path,
    locations: &[crate::support::JsonLineLocation],
) -> Result<bool> {
    let mut file = fs::File::open(path)?;
    for record in locations {
        file.seek(SeekFrom::Start(record.offset))?;
        if reader_contains_workspace_marker(
            (&mut file).take(record.length),
            b"thread_settings_applied",
            1024 * 1024,
        )? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn reader_contains_workspace_marker(
    mut reader: impl Read,
    name: &[u8],
    chunk: usize,
) -> Result<bool> {
    // Overlap the reads so a marker split across them is still visible.
    let mut buffer = vec![0_u8; chunk.max(name.len())];
    let mut overlap = 0;
    loop {
        let read = reader.read(&mut buffer[overlap..])?;
        if read == 0 {
            return Ok(false);
        }
        let length = overlap + read;
        let window = &buffer[..length];
        if window_contains_marker(window, name)? {
            return Ok(true);
        }
        overlap = (name.len() - 1).min(length);
        buffer.copy_within(length - overlap..length, 0);
    }
}

fn window_contains_marker(bytes: &[u8], name: &[u8]) -> Result<bool> {
    // Both names start with "th". Skip to the less common second byte using the standard
    // library's optimized byte search, then compare the complete name.
    let mut rest = bytes;
    while !rest.is_empty() {
        rest.skip_until(name[1])?;
        let consumed = bytes.len() - rest.len();
        if consumed >= 2 && bytes[consumed - 2..].starts_with(name) {
            return Ok(true);
        }
    }
    let mut rest = bytes;
    while !rest.is_empty() {
        rest.skip_until(b'\\')?;
        if let Some(hex) = rest.strip_prefix(b"u00")
            && escapes_ascii_letter(hex.get(..2))
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn escapes_ascii_letter(hex: Option<&[u8]>) -> bool {
    hex.and_then(|hex| std::str::from_utf8(hex).ok())
        .and_then(|hex| u8::from_str_radix(hex, 16).ok())
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
}

fn push_record(builder: &mut EventBuilder, record: &Value, items: &mut CodexItems) -> bool {
    let timestamp = parse_timestamp(record.get("timestamp"));
    match record.get("type").and_then(Value::as_str) {
        Some("session_meta" | "turn_context") => {
            if let Some(payload) = record.get("payload") {
                push_codex_session_metadata(builder, payload, timestamp, false);
            }
        }
        Some("response_item") => {
            if let Some(payload) = record.get("payload") {
                return push_response_item(builder, payload, timestamp, items);
            }
        }
        Some("event_msg") => {
            if let Some(payload) = record.get("payload") {
                push_codex_session_metadata(builder, payload, timestamp, true);
                return push_event_message(builder, payload, timestamp, items);
            }
        }
        _ => {}
    }
    false
}

fn push_codex_session_metadata(
    builder: &mut EventBuilder,
    payload: &Value,
    timestamp: Option<DateTime<Utc>>,
    cumulative_usage: bool,
) {
    let model = string_at(
        payload,
        &[
            &["model"],
            &["model_id"],
            &["model_slug"],
            &["info", "model"],
        ],
    );
    let reasoning_mode = string_at(
        payload,
        &[
            &["effort"],
            &["reasoning_effort"],
            &["reasoning", "effort"],
            &["info", "reasoning_effort"],
        ],
    );
    let total_tokens = value_at(
        payload,
        &[
            &["total_tokens"],
            &["total_token_usage", "total_tokens"],
            &["info", "total_token_usage", "total_tokens"],
            &["usage", "total_tokens"],
        ],
    )
    .and_then(Value::as_u64);
    if model.is_none() && reasoning_mode.is_none() && total_tokens.is_none() {
        return;
    }
    builder.push(
        EventKind::ProviderEvent,
        json!({
            "model": model,
            "reasoning_mode": reasoning_mode,
            "total_tokens": total_tokens,
            "token_usage": if cumulative_usage { "cumulative" } else { "incremental" },
        }),
        timestamp,
        ReplayPolicy::HistoricalOnly,
        Some("omnisession.session_metadata".to_owned()),
        None,
    );
}

#[derive(Default)]
struct CodexItems {
    message: Option<VisibleMessage>,
    previous_message: Option<(u64, PreviousDisplay)>,
    mirrors: MessageMirrors,
    identity: MessageIdentity,
    cited_display: bool,
    plan_mode: bool,
    raw_proposal: Option<[u8; 32]>,
    in_turn: bool,
    update: Option<u64>,
    projected: Option<u64>,
    // A completed display item and a response item can describe the same tool result.
    outcomes: PendingOutcomes,
    tool_turn: Option<String>,
    tool_owner: Option<u64>,
    tool_events: usize,
}

struct VisibleMessage {
    kind: EventKind,
    sequence: u64,
}

fn is_implicit_turn(id: &str) -> bool {
    id.strip_prefix("rollout-")
        .is_some_and(|index| !index.is_empty() && index.bytes().all(|byte| byte.is_ascii_digit()))
}

impl CodexItems {
    fn can_publish_empty(&self, kind: &EventKind) -> bool {
        match kind {
            EventKind::MessageAssistant => self.identity.scoped(true).is_some(),
            EventKind::MessageUser => {
                self.update.is_some() && self.identity.scoped(false).is_some()
            }
            _ => false,
        }
    }

    fn finish_identified_item(&mut self) -> Option<u64> {
        self.tool_turn = None;
        self.tool_owner = None;
        self.update = None;
        self.identity = MessageIdentity::default();
        self.projected
    }

    fn clear_mirrors(&mut self) {
        self.message = None;
        self.mirrors.clear();
        self.outcomes = PendingOutcomes::default();
    }

    fn turn_boundary(&mut self, turn_id: Option<&str>) {
        if turn_id.is_some_and(is_implicit_turn) {
            // The migrator writes model records before synthesizing a display turn.
            self.mirrors.retain_unpaired_models();
            self.outcomes.retain_models();
        } else {
            self.clear_mirrors();
        }
    }

    fn project_identified(
        &mut self,
        builder: &mut EventBuilder,
        payload: &Value,
        turn: &str,
        id: &str,
        timestamp: Option<DateTime<Utc>>,
    ) -> bool {
        if payload["type"] != "item_completed" {
            return push_response_item(builder, payload, timestamp, self);
        }
        if !turn.is_empty()
            && !id.is_empty()
            && payload["item"]["type"] == "Plan"
            && let Some(text) = payload["item"]["text"].as_str()
        {
            self.push_plan(builder, turn, id, text, timestamp);
            return false;
        }
        push_completed_item(builder, &payload["item"], timestamp, self)
    }

    /// Codex displays a completed plan as a separate assistant item, even when
    /// another assistant message in the turn has the same text.
    fn push_plan(
        &mut self,
        builder: &mut EventBuilder,
        turn: &str,
        id: &str,
        text: &str,
        timestamp: Option<DateTime<Utc>>,
    ) {
        self.message = None;
        let payload = json!({"type": "Plan", "turn_id": turn, "id": id, "text": text});
        if let Some(seq) = self.update
            && let Some(previous) = builder.event_at_mut(seq)
        {
            self.previous_message = Some((
                seq,
                PreviousDisplay::replace_payload(
                    previous,
                    EventKind::MessageAssistant,
                    payload,
                    "event_msg.item_completed",
                ),
            ));
            self.projected = Some(seq);
        } else {
            self.projected = Some(builder.checkpoint());
            builder.push(
                EventKind::MessageAssistant,
                payload,
                timestamp,
                ReplayPolicy::Contextual,
                Some("event_msg.item_completed".to_owned()),
                None,
            );
        }
    }

    fn push_historical(
        &mut self,
        builder: &mut EventBuilder,
        kind: EventKind,
        mut payload: Value,
        timestamp: Option<DateTime<Utc>>,
        raw_type: String,
        result_id: Option<&str>,
    ) {
        self.message = None;
        if let Some(fields) = payload.as_object_mut() {
            for key in [
                "omnisession_codex_rollback",
                "omnisession_codex_checkpoint",
                "omnisession_codex_prior_display",
                "omnisession_codex_before_compaction",
                "omnisession_codex_positioned_reply",
                "omnisession_codex_checkpoint_position",
                "omnisession_codex_replacement",
                "omnisession_codex_paginated",
                "omnisession_codex_summary_withdrawn",
            ] {
                fields.remove(key);
            }
        }
        let display = raw_type == "event_msg.item_completed";
        let result_id = result_id.filter(|id| !id.is_empty());
        let matched = result_id.and_then(|id| {
            self.outcomes.take(
                id,
                display,
                self.update,
                self.tool_turn.as_deref(),
                self.tool_owner,
            )
        });
        let original = self.update.or(matched);
        if let Some(sequence) = original
            && let Some(previous) = builder.event_at_mut(sequence)
        {
            let has_model = previous
                .source
                .raw_record_type
                .as_deref()
                .is_some_and(|raw| raw.starts_with("response_item.") && raw.ends_with("_output"));
            let failed_display = previous.kind == EventKind::ToolFailed
                && (!has_model || model_result_kind(&previous.payload) != EventKind::ToolFailed);
            if display && has_model {
                // Codex shows a command's display output but gives the model the native result.
                previous.kind = if kind == EventKind::ToolFailed {
                    kind
                } else {
                    model_result_kind(&previous.payload)
                };
            } else {
                previous.kind = if !display && failed_display {
                    EventKind::ToolFailed
                } else {
                    kind
                };
                previous.payload = payload;
                previous.source.raw_record_type = Some(raw_type);
                previous.replay_policy = ReplayPolicy::HistoricalOnly;
            }
            self.projected = Some(sequence);
            return;
        }
        let sequence = builder.checkpoint();
        if let Some(id) = result_id {
            self.outcomes.push(
                id,
                display,
                sequence,
                self.tool_turn.clone(),
                self.tool_owner,
            );
        }
        self.projected = Some(sequence);
        if kind != EventKind::PlanUpdated {
            self.tool_events += 1;
        }
        builder.push(
            kind,
            payload,
            timestamp,
            ReplayPolicy::HistoricalOnly,
            Some(raw_type),
            None,
        );
    }
}

fn push_completed_item(
    builder: &mut EventBuilder,
    item: &Value,
    timestamp: Option<DateTime<Utc>>,
    items: &mut CodexItems,
) -> bool {
    let kind = match item["type"].as_str() {
        Some("UserMessage") => EventKind::MessageUser,
        Some("AgentMessage") => EventKind::MessageAssistant,
        _ => {
            if let Some((kind, payload)) = completed_item::historical_item(item) {
                let id = (kind != EventKind::PlanUpdated)
                    .then(|| item["id"].as_str())
                    .flatten();
                items.push_historical(
                    builder,
                    kind,
                    payload,
                    timestamp,
                    "event_msg.item_completed".to_owned(),
                    id,
                );
            }
            return false;
        }
    };
    let Some(content) = item["content"].as_array() else {
        return false;
    };
    let parts = content
        .iter()
        .filter_map(|part| match kind {
            EventKind::MessageAssistant => completed_item::assistant::text(part),
            EventKind::MessageUser if part["type"] == "text" => part["text"].as_str(),
            _ => None,
        })
        .collect::<Vec<_>>();
    let contextual = parts.iter().any(|part| is_contextual_user_text(part));
    let alternate = (parts.len() > 1).then(|| parts.join("\n"));
    let text = omitted_input_media(&kind, &parts, content).unwrap_or_else(|| parts.concat());
    if text.is_empty() && !items.can_publish_empty(&kind) {
        return false;
    }
    let attachment = (kind == EventKind::MessageUser)
        .then(|| attachments::completed(content, &parts))
        .flatten();
    push_message_text(
        builder,
        kind,
        &text,
        timestamp,
        "event_msg.item_completed",
        MessageMatch {
            alternate: alternate.as_deref(),
            attachment,
            ..MessageMatch::default()
        },
        items,
    ) && !contextual
}

fn push_message_text<'a>(
    builder: &mut EventBuilder,
    kind: EventKind,
    text: &str,
    timestamp: Option<DateTime<Utc>>,
    raw_type: &'static str,
    matching: impl Into<MessageMatch<'a>>,
    items: &mut CodexItems,
) -> bool {
    let mut matching = matching.into();
    matching.identity = items.identity;
    let plan_text = (kind == EventKind::MessageAssistant
        && raw_type == "response_item.message"
        && items.plan_mode)
        .then_some(matching.alternate.unwrap_or(text));
    items.raw_proposal = plan_text.and_then(display_text::proposal_digest);
    let plan_display = plan_text.and_then(display_text::without_plan);
    let text = if let Some(visible) = &plan_display {
        // Plan-only replies have a dedicated completed Plan item and no assistant message.
        if visible.trim().is_empty() {
            return false;
        }
        matching.alternate = None;
        visible.as_str()
    } else {
        text
    };
    if kind == EventKind::MessageAssistant {
        matching.citation = if raw_type == "response_item.message" {
            message_mirrors::model_citation_spelling(matching.alternate.unwrap_or(text))
        } else {
            items
                .cited_display
                .then(|| message_mirrors::display_citation_spelling(text))
        };
    }
    let user_turn = kind == EventKind::MessageUser && !is_contextual_user_text(text);
    if text.is_empty()
        && !(raw_type == "event_msg.item_completed" && items.can_publish_empty(&kind))
    {
        return user_turn;
    }
    if !items.in_turn {
        // Without a named turn, only adjacent content can identify a mirror.
        matching.adjacent = Some(
            items
                .message
                .as_ref()
                .map_or_else(|| builder.checkpoint(), |message| message.sequence),
        );
    }
    let update = items
        .update
        .or_else(|| items.mirrors.native_match(&kind, items.identity, raw_type))
        .filter(|seq| builder.event_at_mut(*seq).is_some());
    let matched = if let Some(sequence) = update {
        items.mirrors.update(
            &kind,
            text,
            matching,
            raw_type,
            sequence,
            builder.checkpoint(),
        );
        Some(sequence)
    } else {
        items
            .mirrors
            .observe(&kind, text, matching, raw_type, builder.checkpoint())
    };
    let sequence = matched.unwrap_or_else(|| builder.checkpoint());
    items.message = Some(VisibleMessage {
        kind: kind.clone(),
        sequence,
    });
    items.projected = Some(sequence);
    if let Some(old) = matched.and_then(|seq| builder.event_at_mut(seq))
        && (update.is_some() || raw_type != "response_item.message")
    {
        items.previous_message = PreviousDisplay::replace_message(old, kind, text, raw_type)
            .map(|previous| (sequence, previous));
        return false;
    }
    if matched.is_some() {
        return false;
    }
    builder.push(
        kind,
        json!({ "text": text }),
        timestamp,
        ReplayPolicy::Contextual,
        Some(raw_type.to_owned()),
        None,
    );
    user_turn
}

fn is_contextual_user_text(text: &str) -> bool {
    let text = text.trim_start();
    [
        "# AGENTS.md instructions for ",
        "<environment_context>",
        "<turn_aborted>",
        "<user_shell_command>",
        "<subagent_notification>",
    ]
    .iter()
    .any(|prefix| text.starts_with(prefix))
}

fn hook_attribution(metadata: &Value, index: usize) -> Option<bool> {
    let source = &metadata["content_item_metadata"][index]["provenance"];
    match source["type"].as_str() {
        Some("user") => return Some(false),
        Some("harness") if source["hook"] == true => return Some(true),
        _ => {}
    }
    match metadata["content_item_kinds"][index].as_str() {
        Some("user.text") => Some(false),
        Some("hooks.additional_context") => Some(true),
        _ => None,
    }
}

/// Codex serializes each stop-hook fragment as one XML element with a run ID and escaped text.
fn is_hook_fragment(text: &str) -> bool {
    let Some(fragment) = text.trim().strip_prefix("<hook_prompt") else {
        return false;
    };
    let Some((attribute, body)) = fragment.split_once('>') else {
        return false;
    };
    let Some((name, value)) = attribute.split_once('=') else {
        return false;
    };
    let mut value = value.trim().chars();
    let Some(quote @ ('\'' | '"')) = value.next() else {
        return false;
    };
    attribute.starts_with(char::is_whitespace)
        && name.trim() == "hook_run_id"
        && value
            .as_str()
            .strip_suffix(quote)
            .is_some_and(|id| !id.trim().is_empty() && !id.contains([quote, '<']))
        && body
            .strip_suffix("</hook_prompt>")
            .is_some_and(|body| !body.contains('<'))
}

/// Codex adds framing text around attachments when it builds the model input.
fn message_parts(content: &[Value], mut keep: impl FnMut(usize, &str) -> bool) -> Vec<&str> {
    let part_type = |index: Option<usize>| {
        index
            .and_then(|index| content.get(index))
            .and_then(|part| part.get("type"))
            .and_then(Value::as_str)
    };
    content
        .iter()
        .enumerate()
        .filter(|(index, _)| {
            matches!(
                part_type(Some(*index)),
                Some("input_text" | "output_text" | "text")
            )
        })
        .filter_map(|(index, part)| Some((index, part.get("text").and_then(Value::as_str)?)))
        .filter(|(index, text)| {
            keep(*index, text)
                && !is_media_wrapper(
                    text,
                    part_type(index.checked_sub(1)),
                    part_type(Some(index + 1)),
                )
        })
        .map(|(_, text)| text)
        .collect()
}

fn omitted_input_media(kind: &EventKind, parts: &[&str], content: &[Value]) -> Option<String> {
    if *kind != EventKind::MessageUser || parts.iter().any(|part| !part.trim().is_empty()) {
        return None;
    }
    let images = content
        .iter()
        .filter(|part| {
            matches!(
                part["type"].as_str(),
                Some("input_image" | "image" | "local_image")
            )
        })
        .count();
    let audio = content.iter().any(|part| {
        matches!(
            part["type"].as_str(),
            Some("input_audio" | "audio" | "local_audio")
        )
    });
    omitted_media_text(images, audio)
}

fn omitted_media_text(images: usize, audio: bool) -> Option<String> {
    match (images, audio) {
        (0, false) => None,
        (0, true) => Some("[audio omitted]".to_owned()),
        (_, false) => Some(omitted_images_text(images)),
        (_, true) => Some(format!("{} [audio omitted]", omitted_images_text(images))),
    }
}

// Before per-part attribution, a hook message contained only hook fragments and contextual input.
fn legacy_hook_context(payload: &Value, content: &[Value], metadata: &Value) -> bool {
    !payload["id"]
        .as_str()
        .is_some_and(|id| id.starts_with("msg_input_"))
        && content.iter().enumerate().all(|(index, part)| {
            part["type"] == "input_text"
                && part["text"].as_str().is_some_and(|text| {
                    hook_attribution(metadata, index).unwrap_or_else(|| is_hook_fragment(text))
                        || is_contextual_user_text(text)
                })
        })
}

fn push_response_item(
    builder: &mut EventBuilder,
    payload: &Value,
    timestamp: Option<DateTime<Utc>>,
    items: &mut CodexItems,
) -> bool {
    let item_type = payload.get("type").and_then(Value::as_str).unwrap_or("");
    if paginated_rollback::is_private_instruction(payload) {
        return false;
    }
    if item_type == "message" {
        let kind = match payload.get("role").and_then(Value::as_str) {
            Some("user") => Some(EventKind::MessageUser),
            Some("assistant") => Some(EventKind::MessageAssistant),
            _ => None,
        };
        let Some(kind) = kind else {
            return false;
        };
        if let Some(text) = payload.get("content").and_then(Value::as_str) {
            return push_message_text(
                builder,
                kind,
                text,
                timestamp,
                "response_item.message",
                None,
                items,
            );
        }
        let content = payload["content"].as_array().map_or(&[][..], Vec::as_slice);
        let metadata = &payload["internal_chat_message_metadata_passthrough"];
        let user = kind == EventKind::MessageUser;
        let hook_context = user && legacy_hook_context(payload, content, metadata);
        let mut omitted_hook = false;
        let mut inferred_hook = false;
        let parts = message_parts(content, |index, text| {
            let attribution = hook_attribution(metadata, index);
            let hook = user
                && content[index]["type"] == "input_text"
                && attribution.unwrap_or_else(|| hook_context && is_hook_fragment(text));
            omitted_hook |= hook;
            inferred_hook |= hook && attribution.is_none();
            !hook
        });
        if inferred_hook {
            builder.push(
                EventKind::ProviderEvent,
                json!({"content_omitted": true, "reason": "Legacy hook input has no user attribution."}),
                timestamp,
                ReplayPolicy::HistoricalOnly,
                Some("omnisession.codex_hook_input_omitted".to_owned()),
                None,
            );
        }
        let contextual = parts.iter().any(|text| is_contextual_user_text(text));
        let text = omitted_input_media(&kind, &parts, content).unwrap_or_else(|| parts.join("\n"));
        if omitted_hook && text.is_empty() {
            return false;
        }
        // Codex joins the original text parts without separators in display events.
        let alternate = (parts.len() > 1).then(|| parts.concat());
        let attachment = (kind == EventKind::MessageUser)
            .then(|| attachments::model(content, &parts))
            .flatten();
        return push_message_text(
            builder,
            kind,
            &text,
            timestamp,
            "response_item.message",
            MessageMatch {
                alternate: alternate.as_deref(),
                attachment,
                ..MessageMatch::default()
            },
            items,
        ) && !contextual;
    }

    let Some(kind) = model_tool_kind(item_type, payload) else {
        return false;
    };
    let result_id = model_result_id(payload);
    items.push_historical(
        builder,
        kind,
        compact_tool_value(payload, None),
        timestamp,
        format!("response_item.{item_type}"),
        result_id,
    );
    false
}

fn model_tool_kind(item_type: &str, payload: &Value) -> Option<EventKind> {
    match item_type {
        "function_call" | "custom_tool_call" | "web_search_call" | "computer_call" => {
            Some(EventKind::ToolCalled)
        }
        "local_shell_call" => Some(EventKind::CommandExecuted),
        "function_call_output"
        | "custom_tool_call_output"
        | "local_shell_call_output"
        | "computer_call_output" => Some(model_result_kind(payload)),
        _ => None,
    }
}

fn model_call_id(payload: &Value) -> Option<&str> {
    match payload["type"].as_str()? {
        "function_call" | "custom_tool_call" | "web_search_call" | "computer_call"
        | "local_shell_call" => {
            string_at(payload, &[&["call_id"], &["id"]]).filter(|id| !id.is_empty())
        }
        _ => None,
    }
}

fn model_result_id(payload: &Value) -> Option<&str> {
    match payload["type"].as_str()? {
        "function_call_output"
        | "custom_tool_call_output"
        | "local_shell_call_output"
        | "computer_call_output" => {
            string_at(payload, &[&["call_id"], &["id"]]).filter(|id| !id.is_empty())
        }
        _ => None,
    }
}

fn model_result_kind(payload: &Value) -> EventKind {
    if matches!(payload["status"].as_str(), Some("failed" | "error")) {
        EventKind::ToolFailed
    } else {
        EventKind::ToolCompleted
    }
}

fn compact_tool_value(value: &Value, field: Option<&str>) -> Value {
    if field.is_some_and(|field| {
        matches!(
            field,
            "image_url"
                | "data"
                | "blob"
                | "bytes"
                | "encrypted_content"
                | "encrypted_function_args"
                | "internal_chat_message_metadata_passthrough"
        )
    }) {
        let mut omitted = Map::from_iter([("content_omitted".to_owned(), Value::Bool(true))]);
        if let Some(value) = value.as_str() {
            omitted.insert("original_bytes".to_owned(), Value::from(value.len()));
        } else if let Some(value) = value.as_array() {
            omitted.insert("original_items".to_owned(), Value::from(value.len()));
        }
        return Value::Object(omitted);
    }
    match value {
        Value::String(value) => Value::String(compact_tool_string(value)),
        Value::Array(values) if values.len() > MAX_TOOL_ARRAY_ITEMS => {
            let edge = MAX_TOOL_ARRAY_ITEMS / 2;
            let mut compact = values
                .iter()
                .take(edge)
                .map(|value| compact_tool_value(value, None))
                .collect::<Vec<_>>();
            compact.push(json!({
                "content_omitted": true,
                "omitted_items": values.len() - MAX_TOOL_ARRAY_ITEMS,
            }));
            compact.extend(
                values
                    .iter()
                    .skip(values.len() - edge)
                    .map(|value| compact_tool_value(value, None)),
            );
            Value::Array(compact)
        }
        Value::Array(values) => Value::Array(
            values
                .iter()
                .map(|value| compact_tool_value(value, None))
                .collect(),
        ),
        Value::Object(values) => Value::Object(
            values
                .iter()
                .map(|(field, value)| (field.clone(), compact_tool_value(value, Some(field))))
                .collect::<Map<_, _>>(),
        ),
        _ => value.clone(),
    }
}

fn compact_tool_string(value: &str) -> String {
    let character_count = value.chars().count();
    if character_count <= MAX_TOOL_STRING_CHARACTERS {
        return value.to_owned();
    }
    let edge = MAX_TOOL_STRING_CHARACTERS / 2;
    let prefix = value.chars().take(edge).collect::<String>();
    let suffix = value
        .chars()
        .rev()
        .take(edge)
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>();
    format!(
        "{prefix}\n[{} characters omitted]\n{suffix}",
        character_count - MAX_TOOL_STRING_CHARACTERS
    )
}

fn push_event_message(
    builder: &mut EventBuilder,
    payload: &Value,
    timestamp: Option<DateTime<Utc>>,
    items: &mut CodexItems,
) -> bool {
    let event_type = payload.get("type").and_then(Value::as_str).unwrap_or("");
    if event_type == "item_completed" {
        return push_completed_item(builder, &payload["item"], timestamp, items);
    }
    let kind = match event_type {
        "user_message" => Some(EventKind::MessageUser),
        "agent_message" => Some(EventKind::MessageAssistant),
        _ => None,
    };
    let Some(kind) = kind else {
        return false;
    };
    let text = string_at(payload, &[&["message"], &["text"]]).unwrap_or("");
    // The import notification has no response item. A literal reply with the same
    // text has one, so let the response item supply its role and content.
    if kind == EventKind::MessageAssistant && text == omnis_core::CODEX_IMPORT_MARKER {
        return false;
    }
    let placeholder = if text.trim().is_empty() && kind == EventKind::MessageUser {
        let images = ["images", "file_ids", "local_images"]
            .into_iter()
            .filter_map(|field| payload.get(field).and_then(Value::as_array))
            .map(Vec::len)
            .sum();
        let audio = ["audio", "audio_references", "local_audio"]
            .into_iter()
            .any(|field| {
                payload[field]
                    .as_array()
                    .is_some_and(|audio| !audio.is_empty())
            });
        omitted_media_text(images, audio)
    } else {
        None
    };
    let attachment = (kind == EventKind::MessageUser)
        .then(|| attachments::legacy(payload, text))
        .flatten();
    push_message_text(
        builder,
        kind,
        placeholder.as_deref().unwrap_or(text),
        timestamp,
        "event_msg",
        MessageMatch {
            attachment,
            ..MessageMatch::default()
        },
        items,
    )
}

/// A tag typed without its matching attachment stays visible.
fn is_media_wrapper(text: &str, before: Option<&str>, after: Option<&str>) -> bool {
    if matches!(
        (text, before),
        ("</image>", Some("input_image")) | ("</audio>", Some("input_audio"))
    ) {
        return true;
    }
    match after {
        Some("input_image") => {
            text == "<image>" || (text.starts_with("<image name=") && text.ends_with('>'))
        }
        Some("input_audio") => {
            text == "<audio>" || (text.starts_with("<audio name=") && text.ends_with('>'))
        }
        _ => false,
    }
}

impl ProviderAdapter for CodexAdapter {
    fn provider(&self) -> Provider {
        Provider::Codex
    }

    fn fork_candidates_created_since(
        &self,
        source: &SessionRef,
        project: &Path,
        started_at: DateTime<Utc>,
    ) -> Result<Vec<SessionRef>> {
        Self::fork_candidates_created_since(self, source, project, started_at)
    }

    fn probe(&self) -> ProviderInstallation {
        let executable = provider_executable(Provider::Codex);
        ProviderInstallation {
            provider: Provider::Codex,
            installed: executable.is_some() || self.codex_home.as_deref().is_some_and(Path::is_dir),
            executable,
            data_root: self.codex_home.clone(),
        }
    }

    fn discovery_notes(&self) -> Vec<String> {
        self.listing().notes.clone()
    }

    fn discovery_is_complete(&self) -> bool {
        // A missing Codex home is not evidence that previously cached sessions were deleted.
        self.codex_home.as_deref().is_some_and(Path::is_dir) && self.listing().notes.is_empty()
    }

    fn list_sessions(&self, project: Option<&Path>) -> Result<Vec<NativeSession>> {
        let catalog = self.workspace_catalog.as_deref();
        let raw_sessions = self.sessions();
        if raw_sessions.is_empty() {
            return Ok(Vec::new());
        }
        let observations = catalog.map(WorkspaceCatalog::list).unwrap_or_default();
        let mut sessions = Vec::new();
        for raw in raw_sessions {
            let mut session = raw.native_session();
            if let Some((catalog, observation)) = catalog.zip(observations.get(&raw.id)) {
                catalog.apply(raw, &mut session, observation);
            }
            if project.is_some_and(|project| {
                session
                    .project_path
                    .as_deref()
                    .is_none_or(|directory| !paths_match(directory, project))
            }) {
                continue;
            }
            sessions.push(session);
        }
        sort_sessions(&mut sessions);
        Ok(sessions)
    }

    /// Reads the one matching rollout, so subagent threads that listings hide stay reachable.
    fn find_native_session(&self, id: &str) -> Result<Option<NativeSession>> {
        if Uuid::parse_str(id).is_err() {
            return Ok(None);
        }
        let Some(path) = self.session_path(id) else {
            return Ok(None);
        };
        let Some(mut session) = CodexSession::parse_metadata_path_result(&path)? else {
            return Ok(None);
        };
        session.title = self.title_index().titles.get(id).cloned();
        let mut native = session.native_session();
        if let Some(catalog) = self.workspace_catalog.as_deref()
            && let Some(observation) = catalog.find(id)
        {
            catalog.apply(&session, &mut native, &observation);
        }
        Ok(Some(native))
    }

    fn read_session(&self, session: &SessionRef) -> Result<omnis_ir::CanonicalSnapshot> {
        validate_provider(session, Provider::Codex)?;
        let native = self.find_session(&session.id)?;
        let captured_at = native.updated_at.unwrap_or_else(Utc::now);
        let (events, project_path) = native.canonical_events()?;
        Ok(native.snapshot(
            self.workspace_catalog.as_deref(),
            session,
            events,
            project_path,
            captured_at,
        ))
    }

    fn preview_session(&self, session: &SessionRef) -> Result<omnis_ir::CanonicalSnapshot> {
        const PREVIEW_EVENTS: usize = 1_024;
        validate_provider(session, Provider::Codex)?;
        let native = self.find_session_metadata(&session.id)?;
        let captured_at = native.updated_at.unwrap_or_else(Utc::now);
        // A sampled preview can miss a directory change between its prefix and tail.
        let workspace = native.workspace_metadata(true)?;
        if workspace.rolled_back {
            let (mut events, project_path) = native.canonical_events()?;
            events.retain_preview_events(PREVIEW_EVENTS, captured_at);
            return Ok(native.snapshot(
                self.workspace_catalog.as_deref(),
                session,
                events,
                project_path,
                captured_at,
            ));
        }
        if let Some(records) = workspace.recent_records {
            let events = native.recent_preview(records)?;
            return Ok(native.snapshot(
                self.workspace_catalog.as_deref(),
                session,
                events,
                workspace.directory,
                captured_at,
            ));
        }
        let events = native.preview_events()?;
        Ok(native.snapshot(
            self.workspace_catalog.as_deref(),
            session,
            events,
            workspace.directory,
            captured_at,
        ))
    }

    fn native_workspace(&self, session: &SessionRef) -> Result<Option<NativeWorkspace>> {
        validate_provider(session, Provider::Codex)?;
        let native = self.find_session_metadata(&session.id)?;
        #[cfg(unix)]
        if native
            .stamp
            .is_some_and(|stamp| stamp.len > MAX_PREVIEW_TAIL_SIZE)
            && let Some(observation) = self
                .workspace_catalog
                .as_ref()
                .and_then(|catalog| catalog.verified_workspace(&native))
        {
            return Ok(Some(NativeWorkspace {
                git: native.git_at(observation.project_path.as_deref()),
                directory: observation.project_path,
                startup_directory: None,
            }));
        }
        let workspace = native.workspace_metadata(false)?;
        let directory = if workspace.rolled_back && !workspace.settings.has_owned() {
            native.canonical_events()?.1
        } else {
            workspace.directory
        };
        let git = native.git_at(directory.as_deref());
        if let Some(catalog) = self.workspace_catalog.as_deref() {
            catalog.record(&native, directory.as_deref(), git.branch.as_deref());
        }
        Ok(Some(NativeWorkspace {
            directory,
            startup_directory: workspace.settings.startup,
            git,
        }))
    }

    fn new_session_plan(&self, target: &LaunchTarget) -> Result<LaunchPlan> {
        Ok(LaunchPlan {
            program: "codex".to_owned(),
            args: target.prompt.iter().cloned().collect(),
            cwd: target.cwd.clone(),
            env: Vec::new(),
        })
    }

    fn launch_plan(&self, session: &SessionRef, target: &LaunchTarget) -> Result<LaunchPlan> {
        validate_provider(session, Provider::Codex)?;
        Uuid::parse_str(&session.id).context("Codex session ID must be a UUID")?;
        let mut args = vec![
            if target.fork { "fork" } else { "resume" }.to_owned(),
            session.id.clone(),
        ];
        if let Some(prompt) = &target.prompt {
            args.push(prompt.clone());
        }
        Ok(LaunchPlan {
            program: "codex".to_owned(),
            args,
            cwd: target.cwd.clone(),
            env: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;
    use proptest::prelude::*;

    #[test]
    fn unannotated_local_summary_uses_the_codex_prompt_prefix() {
        let text = format!("{LOCAL_SUMMARY_PREFIX}\nRetain the latest question.");
        let user =
            json!({"type":"message", "role":"user", "content":[{"type":"input_text","text":text}]});
        assert_eq!(replacement_summary(&user, Some(&text)), Some(text.as_str()));
        let mut ordinary = user;
        ordinary["internal_chat_message_metadata_passthrough"] =
            json!({"content_item_kinds":["user.text"]});
        assert_eq!(replacement_summary(&ordinary, Some(&text)), None);
    }

    #[test]
    fn scan_session_files_reports_truncation_and_unreadable_roots() {
        let temporary = tempfile::tempdir().expect("temporary Codex home");
        let day = temporary.path().join("sessions/2026/09/09");
        fs::create_dir_all(&day).expect("session day");
        for index in 0..3 {
            fs::write(day.join(format!("rollout-{index}.jsonl")), "{}\n").expect("jsonl fixture");
        }
        let scan = scan_session_files(temporary.path(), 2);
        assert_eq!(scan.files.len(), 2);
        assert!(
            scan.notes
                .iter()
                .any(|note| note.contains("stopped after 2 newest jsonl files"))
        );

        let missing = scan_session_files(&temporary.path().join("missing-home"), 10);
        assert!(missing.files.is_empty());
        assert!(missing.notes.is_empty());

        let deep = day.join("one/two/three");
        fs::create_dir_all(&deep).expect("depth-limited directory");
        fs::write(deep.join("rollout-deep.jsonl"), "{}\n").expect("deep fixture");
        let depth_limited = scan_session_files(temporary.path(), 10);
        assert!(
            depth_limited
                .notes
                .iter()
                .any(|note| note.contains("scan depth limit"))
        );

        let blocked = tempfile::tempdir().expect("blocked Codex home");
        fs::write(blocked.path().join("sessions"), b"x")
            .expect("file posing as sessions directory");
        let unreadable = scan_session_files(blocked.path(), 10);
        assert!(unreadable.files.is_empty());
        assert!(
            unreadable
                .notes
                .iter()
                .any(|note| note.contains("errors while reading session directories")),
            "{:?}",
            unreadable.notes
        );
    }

    #[test]
    fn scan_session_files_keeps_reverse_lexical_top_files_across_empty_directories() {
        let temporary = tempfile::tempdir().expect("temporary Codex home");
        let day = temporary.path().join("sessions/2026/10/06");
        let nested = day.join("zz-nested");
        fs::create_dir_all(&nested).expect("nested directory");
        fs::create_dir(day.join("zz-empty")).expect("empty directory");
        fs::write(nested.join("rollout-inside.jsonl"), "{}\n").expect("nested fixture");
        let older_day = temporary.path().join("sessions/2026/10/05");
        fs::create_dir_all(&older_day).expect("older session day");
        fs::write(older_day.join("rollout-older.jsonl"), "{}\n").expect("older fixture");
        for name in ["a", "b", "c", "d", "e", "f"] {
            fs::write(day.join(format!("rollout-{name}.jsonl")), "{}\n").expect("rollout fixture");
        }
        fs::write(day.join("zz-unrelated.tmp"), "").expect("unrelated file");

        let mut expected = vec![
            nested.join("rollout-inside.jsonl"),
            older_day.join("rollout-older.jsonl"),
        ];
        expected.extend(
            (b'a'..=b'f').map(|name| day.join(format!("rollout-{}.jsonl", char::from(name)))),
        );
        expected.sort_unstable_by(|left, right| right.cmp(left));
        for limit in 1..=expected.len() {
            let scan = scan_session_files(temporary.path(), limit);
            assert_eq!(
                scan.files,
                expected.iter().take(limit).cloned().collect::<Vec<_>>(),
                "selection differs at limit {limit}"
            );
        }
    }

    #[test]
    fn listing_reuses_cached_rollout_headers_until_the_rollout_changes() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let home = temporary.path().join("codex");
        let day = home.join("sessions/2026/09/15");
        fs::create_dir_all(&day).expect("session day");
        let id = "019a3b1c-0000-7000-8000-000000000001";
        let rollout = day.join(format!("rollout-2026-09-15T10-00-00-{id}.jsonl"));
        let write_rollout = |cwd: &str| {
            fs::write(
                &rollout,
                format!(
                    "{{\"timestamp\":\"2026-09-15T10:00:00Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"{id}\",\"cwd\":\"{cwd}\"}}}}\n"
                ),
            )
            .expect("rollout");
        };
        let cache = temporary.path().join("state/cache/codex-rollouts.json");
        let listed_projects = || {
            let mut adapter = CodexAdapter::with_root(&home);
            adapter.rollout_cache = Some(cache.clone());
            adapter
                .sessions()
                .iter()
                .map(|session| session.project_path.clone())
                .collect::<Vec<_>>()
        };

        write_rollout("/workspace/original");
        assert_eq!(
            listed_projects(),
            [Some(PathBuf::from("/workspace/original"))]
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mode = |path: &Path| {
                fs::metadata(path)
                    .expect("cache metadata")
                    .permissions()
                    .mode()
                    & 0o777
            };
            assert_eq!(mode(&cache), 0o600);
            assert_eq!(mode(cache.parent().expect("cache directory")), 0o700);
        }

        // Unchanged rollouts are not read again, so an edited cache entry shows through.
        let cached = fs::read_to_string(&cache).expect("cache file");
        fs::write(
            &cache,
            cached.replace("/workspace/original", "/workspace/cached"),
        )
        .expect("edited cache");
        assert_eq!(
            listed_projects(),
            [Some(PathBuf::from("/workspace/cached"))]
        );

        // A cache written by another parser build is ignored.
        let mut stale: Value =
            serde_json::from_slice(&fs::read(&cache).expect("cache file")).expect("cache JSON");
        stale["format"] = json!("0-stale");
        fs::write(
            &cache,
            serde_json::to_vec(&stale).expect("stale cache JSON"),
        )
        .expect("stale cache");
        assert_eq!(
            listed_projects(),
            [Some(PathBuf::from("/workspace/original"))]
        );

        write_rollout("/workspace/renamed-longer");
        assert_eq!(
            listed_projects(),
            [Some(PathBuf::from("/workspace/renamed-longer"))]
        );
    }

    #[test]
    fn rollback_scan_ignores_unicode_escapes() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let escaped = temporary.path().join("escaped.jsonl");
        fs::write(
            &escaped,
            "{\"type\":\"event_msg\",\"payload\":{\"text\":\"\\u001b[31mred\\u001b[0m\"}}\n",
        )
        .expect("escaped rollout");
        assert!(!contains_rollback_marker(&escaped).expect("scan escaped rollout"));

        let rolled_back = temporary.path().join("rolled-back.jsonl");
        fs::write(
            &rolled_back,
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"thread_rolled_back\",\"num_turns\":1}}\n",
        )
        .expect("rolled back rollout");
        assert!(contains_rollback_marker(&rolled_back).expect("scan rolled back rollout"));

        let escaped_marker = temporary.path().join("escaped-marker.jsonl");
        fs::write(
            &escaped_marker,
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"thread_rolled_\\u0062ack\"}}\n",
        )
        .expect("escaped marker rollout");
        assert!(contains_rollback_marker(&escaped_marker).expect("scan escaped marker rollout"));
    }

    #[test]
    fn path_uuid_ignores_stems_that_end_mid_character() {
        let mid_character = format!("{}a.jsonl", "\u{e9}".repeat(18));
        assert_eq!(path_uuid(Path::new(&mid_character)), None);
        assert_eq!(
            path_uuid(Path::new(
                "rollout-2026-01-01T00-00-00-11111111-1111-4111-8111-111111111111.jsonl"
            )),
            Some("11111111-1111-4111-8111-111111111111".to_owned())
        );
    }

    #[test]
    fn listing_notes_describe_empty_and_partial_scans() {
        let scan = CodexFileScan {
            files: vec![PathBuf::from("a.jsonl"), PathBuf::from("b.jsonl")],
            notes: vec![
                "Codex scan stopped after 2 newest jsonl files; older sessions are omitted."
                    .to_owned(),
            ],
        };
        let notes = listing_notes(&scan, 0, 1, 1, 0, Some("permission denied"));
        assert!(notes.iter().any(|note| note.contains("stopped after 2")));
        assert!(
            notes
                .iter()
                .any(|note| note.contains("unreadable session file(s): permission denied"))
        );
        assert!(
            notes
                .iter()
                .any(|note| note.contains("listed 0 user sessions") && note.contains("1 subagent"))
        );

        let readable = listing_notes(&scan, 4, 0, 0, 0, None);
        assert_eq!(readable.len(), 1);
        assert!(readable[0].contains("stopped after 2"));
    }

    #[test]
    fn compact_discovery_error_keeps_the_first_line() {
        assert_eq!(
            compact_discovery_error(&anyhow!("first line\nsecond line")),
            "first line"
        );
    }

    fn naive_rollback_marker(bytes: &[u8]) -> bool {
        const MARKER: &[u8] = b"thread_rolled_back";
        bytes.windows(MARKER.len()).any(|window| window == MARKER)
            || bytes.windows(6).any(|window| {
                window.starts_with(b"\\u00")
                    && std::str::from_utf8(&window[4..])
                        .ok()
                        .and_then(|hex| u8::from_str_radix(hex, 16).ok())
                        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
            })
    }

    /// JSON unicode escape of one byte, e.g. the escaped `b` a writer may emit.
    fn json_escape(byte: u8) -> String {
        format!("\\u{byte:04x}")
    }

    fn rollback_filler() -> impl Strategy<Value = u8> {
        prop_oneof![
            8 => b'a'..=b'z',
            1 => Just(b'\\'),
            1 => Just(b'u'),
            1 => Just(b'0'),
            1 => Just(b'_'),
            1 => Just(b'"'),
            1 => Just(b'\n'),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(128))]

        #[test]
        fn path_uuid_never_panics_on_arbitrary_stems(
            stem in any::<String>(),
            extension in "[a-z]{0,6}",
        ) {
            let _ = path_uuid(Path::new(&format!("{stem}.{extension}")));
            let _ = path_uuid(Path::new(&stem));
        }

        #[test]
        fn path_uuid_extracts_a_trailing_uuid(prefix in "[^/\\\\.]{0,24}", id in any::<u128>()) {
            let id = Uuid::from_u128(id).to_string();
            prop_assert_eq!(
                path_uuid(Path::new(&format!("rollout-{prefix}{id}.jsonl"))),
                Some(id)
            );
        }

        #[test]
        fn rollback_scan_matches_naive_search_across_chunk_boundaries(
            filler in proptest::collection::vec(rollback_filler(), 0..160),
            marker in prop_oneof![
                Just(String::new()),
                Just("thread_rolled_back".to_owned()),
                Just(format!("thread_rolled_{}ack", json_escape(b'b'))),
                Just(format!("{}hread_rolled_back", json_escape(b't'))),
                Just(format!("{}[31m", json_escape(0x1b))),
            ],
            chunk in 18_usize..48,
            window in 0_usize..4,
            shift in 0_usize..24,
            random_offset in any::<proptest::sample::Index>(),
            near_boundary in any::<bool>(),
        ) {
            // After the first window, each read advances by `chunk` minus the 17-byte overlap.
            let boundary = chunk + window * (chunk - 17);
            let offset = if near_boundary {
                boundary.saturating_sub(shift).min(filler.len())
            } else {
                random_offset.index(filler.len() + 1)
            };
            let bytes = [&filler[..offset], marker.as_bytes(), &filler[offset..]].concat();
            let temporary = tempfile::tempdir().expect("temporary directory");
            let path = temporary.path().join("rollout.jsonl");
            fs::write(&path, &bytes).expect("synthetic rollout");

            prop_assert_eq!(
                contains_workspace_marker(&path, b"thread_rolled_back", chunk).expect("chunked scan"),
                naive_rollback_marker(&bytes)
            );
        }
    }
}
