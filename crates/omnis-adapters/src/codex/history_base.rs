//! Reads the immutable prefixes that Codex records in paginated fork and revert rollouts.

mod copies;
mod observed;

pub(super) use observed::matching_header;

use std::{
    collections::HashSet,
    io::{BufReader, Read},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use super::{
    CodexAdapter, rollout_file,
    rollout_identity::{self, HeaderEvidence, HistoryMode, RolloutCatalog},
};
use crate::support::{
    BoundedLine, JsonLineSkips, MAX_STREAMED_TRANSCRIPT_FILE_SIZE,
    MAX_STREAMED_TRANSCRIPT_LINE_SIZE, WorkspaceGap, read_bounded_line_with_budget,
};

// Keep the most recent segments when a thread has an exceptionally long chain of forks or reverts.
const MAX_ANCESTORS: usize = 256;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct Position {
    // Despite the wire name, this identifies one physical rollout, including after a revert.
    thread_id: Uuid,
    end_ordinal_exclusive: u64,
    end_byte_offset: u64,
}

impl Position {
    fn matches_start(self, header: &Value) -> bool {
        self.end_byte_offset > 0
            && self.end_ordinal_exclusive > 0
            && header["ordinal"].as_u64() == Some(self.end_ordinal_exclusive)
    }

    fn confirm_end(self, next: u64, reserved_gap: u64) -> Result<()> {
        ensure!(
            (next..=next.saturating_add(reserved_gap)).contains(&self.end_ordinal_exclusive),
            "Codex history_base byte and ordinal cutoffs disagree"
        );
        Ok(())
    }

    fn source_header(
        self,
        adapter: &CodexAdapter,
        catalog: Option<&RolloutCatalog<'_>>,
    ) -> Result<Option<(PathBuf, Value, HeaderEvidence)>> {
        let Some(path) = adapter.ancestor_path(&self.thread_id.to_string(), catalog)? else {
            return Ok(None);
        };
        let Some(header) = rollout_file::header(&path)?.into_iter().next() else {
            return Ok(None);
        };
        let owner = rollout_identity::thread_id(&path).context("missing Codex rollout owner")?;
        let evidence = HeaderEvidence::read(&header, owner);
        ensure!(
            !matches!(evidence, HeaderEvidence::Foreign | HeaderEvidence::Other),
            "Codex referenced an incompatible source rollout"
        );
        Ok(Some((path, header, evidence)))
    }

    fn source_start(self, header: &Value) -> Result<Option<u64>> {
        let Some(start) = header["ordinal"].as_u64() else {
            return Ok(None);
        };
        ensure!(
            start < self.end_ordinal_exclusive,
            "Codex history_base cutoff precedes the source header"
        );
        Ok(Some(start))
    }
}

pub(super) fn has_valid_start(header: &Value) -> bool {
    let base = &header["payload"]["history_base"];
    if base.is_null() {
        header["ordinal"].as_u64() == Some(0)
    } else {
        Position::deserialize(base).is_ok_and(|position| position.matches_start(header))
    }
}

const UNVERIFIED_MODE: &str = "Codex could not verify this rollout's history mode";

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub(super) enum HistoryBase {
    #[default]
    None,
    Prefix(Position),
    Attachment(Position),
    UnknownMode {
        attached: bool,
        boundary: Option<Position>,
    },
    Invalid,
    InvalidAttachment,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum HistoryScope {
    Visible,
    ModelContext,
}

#[derive(Clone, Default)]
struct VisibleStart {
    attached: Option<AttachmentStart>,
    subagent: Option<u64>,
}

#[derive(Clone, Default)]
struct AttachmentStart {
    ordinal: u64,
    owner: Option<Uuid>,
    hidden_rows: usize,
    hidden_records: usize,
    saved_pairs: Vec<SavedPair>,
    literal_tags: Option<LiteralTags>,
    incomplete: bool,
}

#[derive(Clone)]
struct SavedPair {
    row: usize,
    record: usize,
    child_turn: Option<super::MediaDigest>,
}

#[derive(Clone)]
struct LiteralTags {
    ordinal: u64,
    request_digest: super::MediaDigest,
    indices: Vec<usize>,
}

impl VisibleStart {
    fn confirmed_request(&self, record: &Value) -> Option<&LiteralTags> {
        self.attached
            .as_ref()?
            .literal_tags
            .as_ref()
            .filter(|proof| {
                record["ordinal"].as_u64() == Some(proof.ordinal)
                    && super::media_digest(&record.to_string()) == proof.request_digest
            })
    }

    fn project_request(&self, mut record: Value) -> Value {
        if let Some(literal) = self.confirmed_request(&record) {
            project_literal_parts(&mut record, &literal.indices);
        }
        record
    }

    fn hides_row(&self, row: usize) -> bool {
        self.attached
            .as_ref()
            .is_some_and(|start| row > 1 && row <= start.hidden_rows)
    }

    fn hides_parsed_row(&self, row: usize, record: &Value) -> bool {
        self.hides_row(row)
            && !self.owned_settings(record)
            && !self.in_saved_pair(row, |pair| pair.row)
    }

    fn in_saved_pair(&self, index: usize, end: impl Fn(&SavedPair) -> usize) -> bool {
        self.attached.as_ref().is_some_and(|start| {
            let pos = start.saved_pairs.partition_point(|pair| end(pair) < index);
            start
                .saved_pairs
                .get(pos)
                .is_some_and(|pair| end(pair) - index <= 1)
        })
    }

    fn owned_settings(&self, record: &Value) -> bool {
        // The child keeps its own settings even when Codex hides interrupted parent output.
        record["type"] == "event_msg"
            && record["payload"]["type"] == "thread_settings_applied"
            && self
                .attached
                .as_ref()
                .is_some_and(|start| start.owns(record))
    }

    fn hides_record(&self, index: usize, record: &Value) -> bool {
        self.attached
            .as_ref()
            .is_some_and(|start| index > 1 && index <= start.hidden_records)
            && !self.owned_settings(record)
            && !self.in_saved_pair(index, |pair| pair.record)
    }

    fn ordinal(&self) -> Option<u64> {
        self.attached
            .as_ref()
            .map(|start| start.ordinal)
            .max(self.subagent)
    }
}

/// A paginated subagent can store copied model context between its header and its own first turn.
pub(super) fn subagent_history_start(header: &Value) -> Option<u64> {
    let meta = &header["payload"];
    if !HistoryMode::is_paginated(&meta["history_mode"])
        && !rollout_identity::damaged_paginated_mode(header)
    {
        return None;
    }
    // The cutoff is a body-record ordinal. The header's own ordinal cannot cancel it.
    meta["subagent_history_start_ordinal"]
        .as_u64()
        .filter(|&start| start > 0)
}

fn unplaced_workspace_gap(record: &Value, session: &str) -> WorkspaceGap {
    let payload = &record["payload"];
    match (record["type"].as_str(), payload["type"].as_str()) {
        (Some("turn_context"), _) | (Some("event_msg") | None, Some("thread_rolled_back")) => {
            WorkspaceGap::TurnOrRollback
        }
        (Some("event_msg") | None, Some("thread_settings_applied")) => {
            if payload["thread_id"]
                .as_str()
                .is_some_and(|owner| !super::workspace_check::same_thread(session, owner))
            {
                WorkspaceGap::ForeignSettings
            } else {
                WorkspaceGap::PossibleSettings
            }
        }
        (Some("event_msg") | None, None) => WorkspaceGap::Unclassified,
        _ => WorkspaceGap::Other,
    }
}

impl HistoryBase {
    pub(super) fn from_header(path: &Path, header: &Value) -> Self {
        let meta = &header["payload"];
        if header["type"] == "session_meta" && rollout_identity::damaged_paginated_mode(header) {
            let position = Position::deserialize(&meta["history_base"])
                .ok()
                .filter(|position| position.matches_start(header));
            let attached = meta["attached_thread"] == true
                && position.is_none_or(|pos| attachment_boundary(meta, path, pos));
            return Self::UnknownMode {
                attached,
                boundary: position,
            };
        }
        let invalid = if meta["attached_thread"] == true
            && meta["forked_from_id"]
                .as_str()
                .is_some_and(|id| Uuid::try_parse(id).is_ok())
            && !matches!(
                HistoryMode::read(&meta["history_mode"]),
                Some(HistoryMode::Legacy)
            ) {
            Self::InvalidAttachment
        } else {
            Self::Invalid
        };
        let Some(base) = meta.get("history_base").filter(|v| !v.is_null()) else {
            return if invalid == Self::InvalidAttachment
                && (meta["forked_from_ordinal_exclusive"]
                    .as_u64()
                    .is_some_and(|end| end > 0)
                    || header["ordinal"].as_u64().is_some_and(|start| start > 0))
            {
                Self::InvalidAttachment
            } else {
                Self::None
            };
        };
        let Ok(pos) = Position::deserialize(base) else {
            return invalid;
        };
        match HistoryMode::read(&meta["history_mode"]) {
            Some(HistoryMode::Legacy) => Self::None,
            // The provider's public attachment starts at the parent bookmark. The header ordinal
            // is separate evidence for native resume and for reading this file as an ancestor.
            Some(HistoryMode::Paginated)
                if pos.end_byte_offset > 0
                    && pos.end_ordinal_exclusive > 0
                    && attachment_boundary(meta, path, pos) =>
            {
                Self::Attachment(pos)
            }
            Some(HistoryMode::Paginated) if pos.matches_start(header) => Self::Prefix(pos),
            _ => invalid,
        }
    }

    pub(super) fn mode_is_unverified(self) -> bool {
        matches!(self, Self::UnknownMode { .. })
    }

    pub(super) fn is_some(self) -> bool {
        !matches!(self, Self::None)
    }

    pub(super) fn leaf_only(
        self,
        leaf: &Path,
        scope: HistoryScope,
        subagent_start: Option<u64>,
    ) -> Result<Lineage> {
        let visible_leaf_start = self.visible_start(leaf, scope, subagent_start, None)?;
        Ok(Lineage {
            unavailable: visible_leaf_start
                .attached
                .as_ref()
                .filter(|start| start.incomplete)
                .map(|_| UNCERTAIN_ATTACHMENT),
            visible_leaf_start,
            ..Lineage::default()
        })
    }

    pub(super) fn resolve(
        mut self,
        leaf: &Path,
        scope: HistoryScope,
        subagent_start: Option<u64>,
        adapter: &CodexAdapter,
        catalog: Option<&RolloutCatalog<'_>>,
    ) -> Result<Lineage> {
        let leaf_only = self.leaf_only(leaf, scope, subagent_start)?;
        if self.mode_is_unverified() {
            return Ok(Lineage {
                unavailable: Some(UNVERIFIED_MODE),
                ..leaf_only
            });
        }
        if !self.is_some() {
            return Ok(leaf_only);
        }
        let home = adapter
            .codex_home
            .as_deref()
            .context("Codex home was not found")?;
        let fallback = std::cell::OnceCell::new();
        let mut seen = HashSet::from([rollout_identity::rollout_id(leaf)
            .and_then(|id| Uuid::parse_str(id).ok())
            .context("invalid Codex rollout ID")?]);
        let mut missing_rollout = None;
        let mut segments = Vec::new();
        let mut ancestor_revisions = Vec::new();
        let mut budget = MAX_STREAMED_TRANSCRIPT_FILE_SIZE;
        let unavailable = loop {
            let pos = match self {
                Self::None => break None,
                Self::Invalid | Self::InvalidAttachment => {
                    break Some("Codex recorded an invalid history_base");
                }
                Self::UnknownMode { .. } => {
                    break Some(UNVERIFIED_MODE);
                }
                Self::Attachment(_) if scope == HistoryScope::Visible => break None,
                Self::Prefix(pos) | Self::Attachment(pos) => pos,
            };
            if !seen.insert(pos.thread_id) {
                segments.clear();
                break Some("Codex history_base contains a cycle");
            }
            if segments.len() >= MAX_ANCESTORS || pos.end_byte_offset > budget {
                break Some("Codex inherited history exceeds the read limit");
            }
            budget -= pos.end_byte_offset;
            if let Some(catalog) = catalog {
                ancestor_revisions.push((pos.thread_id, catalog.inventory_revision(pos.thread_id)));
            }
            let (path, meta, evidence) = match pos.source_header(adapter, catalog) {
                Ok(Some(source)) => source,
                Ok(None) => {
                    missing_rollout = Some(pos.thread_id);
                    break Some("Codex referenced an unavailable source rollout");
                }
                Err(error) => {
                    let Some(reason) = source_unavailable(&error) else {
                        return Err(FailedAncestors::attach(error, &ancestor_revisions));
                    };
                    missing_rollout = Some(pos.thread_id);
                    break Some(reason);
                }
            };
            // A restored file can have the right ID but predate the migration that created this bookmark.
            if evidence == HeaderEvidence::Native(HistoryMode::Legacy) {
                missing_rollout = Some(pos.thread_id);
                break Some("Codex referenced a source rollout without paginated history");
            }
            if scope == HistoryScope::ModelContext && !meta["payload"]["cwd"].is_string() {
                break Some(
                    "Codex cannot load an inherited rollout without a string creation directory",
                );
            }
            let mut segment = match Segment::from_header(&path, &meta, pos, scope) {
                Err(error) if access_denied(&error) => {
                    missing_rollout = Some(pos.thread_id);
                    break Some(INACCESSIBLE_SOURCE);
                }
                result => {
                    result.map_err(|error| FailedAncestors::attach(error, &ancestor_revisions))?
                }
            };
            if segment.is_none() && scope == HistoryScope::Visible {
                let catalog =
                    catalog.unwrap_or_else(|| fallback.get_or_init(|| RolloutCatalog::new(home)));
                segment = Segment::from_verified_copy(home, catalog, &path, pos, &mut budget)
                    .map_err(|error| FailedAncestors::attach(error, &ancestor_revisions))?;
            }
            let Some(segment) = segment else {
                missing_rollout = Some(pos.thread_id);
                break Some(incomplete_header_reason(&meta));
            };
            self = segment.base;
            segments.push(segment);
        };

        segments.reverse();
        let mut lineage = Lineage {
            ancestor_revisions,
            segments,
            missing_rollout,
            unavailable: unavailable.or(leaf_only.unavailable),
            ..leaf_only
        };
        lineage.refresh_attachment_warning();
        Ok(lineage)
    }

    fn visible_start(
        self,
        path: &Path,
        scope: HistoryScope,
        subagent: Option<u64>,
        prefix_end: Option<u64>,
    ) -> Result<VisibleStart> {
        if scope == HistoryScope::ModelContext {
            return Ok(VisibleStart::default());
        }
        let boundary = match self {
            Self::Attachment(pos) => Some(Some(pos)),
            Self::UnknownMode {
                attached: true,
                boundary,
            } => Some(Some(boundary.context(
                "Codex could not locate the attached rollout's visible history",
            )?)),
            Self::InvalidAttachment => Some(None),
            _ => None,
        };
        let attached = boundary
            .map(|pos| attached_history_start(path, pos, prefix_end))
            .transpose()?;
        Ok(VisibleStart { attached, subagent })
    }
}

fn incomplete_header_reason(header: &Value) -> &'static str {
    if header["payload"]["id"].as_str().is_none() || header["type"] != "session_meta" {
        "Codex referenced an incomplete source session header"
    } else if rollout_identity::damaged_paginated_mode(header) {
        "Codex referenced a source header without a verifiable history mode"
    } else {
        "Codex referenced a source header without a verifiable ordinal"
    }
}

/// Older headers record only the base. It identifies the fork boundary on the original
/// rollout or when it points directly to the logical parent. A later revert can point elsewhere.
fn attachment_boundary(meta: &Value, path: &Path, pos: Position) -> bool {
    if meta["attached_thread"] != true {
        return false;
    }
    let Some(parent) = meta["forked_from_id"]
        .as_str()
        .and_then(|id| Uuid::parse_str(id).ok())
    else {
        return false;
    };
    if let Some(end) = meta["forked_from_ordinal_exclusive"].as_u64() {
        return pos.end_ordinal_exclusive <= end;
    }
    pos.thread_id == parent || meta["id"].as_str() == rollout_identity::rollout_id(path)
}

/// Codex can write settings and model guidance before an abort inherited from a mid-turn fork.
/// The provider starts visible history after that abort. A child interruption follows its own turn start.
fn attached_history_start(
    path: &Path,
    pos: Option<Position>,
    prefix_end: Option<u64>,
) -> Result<AttachmentStart> {
    let ordinal = pos
        .map_or(0, |base| base.end_ordinal_exclusive)
        .checked_add(1)
        .context("Codex attachment ordinal overflow")?;
    let mut preamble = AttachmentPreamble {
        start: AttachmentStart {
            ordinal,
            ..Default::default()
        },
        parent: pos.map(|base| base.thread_id),
        gap: pos.is_none(),
        ..Default::default()
    };
    // Do not use a later turn to settle a bookmark that ends inside the preamble.
    let source = rollout_file::open(path)?.take(prefix_end.unwrap_or(u64::MAX));
    let mut reader = BufReader::new(source);
    let mut line = Vec::new();
    let mut remaining = MAX_STREAMED_TRANSCRIPT_FILE_SIZE;
    while let Some((kind, length)) = read_bounded_line_with_budget(
        &mut reader,
        MAX_STREAMED_TRANSCRIPT_LINE_SIZE,
        remaining,
        &mut line,
    )? {
        remaining -= length;
        preamble.row += 1;
        let Some(record) = matches!(kind, BoundedLine::Complete)
            .then(|| serde_json::from_slice::<Value>(&line).ok())
            .flatten()
        else {
            preamble.gap = true;
            preamble.request = None;
            preamble.previous_ordinal = None;
            // The leaf reader already reports malformed JSON and oversized rows.
            preamble.start.incomplete |=
                prefix_end.is_some() || line.iter().all(u8::is_ascii_whitespace);
            continue;
        };
        preamble.records += 1;
        if preamble.observe(super::saved_message::project(record)) {
            return Ok(preamble.start);
        }
    }
    if preamble.gap || preamble.responses > 0 && (preamble.settings || prefix_end.is_some()) {
        preamble.start.hidden_rows = preamble.row;
        preamble.start.hidden_records = preamble.records;
        preamble.start.incomplete |= preamble.responses > preamble.start.saved_pairs.len();
    }
    Ok(preamble.start)
}

#[derive(Default)]
struct AttachmentPreamble {
    parent: Option<Uuid>,
    start: AttachmentStart,
    row: usize,
    records: usize,
    previous_ordinal: Option<u64>,
    turn_hint: Option<String>,
    foreign_turn: Option<ForeignTurn>,
    request: Option<Value>,
    settings: bool,
    gap: bool,
    responses: usize,
}

struct ForeignTurn {
    owner: Uuid,
    turn: String,
    item: Option<super::MediaDigest>,
}

impl AttachmentPreamble {
    fn observe_header(&mut self, payload: &Value) {
        self.parent = payload["forked_from_id"]
            .as_str()
            .and_then(|id| Uuid::parse_str(id).ok())
            .or(self.parent);
        self.start.owner = payload["id"]
            .as_str()
            .and_then(|id| Uuid::parse_str(id).ok());
    }

    fn observe_foreign_turn(&mut self, record: &Value) {
        let payload = &record["payload"];
        if record["type"] != "event_msg" {
            return;
        }
        let Some(turn) = payload["turn_id"].as_str().filter(|id| !id.is_empty()) else {
            return;
        };
        let item_id = payload["item"]["id"]
            .as_str()
            .filter(|id| !id.is_empty())
            .map(super::media_digest);
        let owner = payload
            .get("thread_id")
            .or_else(|| record.get("thread_id"))
            .and_then(Value::as_str)
            .and_then(|id| Uuid::parse_str(id).ok());
        match payload["type"].as_str() {
            Some("task_started" | "turn_started" | "item_started") => {
                if let Some(owner) = owner.filter(|id| {
                    Some(*id) != self.start.owner
                        && super::event_scope_matches(record, &id.to_string())
                }) {
                    let item = payload["type"] == "item_started";
                    if !item
                        || self
                            .foreign_turn
                            .as_ref()
                            .is_none_or(|active| active.turn != turn || active.owner != owner)
                    {
                        self.foreign_turn = Some(ForeignTurn {
                            owner,
                            turn: turn.to_owned(),
                            item: item.then_some(item_id).flatten(),
                        });
                    }
                }
            }
            Some("task_complete" | "turn_complete" | "item_completed")
                if self.foreign_turn.as_ref().is_some_and(|active| {
                    active.turn == turn
                        && super::event_scope_matches(record, &active.owner.to_string())
                        && (payload["type"] != "item_completed"
                            || active.item.is_some()
                                && active.item == item_id
                                && owner == Some(active.owner))
                }) =>
            {
                self.foreign_turn = None;
            }
            _ => {}
        }
    }

    fn finish_abort(&mut self, record: &Value) {
        let child_turn = self
            .start
            .owns(record)
            .then(|| {
                record["payload"]["turn_id"]
                    .as_str()
                    .map(super::media_digest)
            })
            .flatten();
        let confirmed = child_turn.is_some()
            && self
                .start
                .saved_pairs
                .iter()
                .rev()
                .find_map(|pair| pair.child_turn)
                == child_turn;
        self.start
            .saved_pairs
            .retain(|pair| confirmed && pair.child_turn.is_some());
        self.start.incomplete |=
            child_turn.is_some() && self.responses > self.start.saved_pairs.len();
    }

    fn save_pair(&mut self, raw: &Value, saved: &Value) {
        let child_turn = (self.start.owns(raw) || self.start.owns(saved))
            .then(|| {
                raw["payload"]["turn_id"]
                    .as_str()
                    .or_else(|| saved["payload"]["turn_id"].as_str())
            })
            .flatten()
            .map(super::media_digest);
        self.start.save_pair(self.row, self.records, child_turn);
    }

    fn matches_saved_pair(&self, raw: &Value, saved: &Value, previous: Option<u64>) -> bool {
        let Some((left, right)) = previous.zip(self.previous_ordinal) else {
            return false;
        };
        if [raw, saved].into_iter().any(|row| {
            row["payload"]
                .get("turn_id")
                .is_some_and(|id| id.as_str().is_none_or(str::is_empty))
        }) {
            return false;
        }
        let owned = self.start.owns(raw) || self.start.owns(saved);
        let turn = raw["payload"]
            .get("turn_id")
            .or_else(|| saved["payload"].get("turn_id"));
        if self.foreign_turn.as_ref().is_some_and(|foreign| {
            !owned
                || turn
                    .and_then(Value::as_str)
                    .is_none_or(|id| id == foreign.turn)
        }) || !(self.gap || owned)
            || left < self.start.ordinal
            || left.checked_add(1) != Some(right)
            || !self.start.matches_scope(raw)
            || !self.start.matches_scope(saved)
            || raw["payload"]
                .get("turn_id")
                .zip(saved["payload"].get("turn_id"))
                .is_some_and(|(raw, saved)| raw != saved)
        {
            return false;
        }
        let identity = |row: &Value| {
            request_text(row).map(|text| {
                super::VisibleMessage::new(row, omnis_ir::EventKind::MessageUser, &text)
            })
        };
        identity(raw)
            .zip(identity(saved))
            .is_some_and(|(raw, saved)| raw.mirrors(&saved))
    }

    fn observe(&mut self, record: Value) -> bool {
        let payload = &record["payload"];
        let previous_ordinal =
            std::mem::replace(&mut self.previous_ordinal, record["ordinal"].as_u64());
        let request = self.request.take();
        self.observe_foreign_turn(&record);
        match (record["type"].as_str(), payload["type"].as_str()) {
            (Some("session_meta"), _) if self.row == 1 => {
                self.observe_header(payload);
            }
            (Some("event_msg"), Some("thread_settings_applied"))
                if payload["thread_settings"].is_object() =>
            {
                self.settings = true;
            }
            (Some("dynamic_tools_updated"), _)
                if super::is_dynamic_tool_catalog(&record) && self.start.owns(&record) => {}
            (Some("response_item"), Some(kind)) => {
                self.responses += 1;
                if kind == "message" && payload["role"] == "user" {
                    self.request = Some(record);
                }
            }
            (Some("event_msg"), Some("turn_aborted"))
                if payload["reason"].is_string()
                    && (self.start.matches_scope(&record)
                        || self.parent.is_some_and(|parent| {
                            super::event_scope_matches(&record, &parent.to_string())
                        })) =>
            {
                if let (Some(prev), Some(abort)) = (previous_ordinal, self.previous_ordinal) {
                    self.start.incomplete |= prev.checked_add(1) != Some(abort);
                }
                self.finish_abort(&record);
                self.start.hidden_rows = self.row;
                self.start.hidden_records = self.records;
                return true;
            }
            (
                Some("event_msg"),
                Some(kind @ ("task_started" | "turn_started" | "item_started" | "item_completed")),
            ) if payload["turn_id"].as_str().is_some_and(|id| !id.is_empty())
                && self.start.matches_scope(&record)
                && self.foreign_turn.as_ref().is_none_or(|active| {
                    payload["turn_id"].as_str() != Some(active.turn.as_str())
                        && self.start.owns(&record)
                })
                && (matches!(kind, "task_started" | "turn_started")
                    || payload["thread_id"].is_string()) =>
            {
                let verified = request.as_ref().and_then(|request| {
                    (kind == "item_completed"
                        && previous_ordinal.is_some()
                        && self.start.owner.is_some_and(|owner| {
                            super::receipt_in_turn(
                                &record,
                                &owner.to_string(),
                                self.turn_hint.as_deref(),
                            )
                        }))
                    .then(|| receipt_literal_parts(request, &record))
                    .flatten()
                });
                let recovered = verified.is_some();
                if let Some(literal_tags) = verified
                    && let Some((ordinal, request)) = previous_ordinal.zip(request)
                {
                    self.start.literal_tags = Some(LiteralTags {
                        ordinal,
                        request_digest: super::media_digest(&request.to_string()),
                        indices: literal_tags,
                    });
                }
                let omitted =
                    self.responses > self.start.saved_pairs.len() + usize::from(recovered);
                if self.gap || omitted {
                    let keep = 1 + usize::from(recovered);
                    self.start.hidden_rows = self.row - keep;
                    self.start.hidden_records = self.records - keep;
                    self.start.incomplete |= omitted;
                }
                return true;
            }
            (Some("event_msg"), Some("user_message"))
                if self.start.ordinal > 1
                    && request.as_ref().is_some_and(|raw| {
                        self.matches_saved_pair(raw, &record, previous_ordinal)
                    }) =>
            {
                self.save_pair(request.as_ref().unwrap(), &record);
            }
            _ => {
                self.gap = true;
                self.start.incomplete = true;
                if self.start.matches_scope(&record) {
                    self.turn_hint = payload["turn_id"].as_str().map(str::to_owned);
                }
            }
        }
        false
    }
}

impl AttachmentStart {
    fn save_pair(&mut self, row: usize, record: usize, child_turn: Option<super::MediaDigest>) {
        if self.saved_pairs.len() < super::MAX_CANONICAL_EVENTS {
            self.saved_pairs.push(SavedPair {
                row,
                record,
                child_turn,
            });
        } else {
            self.incomplete = true;
        }
    }

    fn matches_scope(&self, record: &Value) -> bool {
        self.owner
            .is_some_and(|owner| super::event_scope_matches(record, &owner.to_string()))
    }

    fn owns(&self, record: &Value) -> bool {
        (record["payload"]["thread_id"].is_string() || record["thread_id"].is_string())
            && self.matches_scope(record)
    }
}

pub(super) fn project_literal_parts(record: &mut Value, indices: &[usize]) {
    if let Some(parts) = record["payload"]["content"].as_array_mut() {
        for &index in indices {
            // Generic text retains the receipt's literal spelling and carries no native framing.
            parts[index]["type"] = Value::from("text");
        }
    }
}

pub(super) fn receipt_literal_parts(request: &Value, completed: &Value) -> Option<Vec<usize>> {
    let response = &request["payload"];
    let receipt = &completed["payload"];
    let item = &receipt["item"];
    let id = response["id"].as_str().filter(|id| !id.is_empty())?;
    let adjacent = match (request.get("ordinal"), completed.get("ordinal")) {
        (None, None) => true,
        (Some(left), Some(right)) => left.as_u64()?.checked_add(1)? == right.as_u64()?,
        _ => false,
    };
    if request["type"] != "response_item"
        || response["type"] != "message"
        || response["role"] != "user"
        || completed["type"] != "event_msg"
        || receipt["type"] != "item_completed"
        || item["type"] != "UserMessage"
        || item["id"] != id
        || !adjacent
        || ["thread_id", "turn_id"].iter().any(|&key| {
            [request, response]
                .iter()
                .any(|obj| obj.get(key).is_some_and(|v| v != &receipt[key]))
        })
    {
        return None;
    }
    let (Some(raw), Some(saved)) = (response["content"].as_array(), item["content"].as_array())
    else {
        return None;
    };
    let mut literal_tags = Vec::new();
    let mut saved = saved.iter().peekable();
    let is_image = |part: Option<&Value>| part.is_some_and(|p| p["type"] == "input_image");
    let mut parts = raw.iter().enumerate();
    while let Some((index, part)) = parts.next() {
        let before = index.checked_sub(1).and_then(|previous| raw.get(previous));
        let image_tag = part["type"] == "input_text"
            && part["text"].as_str().is_some_and(|text| {
                super::is_image_wrapper(text, is_image(before), is_image(raw.get(index + 1)))
            });
        if saved
            .next_if(|candidate| same_input_part(part, candidate))
            .is_some()
        {
            if image_tag
                || super::local_audio_path(raw, index + 1).is_some()
                || index
                    .checked_sub(1)
                    .is_some_and(|audio| super::local_audio_path(raw, audio).is_some())
            {
                literal_tags.push(index);
            }
            continue;
        }
        if let Some(path) = super::local_image_path(raw, index + 1)
            && let Some(image) = raw.get(index + 1)
            && image["type"] == "input_image"
            && media_reference(image, "image_url").is_some()
            && raw
                .get(index + 2)
                .is_some_and(|end| end["type"] == "input_text" && end["text"] == "</image>")
            && saved
                .next_if(|candidate| {
                    candidate["type"] == "local_image"
                        && candidate["path"].as_str() == Some(path)
                        && image_detail(image)
                            .is_some_and(|detail| image_detail(candidate) == Some(detail))
                })
                .is_some()
        {
            parts.nth(1);
            continue;
        }
        if let Some(path) = super::local_audio_path(raw, index + 1)
            && saved
                .next_if(|candidate| {
                    candidate["type"] == "local_audio" && candidate["path"].as_str() == Some(path)
                })
                .is_some()
        {
            parts.nth(1);
            continue;
        }
        // Local image framing can be absent from the user's saved input.
        if !image_tag {
            return None;
        }
    }
    (!raw.is_empty() && saved.next().is_none()).then_some(literal_tags)
}

// Compare native input references without resolving URLs or local attachment paths.
fn same_input_part(raw: &Value, saved: &Value) -> bool {
    match (raw["type"].as_str(), saved["type"].as_str()) {
        (Some("input_text"), Some("text")) => {
            raw["text"].is_string() && raw["text"] == saved["text"]
        }
        (Some("input_image"), Some("image")) => {
            let Some(reference) = media_reference(raw, "image_url") else {
                return false;
            };
            media_reference(saved, "image_url") == Some(reference)
                && image_detail(raw).is_some_and(|detail| image_detail(saved) == Some(detail))
        }
        (Some("input_audio"), Some("audio")) => media_reference(raw, "audio_url")
            .is_some_and(|reference| media_reference(saved, "audio_url") == Some(reference)),
        _ => false,
    }
}

fn media_reference<'a>(
    part: &'a Value,
    url_field: &'static str,
) -> Option<(&'static str, &'a str)> {
    match (&part[url_field], &part["file_id"]) {
        (url, Value::Null) => Some((url_field, url.as_str().filter(|url| !url.is_empty())?)),
        (Value::Null, file) => Some(("file_id", file.as_str().filter(|file| !file.is_empty())?)),
        _ => None,
    }
}

