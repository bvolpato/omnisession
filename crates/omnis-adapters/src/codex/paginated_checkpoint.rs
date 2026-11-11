//! Track the typed display items backed by the selected Codex compaction checkpoint.

use std::collections::{HashMap, HashSet};

use omnis_ir::EventKind;
use serde_json::Value;
use sha2::{Digest, Sha256};

mod history;
mod legacy;
mod turns;
use super::model_cut::{acceptance_order, input_order, removes_accepted};
use super::paginated_rollback::model_instruction;
use super::{CodexHistory, EventBuilder, MAX_CANONICAL_EVENTS, PaginatedRollback, Withdrawal};
pub(super) use history::CheckpointHistory;
pub(super) use legacy::{LegacyInput, LegacySource};
pub(super) use turns::{CheckpointSelection, CheckpointTurns};

type DigestKey = [u8; 32];
type CompletedPositions = HashMap<(super::CompletedKind, DigestKey, DigestKey), Option<u64>>;

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
    discarded: bool,
    uncertain_selection: bool,
    linked_calls: HashMap<usize, usize>,
    retained: usize,
    prefix_inputs: usize,
    users: Vec<(usize, Option<u64>)>,
    legacy_users: Vec<(usize, LegacyInput)>,
    ordered_outputs: HashMap<usize, u64>,
    cutoff: Option<u64>,
    positions: HashMap<u64, Vec<Copy>>,
    unverified_on_fallback: HashSet<u64>,
}

impl PaginatedCheckpoint {
    pub(super) fn valid_metadata(payload: &Value) -> bool {
        super::checkpoint_wire::valid(payload, true)
    }

