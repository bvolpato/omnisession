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
    let file = physical_file(path)?;
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

/// Codex compares each fork bookmark with the uncompressed length of its ancestor.
pub(super) fn contains_native_prefix(path: &Path, end: u64) -> Result<bool> {
    let mut file = physical_file(path)?;
    if !compressed(path) {
        return Ok(end <= file.metadata()?.len());
    }
    let mut header = [0; 18];
    let read = file.read(&mut header)?;
    if zstd::zstd_safe::get_frame_content_size(&header[..read])
        .ok()
        .flatten()
        .is_some_and(|length| length >= end)
    {
        return Ok(true);
    }
    // A concatenated frame or one without a declared size requires a bounded decode.
    Ok(std::io::copy(&mut open(path)?.take(end), &mut std::io::sink())? == end)
}

const LIMITS: JsonLinesLimits = JsonLinesLimits {
    file_bytes: MAX_STREAMED_TRANSCRIPT_FILE_SIZE,
    records: usize::MAX,
    line_bytes: MAX_STREAMED_TRANSCRIPT_LINE_SIZE,
};

pub(super) fn visit(
    path: &Path,
    visit: impl FnMut(Value, usize) -> Result<()>,
) -> Result<JsonLineSkips> {
    read_records(open(path)?, visit)
}

pub(super) fn read_records(
    reader: impl Read,
    mut visit: impl FnMut(Value, usize) -> Result<()>,
) -> Result<JsonLineSkips> {
    visit_json_reader(
        &mut BufReader::new(reader),
        LIMITS,
        |_, record, skipped_before| visit(record, skipped_before),
    )
    .map(|(skipped, _)| skipped)
}

pub(super) enum Sample {
    Complete(Vec<u8>),
    Edges {
        records: Vec<Value>,
        omitted: usize,
        /// Number of leading records retained without a gap.
        prefix_records: usize,
    },
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
    let mut prefix_records = 0;
    let mut reconstruct = false;
    let (skipped, bytes) = visit_json_reader(
        &mut BufReader::new(Cursor::new(prefix).chain(reader)),
        LIMITS,
        |loc, record, skipped_before| {
            let ordinal = count + skipped_before;
            count += 1;
            reconstruct |= record["type"] == "compacted"
                || (record["type"] == "event_msg"
                    && record["payload"]["type"] == "thread_rolled_back")
                || (record["type"] == "session_meta"
                    && !record["payload"]["history_base"].is_null());
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
    if reconstruct {
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
        prefix_records,
        omitted: count + skipped.total() - head.len(),
        records: head,
    })
}