fn image_detail(part: &Value) -> Option<&str> {
    // Codex supplies high detail when it converts a user image with no explicit detail.
    match part.get("detail").filter(|detail| !detail.is_null()) {
        None => Some("high"),
        Some(detail) => detail
            .as_str()
            .filter(|level| matches!(*level, "auto" | "low" | "high" | "original")),
    }
}

#[derive(Clone)]
struct Segment {
    visible_start: Option<VisibleStart>,
    subagent: Option<u64>,
    verified_revision: Option<omnis_ir::SourceRevision>,
    recovery: Option<RecoveryPlan>,
    path: PathBuf,
    start: u64,
    position: Position,
    base: HistoryBase,
}

impl Segment {
    fn confirm_header_read(&self, next: u64) -> Result<()> {
        ensure!(
            next > self.start,
            "Codex history_base source header changed"
        );
        Ok(())
    }

    fn uncertain_attachment(&self) -> bool {
        self.visible_start
            .as_ref()
            .and_then(|start| start.attached.as_ref())
            .is_some_and(|start| start.incomplete)
    }

    fn saved_attachment_start(meta: &Value, path: &Path, start: u64) -> bool {
        if let Some(end) = meta
            .get("forked_from_ordinal_exclusive")
            .filter(|value| !value.is_null())
        {
            return end.as_u64() == Some(start);
        }
        let bookmark = &meta["history_base"];
        let (parent, cutoff) = match bookmark.as_array() {
            Some(tuple) if tuple.len() == 3 => (&tuple[0], &tuple[1]),
            _ => (&bookmark["thread_id"], &bookmark["end_ordinal_exclusive"]),
        };
        start > 0
            && cutoff.as_u64() == Some(start)
            && parent.as_str().is_some_and(|id| {
                Uuid::try_parse(id).is_ok()
                    && (Some(id) == meta["forked_from_id"].as_str()
                        || meta["id"].as_str() == rollout_identity::rollout_id(path))
            })
    }

