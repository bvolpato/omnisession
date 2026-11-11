mod branch;
mod copies;
mod empty_cutoff;
mod input;
mod markers;
mod membership;

use membership::{Members, Paths};

use std::{
    collections::{HashMap, HashSet},
    fs::File,
    io::{self, BufRead, BufReader, Read, Seek, SeekFrom},
    ops::Range,
    path::Path,
    path::PathBuf,
    sync::{Arc, LazyLock, Mutex, OnceLock, PoisonError},
    time::SystemTime,
};

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use memchr::memmem;
use omnis_ir::{EventKind, Provider, ReplayPolicy, SessionRef};
use serde::Serializer as _;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    LaunchPlan, LaunchTarget, NativeSession, ProviderAdapter, ProviderInstallation,
    support::{
        EventBuilder, ProviderFiles, nested_files_matching, omitted_images_text, parse_timestamp,
        paths_match, provider_executable, provider_file, provider_root, sample_files_concurrently,
        sort_sessions, string_at, validate_provider, value_at, visit_index_json_lines,
    },
};

const MAX_METADATA_RECORDS: usize = 32;
const MAX_METADATA_BYTES: u64 = 2 * 1024 * 1024;
// Legacy transcripts start with `summary` records. Current ones append title records near the end.
const TITLE_HEAD_BYTES: usize = 16 * 1024;
const TITLE_TAIL_BYTES: usize = 64 * 1024;
const MAX_PROMPT_TITLE_CHARACTERS: usize = 200;

#[derive(Clone, Debug)]
pub struct ClaudeAdapter {
    projects_root: Option<PathBuf>,
    session_files: Arc<OnceLock<Vec<(String, PathBuf)>>>,
    notes: Arc<Mutex<Vec<String>>>,
}

impl ClaudeAdapter {
    #[must_use]
    pub fn with_root(projects_root: impl Into<PathBuf>) -> Self {
        Self {
            projects_root: Some(projects_root.into()),
            session_files: Arc::default(),
            notes: Arc::default(),
        }
    }

    fn discover_session_files(&self) -> Vec<(String, PathBuf)> {
        let Some(root) = self.projects_root.as_deref() else {
            return Vec::new();
        };
        // Subagent logs and tool results outnumber transcripts, so skip them before the budgets apply.
        nested_files_matching(root, 8, &|path, is_dir| {
            if is_dir {
                return path
                    .file_name()
                    .is_none_or(|name| name != "subagents" && name != "tool-results");
            }
            path.extension()
                .is_some_and(|extension| extension == "jsonl")
                && path
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .is_some_and(|stem| Uuid::parse_str(stem).is_ok())
        })
        .into_iter()
        .filter_map(|path| Some((path.file_stem()?.to_str()?.to_owned(), path)))
        .collect()
    }

    fn session_files(&self) -> &[(String, PathBuf)] {
        self.session_files
            .get_or_init(|| self.discover_session_files())
    }

    fn find_session(&self, id: &str) -> Result<PathBuf> {
        Uuid::parse_str(id).context("Claude session ID must be a UUID")?;
        // Transcripts live at `<project>/<id>.jsonl`, so exact reads don't depend on discovery budgets.
        let direct = self.projects_root.as_deref().and_then(|root| {
            std::fs::read_dir(root)
                .ok()?
                .flatten()
                .filter(|entry| entry.file_type().is_ok_and(|file_type| file_type.is_dir()))
                .find_map(|entry| provider_file(root, &entry.path().join(format!("{id}.jsonl"))))
        });
        if let Some(path) = direct {
            return Ok(path);
        }
        self.session_files()
            .iter()
            .find_map(|(candidate, path)| (candidate == id).then(|| path.clone()))
            .or_else(|| {
                self.discover_session_files()
                    .into_iter()
                    .find_map(|(candidate, path)| (candidate == id).then_some(path))
            })
            .and_then(|path| {
                self.projects_root
                    .as_deref()
                    .and_then(|root| provider_file(root, &path))
            })
            .ok_or_else(|| anyhow!("Claude session `{id}` was not found"))
    }

    fn session_path(&self, id: &str, source_path: Option<&Path>) -> Result<PathBuf> {
        Uuid::parse_str(id).context("Claude session ID must be a UUID")?;
        let hinted = source_path.and_then(|source_path| {
            let root = self.projects_root.as_deref()?;
            let expected_name = format!("{id}.jsonl");
            if source_path.file_name()?.to_str()? != expected_name {
                return None;
            }
            provider_file(root, source_path)
        });
        hinted.map_or_else(|| self.find_session(id), Ok)
    }

    fn history_index(&self) -> ClaudeHistory {
        let Some(config_root) = self.projects_root.as_deref().and_then(Path::parent) else {
            return ClaudeHistory::default();
        };
        let Some(history) = provider_file(config_root, &config_root.join("history.jsonl")) else {
            return ClaudeHistory::default();
        };
        let mut sessions: HashMap<String, HistoryEntry> = HashMap::new();
        let scan = visit_index_json_lines(&history, |record| {
            let Some(id) = string_at(&record, &[&["sessionId"]]) else {
                return;
            };
            let Some(project) = string_at(&record, &[&["project"]]) else {
                return;
            };
            let entry = sessions.entry(id.to_owned()).or_default();
            // Later records update the workspace and latest activity. The prompt title and
            // creation time stay first.
            entry.project = PathBuf::from(project);
            if let Some(timestamp) = parse_timestamp(record.get("timestamp")) {
                entry.created_at = Some(
                    entry
                        .created_at
                        .map_or(timestamp, |first| first.min(timestamp)),
                );
                entry.updated_at = Some(
                    entry
                        .updated_at
                        .map_or(timestamp, |latest| latest.max(timestamp)),
                );
            }
            if entry.first_prompt.is_none() {
                entry.first_prompt = string_at(&record, &[&["display"]]).and_then(prompt_title);
            }
        });
        let notes = match scan {
            Ok(scan) => scan.notes("Claude history.jsonl"),
            Err(error) => vec![format!("Claude history.jsonl could not be read: {error}.")],
        };
        ClaudeHistory { sessions, notes }
    }
}

impl Default for ClaudeAdapter {
    fn default() -> Self {
        Self {
            projects_root: provider_root("CLAUDE_CONFIG_DIR", &[".claude"])
                .map(|root| root.join("projects")),
            session_files: Arc::default(),
            notes: Arc::default(),
        }
    }
}

#[derive(Default)]
struct HistoryEntry {
    project: PathBuf,
    created_at: Option<DateTime<Utc>>,
    updated_at: Option<DateTime<Utc>>,
    first_prompt: Option<String>,
}

#[derive(Default)]
struct ClaudeHistory {
    sessions: HashMap<String, HistoryEntry>,
    notes: Vec<String>,
}

/// Turns a history prompt into a one-line title.
fn prompt_title(display: &str) -> Option<String> {
    let title = display.split_whitespace().collect::<Vec<_>>().join(" ");
    (!title.is_empty()).then(|| title.chars().take(MAX_PROMPT_TITLE_CHARACTERS).collect())
}

#[derive(Default)]
struct ClaudeMetadata {
    project_path: Option<PathBuf>,
    git_branch: Option<String>,
    created_at: Option<DateTime<Utc>>,
    updated_at: Option<DateTime<Utc>>,
}

/// Title records seen so far. A custom title wins over the latest generated one.
#[derive(Default)]
struct TitleCandidates {
    custom: Option<String>,
    generated: Option<String>,
}

impl TitleCandidates {
    fn observe(&mut self, record: &Value) {
        let generated = match record.get("type").and_then(Value::as_str) {
            Some("summary") => string_at(record, &[&["summary"], &["title"]]),
            Some("ai-title") => string_at(record, &[&["aiTitle"]]),
            _ => None,
        };
        if let Some(title) = generated {
            self.generated = Some(title.to_owned());
        }
        if let Some(title) = string_at(record, &[&["customTitle"]]) {
            self.custom = Some(title.to_owned());
        }
    }

    fn title(self) -> Option<String> {
        self.custom.or(self.generated)
    }
}

/// Title record keys: `"summary"`, plus `Title"` for `"aiTitle"` and `"customTitle"`.
static TITLE_KEYS: LazyLock<[memmem::Finder<'static>; 2]> = LazyLock::new(|| {
    [
        memmem::Finder::new(b"\"summary\""),
        memmem::Finder::new(b"Title\""),
    ]
});

/// Byte filter that skips parsing sampled lines which cannot hold a title.
fn may_carry_title(line: &[u8]) -> bool {
    TITLE_KEYS.iter().any(|key| key.find(line).is_some())
}

/// Observes title records in file order, parsing only lines that contain a title key.
fn observe_titles(titles: &mut TitleCandidates, records: &[u8]) {
    let mut starts = TITLE_KEYS
        .iter()
        .flat_map(|key| key.find_iter(records))
        .map(|position| {
            memchr::memrchr(b'\n', &records[..position]).map_or(0, |newline| newline + 1)
        })
        .collect::<Vec<_>>();
    starts.sort_unstable();
    starts.dedup();
    for start in starts {
        let line = &records[start..];
        let line = memchr::memchr(b'\n', line).map_or(line, |end| &line[..end]);
        if let Ok(record) = serde_json::from_slice::<Value>(line) {
            titles.observe(&record);
        }
    }
}

/// Replaces `buffer` with the bytes in `[start, end)`.
fn read_window(file: &mut File, start: u64, end: u64, buffer: &mut Vec<u8>) -> io::Result<()> {
    buffer.clear();
    file.seek(SeekFrom::Start(start))?;
    file.by_ref()
        .take(end.saturating_sub(start))
        .read_to_end(buffer)?;
    Ok(())
}

#[derive(Clone, Copy)]
enum ClaudeEventSource {
    Record(Uuid),
    Response(usize),
    DetachedSummary(Uuid),
}

struct ClaudeEvent {
    kind: EventKind,
    payload: Value,
    timestamp: Option<DateTime<Utc>>,
    replay_policy: ReplayPolicy,
    raw_type: Option<String>,
    source: Option<ClaudeEventSource>,
}

impl ClaudeEvent {
    fn append_to(mut self, builder: &mut EventBuilder, latest_summary: &mut Option<(Uuid, u64)>) {
        let id = match self.source {
            Some(ClaudeEventSource::Record(id)) => Some(id),
            Some(ClaudeEventSource::DetachedSummary(id)) => {
                if let Some((selected, sequence)) = *latest_summary
                    && selected == id
                {
                    self.payload["summary_sequence"] = json!(sequence);
                }
                None
            }
            _ => None,
        };
        if self.kind == EventKind::CompactionCreated
            && self.replay_policy == ReplayPolicy::Contextual
            && self.payload["text"]
                .as_str()
                .and_then(|text| omnis_core::claude_compaction_summary(text, true))
                .is_some()
            && let Some(id) = id
        {
            *latest_summary = Some((id, builder.checkpoint()));
        } else if self.raw_type.as_deref() == Some(omnis_core::CLAUDE_MISSING_COMPACTION_SUMMARY) {
            *latest_summary = None;
        }
        builder.push(
            self.kind,
            self.payload,
            self.timestamp,
            self.replay_policy,
            self.raw_type,
            id,
        );
    }
}

#[derive(Clone)]
struct ClaudeRecord {
    order: usize,
    is_summary: bool,
    native_transcript: bool,
    branch: branch::Parent,
    parent: Option<Uuid>,
    events: Range<usize>,
}

#[derive(Default)]
struct RecordCopies {
    current: HashMap<Uuid, [u8; 32]>,
    displaced: HashMap<(Uuid, [u8; 32]), SavedCopy>,
    // Only disconnected records need to remember the first absent parent UUID.
    missing: HashMap<Uuid, Uuid>,
}

struct SavedCopy {
    record: ClaudeRecord,
    missing_parent: Option<Uuid>,
}

struct RecordDigest(Sha256);

impl io::Write for RecordDigest {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl RecordDigest {
    fn of(record: &Value) -> [u8; 32] {
        let mut digest = Self(Sha256::new());
        if is_turn_duration(record) && record["durationMs"].is_u64() {
            // Claude can update elapsed time without changing the record's ancestry.
            let zero = Value::from(0);
            let fields = record.as_object().expect("turn duration is an object");
            serde_json::Serializer::new(&mut digest).collect_map(
                fields
                    .iter()
                    .map(|(key, value)| (key, if key == "durationMs" { &zero } else { value })),
            )
        } else {
            serde_json::to_writer(&mut digest, record)
        }
        .expect("writing JSON to a digest cannot fail");
        digest.0.finalize().into()
    }
}

impl RecordCopies {
    fn compares_link(role: Option<&str>, record: &Value) -> bool {
        role == Some("attachment") || is_turn_duration(record)
    }

    fn signature(
        record: &Value,
        compactions: &Compactions,
        duplicates: &HashSet<Uuid>,
        is_summary: bool,
        tracked_link: bool,
    ) -> Option<[u8; 32]> {
        // With an implicit parent, identical records can start different turns.
        let role = string_at(record, &[&["type"], &["message", "role"], &["role"]]);
        let fixed_parent = match role {
            Some("system") if record["subtype"] == "compact_boundary" => {
                record_uuid(record, "logicalParentUuid").is_some()
            }
            Some("user" | "assistant") if is_summary => {
                // The same summary text can describe a different boundary after a UUID collision.
                record_uuid(record, "parentUuid").is_some_and(|id| {
                    compactions.by_id.contains_key(&id) && !duplicates.contains(&id)
                })
            }
            Some("user" | "assistant") => record.get("parentUuid").is_some(),
            Some(_) if Self::compares_link(role, record) => {
                record.get("parentUuid").is_some_and(Value::is_null)
                    || record_uuid(record, "parentUuid")
                        .is_some_and(|parent| !duplicates.contains(&parent))
            }
            _ => tracked_link && record_uuid(record, "parentUuid").is_some(),
        };
        if !fixed_parent || record["isMeta"] == true {
            return None;
        }
        Some(RecordDigest::of(record))
    }

    fn missing_parent(
        &self,
        parent: Option<Uuid>,
        records: &HashMap<Uuid, ClaudeRecord>,
    ) -> Option<Uuid> {
        let parent = parent?;
        self.missing
            .get(&parent)
            .copied()
            .or_else(|| (!records.contains_key(&parent)).then_some(parent))
    }

    fn set_missing(&mut self, id: Uuid, missing: Option<Uuid>) {
        if let Some(missing) = missing {
            self.missing.insert(id, missing);
        } else if !self.missing.is_empty() {
            self.missing.remove(&id);
        }
    }

    fn reconnects(
        &self,
        id: Uuid,
        parent: Option<Uuid>,
        records: &HashMap<Uuid, ClaudeRecord>,
        duplicates: &HashSet<Uuid>,
    ) -> bool {
        let (Some(&missing), Some(parent)) = (self.missing.get(&id), parent) else {
            return false;
        };
        // A repeated cycle still ends at the same missing parent.
        !duplicates.contains(&id)
            && !duplicates.contains(&parent)
            && !duplicates.contains(&missing)
            && records.contains_key(&missing)
            && records.contains_key(&parent)
            && self.missing_parent(Some(parent), records) != Some(missing)
    }

