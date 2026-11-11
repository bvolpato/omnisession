//! Retain public evidence for the bounded set of checkpoints that can still be selected.

use std::collections::{HashSet, VecDeque};

use super::{
    CheckpointSelection, EffectiveRollback, MAX_CANONICAL_EVENTS, PaginatedCheckpoint,
    PaginatedRollback, Withdrawal,
};

#[derive(Default)]
pub(in super::super) struct CheckpointHistory {
    windows: VecDeque<(usize, PaginatedCheckpoint)>,
    weight: usize,
    active: Option<usize>,
    uncertain_before: u64,
    fallback: HashSet<u64>,
}

impl CheckpointHistory {
    pub(in super::super) fn is_empty(&self) -> bool {
        self.active.is_none() && self.uncertain_before == 0 && self.fallback.is_empty()
    }

    pub(in super::super) fn record(
        &mut self,
        checkpoint: Option<PaginatedCheckpoint>,
        selection: CheckpointSelection,
    ) {
        self.active = None;
        self.fallback.clear();
        self.uncertain_before = 0;
        let Some(window) = checkpoint else {
            // This legacy compaction has no explicit replacement. An older one can still survive a later rollback.
            return;
        };
        let weight = 1
            + window.users.len()
            + window.ordered_outputs.len()
            + window.linked_calls.len()
            + window.unverified_on_fallback.len()
            + window.positions.values().map(Vec::len).sum::<usize>();
        self.weight += weight;
        while self.weight > MAX_CANONICAL_EVENTS
            && let Some((evicted, _)) = self.windows.pop_front()
        {
            self.weight -= evicted;
        }
        self.windows.push_back((weight, window));
        self.select(selection);
    }

    pub(in super::super) fn as_ref(&self) -> Option<&PaginatedCheckpoint> {
        self.windows
            .get(self.active?)
            .map(|(_, checkpoint)| checkpoint)
    }

    pub(in super::super) fn status(
        &self,
        seq: u64,
        owner: Option<u64>,
        baseline: Option<Withdrawal>,
    ) -> Option<Withdrawal> {
        if let Some(window) = self.as_ref()
            && window.applies_to(seq)
        {
            return window.status(seq, owner, baseline);
        }
        if seq < self.uncertain_before
            || self.fallback.contains(&seq)
            || owner.is_some_and(|owner| self.fallback.contains(&owner))
        {
            Some(Withdrawal::Unverified)
        } else {
            baseline
        }
    }

    pub(in super::super) fn rollback(
        &mut self,
        count: usize,
        inputs: &PaginatedRollback,
        selection: CheckpointSelection,
    ) -> EffectiveRollback {
        if count == 0 {
            return EffectiveRollback::from_inputs(0, inputs);
        }
        self.select(selection);
        self.active
            .and_then(|index| self.windows.get_mut(index))
            .map_or_else(
                || EffectiveRollback::from_inputs(count, inputs),
                |(_, window)| window.rollback(count, inputs, selection),
            )
    }
    fn select(&mut self, selection: CheckpointSelection) {
        self.active = match selection {
            CheckpointSelection::Selected(ordinal) => {
                self.windows.iter().rposition(|(_, w)| w.ordinal == ordinal)
            }
            CheckpointSelection::Original | CheckpointSelection::Unverified => None,
        };
        self.fallback.clear();
        for (_, window) in self
            .windows
            .iter_mut()
            .skip(self.active.map_or(0, |index| index + 1))
        {
            window.discarded = true;
            self.fallback.extend(&window.unverified_on_fallback);
        }
        self.uncertain_before =
            if self.active.is_none() && selection != CheckpointSelection::Original {
                self.windows.back().map_or(0, |(_, w)| w.display_end)
            } else {
                0
            };
        if let Some(index) = self.active {
            self.windows[index].1.discarded = false;
        }
    }
}
