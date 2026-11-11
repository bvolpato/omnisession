//! Rebuild the public part of a message-only compaction from the selected model inputs.

use std::collections::HashMap;

use serde_json::Value;

use super::super::paginated_rollback::contextual_fragment;
use super::{
    CheckpointSelection, EffectiveRollback, MAX_CANONICAL_EVENTS, PaginatedRollback, Withdrawal,
};

const USER_TOKEN_LIMIT: usize = 20_000;

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub(in super::super) enum LegacySource {
    User(u64),
    Summary(u64),
}

#[derive(Clone, Copy)]
pub(in super::super) struct LegacyInput {
    pub(in super::super) ordinal: u64,
    pub(in super::super) tokens: usize,
    pub(in super::super) source: Option<LegacySource>,
    pub(in super::super) order: Option<u64>,
    pub(in super::super) partial: bool,
}

impl LegacyInput {
    pub(super) fn new(item: &Value, order: Option<u64>, ordinal: u64) -> Self {
        Self {
            ordinal,
            tokens: Self::tokens(item),
            source: None,
            order,
            partial: false,
        }
    }

    pub(in super::super) fn tokens(item: &Value) -> usize {
        let Some(parts) = item["content"].as_array() else {
            return 0;
        };
        super::super::message_parts(parts, |index, _| parts[index]["type"] == "input_text")
            .into_iter()
            .map(str::len)
            .sum::<usize>()
            .div_ceil(4)
    }
}

pub(super) struct LegacyWindow {
    pub(super) ordinal: u64,
    pub(super) display_end: u64,
    pub(super) base: CheckpointSelection,
    users: Vec<LegacyInput>,
    display: HashMap<LegacySource, Option<Withdrawal>>,
    partial: Option<LegacySource>,
    summary: bool,
    summary_input: Option<LegacyInput>,
}

impl LegacyWindow {
    pub(super) fn new(
        ordinal: u64,
        display_end: u64,
        base: CheckpointSelection,
        mut users: Vec<LegacyInput>,
        message: &str,
    ) -> Self {
        let (display, partial) = Self::trim(&mut users);
        let summary = if message.is_empty() {
            "(no summary available)"
        } else {
            message
        };
        let summary_input = (!contextual_fragment(summary)).then(|| LegacyInput {
            ordinal,
            tokens: summary.len().div_ceil(4),
            source: Some(LegacySource::Summary(ordinal)),
            order: None,
            partial: false,
        });
        Self {
            ordinal,
            display_end,
            base,
            users,
            display,
            partial,
            summary: true,
            summary_input,
        }
    }

    fn trim(
        users: &mut Vec<LegacyInput>,
    ) -> (
        HashMap<LegacySource, Option<Withdrawal>>,
        Option<LegacySource>,
    ) {
        let mut budget = USER_TOKEN_LIMIT;
        let mut first = users.len();
        for input in users.iter_mut().rev().take(MAX_CANONICAL_EVENTS) {
            if budget == 0 {
                break;
            }
            first -= 1;
            if input.tokens > budget {
                // A partial native message cannot certify the full published spelling.
                input.partial = true;
                input.tokens = budget;
            }
            budget -= input.tokens;
        }
        users.drain(..first);
        let partial = users
            .iter()
            .find(|input| input.partial)
            .and_then(|input| input.source);
        let display = users
            .iter()
            .filter_map(|input| input.source.map(|seq| (seq, None)))
            .collect();
        (display, partial)
    }

    pub(super) fn rebase(&mut self, base: CheckpointSelection, mut users: Vec<LegacyInput>) {
        let floor = match base {
            CheckpointSelection::Original => 0,
            CheckpointSelection::Selected(ordinal) => ordinal,
            CheckpointSelection::Unverified => u64::MAX,
        };
        // The raw ledger has no entry for a summary produced by an earlier legacy compaction.
        users.extend(self.users.iter().copied().filter(
            |input| matches!(input.source, Some(LegacySource::Summary(ordinal)) if ordinal > floor),
        ));
        users.sort_by_key(|input| input.ordinal);
        let (mut display, partial) = Self::trim(&mut users);
        for (&seq, &status) in &self.display {
            if status.is_some()
                && let Some(current) = display.get_mut(&seq)
            {
                *current = status;
            }
        }
        self.base = base;
        self.users = users;
        self.display = display;
        self.partial = partial;
    }

    pub(super) fn next_inputs(&self) -> Vec<LegacyInput> {
        self.users
            .iter()
            .copied()
            .chain(self.summary.then_some(self.summary_input).flatten())
            .collect()
    }

    pub(super) fn summary_active(&self, ordinal: u64) -> bool {
        let source = LegacySource::Summary(ordinal);
        (ordinal == self.ordinal && self.summary)
            || (self.partial != Some(source) && self.display.get(&source) == Some(&None))
    }

    pub(super) fn status(&self, seq: u64, baseline: Option<Withdrawal>) -> Option<Withdrawal> {
        let source = LegacySource::User(seq);
        let result = self
            .display
            .get(&source)
            .copied()
            .unwrap_or_else(|| baseline.or(Some(Withdrawal::OutsideCheckpoint)));
        if self.partial == Some(source) && result.is_none() {
            Some(Withdrawal::OutsideCheckpoint)
        } else {
            result
        }
    }

    pub(super) fn rollback(
        &mut self,
        count: usize,
        inputs: &PaginatedRollback,
    ) -> EffectiveRollback {
        let suffix = inputs.count_after(self.ordinal);
        let mut effect = EffectiveRollback::from_inputs(count.min(suffix), inputs);
        let mut remaining = count.saturating_sub(suffix);
        if remaining == 0 {
            return effect;
        }
        if self.summary && (self.summary_input.is_some() || !self.users.is_empty()) {
            remaining -= usize::from(self.summary_input.is_some());
            self.summary = false;
            effect.changed = true;
            effect.boundary = Some((self.ordinal, None));
        }
        if remaining == 0 || self.users.is_empty() {
            return effect;
        }
        let cut = self.users.len().saturating_sub(remaining);
        effect.boundary = Some((self.ordinal, self.users[cut].order));
        effect.changed = true;
        for seq in self.users.drain(cut..).filter_map(|input| input.source) {
            self.display.insert(seq, Some(Withdrawal::Confirmed));
        }
        for seq in self.users.iter().filter_map(|input| input.source) {
            if self.display.get(&seq) == Some(&Some(Withdrawal::Confirmed)) {
                self.display.insert(seq, Some(Withdrawal::Unverified));
            }
        }
        effect
    }
}
