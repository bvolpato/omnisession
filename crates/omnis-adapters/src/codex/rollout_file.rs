//! Read the physical representation of a Codex JSONL rollout without modifying the source.

use std::{
    cell::OnceCell,
    collections::VecDeque,
    fs::{self, File},
    io::{BufRead, BufReader, Cursor, Read},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use flate2::read::MultiGzDecoder;
use serde_json::Value;

use super::workspace_check::{WorkspaceScan, same_thread};
use super::workspace_damage::{self, DamagedWorkspace};
use crate::support::WorkspaceGap;
use crate::support::{
    JsonLineSkips, JsonLinesLimits, MAX_PREVIEW_TAIL_SIZE, MAX_STREAMED_TRANSCRIPT_FILE_SIZE,
    MAX_STREAMED_TRANSCRIPT_LINE_SIZE, MAX_TRANSCRIPT_LINE_SIZE, json_lines_prefix,
    visit_json_reader,
};

pub(super) const SAMPLE_RECORDS: usize = 1_024;

pub(super) fn jsonl_file_name(path: &Path) -> Option<&Path> {
    let plain = match path.extension()?.to_str()? {
        "gz" | "zst" => Path::new(path.file_stem()?),
        "jsonl" => Path::new(path.file_name()?),
        _ => return None,
    };
    (plain.extension()? == "jsonl").then_some(plain)
}

pub(super) fn compressed(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension == "zst" || extension == "gz")
}

pub(super) fn representation_priority(path: &Path) -> u8 {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("jsonl") => 2,
        Some("zst") => 1,
        _ => 0,
    }
}

fn physical_file(path: &Path) -> Result<File> {
    if !fs::symlink_metadata(path)?.is_file() {
        bail!("Codex rollout must be a regular file");
    }
    let file = File::open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.len() > MAX_STREAMED_TRANSCRIPT_FILE_SIZE {
        bail!("Codex rollout exceeds safe streaming limit or is not a regular file");
    }
    Ok(file)
}

pub(super) fn open(path: &Path) -> Result<Box<dyn Read>> {
    decoder(path, physical_file(path)?)
}

fn decoder(path: &Path, file: File) -> Result<Box<dyn Read>> {
    let reader: Box<dyn Read> = match path.extension().and_then(|ext| ext.to_str()) {
        Some("gz") => Box::new(MultiGzDecoder::new(file)),
        Some("zst") => {
            let mut decoder = zstd::stream::read::Decoder::new(file)?;
            // The cold-rollout compressor uses standard frames. Bound decoder memory independently of output size.
            decoder.window_log_max(27)?;
            Box::new(decoder)
        }
        _ => Box::new(file),
    };
    Ok(reader)
}

#[derive(Debug)]
pub(super) struct CompressedHeaderError;

impl std::fmt::Display for CompressedHeaderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("reading compressed Codex rollout header")
    }
}

pub(super) fn header(path: &Path) -> Result<Vec<Value>> {
    if !compressed(path) {
        return json_lines_prefix(path, 1);
    }
    // Listing thousands of archives should consume only their first line.
    let mut line = Vec::new();
    BufReader::new(open(path)?.take(MAX_TRANSCRIPT_LINE_SIZE + 1))
        .read_until(b'\n', &mut line)
        .context(CompressedHeaderError)?;
    if u64::try_from(line.len())? > MAX_TRANSCRIPT_LINE_SIZE {
        bail!("Codex rollout header exceeds safe line limit");
    }
    Ok(serde_json::from_slice(&line).ok().into_iter().collect())
}

#[derive(Default)]
pub(super) struct NativeReadBudget {
    encoded: u64,
    decoded: u64,
}

impl NativeReadBudget {
    pub(super) fn contains_prefix(
        &mut self,
        path: &Path,
        end: u64,
        check: impl FnOnce(&mut dyn Read) -> Result<bool>,
    ) -> Result<bool> {
        let file = physical_file(path)?;
        let length = file.metadata()?.len();
        let remaining = MAX_STREAMED_TRANSCRIPT_FILE_SIZE - self.decoded;
        if end > remaining {
            return Ok(false);
        }
        if !compressed(path) {
            if end > length {
                return Ok(false);
            }
            self.decoded += end;
            let mut prefix = file.take(end);
            return Ok(check(&mut prefix)? && prefix.limit() == 0);
        }
        if length > MAX_STREAMED_TRANSCRIPT_FILE_SIZE - self.encoded {
            return Ok(false);
        }
        self.encoded += length;
        let mut reader = decoder(path, file)?.take(remaining + 1);
        let mut prefix = reader.by_ref().take(end);
        if !check(&mut prefix)? || prefix.limit() != 0 {
            return Ok(false);
        }
        // Native Codex must also open the suffix beyond an otherwise valid bookmark.
        let bytes = end + std::io::copy(&mut reader, &mut std::io::sink())?;
        if bytes > remaining {
            return Ok(false);
        }
        self.decoded += bytes;
        Ok(true)
    }
}

