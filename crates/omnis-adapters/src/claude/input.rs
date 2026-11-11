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
use serde_json::{Value, json};

use super::{ClaudeTranscript, EventBuilder};
use crate::support::{
    BoundedLine, MAX_COLLECTED_TRANSCRIPT_FILE_SIZE, MAX_PREVIEW_TAIL_SIZE, MAX_PROVIDER_RECORDS,
    MAX_STREAMED_TRANSCRIPT_LINE_SIZE, read_bounded_line_with_budget,
};

const PREVIEW_RECORDS: usize = 1_024;
// Progress records can connect a reply to its parent. Only their UUID links need storage.
const MAX_PROGRESS_LINKS: usize = 1_000_000;

#[derive(Default)]
pub(super) struct InputCoverage {
    oversized: usize,
    malformed: usize,
    incomplete_tail: bool,
    sampled: bool,
}

impl InputCoverage {
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
    ) -> Result<()> {
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
                return Ok(());
            }
            match kind {
                BoundedLine::Oversized => self.oversized += 1,
                BoundedLine::Complete => match serde_json::from_slice(&line) {
                    Ok(record) => {
                        if !visit(record)? {
                            return Ok(());
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
        Ok(())
    }
}

pub(super) fn read(path: &Path, preview: bool) -> Result<(ClaudeTranscript, InputCoverage)> {
    let mut file = File::open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || (!preview && metadata.len() > MAX_COLLECTED_TRANSCRIPT_FILE_SIZE) {
        bail!("provider file exceeds safe streaming limit");
    }
    let length = metadata.len();
    let mut transcript = ClaudeTranscript::default();
    let mut coverage = InputCoverage::default();
    if preview && length > MAX_PREVIEW_TAIL_SIZE * 2 {
        coverage.sampled = true;
        let mut messages = 0;
        coverage.scan(&mut file, MAX_PREVIEW_TAIL_SIZE, false, |record| {
            transcript.observe(&record);
            messages += usize::from(record["type"] != "progress");
            Ok(messages < PREVIEW_RECORDS)
        })?;

        // Start one byte early so a record exactly on the window boundary survives.
        let start = length - MAX_PREVIEW_TAIL_SIZE - 1;
        file.seek(SeekFrom::Start(start))?;
        let mut reader = BufReader::new(file.take(length - start));
        let skipped = reader.skip_until(b'\n')? as u64;
        let mut tail = VecDeque::new();
        let mut tail_messages = 0;
        coverage.scan(reader, length - start - skipped, true, |record| {
            tail_messages += usize::from(record["type"] != "progress");
            tail.push_back(record);
            while tail_messages > PREVIEW_RECORDS {
                let removed = tail.pop_front().expect("nonempty Claude preview tail");
                tail_messages -= usize::from(removed["type"] != "progress");
            }
            Ok(true)
        })?;
        transcript.branch.start_sampled_tail();
        for record in tail {
            transcript.observe(&record);
        }
    } else {
        let mut records = 0;
        let mut progress_links = 0;
        coverage.scan(file, length, true, |record| {
            if record["type"] == "progress" {
                progress_links += usize::from(record["uuid"].is_string());
                if progress_links > MAX_PROGRESS_LINKS {
                    bail!("Claude progress links exceed safe record limit");
                }
            } else {
                records += 1;
                if records > MAX_PROVIDER_RECORDS {
                    bail!("provider file exceeds safe record limit");
                }
            }
            transcript.observe(&record);
            Ok(true)
        })?;
    }
    Ok((transcript, coverage))
}
