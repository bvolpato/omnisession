//! Recognize a restored native sequence that includes an earlier fragment in full.

use std::collections::HashMap;

use serde_json::Value;
use uuid::Uuid;

use super::{ClaudeRecord, RecordDigest, is_main_record, record_uuid, string_at};

pub(super) fn identified(row: &Value) -> Option<Uuid> {
    is_main_record(row).then(|| record_uuid(row, "uuid"))?
}

fn conversation(row: &Value) -> bool {
    matches!(
        string_at(row, &[&["message", "role"], &["role"], &["type"]]),
        Some("user" | "assistant")
    )
}

fn parent_field(row: &Value) -> &'static str {
    if row["type"] == "system" && row["subtype"] == "compact_boundary" {
        "logicalParentUuid"
    } else {
        "parentUuid"
    }
}

#[derive(Default)]
pub(super) struct Probe {
    start: Option<usize>,
    position: usize,
    repeated: bool,
}

impl Probe {
    pub(super) fn observe(&mut self, row: &Value, records: &HashMap<Uuid, ClaudeRecord>) {
        let position = self.position;
        self.position += 1;
        if self.repeated || records.is_empty() || !is_main_record(row) {
            return;
        }
        if self.start.is_none() {
            let field = parent_field(row);
            if row.get(field).is_some_and(Value::is_null)
                || (row["type"] != "progress"
                    && conversation(row)
                    && record_uuid(row, field).is_some_and(|p| !records.contains_key(&p)))
            {
                self.start = Some(position);
            }
        }
        self.repeated =
            self.start.is_some() && identified(row).is_some_and(|id| records.contains_key(&id));
    }

    pub(super) fn proposed_start(&self) -> Option<usize> {
        self.start.filter(|_| self.repeated)
    }
}

struct Definition {
    digest: [u8; 32],
    last: usize,
}

struct Root {
    position: usize,
    parent: Option<Uuid>,
}

#[derive(Default)]
pub(super) struct Occurrences {
    definitions: HashMap<Uuid, Definition>,
    roots: Vec<Root>,
    position: usize,
    sampled_tail: usize,
    blocked: bool,
}

impl Occurrences {
    pub(super) fn start_tail(&mut self) {
        self.sampled_tail = self.position;
    }

    pub(super) fn observe(&mut self, row: &Value) {
        let position = self.position;
        self.position += 1;
        if self.blocked || !is_main_record(row) {
            return;
        }
        let Some(id) = identified(row) else {
            // Without an explicit identity an earlier turn still depends on file order.
            self.blocked = conversation(row);
            return;
        };
        let field = parent_field(row);
        let parent = record_uuid(row, field);
        if row
            .get(field)
            .is_none_or(|value| !value.is_null() && parent.is_none())
        {
            self.blocked = true;
            return;
        }
        if !self.definitions.is_empty() && parent.is_none_or(|p| !self.definitions.contains_key(&p))
        {
            self.roots.push(Root { position, parent });
        }
        let digest = RecordDigest::of(row);
        let definition = self.definitions.entry(id).or_insert(Definition {
            digest,
            last: position,
        });
        if definition.digest != digest {
            // Different contents under the same UUID keep their original temporal ownership.
            self.blocked = true;
            return;
        }
        definition.last = position;
    }

    pub(super) fn restart(self) -> Option<usize> {
        if self.blocked {
            return None;
        }
        // Every native definition must occur at or after this root. Earlier
        // fragments can overlap or arrive out of order. Their verified contents
        // and links are all present in the complete sequence's own write order.
        let latest = self
            .definitions
            .values()
            .map(|definition| definition.last)
            .min()?;
        self.roots
            .iter()
            .rev()
            .find(|root| {
                root.position <= latest
                    && root.position >= self.sampled_tail
                    && root
                        .parent
                        .is_none_or(|id| !self.definitions.contains_key(&id))
            })
            .map(|root| root.position)
    }
}
