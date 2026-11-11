//! Compares the surviving records in a damaged parent with a complete copy of its bookmark.

use std::{
    io::{self, BufReader, Read, Take},
    path::Path,
};

use anyhow::{Result, ensure};
use serde_json::Value;

use super::Position;
use crate::{
    codex::rollout_file,
    support::{BoundedLine, MAX_STREAMED_TRANSCRIPT_LINE_SIZE, read_bounded_line_with_budget},
};

enum Row {
    Record(Value),
    Malformed,
    Partial,
    Oversized,
    End,
}

struct Rows {
    reader: BufReader<Take<Box<dyn Read>>>,
    line: Vec<u8>,
}

impl Rows {
    fn open(path: &Path, end: u64) -> Result<Self> {
        Ok(Self {
            reader: BufReader::new(rollout_file::open(path)?.take(end)),
            line: Vec::new(),
        })
    }

    fn next(&mut self) -> io::Result<Row> {
        let read = read_bounded_line_with_budget(
            &mut self.reader,
            MAX_STREAMED_TRANSCRIPT_LINE_SIZE,
            u64::MAX,
            &mut self.line,
        );
        Ok(match read {
            Ok(None) => Row::End,
            Ok(Some((BoundedLine::Oversized, _))) => Row::Oversized,
            Err(_) if !self.line.is_empty() => Row::Partial,
            Err(error) => return Err(error),
            Ok(Some(_)) if !self.line.ends_with(b"\n") => Row::Partial,
            Ok(Some(_)) => serde_json::from_slice(&self.line).map_or(Row::Malformed, Row::Record),
        })
    }

    fn matches_record(&mut self, expected: &Value, ordinal: u64) -> bool {
        while let Ok(Row::Record(current)) = self.next() {
            match current["ordinal"].as_u64() {
                Some(index) if index < ordinal => {}
                Some(index) if index == ordinal => return current == *expected,
                _ => break,
            }
        }
        false
    }
}

fn restore_field(record: &mut Value, other: &Value, key: &str) -> bool {
    let (Some(fields), Some(verified)) = (record.as_object_mut(), other.as_object()) else {
        return false;
    };
    let Some(value) = verified.get(key) else {
        return false;
    };
    if let Some(original) = fields.get(key) {
        return original == value;
    }
    let mut extra = fields.keys().filter(|field| !verified.contains_key(*field));
    if let Some(renamed) = extra.next() {
        if extra.next().is_some() || fields.get(renamed) != Some(value) {
            return false;
        }
        let renamed = renamed.clone();
        fields.remove(&renamed);
    }
    fields.insert(key.to_owned(), value.clone());
    true
}

fn matching_header(mut first: Value, other: &Value) -> bool {
    if first == *other {
        return true;
    }
    if first["type"] != "session_meta" || !other["ordinal"].is_u64() {
        return false;
    }
    if super::rollout_identity::damaged_paginated_mode(&first)
        && (!super::HistoryMode::is_paginated(&other["payload"]["history_mode"])
            || !restore_field(&mut first["payload"], &other["payload"], "history_mode"))
    {
        return false;
    }
    restore_field(&mut first, other, "ordinal") && first == *other
}

pub(super) fn agrees(original: &Path, candidate: &Path, position: Position) -> Result<bool> {
    let (Ok(mut preferred), Ok(mut archive)) = (
        Rows::open(original, position.end_byte_offset),
        Rows::open(candidate, position.end_byte_offset),
    ) else {
        return Ok(false);
    };
    let (Ok(Row::Record(first)), Ok(Row::Record(other))) = (preferred.next(), archive.next())
    else {
        return Ok(false);
    };
    if !matching_header(first, &other) {
        return Ok(false);
    }
    let mut previous = other["ordinal"].as_u64();
    let mut aligned = true;
    loop {
        match preferred.next() {
            Ok(Row::Record(mut record)) => {
                let Some(ordinal) = record["ordinal"].as_u64() else {
                    if record.get("type").is_none() && record.get("payload").is_none() {
                        aligned = false;
                        continue;
                    }
                    // Only the next row after an intact match can prove the missing position.
                    if !aligned {
                        return Ok(false);
                    }
                    let Ok(Row::Record(candidate)) = archive.next() else {
                        return Ok(false);
                    };
                    if !candidate["ordinal"].is_u64()
                        || !restore_field(&mut record, &candidate, "ordinal")
                        || record != candidate
                    {
                        return Ok(false);
                    }
                    previous = candidate["ordinal"].as_u64();
                    continue;
                };
                ensure!(
                    previous.is_none_or(|last| last < ordinal)
                        && ordinal < position.end_ordinal_exclusive,
                    "Codex history_base source ordinal is inconsistent"
                );
                if !archive.matches_record(&record, ordinal) {
                    return Ok(false);
                }
                previous = Some(ordinal);
                aligned = true;
            }
            Ok(Row::Malformed) => aligned = false,
            Ok(Row::Partial) => {
                // A torn final line can contain useful bytes before it becomes valid JSON.
                return Ok(aligned
                    && matches!(archive.next(), Ok(Row::Record(_) | Row::Partial))
                    && archive.line.starts_with(&preferred.line));
            }
            Ok(Row::Oversized) => return Ok(false),
            Ok(Row::End) => return Ok(true),
            Err(_) => return Ok(rollout_file::compressed(original)),
        }
    }
}
