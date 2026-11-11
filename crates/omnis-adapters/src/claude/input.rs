//! Bounded transcript reads for Claude's conversation and progress records.

use std::{
    collections::VecDeque,
    fs::File,
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    path::Path,
};

use anyhow::{Result, bail};
use chrono::{DateTime, Utc};
use omnis_ir::{EventKind, ReplayPolicy};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{ClaudeTranscript, EventBuilder, copies};
use crate::support::{
    BoundedLine, MAX_COLLECTED_TRANSCRIPT_FILE_SIZE, MAX_PREVIEW_TAIL_SIZE, MAX_PROVIDER_RECORDS,
    MAX_STREAMED_TRANSCRIPT_LINE_SIZE, read_bounded_line_with_budget,
};

const PREVIEW_RECORDS: usize = 1_024;
const MAX_PREVIEW_GAP_BYTES: u64 = MAX_PREVIEW_TAIL_SIZE;
const MAX_PREVIEW_GAP_RECORDS: usize = 65_536;
// Progress records can connect a reply to its parent. Only their UUID links need storage.
const MAX_PROGRESS_LINKS: usize = 1_000_000;

#[derive(Default)]
pub(super) struct InputCoverage {
    oversized: usize,
    malformed: usize,
    incomplete_tail: bool,
    pub(super) sampled: bool,
}

impl InputCoverage {
    pub(super) fn has_gaps(&self) -> bool {
        self.sampled || self.oversized != 0 || self.malformed != 0
    }

    pub(super) fn report(self, builder: &mut EventBuilder, timestamp: Option<DateTime<Utc>>) {
        builder.push_oversized_record_notice(self.oversized, timestamp);
        if self.malformed > 0 {
            builder.push(
                EventKind::ProviderEvent,
                json!({"omitted_events": self.malformed, "incomplete_final_record": self.incomplete_tail}),
                timestamp,
                ReplayPolicy::HistoricalOnly,
                Some("omnisession.malformed_jsonl".to_owned()),
                None,
            );
        }
        if self.sampled {
            builder.push(
                EventKind::ProviderEvent,
                json!({"native_history_unverified": true, "reason": "Claude preview sampled the transcript head and tail"}),
                timestamp,
                ReplayPolicy::HistoricalOnly,
                Some("omnisession.preview_limit".to_owned()),
                None,
            );
        }
    }

    // A byte window that ends inside a record cannot determine whether that record is malformed.
    fn scan(
        &mut self,
        reader: impl Read,
        length: u64,
        ends_at_eof: bool,
        mut visit: impl FnMut(Value) -> Result<bool>,
    ) -> Result<u64> {
        let mut reader = BufReader::new(reader.take(length));
        let mut remaining = length;
        let mut line = Vec::new();
        while let Some((kind, bytes)) = read_bounded_line_with_budget(
            &mut reader,
            MAX_STREAMED_TRANSCRIPT_LINE_SIZE,
            remaining,
            &mut line,
        )? {
            remaining -= bytes;
            let clipped = remaining == 0 && !ends_at_eof && !line.ends_with(b"\n");
            if clipped {
                return Ok(length - bytes);
            }
            match kind {
                BoundedLine::Oversized => self.oversized += 1,
                BoundedLine::Complete => match serde_json::from_slice(&line) {
                    Ok(record) => {
                        if !visit(record)? {
                            return Ok(length - remaining);
                        }
                    }
                    Err(error) if !line.iter().all(u8::is_ascii_whitespace) => {
                        self.malformed += 1;
                        self.incomplete_tail |= error.is_eof() && !line.ends_with(b"\n");
                    }
                    Err(_) => {}
                },
            }
        }
        if remaining > 0 {
            bail!("Claude transcript became shorter during the read");
        }
        Ok(length)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GapRecord<'a> {
    #[serde(rename = "type")]
    kind: Option<&'a str>,
    subtype: Option<&'a str>,
    role: Option<&'a str>,
    #[serde(borrow)]
    message: Option<GapMessage<'a>>,
    #[serde(default)]
    is_sidechain: bool,
    team_name: Option<&'a str>,
    #[serde(default)]
    is_compact_summary: bool,
    uuid: Option<&'a str>,
    parent_uuid: Option<&'a str>,
}

#[derive(Deserialize)]
struct GapMessage<'a> {
    role: Option<&'a str>,
}

