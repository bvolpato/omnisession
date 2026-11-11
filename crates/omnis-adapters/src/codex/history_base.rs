//! Reads the immutable prefixes that Codex records in paginated fork and revert rollouts.

mod copies;

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
    rollout_identity::{self, RolloutCatalog},
};
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

impl Position {
    fn source_start(self, header: &Value) -> Result<u64> {
        let start = header["ordinal"]
            .as_u64()
            .context("missing Codex source ordinal")?;
        ensure!(
            start < self.end_ordinal_exclusive,
            "Codex history_base cutoff precedes the source header"
        );
        Ok(start)
    }
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
        let visible_leaf_start = self.visible_start(leaf, scope, subagent_start, None)?;
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
            let source = find(&pos.thread_id.to_string()).and_then(|path| {
                path.map(|path| rollout_file::header(&path).map(|header| (path, header)))
                    .transpose()
            });
            let (path, records) = match source {
                Ok(Some(source)) => source,
                Ok(None) => {
                    missing_rollout = Some(pos.thread_id);
                    break Some("Codex referenced an unavailable source rollout");
                }
                Err(error) if error.is::<rollout_file::CompressedHeaderError>() => {
                    missing_rollout = Some(pos.thread_id);
                    break Some("Codex could not decode a referenced compressed rollout header");
                }
                Err(error) => return Err(error),
            };
            let meta = records
                .first()
                .context("missing Codex source rollout header")?;
            ensure!(
                meta["type"] == "session_meta"
                    && meta["payload"]["id"].as_str() == rollout_identity::thread_id(&path),
                "Codex referenced an incompatible source rollout"
            );
            // A restored file can have the right ID but predate the migration that created this bookmark.
            if meta["payload"]["history_mode"] != "paginated" {
                missing_rollout = Some(pos.thread_id);
                break Some("Codex referenced a source rollout without paginated history");
            }
            if scope == HistoryScope::ModelContext && !meta["payload"]["cwd"].is_string() {
                break Some(
                    "Codex cannot load an inherited rollout without a string creation directory",
                );
            }
            self = Self::from_header(&path, meta);
            let start = pos.source_start(meta)?;
            let subagent = subagent_history_start(meta);
            let visibility = self.visible_start(&path, scope, subagent, Some(pos.end_byte_offset));
            let visible_start = match visibility {
                Ok(start) => Some(start),
                Err(error) if rollout_file::compressed(&path) && error.is::<std::io::Error>() => {
                    None
                }
                Err(error) => return Err(error),
            };
            segments.push(Segment {
                visible_start,
                subagent,
                verified_revision: None,
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
        prefix_end: Option<u64>,
    ) -> Result<VisibleStart> {
        if scope == HistoryScope::ModelContext {
            return Ok(VisibleStart::default());
        }
        let attached = match self {
            Self::Attachment(pos) => Some(attached_history_start(path, pos, prefix_end)?),
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
    fn record_ordinal(&self, record: &Value, expected: u64) -> Result<u64> {
        let ordinal = record["ordinal"]
            .as_u64()
            .context("missing Codex source ordinal")?;
        // A copied subagent prefix can reserve ordinals without a record at each one.
        if expected > self.start
            && ordinal > expected
            && self.subagent.is_some_and(|first| ordinal <= first)
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
                    && subagent_history_start(record) == self.subagent
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
            standalone = adapter.selector.listing(home, std::iter::empty());
            &standalone
        };
        let mut budget = MAX_STREAMED_TRANSCRIPT_FILE_SIZE;
        // Only segments newer than an unrecoverable gap can contribute to the projection.
        for &(index, reason) in damage.0.iter().rev() {
            if let Some(verified) = self.segments[index].verified_copy(home, catalog, &mut budget) {
                self.segments[index] = verified;
            } else {
                self.first_available = index + 1;
                self.unavailable = Some(reason);
                break;
            }
        }
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
                    .filter_map(|s| s.visible_start),
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
    let projection = read_prefix_from(segment, &mut rollout_file::open(&segment.path)?, visit)?;
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
    let mut skipped = 0;
    let mut malformed = false;
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
        match kind {
            BoundedLine::Oversized => {
                ensure!(next > start, "Codex history_base source header changed");
                if malformed {
                    remaining -= length;
                    continue;
                }
                ensure!(
                    next < position.end_ordinal_exclusive,
                    "Codex history_base byte and ordinal cutoffs disagree"
                );
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
                        remaining -= length;
                        continue;
                    }
                };
                if malformed {
                    // A broken separator can join or split rows. A later readable ordinal still
                    // cannot go backwards or fall outside the saved interval.
                    let Some(ordinal) = record["ordinal"].as_u64() else {
                        remaining -= length;
                        continue;
                    };
                    ensure!(
                        ordinal >= next && ordinal < position.end_ordinal_exclusive,
                        "Codex history_base source ordinal is inconsistent"
                    );
                    next = ordinal;
                } else {
                    ensure!(
                        next < position.end_ordinal_exclusive,
                        "Codex history_base byte and ordinal cutoffs disagree"
                    );
                    next = segment.record_ordinal(&record, next)?;
                }
                if !malformed
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
        malformed || next == position.end_ordinal_exclusive,
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
