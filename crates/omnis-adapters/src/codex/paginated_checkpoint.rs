//! Track the typed display items backed by the selected Codex compaction checkpoint.

use std::collections::{HashMap, HashSet};

use omnis_ir::EventKind;
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::paginated_rollback::model_instruction;
use super::{CodexHistory, EventBuilder, MAX_CANONICAL_EVENTS, PaginatedRollback, Withdrawal};

type DigestKey = [u8; 32];
type CompletedPositions = HashMap<(DigestKey, DigestKey), Option<u64>>;

pub(super) struct EffectiveRollback {
    pub(super) input_count: usize,
    pub(super) boundary: Option<(u64, Option<u64>)>,
    pub(super) changed: bool,
}

impl EffectiveRollback {
    pub(super) fn from_inputs(count: usize, inputs: &PaginatedRollback) -> Self {
        Self {
            input_count: count,
            boundary: inputs.boundary(count),
            changed: count > 0 && inputs.has_inputs(),
        }
    }
}

struct Copy {
    index: usize,
    answer: Option<DigestKey>,
}

pub(super) struct PaginatedCheckpoint {
    ordinal: u64,
    display_end: u64,
    numbered: bool,
    discarded: bool,
    retained: usize,
    prefix_inputs: usize,
    users: Vec<(usize, Option<u64>)>,
    ordered_outputs: HashMap<usize, u64>,
    cutoff: Option<u64>,
    positions: HashMap<u64, Vec<Copy>>,
    unverified_on_fallback: HashSet<u64>,
}

impl PaginatedCheckpoint {
    pub(super) fn valid_metadata(payload: &Value) -> bool {
        let metadata = &payload["replacement_history_metadata"];
        metadata.is_null()
            || metadata.as_array().is_some_and(|metadata| {
                payload["replacement_history"]
                    .as_array()
                    .is_some_and(|items| items.len() == metadata.len())
                    && metadata.iter().all(|entry| {
                        entry.is_object()
                            && (entry["user_input_order"].is_null()
                                || entry["user_input_order"].is_u64())
                            && entry
                                .get("inherited_user_message")
                                .is_none_or(Value::is_boolean)
                    })
            })
    }

    pub(super) fn read(
        history: &CodexHistory,
        builder: &mut EventBuilder,
        payload: &Value,
    ) -> Option<Self> {
        let items = payload["replacement_history"].as_array()?;
        let start = items.len().saturating_sub(MAX_CANONICAL_EVENTS);
        let metadata = payload["replacement_history_metadata"].as_array();
        let mut checkpoint = Self {
            ordinal: history.source_ordinal,
            display_end: builder.checkpoint(),
            numbered: payload["window_number"].is_u64() || payload["window_id"].is_u64(),
            discarded: false,
            retained: items.len(),
            prefix_inputs: items[..start]
                .iter()
                .filter(|item| model_instruction(item))
                .count(),
            users: Vec::new(),
            ordered_outputs: HashMap::new(),
            cutoff: None,
            positions: HashMap::new(),
            unverified_on_fallback: HashSet::new(),
        };
        let aliases = history.paginated_inputs.resolved_turns();
        let input_copies = history
            .paginated_inputs
            .confirmed_input_copies(&aliases, items);
        let answer_copies = history.paginated_answers.confirmed_answer_copies(&aliases);
        let original = input_copies.original_display_users();
        let turn_key = |turn: &str| {
            let turn = Sha256::digest(history.aliases.resolve(turn).as_bytes()).into();
            aliases.get(&turn).copied().unwrap_or(turn)
        };
        let completed = completed_positions(history, turn_key);
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
        // Only retained replacement rows can prove which public display item they contain.
        for (index, item) in items.iter().enumerate().skip(start) {
            let order = metadata.and_then(|entries| {
                let entry = &entries[index];
                (entry["inherited_user_message"] != true)
                    .then(|| entry["user_input_order"].as_u64())
                    .flatten()
            });
            let instruction = model_instruction(item);
            if instruction {
                checkpoint.users.push((index, order));
            }
            if let Some(order) = order
                && (item["type"] == "function_call"
                    || (item["type"] == "message" && item["role"] == "assistant"))
            {
                checkpoint.ordered_outputs.insert(index, order);
            }
            let witnessed = match item["role"].as_str() {
                Some("user") => input_copies.get(item).map(|seq| (seq, None)),
                Some("assistant") => answer_copies
                    .get(item)
                    .map(|(seq, spelling)| (seq, Some(spelling))),
                _ => None,
            };
            let Some((seq, answer)) = witnessed.or_else(|| {
                if item["role"] == "user" && input_copies.has_witness(item) {
                    // The source recorded this input with different content or attachments.
                    return None;
                }
                let (seq, answer) = exact_display_copy(item, builder, &completed, turn_key)?;
                (item["role"] != "user" || history.paginated_inputs.display_is_plain_text(seq))
                    .then_some((seq, answer))
            }) else {
                continue;
            };
            checkpoint
                .positions
                .entry(seq)
                .or_default()
                .push(Copy { index, answer });
            if instruction && item["role"] == "user" && !original.contains(&seq) {
                checkpoint.unverified_on_fallback.insert(seq);
            }
        }
        Some(checkpoint)
    }

