//! Read the physical representation of a Codex JSONL rollout without modifying the source.

use std::{
    collections::VecDeque,
    fs::{self, File},
    io::{BufRead, BufReader, Cursor, Read},
    path::Path,
};

use anyhow::{Context, Result, bail};
use flate2::read::MultiGzDecoder;
use serde_json::Value;

use crate::support::{
    JsonLinesLimits, MAX_PREVIEW_TAIL_SIZE, MAX_STREAMED_TRANSCRIPT_FILE_SIZE,
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

const LIMITS: JsonLinesLimits = JsonLinesLimits {
    file_bytes: MAX_STREAMED_TRANSCRIPT_FILE_SIZE,
    records: usize::MAX,
    line_bytes: MAX_STREAMED_TRANSCRIPT_LINE_SIZE,
};

pub(super) fn visit(path: &Path, visit: impl FnMut(Value) -> Result<()>) -> Result<usize> {
    read_records(open(path)?, visit)
}

/// Returns whether every decoded byte belonged to a record the visitor accepted.
pub(super) fn visit_all_records(
    path: &Path,
    mut visit: impl FnMut(Value) -> Result<bool>,
) -> Result<bool> {
    let mut delivered_bytes = 0;
    let (_, decoded_bytes) = visit_json_reader(
        &mut BufReader::new(open(path)?),
        LIMITS,
        |location, record| {
            if visit(record)? {
                delivered_bytes += location.length;
            }
            Ok(())
        },
    )?;
    Ok(delivered_bytes == decoded_bytes)
}

pub(super) fn read_records(
    reader: impl Read,
    mut visit: impl FnMut(Value) -> Result<()>,
) -> Result<usize> {
    visit_json_reader(&mut BufReader::new(reader), LIMITS, |_, record| {
        visit(record)
    })
    .map(|(oversized, _)| oversized)
}

pub(super) enum Sample {
    Complete(Vec<u8>),
    Edges { records: Vec<Value>, omitted: usize },
    Reconstruct,
}

/// Read small rollouts once. Larger compressed files have no seekable tail, so sample the
/// decoded stream and inspect every record for a rollback that would invalidate those edges.
pub(super) fn sample(path: &Path, budget: u64) -> Result<Sample> {
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
    let mut reconstruct = false;
    let (oversized, bytes) = visit_json_reader(
        &mut BufReader::new(Cursor::new(prefix).chain(reader)),
        LIMITS,
        |loc, record| {
            count += 1;
            reconstruct |= record["type"] == "compacted"
                || (record["type"] == "event_msg"
                    && record["payload"]["type"] == "thread_rolled_back")
                || (record["type"] == "session_meta"
                    && !record["payload"]["history_base"].is_null());
            if head.len() < SAMPLE_RECORDS && loc.offset + loc.length <= MAX_PREVIEW_TAIL_SIZE {
                head.push(record);
            } else {
                while tail.front().is_some_and(|(offset, _)| {
                    loc.offset + loc.length - offset > MAX_PREVIEW_TAIL_SIZE
                }) {
                    tail.pop_front();
                }
                if loc.length <= MAX_PREVIEW_TAIL_SIZE {
                    if tail.len() == SAMPLE_RECORDS {
                        tail.pop_front();
                    }
                    tail.push_back((loc.offset, record));
                }
            }
            Ok(())
        },
    )?;
    if reconstruct {
        return Ok(Sample::Reconstruct);
    }
    head.extend(tail.into_iter().filter_map(|(offset, record)| {
        (bytes - offset <= MAX_PREVIEW_TAIL_SIZE).then_some(record)
    }));
    Ok(Sample::Edges {
        omitted: count + oversized - head.len(),
        records: head,
    })
}
