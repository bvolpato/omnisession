//! Reads the immutable prefixes that Codex records in paginated fork and revert rollouts.

use std::{
    collections::HashSet,
    io::BufReader,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use super::{rollout_file, rollout_identity};
use crate::support::{
    BoundedLine, MAX_STREAMED_TRANSCRIPT_FILE_SIZE, MAX_STREAMED_TRANSCRIPT_LINE_SIZE,
    read_bounded_line_with_budget,
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

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub(super) enum HistoryBase {
    #[default]
    None,
    Prefix(Position),
    Attachment(Position),
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
    if meta["history_mode"] != "paginated" {
        return None;
    }
    let first = header["ordinal"].as_u64()?.saturating_add(1);
    meta["subagent_history_start_ordinal"]
        .as_u64()
        .filter(|&start| first < start)
}

impl HistoryBase {
    pub(super) fn from_header(path: &Path, header: &Value) -> Self {
        let meta = &header["payload"];
        let Some(base) = meta.get("history_base").filter(|v| !v.is_null()) else {
            return Self::None;
        };
        if meta["history_mode"] != "paginated" {
            return Self::Invalid;
        }
        match serde_json::from_value::<Position>(base.clone()) {
            Ok(pos)
                if pos.end_byte_offset > 0
                    && pos.end_ordinal_exclusive > 0
                    && header["ordinal"].as_u64() == Some(pos.end_ordinal_exclusive) =>
            {
                if attachment_boundary(meta, path, pos) {
                    Self::Attachment(pos)
                } else {
                    Self::Prefix(pos)
                }
            }
            _ => Self::Invalid,
        }
    }

    pub(super) fn is_some(self) -> bool {
        !matches!(self, Self::None)
    }

    pub(super) fn resolve(
        mut self,
        leaf: &Path,
        scope: HistoryScope,
        subagent_start: Option<u64>,
        find: impl Fn(&str) -> Result<Option<PathBuf>>,
    ) -> Result<Lineage> {
        let visible_leaf_start = self.visible_start(leaf, scope, subagent_start)?;
        if !self.is_some() {
            return Ok(Lineage {
                visible_leaf_start,
                ..Lineage::default()
            });
        }
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
            let Some(path) = find(&pos.thread_id.to_string())? else {
                missing_rollout = Some(pos.thread_id);
                break Some("Codex referenced an unavailable source rollout");
            };
            let records = rollout_file::header(&path)?;
            let meta = records
                .first()
                .context("missing Codex source rollout header")?;
            ensure!(
                meta["type"] == "session_meta"
                    && meta["payload"]["history_mode"] == "paginated"
                    && meta["payload"]["id"].as_str() == rollout_identity::thread_id(&path),
                "Codex referenced an incompatible source rollout"
            );
            if scope == HistoryScope::ModelContext && !meta["payload"]["cwd"].is_string() {
                break Some(
                    "Codex cannot load an inherited rollout without a string creation directory",
                );
            }
            self = Self::from_header(&path, meta);
            let start = meta["ordinal"]
                .as_u64()
                .context("missing Codex source ordinal")?;
            ensure!(
                start < pos.end_ordinal_exclusive,
                "Codex history_base cutoff precedes the source header"
            );
            let visible_start = self.visible_start(&path, scope, subagent_history_start(meta))?;
            segments.push(Segment {
                visible_start,
                path,
                start,
                position: pos,
                base: self,
            });
        };

        segments.reverse();
        Ok(Lineage {
            segments,
            first_available: 0,
            visible_leaf_start,
            missing_rollout,
            unavailable,
        })
    }

    fn visible_start(
        self,
        path: &Path,
        scope: HistoryScope,
        subagent: Option<u64>,
    ) -> Result<VisibleStart> {
        if scope == HistoryScope::ModelContext {
            return Ok(VisibleStart::default());
        }
        let attached = match self {
            Self::Attachment(pos) => Some(attached_history_start(path, pos)?),
            _ => None,
        };
        Ok(VisibleStart { attached, subagent })
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
fn attached_history_start(path: &Path, pos: Position) -> Result<u64> {
    let start = pos
        .end_ordinal_exclusive
        .checked_add(1)
        .context("Codex attachment ordinal overflow")?;
    let mut reader = BufReader::new(rollout_file::open(path)?);
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

struct Segment {
    visible_start: VisibleStart,
    path: PathBuf,
    start: u64,
    position: Position,
    base: HistoryBase,
}

impl Segment {
    fn record_ordinal(&self, record: &Value, expected: u64) -> Result<u64> {
        let ordinal = record["ordinal"]
            .as_u64()
            .context("missing Codex source ordinal")?;
        // A copied subagent prefix can reserve ordinals without a record at each one.
        if expected > self.start
            && ordinal > expected
            && self
                .visible_start
                .subagent
                .is_some_and(|first| ordinal <= first)
        {
            ensure!(
                ordinal < self.position.end_ordinal_exclusive,
                "Codex history_base byte and ordinal cutoffs disagree"
            );
            return Ok(ordinal);
        }
        ensure!(
            ordinal == expected,
            "Codex history_base source ordinal is inconsistent"
        );
        if ordinal == self.start {
            ensure!(
                record["type"] == "session_meta"
                    && HistoryBase::from_header(&self.path, record) == self.base
                    && subagent_history_start(record) == self.visible_start.subagent
                    && record["payload"]["history_mode"] == "paginated"
                    && record["payload"]["id"].as_str() == rollout_identity::thread_id(&self.path),
                "Codex history_base source header changed"
            );
        }
        Ok(ordinal)
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
struct DamagedPrefix {
    next_segment: usize,
    reason: &'static str,
}

impl std::fmt::Display for DamagedPrefix {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.reason)
    }
}

impl std::error::Error for DamagedPrefix {}

impl Lineage {
    pub(super) fn project<T>(&mut self, read: impl Fn(&Self) -> Result<T>) -> Result<T> {
        let error = match read(self) {
            Ok(value) => return Ok(value),
            Err(error) => error,
        };
        let Some(damage) = error.downcast_ref::<DamagedPrefix>() else {
            return Err(error);
        };
        // Start a fresh projection. Earlier records may have changed turns that survived the gap.
        self.first_available = damage.next_segment;
        self.unavailable = Some(damage.reason);
        read(self)
    }

    pub(super) fn includes_leaf_record(&self, record: &Value) -> bool {
        self.visible_leaf_start.ordinal().is_none_or(|start| {
            record["type"] == "session_meta"
                || record["ordinal"]
                    .as_u64()
                    .is_some_and(|ordinal| ordinal >= start)
        })
    }

    pub(super) fn hidden_context_types(&self) -> impl Iterator<Item = &'static str> {
        let starts = || {
            std::iter::once(self.visible_leaf_start).chain(
                self.segments[self.first_available..]
                    .iter()
                    .map(|s| s.visible_start),
            )
        };
        [
            (
                starts().any(|s| s.attached.is_some()),
                omnis_core::CODEX_ATTACHED_PARENT_CONTEXT,
            ),
            (
                starts().any(|s| s.subagent.is_some()),
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
        Ok(self.segments.iter().all(|segment| {
            rollout_file::contains_native_prefix(&segment.path, segment.position.end_byte_offset)
                .unwrap_or(false)
        }))
    }

    pub(super) fn visit(&self, mut visit: impl FnMut(Value, usize) -> Result<()>) -> Result<usize> {
        let mut skipped = 0;
        let mut damage = None;
        for (index, segment) in self.segments.iter().enumerate().skip(self.first_available) {
            let prefix = if damage.is_some() {
                // Check every newer bookmark in this pass. Multiple damaged sources need one retry.
                read_prefix(segment, &mut |_, _| Ok(()))
            } else {
                read_prefix(segment, &mut |record, local| visit(record, skipped + local))
            }?;
            match prefix {
                PrefixRead::Complete(count) => skipped += count,
                PrefixRead::Damaged(reason) => {
                    damage = Some(DamagedPrefix {
                        next_segment: index + 1,
                        reason,
                    });
                }
            }
        }
        damage.map_or(Ok(skipped), |damage| Err(damage.into()))
    }
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
    let start = segment.start;
    let position = segment.position;
    let mut remaining = position.end_byte_offset;
    let mut next = start;
    let mut skipped = 0;
    let mut malformed = false;
    let mut line = Vec::new();
    let mut reader = BufReader::new(rollout_file::open(&segment.path)?);
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
            next < position.end_ordinal_exclusive,
            "Codex history_base byte and ordinal cutoffs disagree"
        );
        match kind {
            BoundedLine::Oversized => {
                ensure!(next > start, "Codex history_base source header changed");
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
                        next += 1;
                        remaining -= length;
                        continue;
                    }
                };
                next = segment.record_ordinal(&record, next)?;
                if !malformed
                    && next > start
                    && segment
                        .visible_start
                        .ordinal()
                        .is_none_or(|visible_start| next >= visible_start)
                {
                    visit(record, skipped)?;
                }
            }
        }
        next += 1;
        remaining -= length;
    }
    ensure!(
        next == position.end_ordinal_exclusive,
        "Codex history_base byte and ordinal cutoffs disagree"
    );
    Ok(if malformed {
        PrefixRead::Damaged(
            "Codex referenced a source rollout with malformed JSON before its cutoff",
        )
    } else {
        PrefixRead::Complete(skipped)
    })
}
