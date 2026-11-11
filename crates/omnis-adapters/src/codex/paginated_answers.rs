//! Match paginated assistant items to the model records that Codex rolls back.

use std::collections::{BTreeMap, HashMap, VecDeque};

use serde_json::Value;

use super::MAX_CANONICAL_EVENTS;
use super::message_mirrors::{MessageIdentity, ScopedMessageId};

struct ModelAnswer {
    key: ScopedMessageId,
    ordinal: u64,
    order: Option<u64>,
    removed: bool,
}

#[derive(Default)]
pub(super) struct PaginatedAnswers {
    model: VecDeque<ModelAnswer>,
    displayed: BTreeMap<u64, ScopedMessageId>,
    after_rollback: HashMap<ScopedMessageId, bool>,
}

impl PaginatedAnswers {
    pub(super) fn segment(&mut self) {
        self.model.clear();
        self.displayed.clear();
        self.after_rollback.clear();
    }

    pub(super) fn model(&mut self, record: &Value, ordinal: u64) {
        let payload = &record["payload"];
        let identity = MessageIdentity::new(
            payload["internal_chat_message_metadata_passthrough"]["turn_id"].as_str(),
            payload["id"].as_str(),
            false,
        );
        let Some(key) = identity.scoped(true) else {
            return;
        };
        if self.model.len() == MAX_CANONICAL_EVENTS {
            self.model.pop_front();
        }
        let metadata = &record["metadata"];
        let order = metadata["user_input_order"]
            .as_u64()
            .filter(|_| metadata["inherited_user_message"] != true);
        self.model.push_back(ModelAnswer {
            key,
            ordinal,
            order,
            removed: false,
        });
        self.after_rollback.remove(&key);
    }

    /// A completed item can arrive after Codex has already rolled back its model copy.
    pub(super) fn display(&mut self, sequence: u64, turn: &str, id: &str) -> Option<bool> {
        let key = MessageIdentity::new(Some(turn), Some(id), true).scoped(true)?;
        self.displayed.insert(sequence, key);
        if self.displayed.len() > MAX_CANONICAL_EVENTS {
            self.displayed.pop_first();
        }
        self.after_rollback.get(&key).copied()
    }

    pub(super) fn rollback(
        &mut self,
        boundary_ordinal: u64,
        order: Option<u64>,
    ) -> HashMap<u64, bool> {
        self.after_rollback.clear();
        for answer in &mut self.model {
            // Codex can accept a reply after a queued instruction and write it before that instruction.
            answer.removed |= answer.ordinal >= boundary_ordinal
                || order
                    .zip(answer.order)
                    .is_some_and(|(boundary, accepted)| accepted >= boundary);
            // If Codex recorded this ID more than once, one surviving copy keeps it in the model window.
            *self.after_rollback.entry(answer.key).or_insert(true) &= answer.removed;
        }
        self.displayed
            .iter()
            .filter_map(|(&sequence, key)| Some((sequence, *self.after_rollback.get(key)?)))
            .collect()
    }

    pub(super) fn discard_before(&mut self, first_sequence: u64) {
        while self
            .displayed
            .first_key_value()
            .is_some_and(|(&seq, _)| seq < first_sequence)
        {
            self.displayed.pop_first();
        }
    }
}