    fn observe(
        &mut self,
        id: Uuid,
        signature: Option<[u8; 32]>,
        records: &mut HashMap<Uuid, ClaudeRecord>,
        branch: &mut branch::Branch,
    ) -> bool {
        let previous = match signature {
            Some(digest) => self.current.insert(id, digest),
            None => self.current.remove(&id),
        };
        if let Some(previous) = previous {
            if signature == Some(previous) {
                return true;
            }
            // Keep an older lookup only when a different record actually replaces it.
            self.displaced
                .entry((id, previous))
                .or_insert_with(|| SavedCopy {
                    record: records[&id].clone(),
                    missing_parent: self.missing.get(&id).copied(),
                });
        }
        if let Some(earlier) = signature.and_then(|digest| self.displaced.get(&(id, digest))) {
            records.insert(id, earlier.record.clone());
            self.set_missing(id, earlier.missing_parent);
            branch.record_replaced(id);
            return true;
        }
        false
    }
}

struct PreservedContext {
    boundary: Uuid,
    anchor: Uuid,
    records: Vec<Uuid>,
}

#[derive(Default)]
struct Compactions {
    #[cfg(test)]
    context_reads: std::cell::Cell<usize>,
    // Several boundaries can retain the same long segment. Expand only the selected segment.
    headers: Vec<CompactionHeader>,
    // A validated summary can only refer to the first occurrence of a UUID.
    originals: HashMap<Uuid, ClaudeRecord>,
    by_id: HashMap<Uuid, usize>,
    by_summary: HashMap<usize, SummaryHeader>,
    // A missing anchor can still connect later messages to the boundary that declared it.
    absent_anchors: HashMap<Uuid, Option<AbsentAnchor>>,
    absent_summary_nodes: HashMap<usize, Uuid>,
    rejected_replacements: HashMap<usize, usize>,
    pending: Option<usize>,
    paths: Paths,
    marker_paths: markers::MarkerPaths,
    source_records: Option<HashMap<RejectionScope, Vec<usize>>>,
}

struct CompactionHeader {
    order: usize,
    value: Value,
}

#[derive(Clone, Copy)]
struct AbsentAnchor {
    boundary_child: branch::Parent,
    anchor_child: branch::Parent,
}

impl CompactionHeader {
    fn anchor(&self) -> Option<Uuid> {
        let metadata = &self.value["compactMetadata"];
        let declaration = if metadata["preservedMessages"].is_null() {
            &metadata["preservedSegment"]
        } else {
            &metadata["preservedMessages"]
        };
        record_uuid(declaration, "anchorUuid")
    }
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
enum RejectionScope {
    Source(usize),
    Turn(usize),
}

enum SummaryMembers {
    Accepted(Members),
    RejectedSources {
        sources: Members,
        turns: Members,
    },
    RejectedWalk {
        records: Members,
        head_source: Option<usize>,
    },
}

struct SummaryHeader {
    markers: markers::Markers,
    members: SummaryMembers,
    error: Option<String>,
    header: usize,
    cutoff: Option<usize>,
    parent: Option<Uuid>,
    anchor: Option<Uuid>,
}

fn record_uuid(record: &Value, field: &str) -> Option<Uuid> {
    record[field].as_str()?.parse().ok()
}

impl Compactions {
    fn boundary_child(
        &self,
        parent: Uuid,
        records: &HashMap<Uuid, ClaudeRecord>,
    ) -> Option<branch::Parent> {
        if self.absent_anchors.is_empty() {
            return None;
        }
        let header = &self.headers[*self.by_id.get(&parent)?];
        let anchor = header.anchor()?;
        if records.contains_key(&anchor) {
            return None;
        }
        Some(self.absent_anchors.get(&anchor)?.as_ref()?.boundary_child)
    }

    fn absent_anchor(&self, branch: &branch::Branch, parent: branch::Parent) -> Option<Uuid> {
        if self.absent_summary_nodes.is_empty() {
            return None;
        }
        self.absent_summary_nodes
            .get(&branch.summary_for(parent)?)
            .copied()
    }

    fn declares_summary(&self, record: &Value, id: Option<Uuid>) -> bool {
        if self.by_id.is_empty()
            || record["type"] == "progress"
            || id.is_none()
            || !matches!(
                string_at(record, &[&["message", "role"], &["role"], &["type"]]),
                Some("user" | "assistant")
            )
        {
            return false;
        }
        let header = if record.get("parentUuid").is_none() {
            self.pending
        } else {
            record_uuid(record, "parentUuid").and_then(|parent| self.by_id.get(&parent).copied())
        };
        header.is_some_and(|header| self.headers[header].anchor() == id)
    }

    fn observe(&mut self, record: &Value, order: usize) {
        if record["type"] == "system" && record["subtype"] == "compact_boundary" {
            let index = self.headers.len();
            self.headers.push(CompactionHeader {
                order,
                value: json!({"uuid": record["uuid"], "compactMetadata":record["compactMetadata"]}),
            });
            if let Some(id) = record_uuid(record, "uuid") {
                self.by_id.insert(id, index);
            }
            self.pending = Some(index);
        } else if self.pending.is_some()
            && record["isCompactSummary"] != true
            && record["isMeta"] != true
            && matches!(record["type"].as_str(), Some("user" | "assistant"))
            && record.get("parentUuid").is_some()
            && record_uuid(record, "parentUuid").and_then(|parent| self.by_id.get(&parent).copied())
                != self.pending
        {
            // A rewind can abandon a boundary before Claude writes its summary.
            self.pending = None;
        }
    }

    fn link_summary(&mut self, record: &Value, index: usize) {
        let parent = record_uuid(record, "parentUuid");
        let header = parent
            .and_then(|id| self.by_id.get(&id).copied())
            .or(self.pending.take());
        if let Some(header) = header {
            self.by_summary.insert(
                index,
                SummaryHeader {
                    markers: markers::Markers::None,
                    members: SummaryMembers::Accepted(Members::None),
                    error: None,
                    header,
                    cutoff: None,
                    parent,
                    anchor: record_uuid(record, "uuid"),
                },
            );
        }
    }

    fn precedes_cutoff(&self, summary: usize, order: usize) -> bool {
        self.by_summary
            .get(&summary)
            .and_then(|link| link.cutoff)
            .is_some_and(|cutoff| order < cutoff)
    }

    fn empty_cutoff(&self, summary: usize) -> bool {
        self.by_summary.get(&summary).is_some_and(|link| {
            link.cutoff.is_some() && matches!(&link.members, SummaryMembers::Accepted(Members::Listed(ids)) if ids.is_empty())
        })
    }

    fn first_record<'a>(
        &'a self,
        id: &Uuid,
        records: &'a HashMap<Uuid, ClaudeRecord>,
    ) -> Option<&'a ClaudeRecord> {
        self.originals.get(id).or_else(|| records.get(id))
    }

    fn context(
        &self,
        summary: Option<usize>,
        records: &HashMap<Uuid, ClaudeRecord>,
    ) -> Result<Option<PreservedContext>> {
        let Some(link) = summary.and_then(|index| self.by_summary.get(&index)) else {
            return Ok(None);
        };
        if link.error.is_some() {
            return Ok(None);
        }
        #[cfg(test)]
        self.context_reads.set(self.context_reads.get() + 1);
        let context = compaction_preserved_context(&self.headers[link.header].value, |id| {
            self.first_record(id, records)
        })?;
        if let Some(context) = &context
            && (Some(context.anchor) != link.anchor || Some(context.boundary) != link.parent)
        {
            return Err(anyhow!(
                "retained compaction summary belongs to another boundary"
            ));
        }
        Ok(context)
    }

    fn validate_members(
        &self,
        summary: usize,
        records: &HashMap<Uuid, ClaudeRecord>,
        duplicates: &HashSet<Uuid>,
    ) -> Result<Option<PreservedContext>> {
        let Some(context) = self.context(Some(summary), records)? else {
            return Ok(None);
        };
        Self::validate_context(&context, records, duplicates)?;
        Ok(Some(context))
    }

    fn validate_context(
        context: &PreservedContext,
        records: &HashMap<Uuid, ClaudeRecord>,
        duplicates: &HashSet<Uuid>,
    ) -> Result<()> {
        if records.contains_key(&context.anchor) || duplicates.contains(&context.boundary) {
            return Err(anyhow!(
                "retained compaction context has an ambiguous anchor"
            ));
        }
        let boundary = records
            .get(&context.boundary)
            .ok_or_else(|| anyhow!("retained compaction context has no boundary"))?;
        let mut seen = HashSet::new();
        for id in &context.records {
            let record = records.get(id).ok_or_else(|| {
                anyhow!("retained compaction context references a missing record")
            })?;
            if record.order >= boundary.order {
                return Err(anyhow!(
                    "retained compaction context references a record at or after its boundary"
                ));
            }
            if record.is_summary {
                return Err(anyhow!(
                    "retained compaction context contains a compaction record"
                ));
            }
            if duplicates.contains(id) || !seen.insert(*id) {
                return Err(anyhow!(
                    "retained compaction context references an ambiguous record"
                ));
            }
        }
        Ok(())
    }

    fn index_paths(&mut self, records: &HashMap<Uuid, ClaudeRecord>) {
        let mut parents = vec![None; records.len()];
        for id in records.keys() {
            let record = self.first_record(id, records).expect("indexed record");
            parents[record.order] = record
                .parent
                .and_then(|p| self.first_record(&p, records))
                .map(|p| p.order);
        }
        self.paths.rebuild(&parents);
    }

    fn segment(&mut self, ids: &[Uuid], records: &HashMap<Uuid, ClaudeRecord>) -> Members {
        let order = |id: &Uuid| {
            self.first_record(id, records)
                .expect("known segment record")
                .order
        };
        let (Some(head), Some(tail)) = (ids.first().map(order), ids.last().map(order)) else {
            return Members::None;
        };
        let (min, max) = ids
            .iter()
            .map(order)
            .fold((head, head), |(min, max), i| (min.min(i), max.max(i)));
        let len = ids.len();
        if max - min + 1 == len {
            return Members::Range(min..=max);
        }
        if self.paths.distance(head, tail) != Some(len - 1) {
            self.index_paths(records);
        }
        Members::Segment { tail, len }
    }

    fn accept(
        &mut self,
        summary: usize,
        context: &PreservedContext,
        records: &HashMap<Uuid, ClaudeRecord>,
        verified_anchor: bool,
    ) {
        let header = self.by_summary[&summary].header;
        let segment = self.headers[header].value["compactMetadata"]["preservedMessages"].is_null();
        let members = if segment {
            self.segment(&context.records, records)
        } else {
            Members::listed(context.records.iter().map(|id| records[id].order).collect())
        };
        let markers = if context
            .records
            .iter()
            .any(|id| records[id].branch.transparent)
        {
            self.marker_paths.start(records, &self.originals);
            if segment {
                let head = records[&context.records[0]].order;
                let tail = records[context.records.last().expect("segment has markers")].order;
                self.marker_paths.segment(head, tail, context.records.len())
            } else {
                self.marker_paths.listed(
                    &context
                        .records
                        .iter()
                        .filter_map(|id| {
                            let r = &records[id];
                            r.branch.transparent.then_some(r.order)
                        })
                        .collect::<Vec<_>>(),
                )
            }
        } else {
            markers::Markers::None
        };
        let link = self.by_summary.get_mut(&summary).expect("linked summary");
        link.cutoff = verified_anchor.then_some(self.headers[header].order);
        link.members = SummaryMembers::Accepted(members);
        link.markers = markers;
    }

    fn nearest_marker(
        &self,
        summary: usize,
        query: &markers::MarkerQuery,
    ) -> Result<Option<Uuid>, markers::Unindexed> {
        let Some(header) = self.by_summary.get(&summary) else {
            return Ok(None);
        };
        self.marker_paths.nearest(&header.markers, query)
    }

    fn contains(&self, summary: usize, id: Uuid, records: &HashMap<Uuid, ClaudeRecord>) -> bool {
        let Some(link) = self.by_summary.get(&summary) else {
            return false;
        };
        let SummaryMembers::Accepted(members) = &link.members else {
            return false;
        };
        self.first_record(&id, records)
            .is_some_and(|record| members.contains(record.order, &self.paths))
    }

    fn reject(
        &mut self,
        summary: usize,
        reason: String,
        records: &HashMap<Uuid, ClaudeRecord>,
        branch: &branch::Branch,
        verified_anchor: bool,
    ) {
        let boundary = &self.headers[self.by_summary[&summary].header];
        let cutoff = boundary.order;
        let before = |id: &Uuid| {
            self.first_record(id, records)
                .filter(|record| record.order < cutoff)
        };
        let metadata = &boundary.value["compactMetadata"];
        let predecessor = branch.previous_summary(summary).filter(|index| {
            verified_anchor
                && !self.absent_summary_nodes.contains_key(&summary)
                && self
                    .by_summary
                    .get(index)
                    .is_some_and(|prior| prior.cutoff.is_some())
        });
        let mut replacement = None;
        let mut note_overlap = |id: Uuid, record: &ClaudeRecord| {
            if replacement.is_none()
                && !record.branch.transparent
                && record.branch.source.is_some()
                && let Some(prior) = predecessor
                && self.contains(prior, id, records)
            {
                replacement = Some(prior);
            }
        };
        let members = if metadata["preservedMessages"].is_null() {
            let declaration = &metadata["preservedSegment"];
            let head = record_uuid(declaration, "headUuid");
            let head_source = head.and_then(|id| {
                let record = before(&id)?;
                note_overlap(id, record);
                record.branch.source
            });
            let mut current = record_uuid(declaration, "tailUuid");
            let mut seen = HashSet::new();
            let mut ids = Vec::new();
            while let Some(id) = current {
                let Some(record) = before(&id) else {
                    break;
                };
                if !seen.insert(id) {
                    break;
                }
                note_overlap(id, record);
                ids.push(id);
                if Some(id) == head {
                    break;
                }
                current = record.parent;
            }
            ids.reverse();
            if self.source_records.is_none() {
                let mut sources = HashMap::new();
                for id in records.keys() {
                    let first = self.first_record(id, records).expect("indexed record");
                    Self::index_source(&mut sources, first, branch);
                }
                self.source_records = Some(sources);
            }
            SummaryMembers::RejectedWalk {
                records: self.segment(&ids, records),
                head_source,
            }
        } else {
            let sources: Vec<_> = metadata["preservedMessages"]["uuids"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|entry| {
                    let id = entry.as_str()?.parse().ok()?;
                    let record = before(&id)?;
                    note_overlap(id, record);
                    record.branch.source
                })
                .collect();
            SummaryMembers::RejectedSources {
                turns: Members::listed(sources.iter().map(|&source| branch.turn(source)).collect()),
                sources: Members::listed(sources),
            }
        };
        if let Some(prior) = replacement {
            self.rejected_replacements.insert(summary, prior);
        }
        let link = self
            .by_summary
            .get_mut(&summary)
            .expect("rejected retention has a boundary");
        link.error = Some(reason);
        link.members = members;
    }

