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
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum HistoryScope {
    Visible,
    ModelContext,
}

#[derive(Clone, Copy, Default)]
struct VisibleStart {
    attached: Option<u64>,
    subagent: Option<u64>,
}

impl VisibleStart {
    fn ordinal(self) -> Option<u64> {
        self.attached.max(self.subagent)
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
        let Some(base) = meta.get("history_base").filter(|v| !v.is_null()) else {
            return Self::None;
        };
        let Ok(pos) = Position::deserialize(base) else {
            return Self::Invalid;
        };
        match HistoryMode::read(&meta["history_mode"]) {
            Some(HistoryMode::Legacy) => Self::None,
            Some(HistoryMode::Paginated) if pos.matches_start(header) => {
                if attachment_boundary(meta, path, pos) {
                    Self::Attachment(pos)
                } else {
                    Self::Prefix(pos)
                }
            }
            _ => Self::Invalid,
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
        Ok(Lineage {
            visible_leaf_start: self.visible_start(leaf, scope, subagent_start, None)?,
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
        let mut budget = MAX_STREAMED_TRANSCRIPT_FILE_SIZE;
        let unavailable = loop {
            let pos = match self {
                Self::None => break None,
                Self::Invalid => break Some("Codex recorded an invalid history_base"),
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
            let (path, meta, evidence) = match pos.source_header(adapter, catalog) {
                Ok(Some(source)) => source,
                Ok(None) => {
                    missing_rollout = Some(pos.thread_id);
                    break Some("Codex referenced an unavailable source rollout");
                }
                Err(error) => {
                    let Some(reason) = source_unavailable(&error) else {
                        return Err(error);
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
                result => result?,
            };
            if segment.is_none() && scope == HistoryScope::Visible {
                let catalog =
                    catalog.unwrap_or_else(|| fallback.get_or_init(|| RolloutCatalog::new(home)));
                segment =
                    copies::verified_copy(home, catalog, &path, pos, &mut budget, |alternate| {
                        let records = rollout_file::header(alternate)?;
                        match records.first() {
                            Some(header) => Segment::from_header(alternate, header, pos, scope),
                            None => Ok(None),
                        }
                    })?;
            }
            let Some(segment) = segment else {
                missing_rollout = Some(pos.thread_id);
                break Some(incomplete_header_reason(&meta));
            };
            self = segment.base;
            segments.push(segment);
        };

        segments.reverse();
        Ok(Lineage {
            segments,
            first_available: 0,
            missing_rollout,
            unavailable,
            ..leaf_only
        })
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
        let attached = match self {
            Self::Attachment(pos) => Some(attached_history_start(path, pos, prefix_end)?),
            Self::UnknownMode {
                attached: true,
                boundary,
            } => {
                let pos = boundary
                    .context("Codex could not locate the attached rollout's visible history")?;
                Some(attached_history_start(path, pos, prefix_end)?)
            }
            _ => None,
        };
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
fn attached_history_start(path: &Path, pos: Position, prefix_end: Option<u64>) -> Result<u64> {
    let start = pos
        .end_ordinal_exclusive
        .checked_add(1)
        .context("Codex attachment ordinal overflow")?;
    // A fork inherits only the saved prefix. A later frame can change or become unreadable.
    let source = rollout_file::open(path)?.take(prefix_end.unwrap_or(u64::MAX));
    let mut reader = BufReader::new(source);
    let mut line = Vec::new();
    let mut remaining = MAX_STREAMED_TRANSCRIPT_FILE_SIZE;
    for _ in 0..4 {
        let Some((BoundedLine::Complete, length)) = read_bounded_line_with_budget(
            &mut reader,
            MAX_STREAMED_TRANSCRIPT_LINE_SIZE,
            remaining,
            &mut line,
        )?
        else {
            break;
        };
        remaining -= length;
        let Ok(record) = serde_json::from_slice::<Value>(&line) else {
            break;
        };
        match (record["type"].as_str(), record["payload"]["type"].as_str()) {
            (Some("session_meta"), _) | (Some("response_item"), Some(_)) => {}
            (Some("event_msg"), Some("thread_settings_applied"))
                if record["payload"]["thread_settings"].is_object() => {}
            (Some("event_msg"), Some("turn_aborted"))
                if record["payload"]["reason"].is_string() =>
            {
                return Ok(record["ordinal"]
                    .as_u64()
                    .map_or(start, |ordinal| ordinal.saturating_add(1)));
            }
            _ => break,
        }
    }
    Ok(start)
}

#[derive(Clone)]
struct Segment {
    visible_start: Option<VisibleStart>,
    subagent: Option<u64>,
    verified_revision: Option<omnis_ir::SourceRevision>,
    path: PathBuf,
    start: u64,
    position: Position,
    base: HistoryBase,
}

impl Segment {
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
        if HeaderEvidence::read(header, owner) != HeaderEvidence::Native(HistoryMode::Paginated) {
            return Ok(None);
        }
        let base = HistoryBase::from_header(path, header);
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
    ) -> Result<Option<u64>> {
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
        if expected > self.start && record["type"].is_string() && record.get("ordinal").is_none() {
            return Ok(None);
        }
        let ordinal = record["ordinal"]
            .as_u64()
            .context("missing Codex source ordinal")?;
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
    segments: Vec<Segment>,
    first_available: usize,
    visible_leaf_start: VisibleStart,
    pub(super) missing_rollout: Option<Uuid>,
    pub(super) unavailable: Option<&'static str>,
}

#[derive(Debug)]
struct DamagedPrefixes(Vec<(usize, &'static str)>);

impl std::fmt::Display for DamagedPrefixes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("damaged Codex source rollout prefix")
    }
}

impl std::error::Error for DamagedPrefixes {}

impl Lineage {
    pub(super) fn project<T>(
        &mut self,
        adapter: &CodexAdapter,
        catalog: Option<&RolloutCatalog<'_>>,
        read: impl Fn(&Self) -> Result<T>,
    ) -> Result<T> {
        let error = match read(self) {
            Ok(value) => return Ok(value),
            Err(error) => error,
        };
        let Some(damage) = error.downcast_ref::<DamagedPrefixes>() else {
            return Err(error);
        };
        let home = adapter
            .codex_home
            .as_deref()
            .context("Codex home was not found")?;
        let standalone;
        let catalog = if let Some(catalog) = catalog {
            catalog
        } else {
            standalone = RolloutCatalog::new(home);
            &standalone
        };
        let mut budget = MAX_STREAMED_TRANSCRIPT_FILE_SIZE;
        // Only segments newer than an unrecoverable gap can contribute to the projection.
        for &(index, reason) in damage.0.iter().rev() {
            if let Some(verified) =
                self.segments[index].verified_copy(home, catalog, &mut budget)?
            {
                self.segments[index] = verified;
            } else {
                self.first_available = index + 1;
                self.unavailable = Some(reason);
                break;
            }
        }
        read(self)
    }

    pub(super) fn leaf_record_visibility(&self, record: &Value) -> Option<bool> {
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
        if self.visible_leaf_start.ordinal().is_none() {
            return read(&mut visit);
        }
        let mut unplaced = JsonLineSkips::default();
        let skipped = read(&mut |record, skipped| {
            match self.leaf_record_visibility(&record) {
                Some(true) => visit(record, skipped.combined(unplaced))?,
                Some(false) => {}
                None => unplaced.observe_unplaced(unplaced_workspace_gap(&record, session)),
            }
            Ok(())
        })?;
        Ok(skipped.combined(unplaced))
    }

    pub(super) fn hidden_context_types(&self) -> impl Iterator<Item = &'static str> {
        let starts = || {
            std::iter::once(self.visible_leaf_start).chain(
                self.segments[self.first_available..]
                    .iter()
                    .filter_map(|s| s.visible_start),
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

    pub(super) fn visit(&self, mut visit: impl FnMut(Value, usize) -> Result<()>) -> Result<usize> {
        let mut skipped = 0;
        let mut damage = Vec::new();
        for (index, segment) in self.segments.iter().enumerate().skip(self.first_available) {
            let prefix = if damage.is_empty() {
                read_prefix(segment, &mut |record, local| visit(record, skipped + local))
            } else {
                // Check every newer bookmark in this pass. Multiple damaged sources need one retry.
                read_prefix(segment, &mut |_, _| Ok(()))
            }?;
            match prefix {
                PrefixRead::Complete(count) => skipped += count,
                PrefixRead::Damaged(reason) => {
                    damage.push((index, reason));
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

enum PrefixRead {
    Complete(usize),
    Damaged(&'static str),
}

/// A bookmark counts uncompressed bytes, including line terminators. Check its exclusive ordinal
/// too, so an inconsistent bookmark never imports a turn after the fork.
fn read_prefix(
    segment: &Segment,
    visit: &mut dyn FnMut(Value, usize) -> Result<()>,
) -> Result<PrefixRead> {
    let valid_revision = || {
        segment.verified_revision.is_none_or(|expected| {
            CodexAdapter::index_file_revision(&segment.path) == Some(expected)
        })
    };
    ensure!(
        valid_revision(),
        "selected alternate Codex source rollout changed"
    );
    let mut source = match rollout_file::open(&segment.path) {
        Err(error) if access_denied(&error) => return Ok(PrefixRead::Damaged(INACCESSIBLE_SOURCE)),
        result => result?,
    };
    let projection = read_prefix_from(segment, &mut source, visit)?;
    ensure!(
        valid_revision(),
        "selected alternate Codex source rollout changed"
    );
    Ok(projection)
}

fn read_prefix_from(
    segment: &Segment,
    source: &mut dyn Read,
    visit: &mut dyn FnMut(Value, usize) -> Result<()>,
) -> Result<PrefixRead> {
    let Some(visible_start) = segment.visible_start else {
        return Ok(PrefixRead::Damaged(COMPRESSED_PREFIX_DAMAGE));
    };
    let start = segment.start;
    let position = segment.position;
    let mut remaining = position.end_byte_offset;
    let mut next = start;
    let mut max_reserved_gap = 0;
    let mut skipped = 0;
    let mut malformed = false;
    let mut damage = None;
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
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                return Ok(PrefixRead::Damaged(INACCESSIBLE_SOURCE));
            }
            Err(error) if error.kind() != std::io::ErrorKind::UnexpectedEof => {
                if rollout_file::compressed(&segment.path) {
                    return Ok(PrefixRead::Damaged(COMPRESSED_PREFIX_DAMAGE));
                }
                return Err(error)
                    .context("reading referenced Codex history up to its byte cutoff");
            }
            _ => {
                ensure!(next > start, "Codex history_base source header changed");
                return Ok(PrefixRead::Damaged(
                    "Codex referenced a source rollout shorter than its recorded byte cutoff",
                ));
            }
        };
        ensure!(
            malformed || next < position.end_ordinal_exclusive,
            "Codex history_base byte and ordinal cutoffs disagree"
        );
        match kind {
            BoundedLine::Oversized => {
                ensure!(next > start, "Codex history_base source header changed");
                if malformed {
                    remaining -= length;
                    continue;
                }
                skipped += 1;
            }
            BoundedLine::Complete => {
                let record: Value = match serde_json::from_slice(&line) {
                    Ok(record) => record,
                    Err(error) if next == start => {
                        return Err(error)
                            .context("invalid JSON in referenced Codex source header");
                    }
                    Err(_) => {
                        malformed = true;
                        damage = Some(MALFORMED_ROWS);
                        remaining -= length;
                        continue;
                    }
                };
                if let Some(ordinal) =
                    segment.record_ordinal(&record, next, max_reserved_gap, malformed)?
                {
                    next = ordinal;
                    max_reserved_gap = 0;
                } else if malformed {
                    remaining -= length;
                    continue;
                } else {
                    // A missing ordinal can span reserved positions up to the first local subagent item.
                    if let Some(first) = segment.subagent {
                        max_reserved_gap = max_reserved_gap.max(first.saturating_sub(next));
                    }
                    damage = Some(MISSING_ORDINAL);
                }
                if damage.is_none()
                    && next > start
                    && visible_start.ordinal().is_none_or(|begin| next >= begin)
                {
                    visit(record, skipped)?;
                }
            }
        }
        next += 1;
        remaining -= length;
    }
    ensure!(
        malformed
            || (next..=next.saturating_add(max_reserved_gap))
                .contains(&position.end_ordinal_exclusive),
        "Codex history_base byte and ordinal cutoffs disagree"
    );
    Ok(damage.map_or(PrefixRead::Complete(skipped), PrefixRead::Damaged))
}