    fn from_header(
        path: &Path,
        header: &Value,
        position: Position,
        scope: HistoryScope,
    ) -> Result<Option<Self>> {
        let Some(start) = position.source_start(header)? else {
            return Ok(None);
        };
        let owner = rollout_identity::thread_id(path).context("missing Codex rollout owner")?;
        let base = HistoryBase::from_header(path, header);
        let local_attachment = scope == HistoryScope::Visible
            && base == HistoryBase::InvalidAttachment
            && header["type"] == "session_meta"
            && header["payload"]["id"].as_str() == Some(owner)
            && HistoryMode::is_paginated(&header["payload"]["history_mode"])
            && Self::saved_attachment_start(&header["payload"], path, start);
        if !local_attachment
            && HeaderEvidence::read(header, owner) != HeaderEvidence::Native(HistoryMode::Paginated)
        {
            return Ok(None);
        }
        let subagent = subagent_history_start(header);
        let visibility = base.visible_start(path, scope, subagent, Some(position.end_byte_offset));
        let visible_start = match visibility {
            Ok(start) => Some(start),
            Err(error) if rollout_file::compressed(path) && error.is::<std::io::Error>() => None,
            Err(error) => return Err(error),
        };
        Ok(Some(Self {
            visible_start,
            subagent,
            verified_revision: None,
            recovery: None,
            path: path.to_owned(),
            start,
            position,
            base,
        }))
    }