    fn index_source(
        groups: &mut HashMap<RejectionScope, Vec<usize>>,
        record: &ClaudeRecord,
        branch: &branch::Branch,
    ) {
        if let Some(source) = record.branch.source {
            for scope in [
                RejectionScope::Source(source),
                RejectionScope::Turn(branch.turn(source)),
            ] {
                groups.entry(scope).or_default().push(record.order);
            }
        }
    }

    fn rejected(&self, summary: usize, scope: RejectionScope, branch: &branch::Branch) -> bool {
        let Some(link) = self.by_summary.get(&summary) else {
            return false;
        };
        match &link.members {
            SummaryMembers::Accepted(_) => false,
            SummaryMembers::RejectedSources { sources, turns } => match scope {
                RejectionScope::Source(source) => sources.contains(source, &self.paths),
                RejectionScope::Turn(turn) => turns.contains(turn, &self.paths),
            },
            SummaryMembers::RejectedWalk {
                head_source,
                records,
            } => {
                head_source.is_some_and(|head| match scope {
                    RejectionScope::Source(source) => source == head,
                    RejectionScope::Turn(turn) => turn == branch.turn(head),
                }) || self
                    .source_records
                    .as_ref()
                    .and_then(|groups| groups.get(&scope))
                    .is_some_and(|aliases| {
                        aliases.iter().any(|&r| records.contains(r, &self.paths))
                    })
            }
        }
    }

    fn remember_first(
        &mut self,
        id: Uuid,
        record: &ClaudeRecord,
        records: &HashMap<Uuid, ClaudeRecord>,
        branch: &branch::Branch,
    ) {
        if self.marker_paths.active() {
            let parent = record
                .parent
                .and_then(|p| self.originals.get(&p).or_else(|| records.get(&p)));
            self.marker_paths.push(id, record, parent);
        }
        if self.paths.active() {
            let parent = record
                .parent
                .and_then(|p| self.first_record(&p, records))
                .map(|r| r.order);
            self.paths.push(parent);
        }
        if let Some(groups) = &mut self.source_records {
            Self::index_source(groups, record, branch);
        }
    }
}

fn compaction_preserved_context<'a>(
    boundary: &Value,
    record_for: impl Fn(&Uuid) -> Option<&'a ClaudeRecord>,
) -> Result<Option<PreservedContext>> {
    let metadata = &boundary["compactMetadata"];
    let uuid = |value: Option<&Value>| {
        value
            .and_then(Value::as_str)
            .and_then(|value| Uuid::parse_str(value).ok())
            .ok_or_else(|| anyhow!("retained compaction context contains an invalid UUID"))
    };
    if let Some(preserved) = metadata
        .get("preservedMessages")
        .filter(|value| !value.is_null())
    {
        let anchor = uuid(preserved.get("anchorUuid"))?;
        let listed = preserved
            .get("uuids")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("retained compaction context has no message list"))?
            .iter()
            .map(|value| uuid(Some(value)))
            .collect::<Result<Vec<_>>>()?;
        // Claude skips the whole relink if a listed UUID is absent from its transcript map.
        // Progress records can bridge parents, but only allUuids can include them.
        if listed
            .iter()
            .any(|id| record_for(id).is_some_and(|record| !record.native_transcript))
        {
            return Err(anyhow!(
                "Claude's retained context names a record outside its native transcript"
            ));
        }
        return Ok(Some(PreservedContext {
            boundary: uuid(boundary.get("uuid"))?,
            anchor,
            records: listed,
        }));
    }
    let Some(segment) = metadata
        .get("preservedSegment")
        .filter(|value| !value.is_null())
    else {
        return Ok(None);
    };
    let anchor = uuid(segment.get("anchorUuid"))?;
    let head = uuid(segment.get("headUuid"))?;
    let mut current = uuid(segment.get("tailUuid"))?;
    let mut retained = Vec::new();
    let mut visited = HashSet::new();
    while visited.insert(current) {
        retained.push(current);
        let record = record_for(&current)
            .ok_or_else(|| anyhow!("retained compaction context references a missing record"))?;
        if !record.native_transcript && (current == head || retained.len() == 1) {
            return Err(anyhow!(
                "Claude's retained context names a record outside its native transcript"
            ));
        }
        if current == head {
            retained.reverse();
            return Ok(Some(PreservedContext {
                boundary: uuid(boundary.get("uuid"))?,
                anchor,
                records: retained,
            }));
        }
        current = record
            .parent
            .ok_or_else(|| anyhow!("retained compaction context has a broken parent chain"))?;
    }
    Err(anyhow!(
        "retained compaction context has a cyclic parent chain"
    ))
}

fn claude_message_kind(is_summary: bool, role: Option<&str>) -> Option<EventKind> {
    if is_summary {
        return Some(EventKind::CompactionCreated);
    }
    match role {
        Some("user") => Some(EventKind::MessageUser),
        Some("assistant") => Some(EventKind::MessageAssistant),
        _ => None,
    }
}

fn is_turn_duration(record: &Value) -> bool {
    record["type"] == "system" && record["subtype"] == "turn_duration"
}

fn is_transcript_record(record: &Value) -> bool {
    match record["type"].as_str() {
        Some(kind) => matches!(kind, "user" | "assistant" | "system" | "attachment"),
        None => matches!(
            string_at(record, &[&["message", "role"], &["role"]]),
            Some("user" | "assistant")
        ),
    }
}

fn is_main_record(record: &Value) -> bool {
    record["isSidechain"] != true && record["teamName"].as_str().is_none()
}

impl ClaudeMetadata {
    fn observe_workspace(&mut self, record: &Value) {
        if let Some(cwd) = string_at(record, &[&["cwd"]]) {
            self.project_path = Some(PathBuf::from(cwd));
        }
        if let Some(branch) = string_at(record, &[&["gitBranch"]]) {
            self.git_branch = Some(branch.to_owned());
        }
    }

    fn observe_timestamp(&mut self, record: &Value) -> Option<DateTime<Utc>> {
        let timestamp = parse_timestamp(record.get("timestamp"));
        if let Some(timestamp) = timestamp {
            self.created_at = Some(
                self.created_at
                    .map_or(timestamp, |current| current.min(timestamp)),
            );
            self.updated_at = Some(
                self.updated_at
                    .map_or(timestamp, |current| current.max(timestamp)),
            );
        }
        timestamp
    }
}

/// Bounded transcript sample for discovery. Discovery never scans a whole transcript.
struct TranscriptSample {
    file: File,
    length: u64,
    modified: Option<SystemTime>,
    head_end: u64,
    workspace: ClaudeMetadata,
    titles: TitleCandidates,
}

impl TranscriptSample {
    /// Samples the head. `buffer` is reused across transcripts to avoid per-file allocations.
    fn head(path: &Path, need_workspace: bool, buffer: &mut Vec<u8>) -> Result<Self> {
        let file = File::open(path)?;
        let metadata = file.metadata()?;
        let mut sample = Self {
            file,
            length: metadata.len(),
            modified: metadata.modified().ok(),
            head_end: 0,
            workspace: ClaudeMetadata::default(),
            titles: TitleCandidates::default(),
        };
        if need_workspace {
            sample.scan_workspace_head()?;
        } else {
            sample.read_head_metadata(buffer)?;
        }
        Ok(sample)
    }

    /// Scans up to 32 records within 2 MiB for the first `cwd`, observing title records among the
    /// scanned records. The tail sample starts after them, so none are skipped.
    fn scan_workspace_head(&mut self) -> Result<()> {
        let mut reader = BufReader::new((&mut self.file).take(MAX_METADATA_BYTES));
        let mut line = Vec::new();
        for _ in 0..MAX_METADATA_RECORDS {
            line.clear();
            let read = u64::try_from(reader.read_until(b'\n', &mut line)?)?;
            if read == 0 {
                break;
            }
            let title_line = may_carry_title(&line);
            self.head_end += read;
            let Ok(record) = serde_json::from_slice::<Value>(&line) else {
                continue;
            };
            if title_line {
                self.titles.observe(&record);
            }
            if is_main_record(&record) {
                self.workspace.observe_workspace(&record);
                if self.workspace.project_path.is_some() {
                    self.workspace.observe_timestamp(&record);
                    break;
                }
            }
        }
        Ok(())
    }

    /// Reads complete records from the first 16 KiB.
    fn read_head_metadata(&mut self, buffer: &mut Vec<u8>) -> Result<()> {
        read_window(
            &mut self.file,
            0,
            self.length.min(TITLE_HEAD_BYTES as u64),
            buffer,
        )?;
        let complete = if buffer.len() as u64 == self.length {
            buffer.len()
        } else {
            memchr::memrchr(b'\n', buffer).map_or(0, |end| end + 1)
        };
        self.observe_sample(&buffer[..complete]);
        self.head_end = complete as u64;
        Ok(())
    }

    /// Reads complete records from the last 64 KiB that the head did not cover.
    fn read_tail_metadata(&mut self, buffer: &mut Vec<u8>) -> Result<()> {
        let start = self
            .length
            .saturating_sub(TITLE_TAIL_BYTES as u64)
            .max(self.head_end);
        if start >= self.length {
            return Ok(());
        }
        // Read one byte early so the first segment is the partial record before `start`, if any.
        let aligned = start.saturating_sub(1);
        read_window(&mut self.file, aligned, self.length, buffer)?;
        let records = if start > 0 {
            buffer
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(&buffer[buffer.len()..], |end| &buffer[end + 1..])
        } else {
            &buffer[..]
        };
        self.observe_sample(records);
        Ok(())
    }

    fn observe_sample(&mut self, mut records: &[u8]) {
        observe_titles(&mut self.titles, records);
        // Branches can change. Read the last valid branch in the sampled records.
        while let Some(position) = memmem::rfind(records, b"\"gitBranch\"") {
            let start = memchr::memrchr(b'\n', &records[..position]).map_or(0, |index| index + 1);
            let end = memchr::memchr(b'\n', &records[position..])
                .map_or(records.len(), |index| position + index);
            if let Ok(record) = serde_json::from_slice::<Value>(&records[start..end])
                && is_main_record(&record)
                && let Some(branch) = string_at(&record, &[&["gitBranch"]])
            {
                self.workspace.git_branch = Some(branch.to_owned());
                break;
            }
            records = &records[..start];
        }
    }
}

fn text_payload(text: &str) -> Value {
    json!({ "text": text })
}

fn push_events(events: &mut Vec<ClaudeEvent>, record: &Value, is_summary: bool) {
    if record.get("isMeta").and_then(Value::as_bool) == Some(true) {
        return;
    }
    let raw_type = record
        .get("type")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let timestamp = parse_timestamp(record.get("timestamp"));
    let event_id = record
        .get("uuid")
        .and_then(Value::as_str)
        .and_then(|id| Uuid::parse_str(id).ok());
    let role = string_at(record, &[&["message", "role"], &["role"], &["type"]]);
    let Some(message_kind) = claude_message_kind(is_summary, role) else {
        return;
    };
    if message_kind == EventKind::MessageAssistant {
        push_claude_session_metadata(events, record, timestamp);
    }
    let Some(content) = value_at(record, &[&["message", "content"], &["content"]]) else {
        return;
    };
    if let Some(text) = content.as_str().filter(|text| !text.is_empty()) {
        events.push(ClaudeEvent {
            kind: message_kind,
            payload: text_payload(text),
            timestamp,
            replay_policy: ReplayPolicy::Contextual,
            raw_type,
            source: event_id.map(ClaudeEventSource::Record),
        });
        return;
    }
    let Some(parts) = content.as_array() else {
        return;
    };
    let mut visible_text = false;
    let mut images = 0_usize;
    for part in parts {
        match part.get("type").and_then(Value::as_str) {
            Some("image") => images += 1,
            Some("text") => {
                if let Some(text) = part
                    .get("text")
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
                {
                    visible_text = true;
                    events.push(ClaudeEvent {
                        kind: message_kind.clone(),
                        payload: text_payload(text),
                        timestamp,
                        replay_policy: ReplayPolicy::Contextual,
                        raw_type: raw_type.clone(),
                        source: event_id.map(ClaudeEventSource::Record),
                    });
                }
            }
            Some("tool_use" | "tool_result") => {
                events.push(tool_event(part, timestamp, raw_type.clone(), event_id));
            }
            _ => {}
        }
    }
    // An image-only turn keeps a placeholder so the reply still follows a request.
    if message_kind == EventKind::MessageUser && !visible_text && images > 0 {
        events.push(ClaudeEvent {
            kind: EventKind::MessageUser,
            payload: text_payload(&omitted_images_text(images)),
            timestamp,
            replay_policy: ReplayPolicy::Contextual,
            raw_type,
            source: event_id.map(ClaudeEventSource::Record),
        });
    }
}

fn can_own_summary_cutoff(record: &Value) -> bool {
    if record["isCompactSummary"] != true || record["isMeta"] == true {
        return false;
    }
    let role = string_at(record, &[&["message", "role"], &["role"], &["type"]]);
    if !matches!(role, Some("user" | "assistant"))
        || record.get("type").is_some_and(|kind| kind.as_str() != role)
    {
        return false;
    }
    let Some(content) = value_at(record, &[&["message", "content"], &["content"]]) else {
        return false;
    };
    // A text summary can own the validated retention list even when its body is blank.
    content.is_string()
        || content.as_array().is_some_and(|parts| {
            parts.is_empty()
                || parts
                    .iter()
                    .any(|part| part["type"] == "text" && part["text"].is_string())
        })
}

fn check_summary(events: &mut Vec<ClaudeEvent>, start: usize, record: &Value) {
    if record["isMeta"] == true {
        return;
    }
    let marked = record["isCompactSummary"] == true;
    if marked
        && events[start..].iter().any(|event| {
            event.kind == EventKind::CompactionCreated
                && event.payload["text"]
                    .as_str()
                    .and_then(|text| omnis_core::claude_compaction_summary(text, true))
                    .is_some()
        })
    {
        return;
    }
    let parts = events.split_off(start);
    let reason = if marked {
        "Claude compacted the conversation, but the summary has no text"
    } else {
        "Claude named this record as a compaction anchor without marking it as a summary"
    };
    events.push(summary_gap(
        record_uuid(record, "uuid"),
        parse_timestamp(record.get("timestamp")),
        reason,
    ));
    if !marked {
        for mut part in parts {
            if part.kind != EventKind::CompactionCreated {
                continue;
            }
            let Some(text) = part.payload["text"].as_str() else {
                continue;
            };
            if omnis_core::claude_compaction_summary(text, false).is_none() {
                // Only recovery can quote unmarked text with an unknown envelope.
                if omnis_core::claude_compaction_summary(text, true).is_none() {
                    continue;
                }
                part.replay_policy = ReplayPolicy::HistoricalOnly;
                part.raw_type = Some(omnis_core::CLAUDE_MISSING_COMPACTION_SUMMARY.into());
            }
            events.push(part);
        }
    }
}