    pub(super) fn read(
        history: &CodexHistory,
        builder: &mut EventBuilder,
        payload: &Value,
    ) -> Option<Self> {
        let items = payload["replacement_history"].as_array()?;
        let start = items.len().saturating_sub(MAX_CANONICAL_EVENTS);
        let metadata = payload["replacement_history_metadata"].as_array();
        let mut checkpoint = Self::new(history.source_ordinal, builder.checkpoint(), items, start);
        let aliases = history.paginated_inputs.resolved_turns();
        let input_copies = history
            .paginated_inputs
            .confirmed_input_copies(&aliases, items);
        let answer_copies =
            history
                .paginated_answers
                .confirmed_answer_copies(&aliases, &items[start..], |item| {
                    history.model_plan_mode(item)
                });
        let original = input_copies.original_display_users();
        let turn_key = |turn: &str| {
            let turn = Sha256::digest(history.aliases.resolve(turn).as_bytes()).into();
            aliases.get(&turn).copied().unwrap_or(turn)
        };
        let completed = completed_positions(history, turn_key);
        // Only retained replacement rows can prove which public display item they contain.
        for (index, item) in items.iter().enumerate().skip(start) {
            let metadata = metadata
                .and_then(|entries| entries.get(index))
                .unwrap_or(&Value::Null);
            let instruction = checkpoint.observe_input_order(index, item, metadata);
            if instruction && item["role"] == "assistant" {
                continue;
            }
            if let Some((seq, spelling)) = answer_copies.get_plan(item) {
                checkpoint.positions.entry(seq).or_default().push(Copy {
                    index,
                    answer: Some(spelling),
                });
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
            if instruction && item["role"] == "user" {
                if history
                    .paginated_inputs
                    .compaction_keeps_display_text(seq, item)
                    && let Some((_, input)) = checkpoint.legacy_users.last_mut()
                {
                    input.source = Some(LegacySource::User(seq));
                }
                if !original.contains(&seq) {
                    checkpoint.unverified_on_fallback.insert(seq);
                }
            }
        }
        for (index, seq, call) in history.paginated_answers.checkpoint_tool_copies(
            &aliases,
            items,
            metadata.map(Vec::as_slice),
            start,
        ) {
            checkpoint.positions.entry(seq).or_default().push(Copy {
                index,
                answer: None,
            });
            if let Some((question, spelling)) = async_display_copy(
                &items[index],
                history.paginated_answers.call_turn(seq, &aliases),
                builder,
                &completed,
            ) {
                checkpoint
                    .positions
                    .entry(question)
                    .or_default()
                    .push(Copy {
                        index,
                        answer: Some(spelling),
                    });
            }
            if let Some(call) = call {
                checkpoint.linked_calls.insert(index, call);
            }
        }
        Some(checkpoint)
    }

    fn new(ordinal: u64, display_end: u64, items: &[Value], start: usize) -> Self {
        Self {
            ordinal,
            display_end,
            discarded: false,
            uncertain_selection: false,
            linked_calls: HashMap::new(),
            retained: items.len(),
            prefix_inputs: items[..start]
                .iter()
                .filter(|item| model_instruction(item))
                .count(),
            users: Vec::new(),
            legacy_users: Vec::new(),
            ordered_outputs: HashMap::new(),
            cutoff: None,
            positions: HashMap::new(),
            unverified_on_fallback: HashSet::new(),
        }
    }

    fn observe_input_order(&mut self, index: usize, item: &Value, metadata: &Value) -> bool {
        let instruction = model_instruction(item);
        if instruction {
            let order = input_order(metadata);
            self.users.push((index, order));
            if item["role"] == "user" {
                self.legacy_users
                    .push((index, LegacyInput::new(item, order, self.ordinal)));
            }
        }
        if let Some(order) = acceptance_order(item, metadata) {
            self.ordered_outputs.insert(index, order);
        }
        instruction
    }

    pub(super) fn rollback(
        &mut self,
        count: usize,
        inputs: &PaginatedRollback,
        selection: CheckpointSelection,
    ) -> EffectiveRollback {
        if count == 0 {
            return EffectiveRollback::from_inputs(count, inputs);
        }
        if selection != CheckpointSelection::Selected(self.ordinal) {
            self.discarded = true;
            self.uncertain_selection = selection == CheckpointSelection::Unverified;
            return EffectiveRollback::from_inputs(count, inputs);
        }
        self.discarded = false;
        let suffix = inputs.count_after(self.ordinal);
        let remaining = count.saturating_sub(suffix);
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

    pub(super) fn display_end(&self) -> u64 {
        self.display_end
    }

    pub(super) fn first_retained_index(&self, seq: u64) -> Option<usize> {
        self.positions
            .get(&seq)?
            .iter()
            .find(|copy| !self.item_withdrawn(copy.index))
            .map(|copy| copy.index)
    }

    pub(super) fn source_ordinal(&self) -> u64 {
        self.ordinal
    }

    pub(super) fn applies_to(&self, seq: u64) -> bool {
        !self.discarded && seq < self.display_end
    }

    pub(super) fn item_withdrawn(&self, index: usize) -> bool {
        let removed = |slot| {
            slot >= self.retained
                || removes_accepted(self.ordered_outputs.get(&slot).copied(), self.cutoff)
        };
        self.discarded
            || removed(index)
            || self
                .linked_calls
                .get(&index)
                .is_some_and(|&call| removed(call))
    }

    pub(super) fn retained_answers(
        &self,
        seq: u64,
    ) -> impl DoubleEndedIterator<Item = (usize, DigestKey)> + '_ {
        self.positions
            .get(&seq)
            .into_iter()
            .flatten()
            .filter_map(|copy| {
                copy.answer
                    .filter(|_| !self.item_withdrawn(copy.index))
                    .map(|text| (copy.index, text))
            })
    }

    pub(super) fn retained_answer(&self, seq: u64) -> Option<DigestKey> {
        self.retained_answers(seq).next_back().map(|(_, text)| text)
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
            if self.uncertain_selection
                || self.unverified_on_fallback.contains(&seq)
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
        for ((kind, id), &seq) in ids {
            if !matches!(
                kind,
                super::CompletedKind::User
                    | super::CompletedKind::Assistant
                    | super::CompletedKind::AsyncAssistant
            ) {
                continue;
            }
            let id: [u8; 32] = Sha256::digest(id.as_bytes()).into();
            completed
                .entry((*kind, turn, id))
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
    let kind = match item["role"].as_str()? {
        "assistant" => super::CompletedKind::Assistant,
        "user" => super::CompletedKind::User,
        _ => return None,
    };
    let seq = completed
        .get(&(kind, turn_key(turn), id))
        .copied()
        .flatten()?;
    let shown = builder.event_at_mut(seq)?;
    let text = matching_display_text(item, shown)?;
    let answer =
        (shown.kind == EventKind::MessageAssistant).then(|| Sha256::digest(text.as_bytes()).into());
    Some((seq, answer))
}

fn async_display_copy(
    call: &Value,
    turn: Option<DigestKey>,
    builder: &mut EventBuilder,
    completed: &CompletedPositions,
) -> Option<(u64, DigestKey)> {
    if call["type"] != "function_call"
        || !matches!(call["namespace"].as_str(), None | Some("functions"))
    {
        return None;
    }
    let name = call["name"].as_str()?;
    if !matches!(
        name,
        "request_user_input_async" | "send_message_to_user_async"
    ) {
        return None;
    }
    let id = Sha256::digest(super::model_call_id(call)?.as_bytes()).into();
    let sequence = completed
        .get(&(super::CompletedKind::AsyncAssistant, turn?, id))
        .copied()
        .flatten()?;
    let text = builder.event_at_mut(sequence)?.payload["text"].as_str()?;
    let args: Value = serde_json::from_str(call["arguments"].as_str()?).ok()?;
    let matches = if name == "send_message_to_user_async" {
        args["message"].as_str()?.trim() == text
    } else {
        matches_questions(&args["questions"], text)?
    };
    matches.then(|| (sequence, Sha256::digest(text.as_bytes()).into()))
}

fn matches_questions(questions: &Value, mut text: &str) -> Option<bool> {
    let questions = questions.as_array()?.as_slice();
    for (index, question) in questions.iter().enumerate() {
        if index != 0 {
            text = text.strip_prefix("\n\n")?;
        }
        let title = question["title"].as_str()?;
        if title.trim().is_empty() {
            return None;
        }
        text = text.strip_prefix(title)?;
        if !question["options"].is_null() {
            let options = question["options"].as_array()?;
            if options.is_empty() {
                return None;
            }
            for option in options {
                let option = option.as_str()?;
                if option.trim().is_empty() {
                    return None;
                }
                text = text.strip_prefix("\n- ")?.strip_prefix(option)?;
            }
        }
    }
    Some(!questions.is_empty() && text.is_empty())
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