pub(super) const LIMITS: JsonLinesLimits = JsonLinesLimits {
    file_bytes: MAX_STREAMED_TRANSCRIPT_FILE_SIZE,
    records: usize::MAX,
    line_bytes: MAX_STREAMED_TRANSCRIPT_LINE_SIZE,
};

// Checking old search text is optional. Bound the decoded work of each verification pass.
pub(super) const LEGACY_PROJECTION_LIMITS: JsonLinesLimits = JsonLinesLimits {
    file_bytes: 64 * 1024 * 1024,
    records: 100_000,
    ..LIMITS
};

pub(super) fn visit_with_limits(
    path: &Path,
    limits: JsonLinesLimits,
    visit: impl FnMut(Value, JsonLineSkips) -> Result<()>,
) -> Result<JsonLineSkips> {
    read_records_with_limits(open(path)?, limits, visit)
}

pub(super) fn visit(
    path: &Path,
    visit: impl FnMut(Value, JsonLineSkips) -> Result<()>,
) -> Result<JsonLineSkips> {
    read_records(open(path)?, visit)
}

/// Returns whether every nonblank record could be read and the visitor accepted it.
pub(super) fn visit_all_records(
    path: &Path,
    mut visit: impl FnMut(Value) -> Result<bool>,
) -> Result<bool> {
    let mut accepted = true;
    let (omissions, _) = visit_json_reader(
        &mut BufReader::new(open(path)?),
        LIMITS,
        workspace_record_candidate,
        |_, record, _| {
            accepted &= visit(record)?;
            Ok(())
        },
    )?;
    Ok(accepted && omissions.oversized == 0 && omissions.malformed == 0)
}

pub(super) fn read_records(
    reader: impl Read,
    visit: impl FnMut(Value, JsonLineSkips) -> Result<()>,
) -> Result<JsonLineSkips> {
    read_records_with_limits(reader, LIMITS, visit)
}

fn read_records_with_limits(
    reader: impl Read,
    limits: JsonLinesLimits,
    mut visit: impl FnMut(Value, JsonLineSkips) -> Result<()>,
) -> Result<JsonLineSkips> {
    let session = OnceCell::<Option<String>>::new();
    visit_json_reader(
        &mut BufReader::new(reader),
        limits,
        |line, oversized| workspace_gap(line, oversized, session.get().and_then(Option::as_deref)),
        |_, record, skipped_before| {
            session.get_or_init(|| {
                (record["type"] == "session_meta")
                    .then(|| {
                        crate::support::string_at(&record["payload"], &[&["id"], &["session_id"]])
                    })
                    .flatten()
                    .map(str::to_owned)
            });
            visit(record, skipped_before)
        },
    )
    .map(|(omissions, _)| omissions)
}

pub(super) enum Sample {
    Complete(Vec<u8>),
    Edges {
        records: Vec<Value>,
        omitted: usize,
        unreadable: JsonLineSkips,
        prefix_records: usize,
        directory: Option<PathBuf>,
    },
    Reconstruct,
}

pub(super) enum WorkspaceRecord {
    Candidate,
    Compacted,
    Other,
}

pub(super) fn workspace_record_candidate(line: &[u8], oversized: bool) -> WorkspaceGap {
    workspace_gap(line, oversized, None)
}

pub(super) fn workspace_gap(line: &[u8], oversized: bool, session: Option<&str>) -> WorkspaceGap {
    let kind = if oversized {
        workspace_damage::oversized_workspace_record(line)
    } else {
        workspace_damage::damaged_workspace_record(line)
    };
    match kind {
        Some(DamagedWorkspace::TurnOrRollback) => WorkspaceGap::TurnOrRollback,
        Some(DamagedWorkspace::PossibleSettings) => {
            let owner =
                field(line, "payload").and_then(|payload| string_field(payload, "thread_id"));
            if session
                .zip(owner.as_deref())
                .is_some_and(|(selected, owner)| !same_thread(selected, owner))
            {
                WorkspaceGap::ForeignSettings
            } else {
                WorkspaceGap::PossibleSettings
            }
        }
        // Later bytes can name saved settings even when the retained prefix ends inside a field.
        Some(DamagedWorkspace::Unclassified) if oversized => WorkspaceGap::PossibleSettings,
        Some(DamagedWorkspace::Unclassified) => WorkspaceGap::Unclassified,
        None => WorkspaceGap::Other,
    }
}

pub(super) fn workspace_record_kind(line: &[u8]) -> WorkspaceRecord {
    if !line.trim_ascii_start().starts_with(b"{") {
        return WorkspaceRecord::Other;
    }
    match string_field(line, "type").as_deref() {
        Some("compacted") => WorkspaceRecord::Compacted,
        None | Some("turn_context" | "session_meta") => WorkspaceRecord::Candidate,
        Some("event_msg")
            if matches!(
                field(line, "payload")
                    .and_then(|payload| string_field(payload, "type"))
                    .as_deref(),
                None | Some("thread_rolled_back" | "thread_settings_applied")
            ) =>
        {
            WorkspaceRecord::Candidate
        }
        _ => WorkspaceRecord::Other,
    }
}