fn summary_gap(id: Option<Uuid>, timestamp: Option<DateTime<Utc>>, reason: &str) -> ClaudeEvent {
    ClaudeEvent {
        kind: EventKind::ProviderEvent,
        payload: json!({"native_history_unverified":true,"reason":reason}),
        timestamp,
        replay_policy: ReplayPolicy::HistoricalOnly,
        raw_type: Some(omnis_core::CLAUDE_MISSING_COMPACTION_SUMMARY.into()),
        source: id.map(ClaudeEventSource::Record),
    }
}

/// Historical record of one `tool_use` or `tool_result` content block.
fn tool_event(
    part: &Value,
    timestamp: Option<DateTime<Utc>>,
    raw_type: Option<String>,
    event_id: Option<Uuid>,
) -> ClaudeEvent {
    let field = |name: &str| part.get(name).cloned().unwrap_or(Value::Null);
    let (kind, payload) = if part.get("type").and_then(Value::as_str) == Some("tool_use") {
        (
            EventKind::ToolCalled,
            json!({ "id": field("id"), "name": field("name"), "input": field("input") }),
        )
    } else if part.get("is_error").and_then(Value::as_bool) == Some(true) {
        (
            EventKind::ToolFailed,
            json!({ "tool_use_id": field("tool_use_id"), "content": field("content") }),
        )
    } else {
        (
            EventKind::ToolCompleted,
            json!({ "tool_use_id": field("tool_use_id"), "content": field("content") }),
        )
    };
    ClaudeEvent {
        kind,
        payload,
        timestamp,
        replay_policy: ReplayPolicy::HistoricalOnly,
        raw_type,
        source: event_id.map(ClaudeEventSource::Record),
    }
}

type ResponseId = (String, String);

fn response_id(record: &Value) -> Option<ResponseId> {
    let message = string_at(record, &[&["message", "id"]])?;
    let request = string_at(record, &[&["requestId"]]).unwrap_or_default();
    Some((message.to_owned(), request.to_owned()))
}

fn push_claude_session_metadata(
    events: &mut Vec<ClaudeEvent>,
    record: &Value,
    timestamp: Option<DateTime<Utc>>,
) {
    let Some(payload) = claude_session_metadata(record) else {
        return;
    };
    events.push(ClaudeEvent {
        kind: EventKind::ProviderEvent,
        payload,
        timestamp,
        replay_policy: ReplayPolicy::HistoricalOnly,
        raw_type: Some("omnisession.session_metadata".to_owned()),
        source: None,
    });
}

/// Folds a later chunk of one response into its metadata event.
///
/// Chunks repeat the response's usage, so the largest total wins instead of their sum.
fn merge_response_metadata(existing: &mut Value, chunk: &Value) {
    for field in ["model", "reasoning_mode"] {
        if existing[field].is_null() && !chunk[field].is_null() {
            existing[field] = chunk[field].clone();
        }
    }
    if let Some(tokens) = chunk["total_tokens"].as_u64()
        && existing["total_tokens"]
            .as_u64()
            .is_none_or(|current| tokens > current)
    {
        existing["total_tokens"] = Value::from(tokens);
    }
}

fn claude_session_metadata(record: &Value) -> Option<Value> {
    let message = record.get("message").unwrap_or(record);
    let model = string_at(message, &[&["model"], &["model_id"]]);
    let content = value_at(record, &[&["message", "content"], &["content"]]);
    let reasoning_mode = content
        .and_then(Value::as_array)
        .is_some_and(|parts| {
            parts.iter().any(|part| {
                matches!(
                    part.get("type").and_then(Value::as_str),
                    Some("thinking" | "redacted_thinking")
                )
            })
        })
        .then_some("thinking");
    let usage = message.get("usage");
    let total_tokens = usage
        .map(|usage| {
            [
                "input_tokens",
                "cache_creation_input_tokens",
                "cache_read_input_tokens",
                "output_tokens",
            ]
            .into_iter()
            .filter_map(|field| usage.get(field).and_then(Value::as_u64))
            .fold(0_u64, u64::saturating_add)
        })
        .filter(|tokens| *tokens > 0);
    (model.is_some() || reasoning_mode.is_some() || total_tokens.is_some()).then(|| {
        json!({
            "model": model,
            "reasoning_mode": reasoning_mode,
            "total_tokens": total_tokens,
            "token_usage": "incremental",
        })
    })
}

#[derive(Default)]
struct ClaudeTranscript {
    branch: branch::Branch,
    metadata: ClaudeMetadata,
    titles: TitleCandidates,
    events: Vec<ClaudeEvent>,
    records: HashMap<Uuid, ClaudeRecord>,
    duplicate_records: HashSet<Uuid>,
    record_copies: RecordCopies,
    compactions: Compactions,
    has_records: bool,
    has_conversation: bool,
    has_main_conversation: bool,
}

impl ClaudeTranscript {
    fn observe(&mut self, record: &Value) {
        self.observe_definition(record, Some(record));
    }

    fn observe_definition(&mut self, record: &Value, raw: Option<&Value>) {
        let main_record = is_main_record(record);
        let id = record_uuid(record, "uuid");
        let record_parent = record_uuid(record, "parentUuid");
        let causal_parent = if record["type"] == "system" && record["subtype"] == "compact_boundary"
        {
            record_uuid(record, "logicalParentUuid").or_else(|| self.branch.tip_uuid())
        } else if self.branch.inherits_previous(record, &self.compactions) {
            self.branch.tip_uuid()
        } else {
            record_parent
        };
        let is_summary =
            record["isCompactSummary"] == true || self.compactions.declares_summary(record, id);
        let mut restored_order = None;
        if main_record && let Some(id) = id {
            let signature = raw.and_then(|definition| {
                RecordCopies::signature(
                    definition,
                    &self.compactions,
                    &self.duplicate_records,
                    is_summary,
                    !self.record_copies.missing.is_empty()
                        && self.record_copies.missing.contains_key(&id),
                )
            });
            if self
                .record_copies
                .observe(id, signature, &mut self.records, &mut self.branch)
            {
                if self.record_copies.reconnects(
                    id,
                    causal_parent,
                    &self.records,
                    &self.duplicate_records,
                ) {
                    restored_order = Some(self.records[&id].order);
                } else {
                    let existing = &self.records[&id];
                    if record["type"] != "system"
                        && !existing.branch.transparent
                        && !self.branch.selected_summary().is_some_and(|summary| {
                            self.compactions.precedes_cutoff(summary, existing.order)
                        })
                    {
                        self.branch
                            .observe_existing(existing.branch, &self.compactions);
                    }
                    return;
                }
            }
        }
        let native_millis = self.observe_metadata(record, main_record);
        if !main_record {
            return;
        }
        if !is_summary && self.compactions.pending.is_some() {
            self.link_absent_anchor(Some((record, record_parent)));
        }
        self.compactions.observe(record, self.records.len());
        let start = self.events.len();
        let verified_anchor = if record["type"] == "progress" {
            None
        } else {
            push_events(&mut self.events, record, is_summary);
            is_summary.then(|| {
                check_summary(&mut self.events, start, record);
                can_own_summary_cutoff(record)
            })
        };
        let previous_summary = self.branch.selected_summary();
        let branch = self.branch.observe(
            record,
            is_summary,
            &self.records,
            &self.duplicate_records,
            &self.compactions,
            start..self.events.len(),
        );
        if !is_summary
            && let Some(node) = branch.node
            && let Some(metadata) = self.events.get_mut(start)
            && metadata.kind == EventKind::ProviderEvent
        {
            metadata.source = Some(ClaudeEventSource::Response(node));
        }
        self.branch
            .clock
            .observe(record, branch.node, branch.transparent, native_millis);
        self.update_retention(record, previous_summary, verified_anchor);
        if let Some(id) = id {
            let definition = ClaudeRecord {
                order: restored_order.unwrap_or(self.records.len()),
                is_summary,
                native_transcript: is_transcript_record(record),
                branch,
                parent: record_parent,
                events: start..self.events.len(),
            };
            self.save_record(id, definition, raw, causal_parent, restored_order.is_some());
        }
    }

    fn observe_metadata(&mut self, record: &Value, main_record: bool) -> Option<i64> {
        self.has_records = true;
        let timestamp = self.metadata.observe_timestamp(record);
        self.titles.observe(record);
        if matches!(
            record.get("type").and_then(Value::as_str),
            Some("user" | "assistant")
        ) {
            self.has_conversation = true;
            self.has_main_conversation |= main_record;
        }
        if main_record {
            self.metadata.observe_workspace(record);
        }
        // Native numeric dates are milliseconds, including values near the Unix epoch.
        if record["timestamp"].is_number() {
            record["timestamp"]
                .as_i64()
                .filter(|n| n.unsigned_abs() <= 8_640_000_000_000_000)
        } else {
            timestamp.map(|at| at.timestamp_millis())
        }
    }

    fn link_absent_anchor(&mut self, next: Option<(&Value, Option<Uuid>)>) {
        let Some(header) = self
            .compactions
            .pending
            .map(|i| &self.compactions.headers[i])
        else {
            return;
        };
        let (Some(boundary), Some(anchor)) = (record_uuid(&header.value, "uuid"), header.anchor())
        else {
            return;
        };
        if let Some((next, next_parent)) = next {
            let conversation = next["type"] != "progress"
                && next["isMeta"] != true
                && matches!(
                    string_at(next, &[&["message", "role"], &["role"], &["type"]]),
                    Some("user" | "assistant")
                );
            let new_boundary = next["type"] == "system" && next["subtype"] == "compact_boundary";
            if !conversation
                && !new_boundary
                && next_parent != Some(anchor)
                && next_parent != Some(boundary)
            {
                return;
            }
        } else {
            // At EOF Claude applies a complete descriptor. An invalid descriptor leaves the previous conversation active.
            let context = compaction_preserved_context(&header.value, |id| {
                self.compactions.first_record(id, &self.records)
            });
            if !context.ok().flatten().is_some_and(|c| {
                Compactions::validate_context(&c, &self.records, &self.duplicate_records).is_ok()
            }) {
                return;
            }
        }
        if self.records.contains_key(&anchor) {
            return;
        }
        if self.duplicate_records.contains(&boundary)
            || self.compactions.absent_anchors.contains_key(&anchor)
        {
            self.compactions.absent_anchors.insert(anchor, None);
            return;
        }
        // The descriptor supplies the two UUIDs. Only a coverage notice enters the transcript.
        // A genuine summary can still arrive later and resolve the native UUID.
        // A direct boundary child precedes the retained messages. A child of the
        // declared anchor follows them. Both share the same earlier recovery history.
        let mut link = json!({"type":"user","parentUuid":boundary});
        let boundary_child = self.observe_anchor_gap(&link, boundary, anchor);
        let previous = self.branch.selected_summary();
        link["uuid"] = json!(anchor);
        let anchor_child = self.observe_anchor_gap(&link, boundary, anchor);
        self.update_retention(&link, previous, Some(true));
        self.compactions.absent_anchors.insert(
            anchor,
            Some(AbsentAnchor {
                boundary_child,
                anchor_child,
            }),
        );
    }

    fn observe_anchor_gap(&mut self, link: &Value, boundary: Uuid, anchor: Uuid) -> branch::Parent {
        let start = self.events.len();
        self.events.push(summary_gap(
            Some(boundary),
            None,
            "Claude's compaction boundary declares a summary record missing from the transcript",
        ));
        let parent = self.branch.observe(
            link,
            true,
            &self.records,
            &self.duplicate_records,
            &self.compactions,
            start..self.events.len(),
        );
        self.compactions
            .absent_summary_nodes
            .insert(parent.node.expect("gap has a conversation node"), anchor);
        parent
    }

    fn save_record(
        &mut self,
        id: Uuid,
        record: ClaudeRecord,
        raw: Option<&Value>,
        causal_parent: Option<Uuid>,
        restored: bool,
    ) {
        let gap = if !self.record_copies.missing.is_empty()
            || self.branch.may_have_missing_parent(record.branch)
        {
            self.record_copies
                .missing_parent(causal_parent, &self.records)
        } else {
            None
        };
        let gap = gap
            .or_else(|| self.compactions.absent_anchor(&self.branch, record.branch))
            .or_else(|| {
                record_uuid(raw?, "sourceToolAssistantUUID")
                    .filter(|issuer| !self.records.contains_key(issuer))
            });
        if gap.is_some()
            && record.branch.transparent
            && record.parent.is_some()
            && let Some(raw) = raw
            && raw["isMeta"] != true
        {
            // A repeated link can now connect its descendants to verified history.
            self.record_copies
                .current
                .entry(id)
                .or_insert_with(|| RecordDigest::of(raw));
        }
        self.record_copies.set_missing(id, gap);
        if !self.records.contains_key(&id) {
            self.compactions
                .remember_first(id, &record, &self.records, &self.branch);
        } else if restored && let Some(groups) = &mut self.compactions.source_records {
            Compactions::index_source(groups, &record, &self.branch);
        }
        if let Some(previous) = self.records.insert(id, record) {
            if !restored {
                if self.duplicate_records.insert(id) {
                    self.compactions
                        .marker_paths
                        .record_replacement(previous.order);
                }
                self.compactions.originals.entry(id).or_insert(previous);
            }
            self.branch.record_replaced(id);
        }
    }

    fn update_retention(
        &mut self,
        record: &Value,
        previous: Option<usize>,
        verified_anchor: Option<bool>,
    ) {
        let selected = self.branch.selected_summary();
        if selected == previous {
            return;
        }
        if let (Some(index), Some(verified)) = (selected, verified_anchor) {
            self.compactions.link_summary(record, index);
            match self
                .compactions
                .validate_members(index, &self.records, &self.duplicate_records)
            {
                Ok(Some(context)) => {
                    self.compactions
                        .accept(index, &context, &self.records, verified);
                    if verified {
                        self.branch.verified_cutoffs.push(index);
                    }
                    self.branch
                        .remember_retention(&context.records, &self.records);
                }
                Err(error) => {
                    self.compactions.reject(
                        index,
                        error.to_string(),
                        &self.records,
                        &self.branch,
                        verified,
                    );
                    self.branch.mark_unverified_retention(index);
                }
                Ok(None) => {}
            }
        }
        self.branch.select_retention();
    }

    /// Validate the UUIDs before moving any conversation nodes. Opaque records can mark
    /// the rewind cutoff without adding a message to the selected path.
    fn retained_nodes(
        &self,
        summary: usize,
        tip: Option<Uuid>,
    ) -> Result<Option<(Vec<usize>, usize)>> {
        let Some(preserved) = self.compactions.context(Some(summary), &self.records)? else {
            return Ok(None);
        };
        let mut selected = Vec::new();
        let mut retained_end = None;
        for id in preserved.records {
            let record = self
                .compactions
                .first_record(&id, &self.records)
                .expect("validated compaction member");
            if !record.events.is_empty()
                && let Some(node) = record.branch.node
            {
                selected.push(node);
            }
            if tip == Some(id) {
                retained_end = Some(selected.len());
            }
        }
        let end = retained_end.unwrap_or(selected.len());
        Ok(Some((selected, end)))
    }