    fn record_ordinal(
        &self,
        record: &Value,
        expected: u64,
        max_reserved_gap: u64,
        malformed: bool,
        hidden: bool,
    ) -> Result<Option<u64>> {
        if hidden && record["ordinal"].as_u64() != Some(expected) {
            return Ok(None);
        }
        if malformed {
            // A broken separator can join or split rows. Later ordinals must still follow earlier
            // records and stay inside the bookmark.
            let index = record["ordinal"].as_u64();
            ensure!(
                index
                    .is_none_or(|ordinal| ordinal >= expected
                        && ordinal < self.position.end_ordinal_exclusive),
                "Codex history_base source ordinal is inconsistent"
            );
            return Ok(index);
        }
        let Some(ordinal) = record["ordinal"].as_u64() else {
            ensure!(
                expected > self.start && record["type"].is_string(),
                "missing Codex source ordinal"
            );
            return Ok(None);
        };
        // A copied subagent prefix can reserve ordinals without a record at each one.
        if expected > self.start
            && ordinal > expected
            && (ordinal - expected <= max_reserved_gap
                || self.subagent.is_some_and(|first| ordinal <= first))
        {
            ensure!(
                ordinal < self.position.end_ordinal_exclusive,
                "Codex history_base byte and ordinal cutoffs disagree"
            );
            return Ok(Some(ordinal));
        }
        ensure!(
            ordinal == expected,
            "Codex history_base source ordinal is inconsistent"
        );
        if ordinal == self.start {
            ensure!(
                record["type"] == "session_meta"
                    && HistoryBase::from_header(&self.path, record) == self.base
                    && subagent_history_start(record) == self.subagent
                    && HistoryMode::is_paginated(&record["payload"]["history_mode"])
                    && record["payload"]["id"].as_str() == rollout_identity::thread_id(&self.path),
                "Codex history_base source header changed"
            );
        }
        Ok(Some(ordinal))
    }
}

#[derive(Default)]
pub(super) struct Lineage {
    pub(super) ancestor_revisions: Vec<(Uuid, Uuid)>,
    segments: Vec<Segment>,
    first_available: usize,
    visible_leaf_start: VisibleStart,
    pub(super) missing_rollout: Option<Uuid>,
    pub(super) unavailable: Option<&'static str>,
}

