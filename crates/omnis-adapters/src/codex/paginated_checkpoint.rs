//! Track the typed display items backed by the selected Codex compaction checkpoint.

use std::collections::{HashMap, HashSet};

use omnis_ir::EventKind;
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::paginated_rollback::model_instruction;
use super::{CodexHistory, EventBuilder, MAX_CANONICAL_EVENTS, Withdrawal};

pub(super) struct PaginatedCheckpoint {
    ordinal: u64,
    display_end: u64,
    numbered: bool,
    discarded: bool,
    retained: usize,
    users: Vec<usize>,
    positions: HashMap<u64, (usize, usize)>,
    unverified_on_fallback: HashSet<u64>,
}

impl PaginatedCheckpoint {
    pub(super) fn read(
        history: &CodexHistory,
        builder: &mut EventBuilder,
        payload: &Value,
    ) -> Option<Self> {
        let items = payload["replacement_history"].as_array()?;
        let mut checkpoint = Self {
            ordinal: history.source_ordinal,
            display_end: builder.checkpoint(),
            numbered: payload["window_number"].as_u64().is_some(),
            discarded: false,
            retained: items.len(),
            users: Vec::new(),
            positions: HashMap::new(),
            unverified_on_fallback: HashSet::new(),
        };
        let aliases = history.paginated_inputs.resolved_turns();
        let original = history.paginated_inputs.original_display_users(&aliases);
        let turn_key = |turn: &str| {
            let turn = Sha256::digest(history.aliases.resolve(turn).as_bytes()).into();
            aliases.get(&turn).copied().unwrap_or(turn)
        };
        let mut completed = HashMap::new();
        for (turn, ids) in &history.completed {
            let turn = turn_key(turn);
            for (id, &seq) in ids {
                let id: [u8; 32] = Sha256::digest(id.as_bytes()).into();
                completed
                    .entry((turn, id))
                    .and_modify(|prior| {
                        if *prior != Some(seq) {
                            *prior = None;
                        }
                    })
                    .or_insert(Some(seq));
            }
        }
        if !checkpoint.numbered
            && let Some(prior) = &history.paginated_checkpoint
        {
            // Without a full reverse turn scan, an earlier replacement may still own these displayed inputs.
            checkpoint.unverified_on_fallback.extend(
                original
                    .iter()
                    .copied()
                    .filter(|seq| *seq < prior.display_end),
            );
            checkpoint
                .unverified_on_fallback
                .extend(prior.positions.keys().copied());
            checkpoint
                .unverified_on_fallback
                .extend(prior.unverified_on_fallback.iter().copied());
            checkpoint
                .unverified_on_fallback
                .retain(|seq| *seq >= builder.first_sequence());
        }
        // If a checkpoint exceeds the retention budget, only its observed suffix can establish ownership.
        let start = items.len().saturating_sub(MAX_CANONICAL_EVENTS);
        for (index, item) in items.iter().enumerate().skip(start) {
            let instruction = model_instruction(item);
            if instruction {
                checkpoint.users.push(index);
            }
            let Some((turn, id)) = item["internal_chat_message_metadata_passthrough"]["turn_id"]
                .as_str()
                .zip(item["id"].as_str())
            else {
                continue;
            };
            let id: [u8; 32] = Sha256::digest(id.as_bytes()).into();
            let Some(&Some(seq)) = completed.get(&(turn_key(turn), id)) else {
                continue;
            };
            let Some(shown) = builder.event_at_mut(seq) else {
                continue;
            };
            if matches_display(item, shown) {
                checkpoint
                    .positions
                    .entry(seq)
                    .and_modify(|(_, last)| *last = index)
                    .or_insert((index, index));
                if shown.kind == EventKind::MessageUser && instruction && !original.contains(&seq) {
                    checkpoint.unverified_on_fallback.insert(seq);
                }
            }
        }
        Some(checkpoint)
    }

    pub(super) fn rollback(&mut self, count: usize, inputs: &super::PaginatedRollback) {
        let remaining = count.saturating_sub(inputs.count_after(self.ordinal));
        if remaining == 0 {
            return;
        }
        if self.numbered {
            let keep = self.users.len().saturating_sub(remaining);
            if let Some(&cut) = self.users.get(keep) {
                self.retained = cut;
                self.users.truncate(keep);
            }
        } else {
            // Reverse replay skips an unnumbered checkpoint in the revoked turn and uses the original model stream.
            self.discarded = true;
        }
    }

    pub(super) fn status(
        &self,
        seq: u64,
        owner: Option<u64>,
        baseline: Option<Withdrawal>,
    ) -> Option<Withdrawal> {
        if seq >= self.display_end {
            return baseline;
        }
        if self.discarded {
            if self.unverified_on_fallback.contains(&seq)
                || owner.is_some_and(|seq| self.unverified_on_fallback.contains(&seq))
            {
                return Some(Withdrawal::Unverified);
            }
            return baseline;
        }
        match self.positions.get(&seq) {
            Some(&(first, _)) if first >= self.retained => Some(Withdrawal::Confirmed),
            Some(&(_, last)) if last >= self.retained => Some(Withdrawal::Unverified),
            Some(_) => None,
            None => Some(Withdrawal::OutsideCheckpoint),
        }
    }
}

fn matches_display(item: &Value, shown: &omnis_ir::OmniEvent) -> bool {
    if item["type"] != "message"
        || shown.source.raw_record_type.as_deref() != Some("event_msg.item_completed")
    {
        return false;
    }
    let tag = match (&shown.kind, item["role"].as_str()) {
        (EventKind::MessageUser, Some("user")) => "input_text",
        (EventKind::MessageAssistant, Some("assistant")) => "output_text",
        _ => return false,
    };
    let Some(mut text) = shown.payload["text"].as_str() else {
        return false;
    };
    let Some(parts) = item["content"].as_array() else {
        return false;
    };
    for part in parts {
        if part["type"] != tag {
            return false;
        }
        let Some(rest) = part["text"]
            .as_str()
            .and_then(|part| text.strip_prefix(part))
        else {
            return false;
        };
        text = rest;
    }
    text.is_empty()
}
