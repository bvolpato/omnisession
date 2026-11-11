//! Associate published corrections with retained physical tool results.

use super::paginated_rollback::json_digest;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};

type Digest = [u8; 32];

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct Receipt {
    input: Digest,
    output: Digest,
}

impl Receipt {
    pub(super) fn from_item(item: &Value) -> Option<Self> {
        let tag = &item["type"];
        let (input, output) = match tag.as_str()? {
            "CommandExecution" => (
                json_digest(&[tag, &item["command"], &item["cwd"]]),
                &item["aggregated_output"],
            ),
            "DynamicToolCall" => (
                json_digest(&[tag, &item["namespace"], &item["tool"], &item["arguments"]]),
                &item["content_items"],
            ),
            "McpToolCall" => (
                json_digest(&[tag, &item["server"], &item["tool"], &item["arguments"]]),
                &item["result"],
            ),
            _ => return None,
        };
        Some(Self {
            input,
            output: json_digest(output),
        })
    }
}

pub(super) enum Match {
    Missing,
    One(u64),
    Multiple,
}

#[derive(Default)]
pub(super) struct ToolOccurrences(HashMap<String, HashMap<String, BTreeMap<u64, Receipt>>>);

impl ToolOccurrences {
    pub(super) fn find(&self, turn: &str, id: &str, wanted: Option<Receipt>) -> Match {
        let Some((receipt, records)) = wanted.zip(self.0.get(turn).and_then(|items| items.get(id)))
        else {
            return Match::Missing;
        };
        let mut candidates = records
            .iter()
            .filter(|(_, known)| known.input == receipt.input);
        let Some((&first, _)) = candidates.next() else {
            return Match::Missing;
        };
        if candidates.next().is_none() {
            return Match::One(first);
        }
        let mut exact = records.iter().filter(|(_, known)| **known == receipt);
        match (exact.next(), exact.next()) {
            (Some((&seq, _)), None) => Match::One(seq),
            _ => Match::Multiple,
        }
    }

    pub(super) fn has_result(&self, turn: &str, id: &str, seq: u64) -> bool {
        self.0
            .get(turn)
            .and_then(|ids| ids.get(id))
            .is_some_and(|records| records.contains_key(&seq))
    }

    pub(super) fn observe(&mut self, turn: &str, id: &str, seq: u64, receipt: Receipt) {
        self.0
            .entry(turn.to_owned())
            .or_default()
            .entry(id.to_owned())
            .or_default()
            .insert(seq, receipt);
    }

    pub(super) fn bind(&mut self, generated: &str, original: &str) {
        if let Some(items) = self.0.remove(generated) {
            let out = self.0.entry(original.to_owned()).or_default();
            for (id, records) in items {
                out.entry(id).or_default().extend(records);
            }
        }
    }

    pub(super) fn retain(&mut self, mut exists: impl FnMut(u64) -> bool) {
        self.0.retain(|_, items| {
            items.retain(|_, records| {
                records.retain(|&seq, _| exists(seq));
                !records.is_empty()
            });
            !items.is_empty()
        });
    }
}