#[derive(Debug)]
pub(super) struct FailedAncestors {
    pub(super) revisions: Vec<(Uuid, Uuid)>,
    message: String,
}

impl FailedAncestors {
    pub(super) fn attach(error: anyhow::Error, revisions: &[(Uuid, Uuid)]) -> anyhow::Error {
        if revisions.is_empty() {
            error
        } else {
            let message = error.to_string();
            error.context(Self {
                revisions: revisions.to_vec(),
                message,
            })
        }
    }
}

impl std::fmt::Display for FailedAncestors {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

#[derive(Debug)]
struct DamagedPrefixes(Vec<(usize, &'static str, Option<RecoveryPlan>)>);

impl std::fmt::Display for DamagedPrefixes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("damaged Codex source rollout prefix")
    }
}

impl std::error::Error for DamagedPrefixes {}

impl Lineage {
    fn refresh_attachment_warning(&mut self) {
        if self
            .unavailable
            .is_some_and(|reason| reason != UNCERTAIN_ATTACHMENT)
        {
            return;
        }
        let uncertain = std::iter::once(&self.visible_leaf_start)
            .chain(
                self.segments[self.first_available..]
                    .iter()
                    .filter_map(|s| s.visible_start.as_ref()),
            )
            .any(|visible| {
                visible
                    .attached
                    .as_ref()
                    .is_some_and(|start| start.incomplete)
            });
        self.unavailable = uncertain.then_some(UNCERTAIN_ATTACHMENT);
    }

    pub(super) fn project<T>(
        &mut self,
        adapter: &CodexAdapter,
        catalog: Option<&RolloutCatalog<'_>>,
        read: impl Fn(&Self) -> Result<T>,
    ) -> Result<T> {
        let Some(home) = adapter.codex_home.as_deref() else {
            return read(self);
        };
        let standalone = std::cell::OnceCell::new();
        let catalog =
            || catalog.unwrap_or_else(|| standalone.get_or_init(|| RolloutCatalog::new(home)));
        let mut budget = MAX_STREAMED_TRANSCRIPT_FILE_SIZE;
        // An intact backup can verify settings and turns omitted from a damaged preamble.
        for segment in &mut self.segments {
            if segment.uncertain_attachment()
                && let Ok(Some(copy)) = segment.verified_copy(
                    home,
                    catalog(),
                    &mut budget,
                    observed::agrees_across_split_lines,
                )
                && !copy.uncertain_attachment()
            {
                *segment = copy;
            }
        }
        self.refresh_attachment_warning();
        let error = match read(self) {
            Ok(value) => return Ok(value),
            Err(error) => error,
        };
        let Some(damage) = error.downcast_ref::<DamagedPrefixes>() else {
            return Err(error);
        };
        for &(index, reason, ref plan) in damage.0.iter().rev() {
            if let Some(verified) = self.segments[index].verified_copy(
                home,
                catalog(),
                &mut budget,
                observed::agrees,
            )? {
                self.segments[index] = verified;
            } else {
                let segment = &mut self.segments[index];
                segment.recovery.clone_from(plan);
                if let Some(plan) = plan {
                    segment.verified_revision = plan.revision;
                    if plan.preserves_ancestors() {
                        self.unavailable.get_or_insert(reason);
                        continue;
                    }
                }
                self.first_available = if segment.recovery.is_some() {
                    index
                } else {
                    index + 1
                };
                self.unavailable = Some(reason);
                break;
            }
        }
        self.refresh_attachment_warning();
        read(self)
    }

    pub(super) fn attached_leaf(&self) -> bool {
        self.visible_leaf_start.attached.is_some()
    }

    pub(super) fn project_leaf_request(&self, record: Value) -> Value {
        self.visible_leaf_start.project_request(record)
    }

    pub(super) fn leaf_row_visibility(&self, record: &Value, row: usize) -> Option<bool> {
        if self.visible_leaf_start.hides_parsed_row(row, record) {
            return Some(false);
        }
        self.leaf_ordinal_visibility(record)
    }

    pub(super) fn leaf_record_visibility(&self, record: &Value, index: usize) -> Option<bool> {
        if self.visible_leaf_start.hides_record(index, record) {
            return Some(false);
        }
        self.leaf_ordinal_visibility(record)
    }

    fn leaf_ordinal_visibility(&self, record: &Value) -> Option<bool> {
        let Some(start) = self.visible_leaf_start.ordinal() else {
            return Some(true);
        };
        if record["type"] == "session_meta" {
            return Some(true);
        }
        record["ordinal"].as_u64().map(|ordinal| ordinal >= start)
    }

    pub(super) fn visit_leaf(
        &self,
        session: &str,
        read: impl FnOnce(&mut dyn FnMut(Value, JsonLineSkips) -> Result<()>) -> Result<JsonLineSkips>,
        mut visit: impl FnMut(Value, JsonLineSkips) -> Result<()>,
    ) -> Result<JsonLineSkips> {
        self.visit_confirmed_leaf(session, read, |row, mark, _| visit(row, mark))
    }

    pub(super) fn visit_confirmed_leaf(
        &self,
        session: &str,
        read: impl FnOnce(&mut dyn FnMut(Value, JsonLineSkips) -> Result<()>) -> Result<JsonLineSkips>,
        mut visit: impl FnMut(Value, JsonLineSkips, bool) -> Result<()>,
    ) -> Result<JsonLineSkips> {
        let read = |visit: &mut dyn FnMut(Value, JsonLineSkips, bool) -> Result<()>| {
            let mut receipts = super::RequestReceipts::new(session, true);
            let skipped =
                read(&mut |record, mark| receipts.observe_confirmed(record, mark, &mut *visit))?;
            receipts.flush(|row, mark| visit(row, mark, false))?;
            Ok(skipped)
        };
        if self.visible_leaf_start.ordinal().is_none() {
            return read(&mut |row, mark, confirmed| {
                visit(row, mark, confirmed && self.attached_leaf())
            });
        }
        let mut unplaced = JsonLineSkips::default();
        let mut row = 0;
        let skipped = read(&mut |record, skipped, confirmed| {
            row += 1;
            match self.leaf_record_visibility(&record, row) {
                Some(true) => visit(
                    self.visible_leaf_start.project_request(record),
                    skipped.combined(unplaced),
                    confirmed && self.attached_leaf(),
                )?,
                Some(false) => {}
                None => unplaced.observe_unplaced(unplaced_workspace_gap(&record, session)),
            }
            Ok(())
        })?;
        Ok(skipped.combined(unplaced))
    }

    pub(super) fn hidden_context_types(&self) -> impl Iterator<Item = &'static str> {
        let starts = || {
            std::iter::once(&self.visible_leaf_start).chain(
                self.segments[self.first_available..]
                    .iter()
                    .filter_map(|s| s.visible_start.as_ref()),
            )
        };
        [
            (
                starts().any(|s| s.attached.is_some()),
                omnis_core::CODEX_ATTACHED_PARENT_CONTEXT,
            ),
            (
                starts().any(|s| s.subagent.is_some_and(|start| start > 1)),
                omnis_core::CODEX_SUBAGENT_MODEL_CONTEXT,
            ),
        ]
        .into_iter()
        .filter_map(|(hidden, record_type)| hidden.then_some(record_type))
    }

    pub(super) fn source_paths<'a>(&'a self, leaf: &'a Path) -> impl Iterator<Item = &'a Path> {
        self.segments
            .iter()
            .map(|segment| segment.path.as_path())
            .chain(std::iter::once(leaf))
    }

    pub(super) fn inherited_bytes(&self) -> u64 {
        self.segments[self.first_available..]
            .iter()
            .map(|segment| segment.position.end_byte_offset)
            .sum()
    }

    pub(super) fn provider_can_read(
        &self,
        home: &Path,
        scan: &super::CodexFileScan,
    ) -> Result<bool> {
        if self.unavailable.is_some()
            || !super::rollout_identity::provider_can_read_ancestors(
                home,
                scan,
                self.segments.iter().map(|segment| segment.path.as_path()),
            )?
        {
            return Ok(false);
        }
        let mut budget = rollout_file::NativeReadBudget::default();
        Ok(self.segments.iter().all(|segment| {
            budget
                .contains_prefix(&segment.path, segment.position.end_byte_offset, |reader| {
                    Ok(matches!(
                        read_prefix_from(segment, reader, &mut |_, _| Ok(()))?,
                        PrefixRead::Complete(0)
                    ))
                })
                .unwrap_or(false)
        }))
    }

    pub(super) fn visit(
        &self,
        mut visit: impl FnMut(InheritedRecord, usize, bool) -> Result<()>,
    ) -> Result<usize> {
        let mut skipped = 0;
        let mut damage = Vec::new();
        for (index, segment) in self.segments.iter().enumerate().skip(self.first_available) {
            let prefix = if damage.is_empty() {
                read_prefix(segment, &mut |record, local| {
                    if let InheritedRecord::Native(value) = &record
                        && value["type"] == "event_msg"
                        && matches!(
                            value["payload"]["type"].as_str(),
                            Some(
                                "task_started"
                                    | "turn_started"
                                    | "task_complete"
                                    | "turn_complete"
                                    | "turn_aborted"
                            )
                        )
                        && !owned_event(value, &segment.path)
                    {
                        return Ok(());
                    }
                    visit(record, skipped + local, segment.recovery.is_some())
                })
            } else {
                // Check every newer bookmark in this pass. Multiple damaged sources need one retry.
                read_prefix(segment, &mut |_, _| Ok(()))
            }?;
            match prefix {
                PrefixRead::Complete(count) => {
                    skipped += count;
                    if damage.is_empty() {
                        visit(
                            InheritedRecord::SegmentEnd(segment.position.end_ordinal_exclusive),
                            skipped,
                            segment.recovery.is_some(),
                        )?;
                    }
                }
                PrefixRead::Partial(reason, plan) => damage.push((index, reason, Some(plan))),
                PrefixRead::Damaged(reason) => {
                    damage.push((index, reason, None));
                }
            }
        }
        if damage.is_empty() {
            Ok(skipped)
        } else {
            Err(DamagedPrefixes(damage).into())
        }
    }
}

