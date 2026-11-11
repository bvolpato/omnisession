//! Read the physical representation of a Codex JSONL rollout without modifying the source.

use std::{
    collections::VecDeque,
    fs::{self, File},
    io::{BufRead, BufReader, Cursor, Read},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use flate2::read::MultiGzDecoder;
use serde_json::Value;

use super::workspace_check::WorkspaceScan;
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

pub(super) fn open(path: &Path) -> Result<Box<dyn Read>> {
    if !fs::symlink_metadata(path)?.is_file() {
        bail!("Codex rollout must be a regular file");
    }
    let file = File::open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.len() > MAX_STREAMED_TRANSCRIPT_FILE_SIZE {
        bail!("Codex rollout exceeds safe streaming limit or is not a regular file");
    }
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

pub(super) fn header(path: &Path) -> Result<Vec<Value>> {
    if !compressed(path) {
        return json_lines_prefix(path, 1);
    }
    // Listing thousands of archives should consume only their first line.
    let mut line = Vec::new();
    BufReader::new(open(path)?.take(MAX_TRANSCRIPT_LINE_SIZE + 1))
        .read_until(b'\n', &mut line)
        .context("reading compressed Codex rollout header")?;
    if u64::try_from(line.len())? > MAX_TRANSCRIPT_LINE_SIZE {
        bail!("Codex rollout header exceeds safe line limit");
    }
    Ok(serde_json::from_slice(&line).ok().into_iter().collect())
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
    visit_json_reader(
        &mut BufReader::new(reader),
        limits,
        workspace_record_candidate,
        |_, record, skipped_before| visit(record, skipped_before),
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

pub(super) fn workspace_record_candidate(line: &[u8]) -> bool {
    matches!(workspace_record_kind(line), WorkspaceRecord::Candidate)
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
        workspace_record_candidate,
        |loc, record, skipped_before| {
            let ordinal = count + skipped_before.total();
            count += 1;
            workspace.observe(&record)?;
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
        directory: workspace.directory,
        records: head,
    })
}