fn string_field(line: &[u8], name: &str) -> Option<String> {
    serde_json::Deserializer::from_slice(field(line, name)?)
        .into_iter::<String>()
        .next()?
        .ok()
}

/// Reads object keys in order. A field before the damage still identifies the enclosing record.
fn field<'a>(line: &'a [u8], target: &str) -> Option<&'a [u8]> {
    let mut rest = line.trim_ascii_start().strip_prefix(b"{")?;
    loop {
        let mut key = serde_json::Deserializer::from_slice(rest).into_iter::<String>();
        let name = key.next()?.ok()?;
        rest = rest[key.byte_offset()..]
            .trim_ascii_start()
            .strip_prefix(b":")?;
        if name == target {
            return Some(rest);
        }
        let mut ignored =
            serde_json::Deserializer::from_slice(rest).into_iter::<serde::de::IgnoredAny>();
        ignored.next()?.ok()?;
        rest = rest[ignored.byte_offset()..]
            .trim_ascii_start()
            .strip_prefix(b",")?;
    }
}

/// Read small rollouts once. Larger compressed files have no seekable tail, so sample the
/// decoded stream and inspect every record for a rollback that would invalidate those edges.
pub(super) fn sample(
    path: &Path,
    budget: u64,
    creation_directory: Option<&Path>,
    session: &str,
) -> Result<Sample> {
    let mut reader = open(path)?;
    let mut prefix = Vec::new();
    // Indexing normally supplies 16 MiB. Keep a larger caller budget from forcing a huge buffer.
    let budget = budget.min(16 * 1024 * 1024);
    reader.by_ref().take(budget + 1).read_to_end(&mut prefix)?;
    if u64::try_from(prefix.len())? <= budget {
        return Ok(Sample::Complete(prefix));
    }
    let mut head = Vec::new();
    let mut tail = VecDeque::new();
    let mut count = 0;
    let mut prefix_records = 0;
    let mut workspace = WorkspaceScan::new(creation_directory);
    let (unreadable, bytes) = visit_json_reader(
        &mut BufReader::new(Cursor::new(prefix).chain(reader)),
        LIMITS,
        |line, oversized| workspace_gap(line, oversized, Some(session)),
        |loc, record, skipped| {
            let ordinal = count + skipped.total();
            count += 1;
            workspace.settings.observe_gaps(skipped);
            workspace.observe(session, &record)?;
            if head.len() < SAMPLE_RECORDS && loc.offset + loc.length <= MAX_PREVIEW_TAIL_SIZE {
                head.push(record);
                if ordinal == prefix_records {
                    prefix_records += 1;
                }
            } else {
                while tail.front().is_some_and(|(offset, _, _)| {
                    loc.offset + loc.length - offset > MAX_PREVIEW_TAIL_SIZE
                }) {
                    tail.pop_front();
                }
                if loc.length <= MAX_PREVIEW_TAIL_SIZE {
                    if tail.len() == SAMPLE_RECORDS {
                        tail.pop_front();
                    }
                    tail.push_back((loc.offset, ordinal, record));
                }
            }
            Ok(())
        },
    )?;
    workspace.settings.observe_gaps(unreadable);
    if workspace.reconstruct_history || unreadable.workspace_gaps() > 0 {
        return Ok(Sample::Reconstruct);
    }
    for (offset, ordinal, record) in tail {
        if bytes - offset <= MAX_PREVIEW_TAIL_SIZE {
            if ordinal == prefix_records {
                prefix_records += 1;
            }
            head.push(record);
        }
    }
    Ok(Sample::Edges {
        omitted: count - head.len(),
        prefix_records,
        unreadable,
        directory: workspace.settings.directory(workspace.directory),
        records: head,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compressed_ancestor_budget_is_shared_across_the_chain() -> Result<()> {
        let home = tempfile::tempdir()?;
        let path = home.path().join("ancestor.jsonl.zst");
        let records = b"fixture data inside and after a bookmark";
        let frame = zstd::bulk::compress(records, 1)?;
        fs::write(&path, &frame)?;
        let size = |n| MAX_STREAMED_TRANSCRIPT_FILE_SIZE - 2 * n + 1;
        let check = |reader: &mut dyn Read| Ok(std::io::copy(reader, &mut std::io::sink())? > 0);
        for mut budget in [
            NativeReadBudget {
                encoded: size(u64::try_from(frame.len())?),
                decoded: 0,
            },
            NativeReadBudget {
                encoded: 0,
                decoded: size(u64::try_from(records.len())?),
            },
        ] {
            assert!(budget.contains_prefix(&path, 1, check)?);
            assert!(!budget.contains_prefix(&path, 1, check)?);
        }
        let plain = home.path().join("ancestor.jsonl");
        fs::write(&plain, records)?;
        let mut budget = NativeReadBudget {
            decoded: MAX_STREAMED_TRANSCRIPT_FILE_SIZE - u64::try_from(records.len())?,
            ..NativeReadBudget::default()
        };
        assert!(budget.contains_prefix(&plain, 1, check)?);
        assert!(!budget.contains_prefix(&path, 1, check)?);
        Ok(())
    }
}
