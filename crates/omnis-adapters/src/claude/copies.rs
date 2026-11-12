//! Recognize a restored native sequence that includes an earlier fragment in full.

use std::collections::HashMap;

use serde_json::Value;
use uuid::Uuid;

use super::{
    ClaudeEventSource, ClaudeRecord, ClaudeTranscript, EventKind, RecordDigest, is_main_record,
    record_uuid, string_at, value_at,
};

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

pub(super) fn has_known_links(row: &Value, records: &HashMap<Uuid, ClaudeRecord>) -> bool {
    if !is_main_record(row) {
        return true;
    }
    let field = parent_field(row);
    if identified(row).is_none() {
        return !conversation(row) && field != "logicalParentUuid";
    }
    let known = |key| record_uuid(row, key).is_some_and(|id| records.contains_key(&id));
    let root = field == "parentUuid" && row.get(field).is_some_and(Value::is_null);
    (root || known(field))
        && (row["sourceToolAssistantUUID"].is_null() || known("sourceToolAssistantUUID"))
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

enum Issuers {
    Unique(Uuid),
    Reused(HashMap<Uuid, Option<Uuid>>),
}

pub(super) struct Occurrences {
    definitions: HashMap<Uuid, Option<Definition>>,
    calls: HashMap<String, Issuers>,
    ambiguous_results: bool,
    roots: Vec<usize>,
    position: usize,
    sampled_tail: usize,
    blocked: bool,
}

impl Occurrences {
    pub(super) fn new(transcript: &mut ClaudeTranscript) -> Self {
        let mut calls = HashMap::new();
        let mut reused = false;
        for event in &transcript.events {
            if event.kind == EventKind::ToolCalled
                && let Some(ClaudeEventSource::Record(issuer)) = event.source
                && let Some(id) = event.payload["id"].as_str()
            {
                let owners = calls
                    .entry(id.to_owned())
                    .or_insert(Issuers::Unique(issuer));
                if matches!(owners, Issuers::Unique(first) if *first != issuer) {
                    *owners = Issuers::Reused(HashMap::new());
                    reused = true;
                }
            }
        }
        let ambiguous_results = reused
            && transcript
                .selected_events_with_pairs(|events, pairs| {
                    for &(call, result) in pairs {
                        let call = &events[call];
                        if let (
                            Some(ClaudeEventSource::Record(issuer)),
                            Some(ClaudeEventSource::Record(outcome)),
                        ) = (call.source.as_ref(), events[result].source.as_ref())
                            && let Some(Issuers::Reused(bindings)) =
                                call.payload["id"].as_str().and_then(|id| calls.get_mut(id))
                        {
                            let known = bindings.entry(*outcome).or_insert(Some(*issuer));
                            if *known != Some(*issuer) {
                                *known = None;
                            }
                        }
                    }
                })
                .2
                .results;
        Self {
            definitions: transcript
                .records
                .keys()
                .map(|&uuid| (uuid, None))
                .collect(),
            calls,
            ambiguous_results,
            roots: Vec::new(),
            position: 0,
            sampled_tail: 0,
            blocked: false,
        }
    }

    pub(super) fn start_tail(&mut self) {
        self.sampled_tail = self.position;
    }

    // A UUID absent from the whole read remains external after any restart.
    fn dependency_limit(&self, uuid: Uuid) -> usize {
        self.definitions
            .get(&uuid)
            .map_or(self.position, |definition| {
                definition
                    .as_ref()
                    .map_or(0, |definition| definition.last + 1)
            })
    }

    fn first_occurrence_limit(&self, row: &Value, id: Uuid, parent: Option<Uuid>) -> usize {
        let mut limit = parent.map_or(self.position, |uuid| self.dependency_limit(uuid));
        if !row["sourceToolAssistantUUID"].is_null() {
            if let Some(issuer) = record_uuid(row, "sourceToolAssistantUUID") {
                limit = limit.min(self.dependency_limit(issuer));
            }
        } else if let Some(parts) =
            value_at(row, &[&["message", "content"], &["content"]]).and_then(Value::as_array)
        {
            for part in parts {
                if part["type"] == "tool_result"
                    && let Some(owners) = part["tool_use_id"]
                        .as_str()
                        .and_then(|id| self.calls.get(id))
                {
                    let issuer = match owners {
                        Issuers::Unique(issuer) => Some(issuer),
                        Issuers::Reused(bindings) => match bindings.get(&id) {
                            Some(Some(issuer)) => Some(issuer),
                            Some(None) => return 0,
                            None if self.ambiguous_results => return 0,
                            None => None,
                        },
                    };
                    if let Some(&issuer) = issuer {
                        limit = limit.min(self.dependency_limit(issuer));
                    }
                }
            }
        }
        if parent_field(row) == "logicalParentUuid" {
            let metadata = &row["compactMetadata"];
            if let Some(list) = metadata
                .get("preservedMessages")
                .filter(|value| !value.is_null())
            {
                for member in list["uuids"].as_array().into_iter().flatten() {
                    if let Some(uuid) = member.as_str().and_then(|id| id.parse().ok()) {
                        limit = limit.min(self.dependency_limit(uuid));
                    }
                }
            } else {
                for key in ["headUuid", "tailUuid"] {
                    if let Some(uuid) = record_uuid(&metadata["preservedSegment"], key) {
                        limit = limit.min(self.dependency_limit(uuid));
                    }
                }
            }
        }
        limit
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
        if parent.is_none_or(|p| !self.definitions.contains_key(&p)) {
            self.roots.push(position);
        }
        let limit = self.first_occurrence_limit(row, id, parent);
        let digest = RecordDigest::of(row);
        let entry = self.definitions.entry(id).or_default();
        let previous = entry.as_ref().map_or(0, |definition| definition.last + 1);
        if entry
            .as_ref()
            .is_some_and(|definition| definition.digest != digest)
        {
            self.blocked = true;
            return;
        }
        *entry = Some(Definition {
            digest,
            last: position,
        });
        // An earlier occurrence in the candidate already established this record.
        // Otherwise its first occurrence must follow all its dependencies.
        while self
            .roots
            .last()
            .is_some_and(|&root| root >= previous.max(limit))
        {
            self.roots.pop();
        }
    }

    pub(super) fn restart(self) -> Option<usize> {
        if self.blocked {
            return None;
        }
        let latest = self
            .definitions
            .values()
            .flatten()
            .map(|definition| definition.last)
            .min()?;
        self.roots
            .into_iter()
            .rev()
            .find(|&root| root <= latest && root >= self.sampled_tail)
    }
}
