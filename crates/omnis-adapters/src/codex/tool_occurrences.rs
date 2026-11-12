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
    DifferentCommand,
    AwaitOutput([u8; 32]),
    Missing,
    One(u64),
    Multiple,
}

#[derive(Default)]
pub(super) struct ToolOccurrences(HashMap<String, HashMap<String, BTreeMap<u64, Option<Receipt>>>>);

impl ToolOccurrences {
    pub(super) fn find(
        &self,
        turn: &str,
        id: &str,
        wanted: Option<Receipt>,
        input_agrees: impl Fn(u64) -> Option<bool>,
        mut output_matches: impl FnMut(u64) -> bool,
    ) -> Match {
        let Some((receipt, records)) = wanted.zip(self.0.get(turn).and_then(|items| items.get(id)))
        else {
            return Match::Missing;
        };
        // A first display can arrive after several results. Compare its full output against
        // every physical candidate, including results that have no prior display.
        if !records.is_empty() && records.keys().all(|seq| input_agrees(*seq) == Some(false)) {
            return Match::DifferentCommand;
        }
        let compatible = || {
            records.iter().filter(|(seq, prior)| {
                prior.is_none_or(|p| p.input == receipt.input) && input_agrees(**seq) != Some(false)
            })
        };
        let mut exact = compatible().filter(|(seq, _)| output_matches(**seq));
        match (exact.next(), exact.next()) {
            (Some((&seq, _)), None) => return Match::One(seq),
            (Some(_), Some(_)) => return Match::Multiple,
            _ => {}
        }
        let mut candidates = compatible();
        match (candidates.next(), candidates.next()) {
            (Some((&seq, prior)), None) if prior.is_some() || records.len() > 1 => Match::One(seq),
            (Some(_), Some(_)) => {
                let mut previous = records.iter().filter(|(_, p)| **p == Some(receipt));
                match (previous.next(), previous.next()) {
                    (Some((&seq, _)), None) if records.values().all(Option::is_some) => {
                        Match::One(seq)
                    }
                    _ => Match::Multiple,
                }
            }
            _ if records.len() > 1 => Match::Multiple,
            _ => Match::Missing,
        }
    }

    pub(super) fn results(&self, turn: &str, id: &str) -> impl DoubleEndedIterator<Item = u64> {
        self.0
            .get(turn)
            .and_then(|items| items.get(id))
            .into_iter()
            .flat_map(|records| records.keys().copied())
    }

    pub(super) fn observe(&mut self, turn: &str, id: &str, seq: u64, receipt: Option<Receipt>) {
        let record = self
            .0
            .entry(turn.to_owned())
            .or_default()
            .entry(id.to_owned())
            .or_default()
            .entry(seq)
            .or_default();
        if receipt.is_some() {
            *record = receipt;
        }
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
