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

fn matching_header(first: &Value, other: &Value) -> bool {
    if first == other {
        return true;
    }
    if first["type"] != "session_meta" || first["ordinal"].as_u64().is_some() {
        return false;
    }
    let (Some(first), Some(other)) = (first.as_object(), other.as_object()) else {
        return false;
    };
    let Some(ordinal) = other.get("ordinal").filter(|value| value.is_u64()) else {
        return false;
    };
    if !other
        .keys()
        .all(|key| key == "ordinal" || first.contains_key(key))
    {
        return false;
    }
    let mut renamed_ordinal = false;
    for (key, value) in first {
        if key == "ordinal" || other.get(key) == Some(value) {
            continue;
        }
        // A torn ordinal key can leave its number under an otherwise unknown name.
        if other.contains_key(key) || renamed_ordinal || value != ordinal {
            return false;
        }
        renamed_ordinal = true;
    }
    true
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
    if !matching_header(&first, &other) {
        return Ok(false);
    }
    let mut previous = other["ordinal"].as_u64();
    let mut aligned = true;
    loop {
        match preferred.next() {
            Ok(Row::Record(record)) => {
                let Some(ordinal) = record["ordinal"].as_u64() else {
                    if record.get("type").is_some() || record.get("payload").is_some() {
                        return Ok(false);
                    }
                    aligned = false;
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