// A short gap can contain transparent links. A turn or boundary ends the scan.
fn read_gap_links(
    file: &mut File,
    start: u64,
    end: u64,
    observe: &mut impl FnMut(Observation<'_>),
) -> Result<bool> {
    let mut remaining = end - start;
    if remaining > MAX_PREVIEW_GAP_BYTES {
        return Ok(false);
    }
    file.seek(SeekFrom::Start(start))?;
    let mut reader = BufReader::new(file.take(remaining));
    let mut line = Vec::new();
    for _ in 0..MAX_PREVIEW_GAP_RECORDS {
        if remaining == 0 {
            return Ok(true);
        }
        line.clear();
        let read = reader.read_until(b'\n', &mut line)? as u64;
        if read == 0 {
            bail!("Claude transcript became shorter during the read");
        }
        remaining -= read;
        let Ok(record) = serde_json::from_slice::<GapRecord>(&line) else {
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            return Ok(false);
        };
        if record.is_sidechain || record.team_name.is_some() {
            continue;
        }
        let role = record
            .message
            .and_then(|m| m.role)
            .or(record.role)
            .or(record.kind);
        if record.kind != Some("progress")
            && (matches!(record.kind, Some("user" | "assistant"))
                || matches!(role, Some("user" | "assistant"))
                || record.is_compact_summary
                || (record.kind == Some("system") && record.subtype == Some("compact_boundary")))
        {
            return Ok(false);
        }
        if let Some(id) = record.uuid {
            let Some(parent) = record.parent_uuid else {
                return Ok(false);
            };
            // Comparisons need the complete attachment, even when the preview only keeps its link.
            let definition = if record.kind == Some("attachment") {
                serde_json::from_slice(&line).ok()
            } else {
                None
            };
            observe(Observation::Link {
                row: &json!({"type": record.kind, "uuid": id, "parentUuid": parent}),
                definition: definition.as_ref(),
            });
        }
    }
    Ok(remaining == 0)
}

#[derive(Clone, Copy)]
enum Observation<'a> {
    Record(&'a Value),
    Link {
        row: &'a Value,
        definition: Option<&'a Value>,
    },
    Tail(bool),
}

impl Observation<'_> {
    fn apply_from(self, start: usize, position: &mut usize, transcript: &mut ClaudeTranscript) {
        match self {
            Self::Record(row) => {
                let before = *position < start;
                *position += 1;
                if before && copies::identified(row).is_some() {
                    return;
                }
            }
            // Links inside the sampled gap do not count as conversation positions.
            Self::Link { .. } if start > 0 && *position <= start => return,
            _ => {}
        }
        self.apply(transcript);
    }

    fn apply(self, transcript: &mut ClaudeTranscript) {
        match self {
            Self::Record(row) => transcript.observe(row),
            Self::Link { row, definition } => transcript.observe_definition(row, definition),
            Self::Tail(connected) => {
                if !connected {
                    transcript.compactions.pending = None;
                    transcript.compactions.absent_anchors.clear();
                    transcript.compactions.absent_summary_nodes.clear();
                }
                transcript.branch.start_sampled_tail(connected);
            }
        }
    }
}

// A restored file can contain the entire native JSONL stream twice. Check the
// line boundary before reading the two byte ranges on the same open file.
fn repeated_half(file: &mut File, length: u64) -> Result<Option<u64>> {
    if length < MAX_PREVIEW_TAIL_SIZE || !length.is_multiple_of(2) {
        return Ok(None);
    }
    let half = length / 2;
    let mut newline = [0];
    file.seek(SeekFrom::Start(half - 1))?;
    file.read_exact(&mut newline)?;
    if newline[0] != b'\n' {
        return Ok(None);
    }
    let mut first = [0; 8192];
    let mut second = [0; 8192];
    let mut offset = 0;
    while offset < half {
        let count = usize::try_from((half - offset).min(first.len() as u64))?;
        file.seek(SeekFrom::Start(offset))?;
        file.read_exact(&mut first[..count])?;
        file.seek(SeekFrom::Start(half + offset))?;
        file.read_exact(&mut second[..count])?;
        if first[..count] != second[..count] {
            return Ok(None);
        }
        offset += count as u64;
    }
    Ok(Some(half))
}

fn unchanged(file: &File, before: &std::fs::Metadata) -> Result<()> {
    let after = file.metadata()?;
    if after.len() != before.len() || after.modified()? != before.modified()? {
        bail!("Claude transcript changed while verifying a restored copy");
    }
    Ok(())
}