    fn preceding_cutoff(
        &self,
        path: branch::TrailingPath,
        wanted: branch::RetainedParent,
        previous: Option<&(usize, Option<branch::RetainedParent>)>,
    ) -> Option<branch::RetainedParent> {
        let &(summary, previous_tip) = previous?;
        if summary != path.start {
            return None;
        }
        let context = self
            .compactions
            .context(Some(summary), &self.records)
            .ok()??;
        let members = if let Some(previous_tip) = previous_tip {
            let end = context
                .records
                .iter()
                .position(|id| *id == previous_tip.uuid)?;
            &context.records[..=end]
        } else {
            context.records.as_slice()
        };
        if members.contains(&wanted.uuid) {
            return Some(wanted);
        }
        let source = self.branch.cutoff_at(path.cutoff)?;
        members.contains(&source.uuid).then_some(source)
    }

    fn selected_events(&mut self) -> (Vec<usize>, Option<anyhow::Error>, branch::Ambiguity) {
        self.selected_events_with_pairs(|_, _| {})
    }

    fn selected_events_with_pairs(
        &mut self,
        inspect: impl FnOnce(&[ClaudeEvent], &[(usize, usize)]),
    ) -> (Vec<usize>, Option<anyhow::Error>, branch::Ambiguity) {
        self.link_absent_anchor(None);
        let mut path = self.branch.selected_path();
        let mut rewind = branch::Rewind::default();
        let mut retention = None;
        let mut moved = HashMap::new();
        let mut error = None;
        if !self.compactions.headers.is_empty() {
            let present: HashSet<_> = path.iter().copied().collect();
            let mut queries = self.branch.retained_call_queries(&self.events);
            // Later summaries own records retained more than once.
            let mut segments = self
                .branch
                .retained_segments(&path)
                .into_iter()
                .rev()
                .peekable();
            let mut inherited = None;
            let index_retention = || {
                self.branch
                    .index_retention(&self.events, &self.duplicate_records)
            };
            while let Some((summary, tip)) = segments.next() {
                let tip = inherited.take().or(tip);
                let (retained, end) =
                    match self.retained_nodes(summary, tip.map(|parent| parent.uuid)) {
                        Ok(Some(segment)) => segment,
                        Ok(None) => continue,
                        Err(failure) => {
                            error.get_or_insert(failure);
                            continue;
                        }
                    };
                let (kept, discarded) = if end == retained.len() {
                    (retained, Vec::new())
                } else {
                    let parent = tip.and_then(|parent| parent.node);
                    retention.get_or_insert_with(index_retention).kept_before(
                        summary,
                        &retained,
                        end,
                        parent,
                        &mut rewind,
                    )
                };
                // The physical tail starts after the selected response even when its
                // final blocks follow the cutoff in Claude's retained list.
                if let Some(tip) = tip
                    && let Some(tail) = self.branch.trailing_path(summary, tip)
                {
                    let same_epoch = tail.start == tail.cutoff;
                    if !same_epoch {
                        // Pass the tighter cutoff to the preceding summary only when it kept this record.
                        inherited = self.preceding_cutoff(tail, tip, segments.peek());
                        if inherited.is_none() && present.contains(&tail.cutoff) {
                            error.get_or_insert_with(|| anyhow!("Claude's earlier compaction does not establish where the selected rewind starts"));
                        }
                    }
                    if same_epoch || inherited.is_some() || present.contains(&tail.cutoff) {
                        retention
                            .get_or_insert_with(index_retention)
                            .exclude_unlisted_tail(summary, tail, &kept, &discarded, &mut rewind);
                    }
                }
                if let Some(query) = queries.get_mut(&summary) {
                    rewind.keep_calls(summary, &kept, &self.branch, &self.events, query);
                }
                if self.branch.selected_summary() == Some(summary)
                    || tip.is_some()
                    || kept.iter().any(|index| !present.contains(index))
                {
                    for (rank, index) in kept.into_iter().enumerate() {
                        moved.entry(index).or_insert((summary, rank));
                    }
                }
            }
        }
        rewind.finish_exclusions();
        if rewind.ambiguous_response {
            error.get_or_insert_with(|| {
                anyhow!("Claude's rewound assistant records lack a verifiable response identity")
            });
        }
        if moved
            .keys()
            .any(|&node| rewind.conflicts_with(node, &self.branch, &self.events))
        {
            error.get_or_insert_with(|| {
                anyhow!("retained compaction context references a discarded turn")
            });
        }
        path = rewind.apply_to_path(path, &moved);
        let (mut order, ambiguous) = self.branch.event_order(
            &mut self.events,
            &path,
            &rewind,
            &self.duplicate_records,
            inspect,
        );
        if let Some(summary) = self.branch.replaced_checkpoint()
            && let Err(failure) = self.archive_replaced_checkpoint(summary, &mut order)
        {
            error.get_or_insert(failure);
        }
        (order, error, ambiguous)
    }

    fn archive_replaced_checkpoint(
        &mut self,
        summary: usize,
        order: &mut Vec<usize>,
    ) -> Result<()> {
        let Some(&first) = order.first() else {
            return Ok(());
        };
        let Some((retained, _)) = self.retained_nodes(summary, None)? else {
            return Ok(());
        };
        let mut present: HashSet<_> = order.iter().copied().collect();
        let mut historical = Vec::new();
        for node in std::iter::once(summary).chain(retained) {
            for event in self.branch.events_for(node) {
                if matches!(
                    self.events[event].kind,
                    EventKind::CompactionCreated
                        | EventKind::MessageUser
                        | EventKind::MessageAssistant
                ) && present.insert(event)
                {
                    historical.push(event);
                }
            }
        }
        self.branch
            .detached_events
            .entry(first)
            .or_insert(branch::Detached::RejectedReplacement);
        historical.append(order);
        *order = historical;
        Ok(())
    }

    fn validate_source(&self, session: &SessionRef, sampled: bool) -> Result<()> {
        if !sampled && !self.has_records {
            return Err(anyhow!(
                "Claude session `{}` contains no valid records",
                session.id
            ));
        }
        if !sampled && self.has_conversation && !self.has_main_conversation {
            return Err(anyhow!(
                "Claude session `{}` is a sidechain and cannot be resumed directly",
                session.id
            ));
        }
        Ok(())
    }

    fn reorder_events(&mut self, order: Vec<usize>) {
        let mut slots = std::mem::take(&mut self.events)
            .into_iter()
            .map(Some)
            .collect::<Vec<_>>();
        self.events = Vec::with_capacity(order.len());
        for i in order {
            let event = slots[i].take().expect("unique Claude event index");
            if let Some(detached) = self.branch.detached_events.remove(&i) {
                let (uncertain, anchor, reason) = match detached {
                    branch::Detached::Timestamp { uncertain } => (
                        uncertain,
                        None,
                        "Claude's compaction removed the parent of this continuation",
                    ),
                    branch::Detached::Attachment { anchor } => (
                        true,
                        anchor,
                        "Claude's compaction removed the attachment parent. Only a summary verified by its branch and timestamp can precede this continuation",
                    ),
                    branch::Detached::RejectedReplacement => (
                        true,
                        None,
                        "Claude's rejected replacement makes the earlier checkpoint uncertain. The continuation follows its recorded parents",
                    ),
                };
                self.events.push(ClaudeEvent {
                    kind: EventKind::ProviderEvent,
                    payload: json!({"native_history_unverified": uncertain, "reason": reason}),
                    timestamp: event.timestamp,
                    replay_policy: ReplayPolicy::HistoricalOnly,
                    raw_type: Some(omnis_core::CLAUDE_DETACHED_COMPACTION_PARENT.into()),
                    source: anchor
                        .filter(|id| {
                            self.branch.clock.ordered() && !self.duplicate_records.contains(id)
                        })
                        .map(ClaudeEventSource::DetachedSummary),
                });
            }
            self.events.push(event);
        }
    }

    fn snapshot(
        mut self,
        session: &SessionRef,
        coverage: input::InputCoverage,
    ) -> Result<omnis_ir::CanonicalSnapshot> {
        self.validate_source(session, coverage.sampled)?;
        if coverage.has_gaps() {
            self.branch.clock.interrupt();
        }
        let (preservation_error, ambiguity) = if !self.compactions.headers.is_empty()
            || self.branch.needs_event_order(&self.events)
        {
            let (order, error, ambiguous) = self.selected_events();
            self.reorder_events(order);
            (error, ambiguous)
        } else {
            (None, branch::Ambiguity::default())
        };
        self.branch.fold_response_metadata(&mut self.events);
        let metadata = self.metadata;
        let captured_at = metadata.updated_at.unwrap_or_else(Utc::now);
        let mut builder = EventBuilder::new(Provider::Claude, &session.id);
        let mut latest_summary = None;
        for event in self.events {
            event.append_to(&mut builder, &mut latest_summary);
        }
        coverage.report(&mut builder, metadata.updated_at);
        let mut report_unverified = |raw_type: &str, reason: &str| {
            builder.push(
                EventKind::ProviderEvent,
                json!({"native_history_unverified": true, "reason": reason}),
                metadata.updated_at,
                ReplayPolicy::HistoricalOnly,
                Some(raw_type.to_owned()),
                None,
            );
        };
        if ambiguity.assistants {
            report_unverified(
                "omnisession.ambiguous_assistant_record",
                "Different versions of one Claude assistant record could supply the same response block",
            );
        }
        if ambiguity.missing_source {
            report_unverified(
                "omnisession.missing_tool_source",
                "The transcript is missing the issuing call for a selected Claude tool result",
            );
        }
        if ambiguity.results {
            report_unverified(
                "omnisession.ambiguous_tool_results",
                "Some selected Claude tool results could not be matched to a single call and outcome",
            );
        }
        if self.branch.missing_ancestor() {
            report_unverified(
                "omnisession.missing_parent",
                "Claude's active conversation references a missing parent",
            );
        }
        if self.branch.uncertain_cutoff() || self.branch.uncertain_timestamps() {
            report_unverified(
                "omnisession.uncertain_compaction_parent",
                "Claude's parent link and successive compactions do not establish the same active history",
            );
        }
        let compaction_notice = "omnisession.compaction_preservation_unavailable";
        if let Some(reason) = self
            .branch
            .unverified_retention()
            .and_then(|summary| self.compactions.by_summary.get(&summary)?.error.as_deref())
        {
            report_unverified(compaction_notice, reason);
        }
        if let Some(error) = preservation_error {
            report_unverified(compaction_notice, &error.to_string());
        } else if self.compactions.pending.is_some() {
            report_unverified(
                compaction_notice,
                "Claude wrote a compaction boundary without its summary",
            );
        }
        Ok(builder.snapshot(
            session.clone(),
            self.titles.title(),
            metadata.project_path,
            metadata.git_branch,
            captured_at,
        ))
    }
}

impl ProviderAdapter for ClaudeAdapter {
    fn provider(&self) -> Provider {
        Provider::Claude
    }

    fn probe(&self) -> ProviderInstallation {
        let executable = provider_executable(Provider::Claude);
        ProviderInstallation {
            provider: Provider::Claude,
            installed: executable.is_some()
                || self.projects_root.as_deref().is_some_and(Path::is_dir),
            executable,
            data_root: self.projects_root.clone(),
        }
    }