const UNCERTAIN_ATTACHMENT: &str =
    "Codex could not verify which messages were visible before the attachment bookmark";
const COMPRESSED_PREFIX_DAMAGE: &str =
    "Codex could not read a compressed source rollout before its fork cutoff";
const INACCESSIBLE_SOURCE: &str = "Codex could not read a referenced source rollout";
const MISSING_ORDINAL: &str = "Codex referenced a source record without a verifiable ordinal";
const MALFORMED_ROWS: &str =
    "Codex referenced a source rollout with malformed JSON before its cutoff";

fn source_unavailable(error: &anyhow::Error) -> Option<&'static str> {
    if error.is::<rollout_identity::InaccessibleInventory>() {
        Some("Codex could not search all directories for a referenced rollout")
    } else if access_denied(error) {
        Some(INACCESSIBLE_SOURCE)
    } else if error.is::<rollout_file::CompressedHeaderError>() {
        Some("Codex could not decode a referenced compressed rollout header")
    } else {
        None
    }
}

fn access_denied(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|error| error.kind() == std::io::ErrorKind::PermissionDenied)
    })
}

pub(super) enum InheritedRecord {
    SegmentEnd(u64),
    Native(Value),
    ConfirmedRequest(Value),
    WithheldRequest { client_submission: bool },
    WithheldReply,
    WithheldWorkspace,
}

impl InheritedRecord {
    pub(super) fn request(record: Value, confirmed: bool) -> Self {
        if confirmed {
            Self::ConfirmedRequest(record)
        } else {
            Self::Native(record)
        }
    }
}

