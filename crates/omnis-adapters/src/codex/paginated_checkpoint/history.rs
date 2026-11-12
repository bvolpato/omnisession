//! Retain public evidence for the bounded set of checkpoints that can still be selected.

use std::collections::{HashSet, VecDeque};

use super::legacy::LegacyWindow;
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
    base: CheckpointSelection,
    legacy: Option<LegacyWindow>,
}

impl CheckpointHistory {
    pub(in super::super) fn is_empty(&self) -> bool {
        self.active.is_none()
            && self.uncertain_before == 0
            && self.fallback.is_empty()
            && self.legacy.is_none()
    }

    pub(in super::super) fn record(
        &mut self,
        checkpoint: Option<PaginatedCheckpoint>,
        selection: CheckpointSelection,
    ) {
        let Some(window) = checkpoint else {
            self.select(selection);
            return;
        };
        let weight = 1
            + window.users.len()
            + window.legacy_users.len()
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

    pub(in super::super) fn record_legacy(
        &mut self,
        ordinal: u64,
        display_end: u64,
        message: &str,
        inputs: &PaginatedRollback,
        selection: CheckpointSelection,
    ) {
        self.select(selection);
        let (after, mut users) = if let Some(previous) = self
            .active_legacy()
            .filter(|window| window.base == self.base)
        {
            (previous.ordinal, previous.next_inputs())
        } else {
            self.base_users()
        };
        users.extend(inputs.legacy_inputs(after, ordinal));
        self.legacy = Some(LegacyWindow::new(
            ordinal,
            display_end,
            self.base,
            users,
            message,
        ));
    }

    fn base_users(&self) -> (u64, Vec<super::LegacyInput>) {
        self.as_ref().map_or_else(
            || {
                let start = if self.base == CheckpointSelection::Original {
                    0
                } else {
                    u64::MAX
                };
                (start, Vec::new())
            },
            |base| {
                let users = base
                    .legacy_users
                    .iter()
                    .filter_map(|(index, input)| (!base.item_withdrawn(*index)).then_some(*input))
                    .collect();
                (base.ordinal, users)
            },
        )
    }

    pub(in super::super) fn selected_base(&self) -> CheckpointSelection {
        self.base
    }

    pub(in super::super) fn saw_legacy(&self) -> bool {
        self.legacy.is_some()
    }

    fn active_legacy(&self) -> Option<&LegacyWindow> {
        self.legacy
            .as_ref()
            .filter(|window| match self.selected_base() {
                CheckpointSelection::Selected(base) => base < window.ordinal,
                CheckpointSelection::Original => true,
                CheckpointSelection::Unverified => self
                    .windows
                    .back()
                    .is_none_or(|(_, last)| last.ordinal < window.ordinal),
            })
    }

    pub(in super::super) fn legacy_summary_active(&self, ordinal: u64) -> bool {
        self.active_legacy()
            .is_some_and(|window| window.summary_active(ordinal))
    }

    pub(in super::super) fn legacy_ordinal(&self) -> Option<u64> {
        self.active_legacy().map(|window| window.ordinal)
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
        if let Some(legacy) = self
            .active_legacy()
            .filter(|legacy| seq < legacy.display_end)
        {
            return legacy.status(seq, baseline);
        }
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
        if let Some(legacy) = self.active_legacy() {
            if legacy.base != self.base {
                let ordinal = legacy.ordinal;
                let (after, mut users) = self.base_users();
                users.extend(inputs.legacy_inputs(after, ordinal));
                if let Some(legacy) = &mut self.legacy {
                    legacy.rebase(self.base, users);
                }
            }
            if let Some(legacy) = &mut self.legacy {
                return legacy.rollback(count, inputs);
            }
        }
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
        self.base = if self.active.is_none() && selection != CheckpointSelection::Original {
            CheckpointSelection::Unverified
        } else {
            selection
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