pub(super) fn read(path: &Path, preview: bool) -> Result<(ClaudeTranscript, InputCoverage)> {
    let mut file = File::open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || (!preview && metadata.len() > MAX_COLLECTED_TRANSCRIPT_FILE_SIZE) {
        bail!("provider file exceeds safe streaming limit");
    }
    let length = metadata.len();
    let mut transcript = ClaudeTranscript::default();
    if (!preview || length <= MAX_PREVIEW_TAIL_SIZE * 2)
        && let Some(half) = repeated_half(&mut file, length)?
    {
        let mut known = true;
        let coverage = visit(&mut file, half, false, 2, |observation| {
            if let Observation::Record(row) = observation {
                known &= copies::has_known_links(row, &transcript.records);
            }
            observation.apply(&mut transcript);
        })?;
        if known
            && !transcript.records.is_empty()
            && transcript.duplicate_records.is_empty()
            && coverage.malformed == 0
            && coverage.oversized == 0
            && transcript
                .compactions
                .by_summary
                .values()
                .all(|summary| summary.error.is_none())
        {
            unchanged(&file, &metadata)?;
            return Ok((transcript, coverage));
        }
        transcript = ClaudeTranscript::default();
    }
    let mut probe = copies::Probe::default();
    let coverage = visit(&mut file, length, preview, 1, |observation| {
        if let Observation::Record(row) = observation {
            probe.observe(row, &transcript.records);
        }
        observation.apply(&mut transcript);
    })?;
    let Some(proposed) = probe.proposed_start() else {
        return Ok((transcript, coverage));
    };
    // Verify the entire copy while reducing it. A conflicting definition or a
    // different restore boundary needs one further bounded pass on the same file.
    let mut index = copies::Occurrences::new(&mut transcript);
    transcript = ClaudeTranscript::default();
    let mut position = 0;
    visit(&mut file, length, preview, 1, |observation| {
        match observation {
            Observation::Record(row) => index.observe(row),
            Observation::Tail(_) => index.start_tail(),
            Observation::Link { .. } => {}
        }
        observation.apply_from(proposed, &mut position, &mut transcript);
    })?;
    let start = index.restart().unwrap_or_default();
    if start != proposed {
        transcript = ClaudeTranscript::default();
        position = 0;
        visit(&mut file, length, preview, 1, |observation| {
            observation.apply_from(start, &mut position, &mut transcript);
        })?;
    }
    unchanged(&file, &metadata)?;
    Ok((transcript, coverage))
}

fn visit(
    file: &mut File,
    length: u64,
    preview: bool,
    physical_copies: usize,
    mut observe: impl FnMut(Observation<'_>),
) -> Result<InputCoverage> {
    file.rewind()?;
    let mut coverage = InputCoverage::default();
    if preview && length > MAX_PREVIEW_TAIL_SIZE * 2 {
        coverage.sampled = true;
        let mut messages = 0;
        let head_end = coverage.scan(&mut *file, MAX_PREVIEW_TAIL_SIZE, false, |record| {
            observe(Observation::Record(&record));
            messages += usize::from(record["type"] != "progress");
            Ok(messages < PREVIEW_RECORDS)
        })?;

        // Start one byte early so a record exactly on the window boundary survives.
        let start = length - MAX_PREVIEW_TAIL_SIZE - 1;
        file.seek(SeekFrom::Start(start))?;
        let mut reader = BufReader::new((&mut *file).take(length - start));
        let skipped = reader.skip_until(b'\n')? as u64;
        let mut tail = VecDeque::new();
        let mut tail_messages = 0;
        let mut tail_trimmed = false;
        coverage.scan(reader, length - start - skipped, true, |record| {
            tail_messages += usize::from(record["type"] != "progress");
            tail.push_back(record);
            while tail_messages > PREVIEW_RECORDS {
                tail_trimmed = true;
                let removed = tail.pop_front().expect("nonempty Claude preview tail");
                tail_messages -= usize::from(removed["type"] != "progress");
            }
            Ok(true)
        })?;
        let connected =
            !tail_trimmed && read_gap_links(file, head_end, start + skipped, &mut observe)?;
        observe(Observation::Tail(connected));
        for record in tail {
            observe(Observation::Record(&record));
        }
    } else {
        let mut records = 0;
        let mut progress_links = 0;
        coverage.scan(file, length, true, |record| {
            if record["type"] == "progress" {
                progress_links += physical_copies * usize::from(record["uuid"].is_string());
                if progress_links > MAX_PROGRESS_LINKS {
                    bail!("Claude progress links exceed safe record limit");
                }
            } else {
                records += physical_copies;
                if records > MAX_PROVIDER_RECORDS {
                    bail!("provider file exceeds safe record limit");
                }
            }
            observe(Observation::Record(&record));
            Ok(true)
        })?;
    }
    Ok(coverage)
}