    pub(super) fn rollback(
        &mut self,
        count: usize,
        inputs: &PaginatedRollback,
    ) -> EffectiveRollback {
        if self.discarded || count == 0 {
            return EffectiveRollback::from_inputs(count, inputs);
        }
        let suffix = inputs.count_after(self.ordinal);
        let remaining = count.saturating_sub(suffix);
        if remaining > 0 && !self.numbered {
            // Reverse replay skips an unnumbered checkpoint in the removed turn.
            self.discarded = true;
            return EffectiveRollback::from_inputs(count, inputs);
        }
        let mut effect = EffectiveRollback::from_inputs(count.min(suffix), inputs);
        if remaining > 0 && (!self.users.is_empty() || self.prefix_inputs > 0) {
            let keep = self.users.len().saturating_sub(remaining);
            let extra = remaining.saturating_sub(self.users.len());
            let (cut, order) = if extra > 0 && self.prefix_inputs > 0 {
                self.prefix_inputs = self.prefix_inputs.saturating_sub(extra);
                // The removed input precedes every retained checkpoint row.
                (0, None)
            } else {
                self.users[keep]
            };
            self.retained = cut;
            self.users.truncate(keep);
            effect.boundary = Some((self.ordinal, order));
            effect.changed = true;
        }
        if let Some((_, Some(order))) = effect.boundary {
            self.cutoff = Some(self.cutoff.map_or(order, |old| old.min(order)));
            // An assistant inter-agent instruction can also precede a queued input in record order.
            self.users.retain(|(index, _)| {
                self.ordered_outputs
                    .get(index)
                    .is_none_or(|accepted| *accepted < order)
            });
        }
        effect
    }

    pub(super) fn source_ordinal(&self) -> u64 {
        self.ordinal
    }

    pub(super) fn applies_to(&self, seq: u64) -> bool {
        !self.discarded && seq < self.display_end
    }

    pub(super) fn item_withdrawn(&self, index: usize) -> bool {
        self.discarded
            || index >= self.retained
            || self
                .cutoff
                .zip(self.ordered_outputs.get(&index))
                .is_some_and(|(cut, accepted)| *accepted >= cut)
    }

    pub(super) fn retained_answer(&self, seq: u64) -> Option<DigestKey> {
        self.positions.get(&seq)?.iter().rev().find_map(|copy| {
            (!self.item_withdrawn(copy.index))
                .then_some(copy.answer)
                .flatten()
        })
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
        let Some(copies) = self.positions.get(&seq) else {
            return Some(Withdrawal::OutsideCheckpoint);
        };
        let removed = copies.iter().any(|copy| self.item_withdrawn(copy.index));
        if !removed {
            return None;
        }
        let kept = copies.iter().any(|copy| !self.item_withdrawn(copy.index));
        if kept && self.retained_answer(seq).is_some() {
            return None;
        }
        Some(if kept {
            Withdrawal::Unverified
        } else {
            Withdrawal::Confirmed
        })
    }
}

fn completed_positions(
    history: &CodexHistory,
    turn_key: impl Fn(&str) -> DigestKey,
) -> CompletedPositions {
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
    completed
}

fn exact_display_copy(
    item: &Value,
    builder: &mut EventBuilder,
    completed: &CompletedPositions,
    turn_key: impl Fn(&str) -> DigestKey,
) -> Option<(u64, Option<DigestKey>)> {
    let turn = item["internal_chat_message_metadata_passthrough"]["turn_id"].as_str()?;
    let id: DigestKey = Sha256::digest(item["id"].as_str()?.as_bytes()).into();
    let seq = completed.get(&(turn_key(turn), id)).copied().flatten()?;
    let shown = builder.event_at_mut(seq)?;
    let text = matching_display_text(item, shown)?;
    let answer =
        (shown.kind == EventKind::MessageAssistant).then(|| Sha256::digest(text.as_bytes()).into());
    Some((seq, answer))
}

fn matching_display_text<'a>(item: &Value, shown: &'a omnis_ir::OmniEvent) -> Option<&'a str> {
    if item["type"] != "message"
        || shown.source.raw_record_type.as_deref() != Some("event_msg.item_completed")
    {
        return None;
    }
    let tag = match (&shown.kind, item["role"].as_str()) {
        (EventKind::MessageUser, Some("user")) => "input_text",
        (EventKind::MessageAssistant, Some("assistant")) => "output_text",
        _ => return None,
    };
    let text = shown.payload["text"].as_str()?;
    let mut rest = text;
    for part in item["content"].as_array()? {
        if part["type"] != tag {
            return None;
        }
        rest = rest.strip_prefix(part["text"].as_str()?)?;
    }
    rest.is_empty().then_some(text)
}