    fn discovery_notes(&self) -> Vec<String> {
        self.notes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn list_sessions(&self, project: Option<&Path>) -> Result<Vec<NativeSession>> {
        let history = self.history_index();
        let mut sessions = sample_files_concurrently(self.session_files(), |session_files| {
            let mut files = self.projects_root.as_deref().map(ProviderFiles::new);
            let mut buffer = Vec::with_capacity(TITLE_TAIL_BYTES + 1);
            let mut sessions = Vec::new();
            for (id, path) in session_files {
                let Some(path) = files.as_mut().and_then(|files| files.resolve(path)) else {
                    continue;
                };
                let indexed = history.sessions.get(id);
                let mut sample = TranscriptSample::head(&path, indexed.is_none(), &mut buffer).ok();
                let project_path = indexed
                    .map(|entry| entry.project.clone())
                    .or_else(|| sample.as_mut()?.workspace.project_path.take());
                if project.is_some_and(|project| {
                    project_path
                        .as_deref()
                        .is_none_or(|recorded| !paths_match(recorded, project))
                }) {
                    continue;
                }
                let title = sample
                    .as_mut()
                    .and_then(|sample| {
                        // Head titles still apply when the tail cannot be read.
                        let _ = sample.read_tail_metadata(&mut buffer);
                        std::mem::take(&mut sample.titles).title()
                    })
                    .or_else(|| indexed.and_then(|entry| entry.first_prompt.clone()));
                let fallback = sample
                    .as_mut()
                    .map(|sample| std::mem::take(&mut sample.workspace))
                    .unwrap_or_default();
                let created_at = indexed
                    .and_then(|entry| entry.created_at)
                    .or(fallback.created_at);
                let file_updated_at = sample
                    .as_ref()
                    .and_then(|sample| sample.modified)
                    .or_else(|| {
                        std::fs::metadata(&path)
                            .ok()
                            .and_then(|metadata| metadata.modified().ok())
                    })
                    .map(DateTime::<Utc>::from);
                sessions.push(NativeSession {
                    session: SessionRef::new(Provider::Claude, id.clone()),
                    title,
                    project_path,
                    git_branch: fallback.git_branch,
                    created_at,
                    updated_at: file_updated_at
                        .or_else(|| indexed.and_then(|entry| entry.updated_at))
                        .or(fallback.updated_at),
                    updated_at_approximate: file_updated_at.is_some(),
                    event_count: 0,
                    source_path: Some(path),
                });
            }
            sessions
        });
        *self.notes.lock().unwrap_or_else(PoisonError::into_inner) = history.notes;
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
        validate_provider(session, Provider::Claude)?;
        let path = self.session_path(&session.id, source_path)?;
        let (transcript, coverage) = input::read(&path, false)?;
        transcript.snapshot(session, coverage)
    }

    fn preview_session(&self, session: &SessionRef) -> Result<omnis_ir::CanonicalSnapshot> {
        self.preview_session_at(session, None)
    }

    fn preview_session_at(
        &self,
        session: &SessionRef,
        source_path: Option<&Path>,
    ) -> Result<omnis_ir::CanonicalSnapshot> {
        validate_provider(session, Provider::Claude)?;
        let path = self.session_path(&session.id, source_path)?;
        let (transcript, coverage) = input::read(&path, true)?;
        transcript.snapshot(session, coverage)
    }

    fn new_session_plan(&self, target: &LaunchTarget) -> Result<LaunchPlan> {
        Ok(LaunchPlan {
            program: "claude".to_owned(),
            args: target.prompt.iter().cloned().collect(),
            cwd: target.cwd.clone(),
            env: Vec::new(),
        })
    }

    fn launch_plan(&self, session: &SessionRef, target: &LaunchTarget) -> Result<LaunchPlan> {
        validate_provider(session, Provider::Claude)?;
        let mut args = vec!["--resume".to_owned(), session.id.clone()];
        if target.fork {
            args.push("--fork-session".to_owned());
        }
        if let Some(prompt) = &target.prompt {
            args.push(prompt.clone());
        }
        Ok(LaunchPlan {
            program: "claude".to_owned(),
            args,
            cwd: target.cwd.clone(),
            env: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{ClaudeAdapter, ClaudeTranscript, input::InputCoverage};
    use crate::ProviderAdapter;
    use omnis_ir::{EventKind, Provider, ReplayPolicy, SessionRef};
    use serde_json::json;

    fn transcript_from_records(records: &[serde_json::Value]) -> ClaudeTranscript {
        let mut transcript = ClaudeTranscript::default();
        for record in records {
            transcript.observe(record);
        }
        transcript
    }

    fn snapshot_from_records(records: &[serde_json::Value]) -> omnis_ir::CanonicalSnapshot {
        transcript_from_records(records)
            .snapshot(
                &SessionRef::new(Provider::Claude, "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"),
                InputCoverage::default(),
            )
            .expect("synthetic Claude snapshot")
    }

    #[test]
    fn wrong_session_hint_uses_the_requested_claude_transcript() {
        let temporary = tempfile::tempdir().expect("temporary Claude root");
        let root = temporary.path();
        let project = root.join("project");
        std::fs::create_dir_all(&project).expect("Claude project directory");
        let requested = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
        let other = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
        let actual = project.join(format!("{requested}.jsonl"));
        let wrong = project.join(format!("{other}.jsonl"));
        std::fs::write(
            &actual,
            "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"correct session\"}}\n",
        )
        .expect("requested synthetic transcript");
        std::fs::write(
            &wrong,
            "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"wrong session\"}}\n",
        )
        .expect("other synthetic transcript");
        let adapter = ClaudeAdapter::with_root(root);
        let session = SessionRef::new(Provider::Claude, requested);

        let snapshot = adapter
            .read_session_at(&session, Some(&wrong))
            .expect("fall back to requested session");

        assert_eq!(snapshot.events.len(), 1);
        assert_eq!(snapshot.events[0].payload["text"], "correct session");
    }

    #[test]
    fn outside_root_path_hint_is_rejected() {
        let root = tempfile::tempdir().expect("temporary Claude root");
        let outside = tempfile::tempdir().expect("outside path");
        let id = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
        let path = outside.path().join(format!("{id}.jsonl"));
        std::fs::write(
            &path,
            "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"outside\"}}\n",
        )
        .expect("outside synthetic transcript");
        let adapter = ClaudeAdapter::with_root(root.path());
        let session = SessionRef::new(Provider::Claude, id);

        assert!(adapter.read_session_at(&session, Some(&path)).is_err());
    }

    #[test]
    fn fixture_canonicalizes_visible_messages_and_historical_tools() {
        let records = include_str!("../tests/fixtures/claude-session.jsonl")
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect::<Vec<_>>();
        let transcript = transcript_from_records(&records);
        let metadata = transcript.metadata;
        let events = transcript.events;

        assert_eq!(
            metadata.project_path.as_deref(),
            Some(std::path::Path::new("/workspace/demo"))
        );
        assert_eq!(events.len(), 5);
        assert_eq!(events[0].kind, EventKind::MessageUser);
        assert_eq!(events[1].kind, EventKind::ProviderEvent);
        assert_eq!(events[2].kind, EventKind::MessageAssistant);
        assert_eq!(events[3].kind, EventKind::ToolCalled);
        assert_eq!(events[3].replay_policy, ReplayPolicy::HistoricalOnly);
        assert_eq!(events[4].kind, EventKind::ToolCompleted);
    }

    #[test]
    fn sidechain_records_are_not_canonicalized_as_main_context() {
        let records = vec![serde_json::json!({
            "type": "assistant",
            "isSidechain": true,
            "message": {"role": "assistant", "content": "subagent-only"}
        })];

        let transcript = transcript_from_records(&records);
        assert!(transcript.events.is_empty());
        let error = transcript
            .snapshot(
                &SessionRef::new(Provider::Claude, "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"),
                InputCoverage::default(),
            )
            .expect_err("sidechain sessions cannot be resumed");
        assert!(error.to_string().contains("is a sidechain"));
    }

    #[test]
    fn compact_summary_is_contextual_compaction_not_a_user_request() {
        let records = vec![serde_json::json!({
            "type": "user",
            "uuid": "44444444-4444-4444-8444-444444444444",
            "isCompactSummary": true,
            "message": {
                "role": "user",
                "content": "Current objective: finish synthetic migration"
            }
        })];

        let events = transcript_from_records(&records).events;

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, EventKind::CompactionCreated);
        assert_eq!(events[0].replay_policy, ReplayPolicy::Contextual);
        assert_eq!(
            events[0].payload["text"],
            "Current objective: finish synthetic migration"
        );
    }

    #[test]
    fn rewind_follows_the_active_branch_through_progress_and_parallel_results() {
        let id = |i| uuid::Uuid::from_u128(i);
        let message = |i, parent: Option<u128>, role: &str, content| json!({"type": role, "uuid": id(i), "parentUuid": parent.map(id), "message": {"role":role, "content":content}});
        let mut first_call = message(
            2,
            Some(1),
            "assistant",
            json!([{"type":"tool_use","id":"a","name":"Read"}]),
        );
        first_call["message"]["id"] = json!("parallel");
        let mut second_call = message(
            3,
            Some(2),
            "assistant",
            json!([{"type":"tool_use","id":"b","name":"Read"}]),
        );
        second_call["message"]["id"] = json!("parallel");
        let records = vec![
            message(1, None, "user", json!("Shared task")),
            first_call,
            second_call,
            message(
                4,
                Some(2),
                "user",
                json!([{"type":"tool_result","tool_use_id":"a","content":"first"}]),
            ),
            message(
                5,
                Some(3),
                "user",
                json!([{"type":"tool_result","tool_use_id":"b","content":"second"}]),
            ),
            message(6, Some(4), "assistant", json!("Shared reply")),
            message(7, Some(6), "user", json!("Discarded request")),
            message(8, Some(7), "assistant", json!("Discarded reply")),
            json!({"type":"progress","uuid":id(9),"parentUuid":id(6)}),
            message(10, Some(9), "user", json!("Replacement request")),
        ];
        let snapshot = snapshot_from_records(&records);
        let items = snapshot
            .events
            .iter()
            .map(|event| {
                event.payload["text"]
                    .as_str()
                    .or(event.payload["id"].as_str())
                    .or(event.payload["content"].as_str())
                    .unwrap()
            })
            .collect::<Vec<_>>();
        assert_eq!(
            items,
            [
                "Shared task",
                "a",
                "b",
                "first",
                "second",
                "Shared reply",
                "Replacement request"
            ]
        );
    }

    #[test]
    fn missing_parent_does_not_import_an_unrelated_exchange() {
        let id = |n| uuid::Uuid::from_u128(n);
        let records = [
            json!({"uuid":id(1),"type":"user","message":{"content":"Unrelated exchange"}}),
            json!({"uuid":id(2),"parentUuid":id(42),"type":"progress"}),
            json!({"uuid":id(3),"parentUuid":id(2),"type":"user","message":{"content":"Keep this request"}}),
            json!({"type":"assistant","message":{"content":"Legacy reply without a parent field"}}),
        ];
        let snapshot = snapshot_from_records(&records);
        assert_eq!(
            snapshot
                .events
                .iter()
                .filter_map(|event| event.payload["text"].as_str())
                .collect::<Vec<_>>(),
            ["Keep this request", "Legacy reply without a parent field"]
        );
        assert_eq!(
            snapshot.events.last().unwrap().payload["native_history_unverified"],
            true
        );
    }

    #[test]
    fn rewinding_to_a_compacted_suffix_keeps_the_summary_and_selected_turns() {
        let id = |n| uuid::Uuid::from_u128(n);
        let message = |n, parent, role: &str, text: &str| {
            json!({
                "uuid":id(n),"parentUuid":id(parent),"type":role,"message":{"role":role,"content":text}
            })
        };
        let records = [
            json!({"uuid":id(1),"parentUuid":null,"type":"user","message":{"content":"Initial task"}}),
            message(2, 1, "assistant", "Retained reply"),
            json!({"uuid":id(3),"type":"system","subtype":"compact_boundary","logicalParentUuid":id(2),
                "compactMetadata":{"preservedMessages":{"anchorUuid":id(4),"uuids":[id(2)]}}}),
            json!({"uuid":id(4),"parentUuid":id(3),"type":"user","isCompactSummary":true,"message":{"content":"Summary"}}),
            message(5, 2, "user", "Discarded question"),
            message(6, 5, "assistant", "Discarded answer"),
            message(7, 2, "user", "Replacement question"),
        ];
        let snapshot = snapshot_from_records(&records);
        assert_eq!(
            snapshot
                .events
                .iter()
                .map(|event| event.payload["text"].as_str().unwrap())
                .collect::<Vec<_>>(),
            [
                "Initial task",
                "Summary",
                "Retained reply",
                "Replacement question"
            ]
        );
    }

    #[test]
    fn rewinding_into_the_middle_of_a_retained_suffix_discards_its_old_tail() {
        let id = |n| uuid::Uuid::from_u128(n);
        let records = [
            json!({"uuid":id(1),"parentUuid":null,"type":"user","message":{"content":"Original task"}}),
            json!({"uuid":id(2),"parentUuid":id(1),"type":"assistant","message":{"content":"First result"}}),
            json!({"uuid":id(3),"parentUuid":id(2),"type":"user","message":{"content":"Abandoned follow-up"}}),
            json!({"uuid":id(4),"type":"system","subtype":"compact_boundary","logicalParentUuid":id(3),
                "compactMetadata":{"preservedMessages":{"anchorUuid":id(5),"uuids":[id(2),id(3)]}}}),
            json!({"uuid":id(5),"parentUuid":id(4),"type":"user","isCompactSummary":true,"message":{"content":"Summary"}}),
            json!({"uuid":id(6),"parentUuid":id(2),"type":"progress"}),
            json!({"uuid":id(7),"parentUuid":id(6),"type":"user","message":{"content":"Replacement follow-up"}}),
        ];
        let snapshot = snapshot_from_records(&records);
        assert_eq!(
            snapshot
                .events
                .iter()
                .filter_map(|event| event.payload["text"].as_str())
                .collect::<Vec<_>>(),
            [
                "Original task",
                "Summary",
                "First result",
                "Replacement follow-up"
            ]
        );
    }

    #[test]
    fn retained_compaction_context_follows_the_summary_once() {
        let retained = "11111111-1111-4111-8111-111111111111";
        let result = "22222222-2222-4222-8222-222222222222";
        let boundary = "33333333-3333-4333-8333-333333333333";
        let anchor = "44444444-4444-4444-8444-444444444444";
        for metadata in [
            json!({"preservedMessages": {"anchorUuid": anchor, "uuids": [retained, result], "allUuids": [retained, result]}}),
            json!({"preservedSegment": {"headUuid": retained, "anchorUuid": anchor, "tailUuid": result}}),
        ] {
            let records = vec![
                json!({"type": "user", "message": {"role": "user", "content": "Finish the migration"}}),
                json!({"type": "assistant", "uuid": retained, "message": {"role": "assistant", "content": [{"type": "text", "text": "The migration is complete"}, {"type": "tool_use", "id": "verify", "name": "Bash", "input": {"command": "synthetic verification"}}]}}),
                json!({"type": "user", "uuid": result, "parentUuid": retained, "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "verify", "content": "passed"}]}}),
                json!({"type": "system", "uuid": boundary, "subtype": "compact_boundary", "compactMetadata": metadata}),
                json!({"type": "user", "uuid": anchor, "parentUuid": boundary, "isCompactSummary": true, "message": {"role": "user", "content": "The migration still needs a final report"}}),
                json!({"type": "user", "parentUuid": result, "message": {"role": "user", "content": "Start the next task"}}),
            ];
            let snapshot = snapshot_from_records(&records);

            let kinds = snapshot
                .events
                .iter()
                .map(|event| event.kind.clone())
                .collect::<Vec<_>>();
            assert_eq!(
                kinds,
                [
                    EventKind::MessageUser,
                    EventKind::CompactionCreated,
                    EventKind::MessageAssistant,
                    EventKind::ToolCalled,
                    EventKind::ToolCompleted,
                    EventKind::MessageUser
                ]
            );
            assert_eq!(
                snapshot.events[2].payload["text"],
                "The migration is complete"
            );
            assert_eq!(
                snapshot.events[3].replay_policy,
                ReplayPolicy::HistoricalOnly
            );
            assert_eq!(
                snapshot.events[4].replay_policy,
                ReplayPolicy::HistoricalOnly
            );
            assert!(
                snapshot
                    .events
                    .windows(2)
                    .all(|events| events[0].sequence < events[1].sequence)
            );
        }
    }

    #[test]
    fn retained_segment_walks_opaque_records_without_importing_another_branch() {
        let head = "11111111-1111-4111-8111-111111111111";
        let bridge = "22222222-2222-4222-8222-222222222222";
        let tail = "33333333-3333-4333-8333-333333333333";
        let anchor = "44444444-4444-4444-8444-444444444444";
        let boundary = "55555555-5555-4555-8555-555555555555";
        let records = vec![
            json!({"type": "user", "uuid": head, "message": {"role": "user", "content": "Check the result"}}),
            json!({"type": "attachment", "uuid": bridge, "parentUuid": head, "attachment": {"type": "future_unknown", "content": "opaque metadata"}}),
            json!({"type": "assistant", "uuid": tail, "parentUuid": bridge, "message": {"role": "assistant", "content": "The result is ready"}}),
            json!({"type": "assistant", "parentUuid": head, "message": {"role": "assistant", "content": "A superseded branch"}}),
            json!({"type": "system", "uuid": boundary, "subtype": "compact_boundary", "compactMetadata": {"preservedSegment": {"headUuid": head, "anchorUuid": anchor, "tailUuid": tail}}}),
            json!({"type": "user", "uuid": anchor, "parentUuid": boundary, "isCompactSummary": true, "message": {"role": "user", "content": "Context before the result"}}),
        ];
        let snapshot = snapshot_from_records(&records);

        let text = snapshot
            .events
            .iter()
            .map(|event| event.payload["text"].as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            text,
            [
                Some("A superseded branch"),
                Some("Context before the result"),
                Some("Check the result"),
                Some("The result is ready")
            ]
        );
    }

    #[test]
    fn invalid_retained_compaction_context_reports_incomplete_fidelity() {
        let head = "11111111-1111-4111-8111-111111111111";
        let missing = "22222222-2222-4222-8222-222222222222";
        let anchor = "44444444-4444-4444-8444-444444444444";
        let boundary = "55555555-5555-4555-8555-555555555555";
        for metadata in [
            json!({"preservedMessages": {"anchorUuid": anchor, "uuids": [missing]}}),
            json!({"preservedMessages": {"anchorUuid": missing, "uuids": [head]}}),
            json!({"preservedMessages": {"anchorUuid": anchor, "uuids": [head, head]}}),
            json!({"preservedMessages": {"anchorUuid": anchor, "uuids": [anchor]}}),
            json!({"preservedMessages": {"anchorUuid": anchor, "uuids": ["invalid"]}}),
            json!({"preservedSegment": {"headUuid": missing, "anchorUuid": anchor, "tailUuid": head}}),
        ] {
            let records = vec![
                json!({"type": "assistant", "uuid": head, "message": {"role": "assistant", "content": "Keep historical context"}}),
                json!({"type": "system", "uuid": boundary, "subtype": "compact_boundary", "compactMetadata": metadata}),
                json!({"type": "user", "uuid": anchor, "parentUuid": boundary, "isCompactSummary": true, "message": {"role": "user", "content": "Current summary"}}),
            ];
            let snapshot = snapshot_from_records(&records);

            assert_eq!(snapshot.events.len(), 3);
            assert_eq!(
                snapshot.events[0].payload["text"],
                "Keep historical context"
            );
            assert_eq!(snapshot.events[1].kind, EventKind::CompactionCreated);
            assert_eq!(
                snapshot.events[2].source.raw_record_type.as_deref(),
                Some("omnisession.compaction_preservation_unavailable")
            );
            assert_eq!(
                snapshot.events[2].payload["native_history_unverified"],
                true
            );
            assert!(snapshot.events[2].payload["omitted_events"].is_null());
        }
    }

    #[test]
    fn new_turns_use_the_latest_verified_compaction() {
        let id = uuid::Uuid::from_u128;
        let message = |n, parent, role: &str, text: &str| json!({"type":role, "uuid":id(n), "parentUuid":id(parent), "message":{"content":text}});
        let boundary = |n, parent, entries: &[u128]| {
            json!({"type":"system", "subtype":"compact_boundary", "uuid":id(n), "logicalParentUuid":id(parent),
                "compactMetadata":{"preservedMessages":{"anchorUuid":id(n+1), "uuids":entries.iter().copied().map(id).collect::<Vec<_>>()}}})
        };
        let summary = |n, text| {
            let mut record = message(n, n - 1, "user", text);
            record["isCompactSummary"] = json!(true);
            record
        };
        let shared = [
            json!({"type":"user","uuid":id(1),"message":{"content":"Initial task"}}),
            message(2, 1, "assistant", "Retained decision"),
            boundary(3, 2, &[2]),
            summary(4, "Selected summary"),
            message(5, 2, "user", "Continue"),
            message(6, 5, "assistant", "Ready"),
        ];
        let expected = [
            "Initial task",
            "Selected summary",
            "Retained decision",
            "Continue",
            "Ready",
            "Replacement",
        ];
        let mut pending = shared.to_vec();
        pending.push(boundary(7, 6, &[]));
        let snapshot = snapshot_from_records(&pending);
        assert_eq!(
            snapshot
                .events
                .iter()
                .filter_map(|event| event.payload["text"].as_str())
                .collect::<Vec<_>>(),
            &expected[..5]
        );
        assert_eq!(
            snapshot.events.last().unwrap().payload["native_history_unverified"],
            true
        );

        let mut interrupted = shared.to_vec();
        interrupted.extend([boundary(7, 6, &[999]), message(9, 6, "user", "Replacement")]);
        let snapshot = snapshot_from_records(&interrupted);
        assert_eq!(
            snapshot
                .events
                .iter()
                .filter_map(|event| event.payload["text"].as_str())
                .collect::<Vec<_>>(),
            expected
        );

        for (retention, retained) in [
            (&[][..], None),
            (&[5][..], Some("Continue")),
            (&[2][..], Some("Retained decision")),
            (&[999][..], None),
        ] {
            let mut rows = shared.to_vec();
            rows.extend([boundary(7, 6, retention), summary(8, "Latest summary")]);
            for (turn, physical_parent, role, text) in [
                (9, 6, "user", "Replacement"),
                (10, 2, "assistant", "Resume from decision"),
            ] {
                rows.push(message(turn, physical_parent, role, text));
                let snapshot = snapshot_from_records(&rows);
                let handoff = omnis_core::import_trajectory(&snapshot);
                let uncertain = retention == [999] || retention.is_empty();
                assert_eq!(
                    handoff.truncated, uncertain,
                    "{retention:?} parent {physical_parent}"
                );
                assert_eq!(handoff.items.last().unwrap().text, text);
                if uncertain {
                    assert!(!omnis_core::trajectory_search_document(&snapshot).source_complete);
                    let mut recovery = Vec::new();
                    omnis_core::write_recovery_handoff(&snapshot, &mut recovery).unwrap();
                    assert!(
                        String::from_utf8(recovery)
                            .unwrap()
                            .contains("Initial task")
                    );
                } else {
                    let visible: Vec<_> = handoff
                        .items
                        .iter()
                        .map(|item| item.text.lines().last().unwrap())
                        .collect();
                    let expected: Vec<_> = std::iter::once("Latest summary")
                        .chain(retained)
                        .chain([text])
                        .collect();
                    assert_eq!(visible, expected);
                }
            }
        }
    }

    fn transcript_with_an_earlier_retained_segment() -> ClaudeTranscript {
        let id = uuid::Uuid::from_u128;
        transcript_from_records(&[
            json!({"type":"user","uuid":id(1),"message":{"content":"Initial task"}}),
            json!({"type":"assistant","uuid":id(2),"parentUuid":id(1),"message":{"content":"First result"}}),
            json!({"type":"user","uuid":id(3),"parentUuid":id(2),"message":{"content":"Follow-up"}}),
            json!({"type":"assistant","uuid":id(4),"parentUuid":id(3),"message":{"content":"Abandoned result"}}),
            json!({"type":"system","subtype":"compact_boundary","uuid":id(5),"logicalParentUuid":id(4),
                   "compactMetadata":{"preservedSegment":{"headUuid":id(2),"tailUuid":id(4),"anchorUuid":id(6)}}}),
            json!({"type":"user","uuid":id(6),"parentUuid":id(5),"isCompactSummary":true,"message":{"content":"First summary"}}),
            json!({"type":"user","uuid":id(7),"parentUuid":id(6),"message":{"content":"Later task"}}),
            json!({"type":"system","subtype":"compact_boundary","uuid":id(8),"logicalParentUuid":id(7)}),
            json!({"type":"user","uuid":id(9),"parentUuid":id(8),"isCompactSummary":true,"message":{"content":"Later summary"}}),
        ])
    }

    #[test]
    fn returns_through_distinct_progress_links_walk_the_shared_chain_once() {
        let id = uuid::Uuid::from_u128;
        let mut transcript = transcript_from_records(&[
            json!({"type":"user","uuid":id(1),"parentUuid":null,"message":{"content":"Task"}}),
            json!({"type":"assistant","uuid":id(2),"parentUuid":id(1),"message":{"content":"Answer"}}),
            json!({"type":"attachment","uuid":id(98),"parentUuid":id(2)}),
        ]);
        for i in 0..128 {
            transcript.observe(&json!({"type":"progress","uuid":id(100+i),"parentUuid":id(if i == 0 {2} else {99+i})}));
        }
        for (boundary, parent, retained) in [(3, 2, vec![id(2), id(98)]), (5, 4, vec![])] {
            transcript.observe(&json!({"type":"system","subtype":"compact_boundary","uuid":id(boundary),"logicalParentUuid":id(parent),
                "compactMetadata":{"preservedMessages":{"uuids":retained,"anchorUuid":id(boundary+1)}}}));
            transcript.observe(&json!({"type":"user","uuid":id(boundary+1),"parentUuid":id(boundary),"isCompactSummary":true,"message":{"content":"Summary"}}));
        }
        for i in 0..40 {
            transcript.observe(&json!({"type":"user","uuid":id(500+2*i),"parentUuid":id(6),"message":{"content":"Later branch"}}));
            transcript.observe(&json!({"type":"user","uuid":id(501+2*i),"parentUuid":id(227-i),"message":{"content":"Earlier branch"}}));
        }
        assert!(transcript.branch.transparent_links_read <= 128);
        let snapshot = transcript
            .snapshot(
                &SessionRef::new(Provider::Claude, "test"),
                InputCoverage::default(),
            )
            .unwrap();
        // The latest checkpoint retained nothing. The fixture gives the removed
        // parent and the new messages no dates from which to recover an earlier turn.
        let handoff = omnis_core::import_trajectory(&snapshot);
        assert!(handoff.truncated);
        let [notice, request] = handoff.items.as_slice() else {
            panic!("only the coverage notice and the new user request should be imported");
        };
        assert!(notice.text.contains("source history was incomplete"));
        assert_eq!(request.text, "Earlier branch");
        assert!(!omnis_core::trajectory_search_document(&snapshot).source_complete);
        let mut recovery = Vec::new();
        omnis_core::write_recovery_handoff(&snapshot, &mut recovery).unwrap();
        let recovery = String::from_utf8(recovery).unwrap();
        assert!(recovery.contains("Answer") && recovery.contains("Earlier branch"));
    }

    #[test]
    fn distinct_retained_progress_endpoints_skip_an_unrelated_attachment() {
        let id = uuid::Uuid::from_u128;
        let mut transcript = transcript_from_records(&[
            json!({"type":"user","uuid":id(1),"parentUuid":null,"message":{"content":"Task"}}),
            json!({"type":"assistant","uuid":id(2),"parentUuid":id(1),"message":{"content":"Answer"}}),
        ]);
        for i in 0..128 {
            transcript.observe(&json!({"type":"progress","uuid":id(100+i),"parentUuid":id(if i==0 {2} else {99+i})}));
        }
        for row in [
            json!({"type":"assistant","uuid":id(3),"parentUuid":id(227),"message":{"content":"Previous reply"}}),
            json!({"type":"system","subtype":"compact_boundary","uuid":id(4),"logicalParentUuid":id(3),"compactMetadata":{"preservedSegment":{"headUuid":id(2),"tailUuid":id(3),"anchorUuid":id(5)}}}),
            json!({"type":"user","uuid":id(5),"parentUuid":id(4),"isCompactSummary":true,"message":{"content":"First summary"}}),
            json!({"type":"attachment","uuid":id(6),"parentUuid":id(5)}),
            json!({"type":"system","subtype":"compact_boundary","uuid":id(7),"logicalParentUuid":id(5),"compactMetadata":{"preservedMessages":{"uuids":[id(6)],"anchorUuid":id(8)}}}),
            json!({"type":"user","uuid":id(8),"parentUuid":id(7),"isCompactSummary":true,"message":{"content":"Second summary"}}),
        ] {
            transcript.observe(&row);
        }
        for i in 0..40 {
            transcript.observe(&json!({"type":"user","uuid":id(500+2*i),"parentUuid":id(227-i),"message":{"content":"Earlier branch"}}));
            transcript.observe(&json!({"type":"user","uuid":id(501+2*i),"parentUuid":id(8),"message":{"content":"Later branch"}}));
        }
        assert!(transcript.branch.transparent_links_read <= 128);
        let snapshot = transcript
            .snapshot(
                &SessionRef::new(Provider::Claude, "test"),
                InputCoverage::default(),
            )
            .unwrap();
        assert_eq!(
            snapshot
                .events
                .iter()
                .filter_map(|e| e.payload["text"].as_str())
                .next_back(),
            Some("Later branch")
        );
    }

    #[test]
    fn returning_to_a_validated_segment_reuses_its_membership() {
        let id = uuid::Uuid::from_u128;
        let mut transcript = transcript_with_an_earlier_retained_segment();
        let before = transcript.compactions.context_reads.get();
        for i in 0..32 {
            transcript.observe(&json!({"type":"user","uuid":id(100+i*2),"parentUuid":id(9),"message":{"content":"Later branch"}}));
            transcript.observe(&json!({"type":"user","uuid":id(101+i*2),"parentUuid":id(2),"message":{"content":"Earlier branch"}}));
        }
        assert_eq!(transcript.compactions.context_reads.get(), before);
    }

    #[test]
    fn late_progress_resolves_distinct_parents_from_one_read_of_an_older_segment() {
        let id = uuid::Uuid::from_u128;
        let mut transcript = transcript_with_an_earlier_retained_segment();
        let initial_reads = transcript.compactions.context_reads.get();
        for i in 0..32 {
            transcript
                .observe(&json!({"type":"progress", "uuid":id(100+i), "parentUuid":id(2+i%3)}));
        }
        for i in 0..4 {
            transcript.observe(&json!({"type":"progress","uuid":id(150),"parentUuid":id(2+i%3)}));
        }
        assert!(transcript.compactions.context_reads.get() - initial_reads <= 1);
        transcript.observe(&json!({"type":"user","uuid":id(200),"parentUuid":id(131),"message":{"content":"Resume here"}}));
        let snapshot = transcript
            .snapshot(
                &SessionRef::new(Provider::Claude, "test"),
                InputCoverage::default(),
            )
            .unwrap();
        assert_eq!(
            snapshot
                .events
                .iter()
                .filter_map(|event| event.payload["text"].as_str())
                .collect::<Vec<_>>(),
            [
                "Initial task",
                "First summary",
                "First result",
                "Follow-up",
                "Resume here"
            ]
        );
    }

    #[test]
    fn a_later_replacement_does_not_change_a_verified_segment() {
        let id = uuid::Uuid::from_u128;
        for repaired in [false, true] {
            let mut transcript = transcript_with_an_earlier_retained_segment();
            transcript.observe(&json!({"type":"progress","uuid":id(100),"parentUuid":id(2)}));
            transcript.observe(&json!({"type":"progress","uuid":id(3),"parentUuid":id(99)}));
            transcript.observe(&json!({"type":"progress","uuid":id(101),"parentUuid":id(2)}));
            if repaired {
                transcript.observe(&json!({"type":"progress","uuid":id(99),"parentUuid":id(2)}));
            }
            transcript.observe(&json!({"type":"user","uuid":id(102),"parentUuid":id(2),"message":{"content":"Continue"}}));
            let snapshot = transcript
                .snapshot(
                    &SessionRef::new(Provider::Claude, "test"),
                    InputCoverage::default(),
                )
                .unwrap();
            assert!(
                snapshot
                    .events
                    .iter()
                    .any(|event| event.payload["text"] == "First summary")
            );
        }
    }

    #[test]
    fn a_new_message_before_a_verified_cutoff_starts_after_the_retained_context() {
        let id = uuid::Uuid::from_u128;
        let records = [
            json!({"type":"user","uuid":id(1),"message":{"content":"Task"}}),
            json!({"type":"assistant","uuid":id(2),"parentUuid":id(1),"message":{"content":"Original answer"}}),
            json!({"type":"system","uuid":id(3),"subtype":"compact_boundary","logicalParentUuid":id(2),
                   "compactMetadata":{"preservedSegment":{"headUuid":id(2),"tailUuid":id(2),"anchorUuid":id(4)}}}),
            json!({"type":"user","uuid":id(4),"parentUuid":id(3),"isCompactSummary":true,"message":{"content":"Discarded summary"}}),
            json!({"type":"user","uuid":id(5),"parentUuid":id(1),"message":{"content":"Move back before compaction"}}),
            json!({"type":"user","uuid":id(6),"parentUuid":id(2),"message":{"content":"Continue from original answer"}}),
        ];
        let snapshot = snapshot_from_records(&records);
        assert_eq!(
            snapshot
                .events
                .iter()
                .map(|event| event.payload["text"].as_str().unwrap())
                .collect::<Vec<_>>(),
            [
                "Task",
                "Discarded summary",
                "Original answer",
                "Continue from original answer"
            ]
        );
    }

    #[test]
    fn only_the_latest_compaction_boundary_restores_retained_context() {
        let first = "11111111-1111-4111-8111-111111111111";
        let second = "22222222-2222-4222-8222-222222222222";
        let first_anchor = "33333333-3333-4333-8333-333333333333";
        let second_anchor = "44444444-4444-4444-8444-444444444444";
        let first_boundary = "55555555-5555-4555-8555-555555555555";
        let second_boundary = "66666666-6666-4666-8666-666666666666";
        for latest_metadata in [
            json!({}),
            json!({"preservedMessages": {"anchorUuid": second_anchor, "uuids": [second]}}),
        ] {
            let restores_latest = latest_metadata.get("preservedMessages").is_some();
            let records = vec![
                json!({"type": "assistant", "uuid": first, "message": {"role": "assistant", "content": "First result"}}),
                json!({"type": "system", "uuid": first_boundary, "subtype": "compact_boundary", "compactMetadata": {"preservedMessages": {"anchorUuid": first_anchor, "uuids": [first]}}}),
                json!({"type": "user", "uuid": first_anchor, "parentUuid": first_boundary, "isCompactSummary": true, "message": {"role": "user", "content": "First summary"}}),
                json!({"type": "assistant", "uuid": second, "message": {"role": "assistant", "content": "Second result"}}),
                json!({"type": "system", "uuid": second_boundary, "subtype": "compact_boundary", "compactMetadata": latest_metadata}),
                json!({"type": "user", "uuid": second_anchor, "parentUuid": second_boundary, "isCompactSummary": true, "message": {"role": "user", "content": "Second summary"}}),
            ];
            let snapshot = snapshot_from_records(&records);
            let text = snapshot
                .events
                .iter()
                .map(|event| event.payload["text"].as_str())
                .collect::<Vec<_>>();
            let expected = if restores_latest {
                [
                    Some("First result"),
                    Some("First summary"),
                    Some("Second summary"),
                    Some("Second result"),
                ]
            } else {
                [
                    Some("First result"),
                    Some("First summary"),
                    Some("Second result"),
                    Some("Second summary"),
                ]
            };
            assert_eq!(text, expected);
        }
    }

    #[test]
    fn retained_message_list_controls_order_and_ignores_stream_only_ids() {
        let first = "11111111-1111-4111-8111-111111111111";
        let second = "22222222-2222-4222-8222-222222222222";
        let stream_only = "33333333-3333-4333-8333-333333333333";
        let anchor = "44444444-4444-4444-8444-444444444444";
        let boundary = "55555555-5555-4555-8555-555555555555";
        let records = vec![
            json!({"type": "assistant", "uuid": first, "message": {"role": "assistant", "content": "First physical record"}}),
            json!({"type": "user", "uuid": second, "message": {"role": "user", "content": "Second physical record"}}),
            json!({"type": "system", "uuid": boundary, "subtype": "compact_boundary", "compactMetadata": {"preservedMessages": {"anchorUuid": anchor, "uuids": [second, first], "allUuids": [first, second, stream_only]}}}),
            json!({"type": "user", "uuid": anchor, "parentUuid": boundary, "isCompactSummary": true, "message": {"role": "user", "content": "Summary"}}),
        ];
        let snapshot = snapshot_from_records(&records);
        let text = snapshot
            .events
            .iter()
            .map(|event| event.payload["text"].as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            text,
            [
                Some("Summary"),
                Some("Second physical record"),
                Some("First physical record")
            ]
        );
    }

    #[test]
    fn ambiguous_retained_records_and_unrelated_anchors_are_reported() {
        let retained = "11111111-1111-4111-8111-111111111111";
        let anchor = "44444444-4444-4444-8444-444444444444";
        let boundary = "55555555-5555-4555-8555-555555555555";
        for ambiguous_record in [Some(0), Some(1), Some(2), None] {
            let mut records = vec![
                json!({"type": "assistant", "uuid": retained, "message": {"role": "assistant", "content": "Historical result"}}),
                json!({"type": "system", "uuid": boundary, "subtype": "compact_boundary", "compactMetadata": {"preservedMessages": {"anchorUuid": anchor, "uuids": [retained]}}}),
                json!({"type": "user", "uuid": anchor, "parentUuid": boundary, "isCompactSummary": true, "message": {"role": "user", "content": "Summary"}}),
            ];
            if let Some(index) = ambiguous_record {
                let mut collision = records[index].clone();
                if index == 2 {
                    collision["message"]["content"] = json!("Different summary with the same UUID");
                }
                records.insert(index, collision);
            } else {
                records[2]["parentUuid"] = serde_json::Value::Null;
            }
            let snapshot = snapshot_from_records(&records);
            let notice = snapshot.events.last().expect("fidelity notice");
            assert_eq!(
                notice.source.raw_record_type.as_deref(),
                Some("omnisession.compaction_preservation_unavailable")
            );
            assert_eq!(notice.payload["native_history_unverified"], true);
            assert!(notice.payload["omitted_events"].is_null());
        }
    }

    #[test]
    fn a_detached_attachment_keeps_its_summary_after_streamed_usage_is_folded() {
        let id = uuid::Uuid::from_u128;
        let message = |n, parent, role: &str, text: &str| {
            json!({"type":role,"uuid":id(n),"parentUuid":(parent != 0).then(|| id(parent)),
                   "message":{"role":role,"content":text}})
        };
        let chunk = |n, parent, text| {
            let mut row = message(n, parent, "assistant", text);
            row["requestId"] = json!("streamed-request");
            row["message"]["id"] = json!("streamed-response");
            row["message"]["usage"] = json!({"input_tokens":12,"output_tokens":16});
            row
        };
        let mut summary = message(8, 7, "user", "Active summary");
        summary["isCompactSummary"] = json!(true);
        let mut rows = [
            message(1, 0, "user", "Earlier question"),
            chunk(2, 1, "First block"),
            chunk(3, 2, "Second block"),
            message(4, 3, "user", "Choose a reply"),
            message(5, 4, "assistant", "Withdrawn reply"),
            message(6, 4, "assistant", "Retained reply"),
            json!({"type":"system","subtype":"compact_boundary","uuid":id(7),"logicalParentUuid":id(6),
                   "compactMetadata":{"preservedMessages":{"anchorUuid":id(8),"uuids":[id(6)]}}}),
            summary,
            json!({"type":"attachment","uuid":id(9),"parentUuid":id(5),
                   "attachment":{"type":"directory","path":"fixture","content":""}}),
            message(10, 9, "user", "Next request"),
        ];
        for (index, row) in rows.iter_mut().enumerate() {
            row["timestamp"] = json!(format!("2026-01-01T00:00:{index:02}Z"));
            row["isSidechain"] = json!(false);
        }
        let snapshot = snapshot_from_records(&rows);
        let handoff = omnis_core::import_conversation(&snapshot);
        assert!(handoff.truncated);
        assert_eq!(handoff.messages.len(), 2);
        assert!(handoff.messages[0].text.ends_with("\nActive summary"));
        assert_eq!(handoff.messages[1].text, "Next request");
    }

    #[test]
    fn streamed_chunks_of_one_response_count_usage_once() {
        // Claude Code writes one record per content block, each repeating the response's usage.
        let usage =
            json!({"input_tokens": 100, "cache_read_input_tokens": 20, "output_tokens": 30});
        let chunk = |content: serde_json::Value| {
            json!({
                "type": "assistant", "requestId": "req_1",
                "message": {"id": "msg_1", "role": "assistant", "model": "claude-test", "usage": usage, "content": [content]}
            })
        };
        let records = vec![
            json!({"type": "user", "message": {"role": "user", "content": "question"}}),
            chunk(json!({"type": "thinking", "thinking": "hidden"})),
            chunk(json!({"type": "text", "text": "answer"})),
            json!({
                "type": "assistant", "requestId": "req_2",
                "message": {"id": "msg_2", "role": "assistant", "usage": {"input_tokens": 10, "output_tokens": 5}, "content": "follow-up"}
            }),
        ];
        let snapshot = transcript_from_records(&records)
            .snapshot(
                &SessionRef::new(Provider::Claude, "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"),
                InputCoverage::default(),
            )
            .expect("Claude snapshot");

        let preview = omnis_core::session_preview(&snapshot);
        assert_eq!(preview.total_tokens, Some(165));
        assert_eq!(preview.reasoning_mode.as_deref(), Some("thinking"));
        assert_eq!(preview.model.as_deref(), Some("claude-test"));
    }

    #[test]
    fn nonadjacent_chunks_update_the_first_metadata_event_without_reordering() {
        let records = vec![
            json!({
                "type": "assistant", "requestId": "req_1", "timestamp": "2026-01-01T00:00:00Z",
                "message": {"id": "msg_1", "usage": {"output_tokens": 10}, "content": "first chunk"}
            }),
            json!({
                "type": "assistant", "requestId": "req_2",
                "message": {"id": "msg_1", "model": "other-model", "usage": {"output_tokens": 5}, "content": "other response"}
            }),
            json!({
                "type": "assistant", "requestId": "req_1", "timestamp": "2026-01-01T00:01:00Z",
                "message": {"id": "msg_1", "model": "claude-test", "usage": {"output_tokens": 30}, "content": [
                    {"type": "thinking", "thinking": "not visible"},
                    {"type": "text", "text": "later chunk"}
                ]}
            }),
            json!({
                "type": "assistant", "requestId": "req_1",
                "message": {"id": "msg_1", "model": "ignored-later-model", "usage": {"output_tokens": 20}}
            }),
        ];
        let session = SessionRef::new(Provider::Claude, "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa");
        let snapshot = transcript_from_records(&records)
            .snapshot(&session, InputCoverage::default())
            .expect("nonadjacent response chunks");

        assert_eq!(snapshot.events.len(), 5);
        assert_eq!(snapshot.events[0].payload["total_tokens"], 30);
        assert_eq!(snapshot.events[0].payload["model"], "claude-test");
        assert_eq!(snapshot.events[0].payload["reasoning_mode"], "thinking");
        assert_eq!(
            snapshot.events[0].timestamp,
            super::parse_timestamp(Some(&records[0]["timestamp"]))
        );
        assert_eq!(snapshot.events[1].payload["text"], "first chunk");
        assert_eq!(snapshot.events[2].payload["total_tokens"], 5);
        assert_eq!(snapshot.events[3].payload["text"], "other response");
        assert_eq!(snapshot.events[4].payload["text"], "later chunk");
        let thread_id = uuid::Uuid::parse_str(&session.id).expect("session UUID");
        for (index, event) in snapshot.events.iter().enumerate() {
            assert_eq!(event.sequence, index as u64);
            assert_eq!(
                event.event_id,
                uuid::Uuid::new_v5(&thread_id, index.to_string().as_bytes())
            );
        }
    }

    #[test]
    fn full_read_and_preview_preserve_late_metadata_from_excluded_records() {
        let temporary = tempfile::tempdir().expect("temporary Claude root");
        let project = temporary.path().join("project");
        std::fs::create_dir(&project).expect("Claude project directory");
        let session = SessionRef::new(Provider::Claude, "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa");
        let records = [
            json!({
                "type": "assistant", "isSidechain": true, "timestamp": "2026-01-01T00:02:00Z",
                "cwd": "/workspace/earlier", "gitBranch": "earlier", "message": {"content": "excluded"}
            }),
            json!({"type": "custom-title", "customTitle": "Chosen title"}),
            json!({
                "type": "user", "timestamp": "2026-01-01T00:01:00Z",
                "uuid": "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb", "message": {"content": "visible"}
            }),
            json!({
                "type": "assistant", "isMeta": true, "timestamp": "2026-01-01T00:03:00Z",
                "cwd": "/workspace/latest", "gitBranch": "latest", "message": {"content": "excluded"}
            }),
            json!({"type": "ai-title", "aiTitle": "Generated title"}),
            json!({"type": "summary", "summary": "Later generated title", "timestamp": "2026-01-01T00:00:00Z"}),
        ];
        let source = project.join(format!("{}.jsonl", session.id));
        let input = records
            .iter()
            .map(serde_json::Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&source, input).expect("synthetic Claude transcript");
        let adapter = ClaudeAdapter::with_root(temporary.path());
        let full = adapter.read_session(&session).expect("full transcript");
        let preview = adapter
            .preview_session(&session)
            .expect("transcript preview");

        assert_eq!(
            serde_json::to_value(&full).expect("full snapshot"),
            serde_json::to_value(preview).expect("preview snapshot")
        );
        assert_eq!(full.title.as_deref(), Some("Chosen title"));
        assert_eq!(
            full.workspace.root,
            std::path::Path::new("/workspace/latest")
        );
        assert_eq!(full.workspace.git.branch.as_deref(), Some("latest"));
        assert_eq!(
            Some(full.captured_at),
            super::parse_timestamp(Some(&records[3]["timestamp"]))
        );
        assert_eq!(full.events.len(), 1);
        assert_eq!(full.events[0].payload["text"], "visible");
        let thread_id = uuid::Uuid::parse_str(&session.id).expect("session UUID");
        assert_eq!(
            full.events[0].event_id,
            uuid::Uuid::new_v5(&thread_id, b"0:bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb")
        );
    }

    #[test]
    fn image_only_user_turn_keeps_a_placeholder() {
        let image = json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "iVBORw0KGgo="}});
        let records = vec![
            json!({"type": "user", "message": {"role": "user", "content": [image, image]}}),
            json!({"type": "assistant", "message": {"role": "assistant", "content": [{"type": "text", "text": "Two screenshots"}]}}),
        ];

        let events = transcript_from_records(&records).events;

        assert_eq!(events[0].kind, EventKind::MessageUser);
        assert_eq!(events[0].payload, json!({"text": "[2 images omitted]"}));
        assert_eq!(events[1].kind, EventKind::MessageAssistant);
    }
}
