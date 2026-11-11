//! Compares the surviving records in a damaged parent with a complete copy of its bookmark.

use std::{
    io::{self, BufReader, Read, Take},
    path::Path,
};

use anyhow::{Result, ensure};
use serde_json::Value;

use super::{HeaderEvidence, HistoryMode, Position};
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

fn restore_field(record: &mut Value, other: &Value, key: &str, allow_missing: bool) -> bool {
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
    } else if !allow_missing {
        return false;
    }
    fields.insert(key.to_owned(), value.clone());
    true
}

fn one_saved_payload(header: &Value, owner: &str) -> bool {
    let Some(fields) = header.as_object() else {
        return false;
    };
    let mut owners = fields.values().filter_map(|field| {
        let id = field["id"].as_str()?;
        (HistoryMode::is_paginated(&field["history_mode"]) && uuid::Uuid::try_parse(id).is_ok())
            .then_some(id)
    });
    owners.next() == Some(owner) && owners.next().is_none()
}

pub(in crate::codex) fn matching_header(mut first: Value, other: &Value) -> bool {
    let Some(owner) = other["payload"]["id"].as_str() else {
        return false;
    };
    if HeaderEvidence::read(other, owner) != HeaderEvidence::Native(HistoryMode::Paginated) {
        return false;
    }
    if first == *other {
        return true;
    }
    // A surviving owner value can corroborate a complete backup. A filename alone cannot.
    if first.get("payload").is_none()
        && (!one_saved_payload(&first, owner)
            || !restore_field(&mut first, other, "payload", false))
    {
        return false;
    }
    if !restore_field(&mut first["payload"], &other["payload"], "id", false) {
        return false;
    }
    if first["type"] != "session_meta" {
        if !super::rollout_identity::damaged_header_type(&first) {
            return false;
        }
        if first.get("type").is_some() {
            first["type"] = other["type"].clone();
        } else if !restore_field(&mut first, other, "type", true) {
            return false;
        }
    }
    if super::rollout_identity::damaged_paginated_mode(&first) {
        if first["payload"].get("history_mode") == Some(&Value::Null) {
            first["payload"]["history_mode"] = other["payload"]["history_mode"].clone();
        } else if !restore_field(
            &mut first["payload"],
            &other["payload"],
            "history_mode",
            true,
        ) {
            return false;
        }
    }
    restore_field(&mut first, other, "ordinal", true) && first == *other
}

/// A complete copy can repair split JSON lines only if every surviving byte agrees.
pub(super) fn agrees_across_split_lines(
    original: &Path,
    candidate: &Path,
    position: Position,
) -> Result<bool> {
    let mut source = Rows::open(original, position.end_byte_offset)?;
    let mut archive = Rows::open(candidate, position.end_byte_offset)?;
    Ok(agreeing_fragments(&mut source, &mut archive, None))
}

fn agreeing_fragments(source: &mut Rows, archive: &mut Rows, mut first: Option<Row>) -> bool {
    loop {
        match archive.next() {
            Ok(Row::End) => return first.is_none() && matches!(source.next(), Ok(Row::End)),
            Ok(Row::Record(_)) => {}
            _ => return false,
        }
        let mut rest = archive.line.as_slice();
        while !rest.is_empty() {
            let Ok(kind @ (Row::Record(_) | Row::Malformed)) =
                first.take().map_or_else(|| source.next(), Ok)
            else {
                return false;
            };
            if let Row::Record(record) = kind
                && record["type"].is_string()
                && record["ordinal"].is_u64()
                && record.get("payload").is_some()
                && rest != source.line
            {
                return false;
            }
            let Some(fragment) = source.line.strip_suffix(b"\n") else {
                return false;
            };
            let Some(remainder) = rest.strip_prefix(fragment) else {
                return false;
            };
            // A real archive delimiter must coincide with a physical source delimiter.
            // Only an additional newline within the archive row can replace a lost byte.
            let Some((_, following)) = remainder.split_first() else {
                return false;
            };
            rest = following;
        }
    }
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
    loop {
        match preferred.next() {
            Ok(Row::Record(mut record)) => {
                let Some(ordinal) = record["ordinal"].as_u64() else {
                    if record.get("type").is_none() && record.get("payload").is_none() {
                        return Ok(agreeing_fragments(
                            &mut preferred,
                            &mut archive,
                            Some(Row::Record(record)),
                        ));
                    }
                    // Only the next row after an intact match can prove the missing position.
                    let Ok(Row::Record(candidate)) = archive.next() else {
                        return Ok(false);
                    };
                    if !candidate["ordinal"].is_u64()
                        || !restore_field(&mut record, &candidate, "ordinal", true)
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
            }
            Ok(Row::Malformed) => {
                return Ok(agreeing_fragments(
                    &mut preferred,
                    &mut archive,
                    Some(Row::Malformed),
                ));
            }
            Ok(Row::Partial) => {
                // A torn final line can contain useful bytes before it becomes valid JSON.
                return Ok(matches!(archive.next(), Ok(Row::Record(_) | Row::Partial))
                    && archive.line.starts_with(&preferred.line));
            }
            Ok(Row::Oversized) => return Ok(false),
            Ok(Row::End) => return Ok(true),
            Err(_) => return Ok(rollout_file::compressed(original)),
        }
    }
}