enum PrefixRead {
    Complete(usize),
    Damaged(&'static str),
    Partial(&'static str, RecoveryPlan),
}

/// A bookmark counts uncompressed bytes, including line terminators. Check its exclusive ordinal
/// too, so an inconsistent bookmark never imports a turn after the fork.
fn read_prefix(
    segment: &Segment,
    visit: &mut dyn FnMut(InheritedRecord, usize) -> Result<()>,
) -> Result<PrefixRead> {
    let revision = CodexAdapter::index_file_revision(&segment.path);
    ensure!(
        segment
            .verified_revision
            .is_none_or(|expected| Some(expected) == revision),
        "selected alternate Codex source rollout changed"
    );
    let mut source = match rollout_file::open(&segment.path) {
        Err(error) if access_denied(&error) => return Ok(PrefixRead::Damaged(INACCESSIBLE_SOURCE)),
        result => result?,
    };
    let owner = rollout_identity::thread_id(&segment.path).unwrap_or_default();
    let mut receipts = super::RequestReceipts::new(owner, false);
    let mut projection = read_prefix_from(segment, &mut source, &mut |record, skipped| {
        if let InheritedRecord::Native(row) = record {
            receipts.observe_confirmed(row, skipped, |row, mark, confirmed| {
                visit(
                    InheritedRecord::request(
                        row,
                        confirmed
                            && segment
                                .visible_start
                                .as_ref()
                                .is_some_and(|start| start.attached.is_some()),
                    ),
                    mark,
                )
            })
        } else {
            receipts.flush(|row, mark| visit(InheritedRecord::Native(row), mark))?;
            visit(record, skipped)
        }
    })?;
    receipts.flush(|row, mark| visit(InheritedRecord::Native(row), mark))?;
    if let PrefixRead::Partial(_, plan) = &mut projection {
        let Some(revision) = revision else {
            return Ok(PrefixRead::Damaged(INACCESSIBLE_SOURCE));
        };
        plan.revision = Some(revision);
    }
    if segment.verified_revision.is_some() || matches!(projection, PrefixRead::Partial(..)) {
        ensure!(
            CodexAdapter::index_file_revision(&segment.path) == revision,
            "selected Codex source rollout changed during projection"
        );
    }
    Ok(projection)
}

fn unreadable_prefix(path: &Path, error: std::io::Error) -> Result<PrefixRead> {
    if error.kind() == std::io::ErrorKind::PermissionDenied {
        Ok(PrefixRead::Damaged(INACCESSIBLE_SOURCE))
    } else if rollout_file::compressed(path) {
        Ok(PrefixRead::Damaged(COMPRESSED_PREFIX_DAMAGE))
    } else {
        Err(error).context("reading referenced Codex history up to its byte cutoff")
    }
}

#[derive(Clone, Debug, Default)]
struct RecoveryPlan {
    revision: Option<omnis_ir::SourceRevision>,
    // Both anchors place every surviving row in the interval, including omitted bodies.
    positions: Vec<(usize, usize, u64)>,
    confirmed_turns: Vec<(usize, usize)>,
    corroborated_mirrors: Vec<usize>,
    withheld_mirrors: Vec<usize>,
    withheld_requests: Vec<usize>,
    opaque_through: usize,
    unresolved: bool,
}

impl RecoveryPlan {
    fn preserves_ancestors(&self) -> bool {
        !self.unresolved
    }

    fn ordinal(&self, row: usize) -> Option<u64> {
        let index = self.positions.partition_point(|&(_, end, _)| end <= row);
        let &(first, _, ordinal) = self.positions.get(index)?;
        ordinal.checked_add(u64::try_from(row.checked_sub(first)?).ok()?)
    }

    fn withheld_workspace(&self, row: usize, start: &VisibleStart, record: &Value) -> bool {
        record["type"] == "turn_context"
            && start.ordinal().is_none_or(|first| {
                record["ordinal"]
                    .as_u64()
                    .or_else(|| self.ordinal(row))
                    .is_none_or(|ordinal| ordinal >= first)
            })
    }

    fn withheld_request(&self, row: usize, start: &VisibleStart) -> bool {
        self.withheld_requests.binary_search(&row).is_ok()
            && self
                .ordinal(row)
                .is_some_and(|ordinal| start.ordinal().is_none_or(|begin| ordinal >= begin))
    }

    fn withheld_reply(
        &self,
        row: usize,
        start: &VisibleStart,
        record: &Value,
        path: &Path,
    ) -> bool {
        row > self.opaque_through
            && self
                .ordinal(row)
                .is_some_and(|ordinal| start.ordinal().is_none_or(|begin| ordinal >= begin))
            && message_role(record) == Some("assistant")
            && owned_event(record, path)
            && !(record["type"] == "event_msg"
                && super::string_at(&record["payload"], &[&["message"], &["text"]])
                    == Some(omnis_core::CODEX_IMPORT_MARKER))
    }

    fn withheld_record(
        &self,
        row: usize,
        start: &VisibleStart,
        record: &Value,
        path: &Path,
    ) -> Option<InheritedRecord> {
        if self.withheld_request(row, start) {
            Some(InheritedRecord::WithheldRequest {
                client_submission: super::client_submission(record),
            })
        } else if self.withheld_reply(row, start, record, path) {
            Some(InheritedRecord::WithheldReply)
        } else if self.withheld_workspace(row, start, record) {
            Some(InheritedRecord::WithheldWorkspace)
        } else {
            None
        }
    }

    fn confirmed_turn(&self, row: usize) -> bool {
        let index = self.confirmed_turns.partition_point(|&(_, end)| end < row);
        self.confirmed_turns
            .get(index)
            .is_some_and(|&(start, _)| row >= start)
    }
}

#[derive(Default)]
struct SequenceEvidence {
    plan: RecoveryPlan,
    anchor: Option<(usize, u64)>,
    last_row: usize,
    continuous: bool,
    pending_rollback: usize,
    turn: Option<(usize, String, bool)>,
    unnumbered_messages: Vec<(usize, bool)>,
    candidates: Vec<(usize, bool, usize, bool)>,
    request_slots: Vec<(usize, bool, usize)>,
    last_request: Option<super::MirrorGroup>,
}

impl SequenceEvidence {
    fn unknown(&mut self, row: usize) {
        self.plan.unresolved = true;
        self.continuous = false;
        self.turn = None;
        self.plan.opaque_through = row;
    }

    fn anchor(&mut self, row: usize, ordinal: u64) {
        if let Some((previous, position)) = self.anchor.replace((row, ordinal))
            && previous + 1 < row
        {
            if self.continuous
                && self.last_row + 1 == row
                && ordinal.checked_sub(position) == u64::try_from(row - previous).ok()
            {
                self.plan.positions.push((previous + 1, row, position + 1));
            } else {
                self.plan.unresolved = true;
                self.plan.opaque_through = self.plan.opaque_through.max(self.pending_rollback);
            }
        }
        self.continuous = true;
        self.pending_rollback = 0;
    }

    fn observe(&mut self, path: &Path, record: &Value, row: usize, hidden: bool) {
        let ordinal = record["ordinal"].as_u64();
        if let Some(ordinal) = ordinal {
            self.anchor(row, ordinal);
        } else {
            self.continuous &= self.last_row + 1 == row
                && record["type"].is_string()
                && record["payload"].is_object();
            if !hidden && is_owned_rollback(record, path) && record["payload"]["num_turns"] != 0 {
                self.pending_rollback = row;
            }
            if !hidden && !recoverable_control(record, path) {
                let owned = owned_event(record, path);
                if let Some(role) = message_role(record) {
                    self.unnumbered_messages
                        .push((row, owned && role == "assistant"));
                } else if !owned || !is_non_turn_body(record) {
                    self.plan.unresolved = true;
                }
            }
        }
        if !hidden {
            self.observe_turn(path, record, row, ordinal.is_some());
        }
        self.last_row = row;
    }

    fn observe_turn(&mut self, path: &Path, record: &Value, row: usize, numbered: bool) {
        let payload = &record["payload"];
        if is_owned_turn_start(record, path) {
            self.last_request = Some(super::MirrorGroup::new(true));
            self.turn = payload["turn_id"]
                .as_str()
                .map(|id| (row, id.to_owned(), numbered));
        } else if is_owned_completion(record, path) {
            if let Some((start, id, numbered_start)) = self.turn.take()
                && payload["turn_id"] == id
                && self
                    .unnumbered_messages
                    .last()
                    .is_some_and(|&(missing, _)| missing > start)
            {
                self.candidates.push((start, numbered_start, row, numbered));
            }
        } else if is_owned_rollback(record, path)
            || payload["type"] == "turn_aborted"
            || !owned_event(record, path)
            || self.turn.as_ref().is_some_and(|(_, id, _)| {
                [record, payload]
                    .iter()
                    .any(|object| object.get("turn_id").is_some_and(|value| value != id))
            })
        {
            self.turn = None;
        }
        if self.turn.is_some() && message_role(record).is_some() {
            if let Some(message) = request_text(record) {
                self.observe_request(record, row, numbered, &message);
            } else {
                super::MirrorGroup::barrier(&mut self.last_request);
            }
        }
    }

    fn observe_request(&mut self, record: &Value, row: usize, numbered: bool, text: &str) {
        let identity = super::VisibleMessage::new(record, omnis_ir::EventKind::MessageUser, text);
        if let super::MirrorObservation::Paired(prior) = super::MirrorGroup::observe(
            &mut self.last_request,
            identity,
            (!numbered).then_some(row),
        ) {
            match (prior, numbered) {
                (Some(prior_row), true) => self.plan.corroborated_mirrors.push(prior_row),
                (None, false) => self.plan.corroborated_mirrors.push(row),
                (Some(_), false) => self.plan.withheld_mirrors.push(row),
                (None, true) => {}
            }
            return;
        }
        if !numbered && let Some((start, _, numbered_start)) = &self.turn {
            self.request_slots.push((*start, *numbered_start, row));
        }
    }

    fn finish(mut self, end_row: usize, end_ordinal: u64) -> RecoveryPlan {
        self.anchor(end_row, end_ordinal);
        for (start, numbered_start, end, numbered_end) in self.candidates {
            if (numbered_start || self.plan.ordinal(start).is_some())
                && (numbered_end || self.plan.ordinal(end).is_some())
            {
                self.plan.confirmed_turns.push((start, end));
            }
        }
        self.plan.corroborated_mirrors.sort_unstable();
        self.plan.corroborated_mirrors.dedup();
        for (start, numbered_start, row) in self.request_slots {
            if (numbered_start || self.plan.ordinal(start).is_some())
                && row > self.plan.opaque_through
                && self.plan.ordinal(row).is_some()
                && !self.plan.confirmed_turn(row)
                && self.plan.corroborated_mirrors.binary_search(&row).is_err()
            {
                self.plan.withheld_requests.push(row);
            }
        }
        // A placed assistant reply cannot withdraw an ancestor even if its unfinished turn
        // prevents replay. A request still needs a confirmed turn or a reserved rollback slot.
        self.plan.unresolved |= self.unnumbered_messages.iter().any(|&(row, owned_reply)| {
            self.plan.ordinal(row).is_none()
                || (!owned_reply
                    && !self.plan.confirmed_turn(row)
                    && self.plan.corroborated_mirrors.binary_search(&row).is_err()
                    && self.plan.withheld_mirrors.binary_search(&row).is_err()
                    && self.plan.withheld_requests.binary_search(&row).is_err())
        });
        self.plan
    }
}

fn request_text(record: &Value) -> Option<String> {
    let payload = &record["payload"];
    let (text, images) = match (record["type"].as_str(), payload["type"].as_str()) {
        (Some("response_item"), Some("message")) if payload["role"] == "user" => {
            if let Some(text) = payload["content"].as_str() {
                (text.to_owned(), 0)
            } else {
                let (parts, images) = super::message_parts(payload);
                if parts
                    .iter()
                    .any(|part| super::is_contextual_user_text(part))
                {
                    return None;
                }
                (parts.join("\n"), if parts.is_empty() { images } else { 0 })
            }
        }
        (Some("event_msg"), Some("user_message")) => {
            let text = super::string_at(payload, &[&["message"], &["text"]]).unwrap_or("");
            let images = ["images", "file_ids", "local_images"]
                .iter()
                .filter_map(|field| payload[*field].as_array())
                .map(Vec::len)
                .sum();
            (text.to_owned(), images)
        }
        _ => return None,
    };
    if text.is_empty()
        && let Some(placeholder) = super::omitted_media_text(payload, images)
    {
        return Some(placeholder);
    }
    (!super::is_contextual_user_text(&text)).then_some(text)
}

fn message_role(record: &Value) -> Option<&str> {
    let payload = &record["payload"];
    match (record["type"].as_str(), payload["type"].as_str()) {
        (Some("response_item"), Some("message")) => payload["role"]
            .as_str()
            .filter(|role| matches!(*role, "user" | "assistant")),
        (Some("event_msg"), Some("user_message")) => Some("user"),
        (Some("event_msg"), Some("agent_message")) => Some("assistant"),
        _ => None,
    }
}

fn is_non_turn_body(record: &Value) -> bool {
    let payload = &record["payload"];
    match record["type"].as_str() {
        Some("event_msg") if payload["type"] == "item_completed" => {
            let item = &payload["item"];
            payload["thread_id"].is_string()
                && payload["turn_id"].as_str().is_some_and(|id| !id.is_empty())
                && item["type"] == "UserMessage"
                && item["id"].as_str().is_some_and(|id| !id.is_empty())
                && item["content"].as_array().is_some_and(|parts| {
                    !parts.is_empty()
                        && parts.iter().all(|part| match part["type"].as_str() {
                            Some("text") => part["text"].is_string(),
                            Some("image") => {
                                media_reference(part, "image_url").is_some()
                                    && image_detail(part).is_some()
                            }
                            Some("audio") => media_reference(part, "audio_url").is_some(),
                            Some("local_image") => {
                                part["path"].is_string() && image_detail(part).is_some()
                            }
                            Some("local_audio") => part["path"].is_string(),
                            Some("skill" | "mention") => {
                                part["name"].is_string() && part["path"].is_string()
                            }
                            _ => false,
                        })
                })
        }
        Some("response_item")
            if payload["type"] == "message"
                && matches!(payload["role"].as_str(), Some("developer" | "system")) =>
        {
            let content = &payload["content"];
            content.is_string()
                || content.as_array().is_some_and(|parts| {
                    !parts.is_empty()
                        && parts.iter().all(|part| {
                            matches!(
                                part["type"].as_str(),
                                Some("text" | "input_text" | "output_text")
                            ) && part["text"].is_string()
                        })
                })
        }
        Some("dynamic_tools_updated") => super::is_dynamic_tool_catalog(record),
        Some("response_item") => matches!(
            payload["type"].as_str(),
            Some(
                "reasoning"
                    | "function_call"
                    | "custom_tool_call"
                    | "web_search_call"
                    | "computer_call"
                    | "local_shell_call"
                    | "function_call_output"
                    | "custom_tool_call_output"
                    | "local_shell_call_output"
                    | "computer_call_output"
            )
        ),
        _ => false,
    }
}

fn valid_context_envelope(payload: &Value) -> bool {
    payload.is_object()
        && payload
            .get("turn_id")
            .is_none_or(|value| value.is_null() || value.as_str().is_some_and(|id| !id.is_empty()))
}

fn recoverable_control(record: &Value, path: &Path) -> bool {
    (record["type"] == "turn_context"
        && valid_context_envelope(&record["payload"])
        && owned_event(record, path))
        || is_owned_turn_start(record, path)
        || is_owned_completion(record, path)
        || is_owned_rollback(record, path)
        || (record["type"] == "event_msg"
            && record["payload"]["type"] == "token_count"
            && owned_event(record, path))
}

fn is_owned_completion(record: &Value, path: &Path) -> bool {
    record["type"] == "event_msg"
        && matches!(
            record["payload"]["type"].as_str(),
            Some("task_complete" | "turn_complete")
        )
        && record["payload"]["turn_id"]
            .as_str()
            .is_some_and(|id| !id.is_empty())
        && owned_event(record, path)
}

fn preserves_subagent_suffix(record: &Value, path: &Path) -> bool {
    if !owned_event(record, path) {
        return false;
    }
    let payload = &record["payload"];
    match (record["type"].as_str(), payload["type"].as_str()) {
        (Some("event_msg"), Some("turn_aborted")) => true,
        (Some("event_msg"), Some("thread_rolled_back")) => payload["num_turns"].as_u64() == Some(0),
        _ => {
            message_role(record).is_some()
                || is_non_turn_body(record)
                || recoverable_control(record, path)
        }
    }
}

struct PrefixCursor {
    next: u64,
    max_reserved_gap: u64,
    malformed: bool,
    damage: Option<&'static str>,
    damaged_preamble_settings: bool,
    recoverable_subagent_gap: bool,
    evidence: SequenceEvidence,
}

impl PrefixCursor {
    fn new(segment: &Segment) -> Self {
        Self {
            next: segment.start,
            max_reserved_gap: 0,
            malformed: false,
            damage: None,
            damaged_preamble_settings: false,
            recoverable_subagent_gap: false,
            evidence: SequenceEvidence::default(),
        }
    }

    fn missing_ordinal(&mut self, start: &VisibleStart, record: &Value, segment: &Segment) {
        self.recoverable_subagent_gap = start.subagent.is_some()
            && !self.malformed
            && (self.damage.is_none() || self.recoverable_subagent_gap)
            && preserves_subagent_suffix(record, &segment.path);
        self.damage = Some(MISSING_ORDINAL);
    }

    fn observe(
        &mut self,
        segment: &Segment,
        start: &VisibleStart,
        record: &Value,
        row: usize,
    ) -> Result<bool> {
        let hidden = start.hides_parsed_row(row, record);
        let preamble = start.hides_row(row);
        let inferred = segment.recovery.as_ref().and_then(|plan| plan.ordinal(row));
        let publishable = inferred.is_none()
            || segment.recovery.as_ref().is_some_and(|plan| {
                recoverable_control(record, &segment.path)
                    || (message_role(record).is_some()
                        && owned_event(record, &segment.path)
                        && (plan.confirmed_turn(row)
                            || plan.corroborated_mirrors.binary_search(&row).is_ok()))
            });
        let ordinal = inferred.or(segment.record_ordinal(
            record,
            self.next,
            self.max_reserved_gap,
            self.malformed,
            hidden,
        )?);
        if segment.recovery.is_none() {
            self.evidence.observe(&segment.path, record, row, hidden);
        }
        if let Some(ordinal) = ordinal {
            self.next = ordinal;
            self.max_reserved_gap = 0;
            let through_subagent_start = start.subagent.is_some_and(|cutoff| ordinal <= cutoff);
            if !hidden
                && segment.recovery.is_some()
                && (self.damage.is_some() || inferred.is_some())
                && (through_subagent_start
                    || (self.recoverable_subagent_gap
                        && self.damage == Some(MISSING_ORDINAL)
                        && inferred.is_none()
                        && start.subagent.is_some_and(|cutoff| ordinal >= cutoff)
                        && preserves_subagent_suffix(record, &segment.path))
                    || ((is_owned_turn_start(record, &segment.path)
                        || (self.damaged_preamble_settings && !preamble))
                        && start.ordinal().is_none_or(|begin| ordinal >= begin)))
            {
                self.damage = None;
                self.recoverable_subagent_gap = false;
                self.damaged_preamble_settings = false;
            }
            if !hidden
                && start.subagent.is_none_or(|cutoff| ordinal >= cutoff)
                && !publishable
                && request_text(record).is_some()
                && segment.recovery.as_ref().is_none_or(|plan| {
                    !plan.withheld_request(row, start)
                        && plan.withheld_mirrors.binary_search(&row).is_err()
                })
            {
                self.missing_ordinal(start, record, segment);
            }
            if !hidden && self.damage.is_none() {
                self.malformed = false;
            }
        } else {
            if !hidden {
                self.damaged_preamble_settings = preamble && start.owned_settings(record);
                self.missing_ordinal(start, record, segment);
            }
            if self.malformed {
                return Ok(false);
            }
            if let Some(first) = segment.subagent {
                self.max_reserved_gap = self.max_reserved_gap.max(first.saturating_sub(self.next));
            }
        }
        let permitted = segment.recovery.as_ref().is_none_or(|plan| {
            row > plan.opaque_through
                && (record["payload"]["type"] != "thread_rolled_back"
                    || owned_event(record, &segment.path))
        });
        let include = !hidden
            && publishable
            && permitted
            && (self.damage.is_none()
                || (ordinal.is_some()
                    && segment.recovery.is_some()
                    && is_owned_rollback(record, &segment.path)))
            && self.next > segment.start
            && start.ordinal().is_none_or(|begin| self.next >= begin);
        self.next += 1;
        Ok(include)
    }
}

fn is_owned_turn_start(record: &Value, path: &Path) -> bool {
    let payload = &record["payload"];
    record["type"] == "event_msg"
        && matches!(
            payload["type"].as_str(),
            Some("task_started" | "turn_started")
        )
        && payload["turn_id"].as_str().is_some_and(|id| !id.is_empty())
        && owned_event(record, path)
}

// A verified rollback can withdraw a recovered turn while later content is still unplaced.
fn is_owned_rollback(record: &Value, path: &Path) -> bool {
    record["type"] == "event_msg"
        && record["payload"]["type"] == "thread_rolled_back"
        && record["payload"]["num_turns"].is_u64()
        && owned_event(record, path)
}

fn owned_event(record: &Value, path: &Path) -> bool {
    [record, &record["payload"]].iter().all(|object| {
        object.get("thread_id").is_none_or(|owner| {
            owner
                .as_str()
                .zip(rollout_identity::thread_id(path))
                .is_some_and(|(id, selected)| super::workspace_check::same_thread(selected, id))
        })
    })
}

fn read_prefix_from(
    segment: &Segment,
    source: &mut dyn Read,
    visit: &mut dyn FnMut(InheritedRecord, usize) -> Result<()>,
) -> Result<PrefixRead> {
    let Some(visible_start) = segment.visible_start.as_ref() else {
        return Ok(PrefixRead::Damaged(COMPRESSED_PREFIX_DAMAGE));
    };
    let position = segment.position;
    let mut remaining = position.end_byte_offset;
    let mut cursor = PrefixCursor::new(segment);
    let mut row = 0;
    let mut skipped = 0;
    let mut line = Vec::new();
    let mut reader = BufReader::new(source.take(remaining));
    while remaining > 0 {
        let line_read = read_bounded_line_with_budget(
            &mut reader,
            MAX_STREAMED_TRANSCRIPT_LINE_SIZE,
            remaining,
            &mut line,
        );
        let (kind, length) = match line_read {
            Ok(Some(line)) => line,
            Err(error) if error.kind() != std::io::ErrorKind::UnexpectedEof => {
                return unreadable_prefix(&segment.path, error);
            }
            _ => {
                segment.confirm_header_read(cursor.next)?;
                return Ok(PrefixRead::Damaged(
                    "Codex referenced a source rollout shorter than its recorded byte cutoff",
                ));
            }
        };
        remaining -= length;
        ensure!(
            cursor.malformed || cursor.next < position.end_ordinal_exclusive,
            "Codex history_base byte and ordinal cutoffs disagree"
        );
        row += 1;
        match kind {
            BoundedLine::Oversized => {
                segment.confirm_header_read(cursor.next)?;
                if cursor.malformed {
                    continue;
                }
                skipped += 1;
                cursor.next += 1;
            }
            BoundedLine::Complete => {
                let record: Value = match serde_json::from_slice(&line) {
                    Ok(record) => record,
                    Err(error) if cursor.next == segment.start => {
                        return Err(error)
                            .context("invalid JSON in referenced Codex source header");
                    }
                    Err(_) => {
                        // Broken separators can split one hidden record into several fragments.
                        // The next verified ordinal anchors the suffix inside this byte bookmark.
                        cursor.malformed = true;
                        if !visible_start.hides_row(row) {
                            cursor.evidence.unknown(row);
                            cursor.damaged_preamble_settings = false;
                            cursor.damage = Some(MALFORMED_ROWS);
                        }
                        continue;
                    }
                };
                let record = super::saved_message::project(record);
                if cursor.observe(segment, visible_start, &record, row)? {
                    visit(
                        if let Some(proof) = visible_start.confirmed_request(&record) {
                            let mut record = record;
                            project_literal_parts(&mut record, &proof.indices);
                            InheritedRecord::ConfirmedRequest(record)
                        } else {
                            InheritedRecord::Native(record)
                        },
                        skipped,
                    )?;
                } else if !visible_start.hides_parsed_row(row, &record)
                    && let Some(withheld) = segment.recovery.as_ref().and_then(|plan| {
                        plan.withheld_record(row, visible_start, &record, &segment.path)
                    })
                {
                    visit(withheld, skipped)?;
                }
            }
        }
    }
    if cursor.malformed {
        cursor.damage.get_or_insert(MALFORMED_ROWS);
    } else {
        position.confirm_end(cursor.next, cursor.max_reserved_gap)?;
    }
    if segment.recovery.is_some() {
        return Ok(PrefixRead::Complete(skipped));
    }
    Ok(match cursor.damage {
        Some(reason) => PrefixRead::Partial(
            reason,
            cursor
                .evidence
                .finish(row + 1, position.end_ordinal_exclusive),
        ),
        None => PrefixRead::Complete(skipped),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_record_owner_must_identify_the_selected_thread() {
        let selected = "abcdef12-1234-4567-89ab-abcdef123456";
        let path = PathBuf::from(format!("rollout-{selected}.jsonl"));
        let upper = selected.to_uppercase();
        let record = |outer: Value, inner: Value| serde_json::json!({"thread_id": outer, "payload": {"thread_id": inner}});
        assert!(owned_event(
            &record(selected.into(), upper.clone().into()),
            &path
        ));
        assert!(owned_event(&record(upper.into(), selected.into()), &path));
        for invalid in [
            Value::Null,
            "invalid".into(),
            "abcdef12-1234-4567-89ab-abcdef123457".into(),
        ] {
            assert!(!owned_event(
                &record(selected.into(), invalid.clone()),
                &path
            ));
            assert!(!owned_event(&record(invalid, selected.into()), &path));
        }
    }
}
