//! Match tool calls, model results, and display results by their native identifiers.

use std::collections::{HashMap, VecDeque};

#[derive(Default)]
pub(super) struct PendingOutcomes {
    model: HashMap<String, VecDeque<ScopedRecord>>,
    display: HashMap<String, VecDeque<ScopedRecord>>,
}

impl PendingOutcomes {
    pub(super) fn take(
        &mut self,
        id: &str,
        display: bool,
        known: Option<u64>,
        turn: Option<&str>,
        owner: Option<u64>,
    ) -> Option<ScopedRecord> {
        let opposite = if display {
            &mut self.model
        } else {
            &mut self.display
        };
        let queue = opposite.get_mut(id)?;
        let index = match known {
            Some(seq) => queue.iter().position(|entry| entry.sequence == seq),
            None => queue.iter().position(|entry| {
                if id.is_empty() {
                    return turn.is_some() && turn == entry.turn.as_deref();
                }
                if !display
                    && turn.is_none()
                    && owner
                        .zip(entry.owner)
                        .is_some_and(|(left, right)| left != right)
                {
                    return false;
                }
                turn.zip(entry.turn.as_deref()).is_none_or(|(a, b)| {
                    a == b
                        || (super::is_implicit_turn(a) != super::is_implicit_turn(b)
                            && owner
                                .zip(entry.owner)
                                .is_none_or(|(left, right)| left == right))
                })
            }),
        };
        let matched = queue.remove(index?)?;
        if queue.is_empty() {
            opposite.remove(id);
        }
        Some(matched)
    }

    pub(super) fn push(
        &mut self,
        id: &str,
        display: bool,
        sequence: u64,
        turn: Option<String>,
        owner: Option<u64>,
    ) {
        let side = if display {
            &mut self.display
        } else {
            &mut self.model
        };
        side.entry(id.to_owned())
            .or_default()
            .push_back(ScopedRecord {
                sequence,
                turn,
                owner,
            });
    }

    pub(super) fn retain_models(&mut self) {
        self.display.clear();
    }

    pub(super) fn retain(&mut self, mut exists: impl FnMut(u64) -> bool) {
        for side in [&mut self.model, &mut self.display] {
            side.retain(|_, queue| {
                queue.retain(|entry| exists(entry.sequence));
                !queue.is_empty()
            });
        }
    }
}

pub(super) struct ScopedRecord {
    pub(super) sequence: u64,
    pub(super) turn: Option<String>,
    pub(super) owner: Option<u64>,
}

#[derive(Default)]
pub(super) struct NativeCalls {
    pending: HashMap<String, VecDeque<ScopedRecord>>,
    // A later display result can identify the turn of both unstamped model records.
    pub(super) linked: HashMap<u64, u64>,
}

impl NativeCalls {
    pub(super) fn push(
        &mut self,
        id: &str,
        turn: Option<String>,
        sequence: u64,
        owner: Option<u64>,
    ) {
        self.pending
            .entry(id.to_owned())
            .or_default()
            .push_back(ScopedRecord {
                sequence,
                turn,
                owner,
            });
    }

    pub(super) fn take(
        &mut self,
        id: &str,
        stamped: Option<&str>,
        active: Option<&str>,
    ) -> Option<ScopedRecord> {
        let queue = self.pending.get_mut(id)?;
        let index = if id.is_empty() {
            let turn = stamped.or(active)?;
            let mut matching = queue
                .iter()
                .enumerate()
                .filter(|(_, call)| call.turn.as_deref() == Some(turn));
            let (index, _) = matching.next()?;
            if matching.next().is_some() {
                return None;
            }
            index
        } else if let Some(turn) = stamped {
            queue
                .iter()
                .position(|call| call.turn.as_deref() == Some(turn))
                .or_else(|| queue.iter().position(|call| call.turn.is_none()))?
        } else {
            // A result can arrive during a later turn. Prefer the active call if the ID was reused.
            active
                .and_then(|turn| {
                    queue
                        .iter()
                        .position(|call| call.turn.as_deref() == Some(turn))
                })
                .unwrap_or(0)
        };
        let call = queue.remove(index);
        if queue.is_empty() {
            self.pending.remove(id);
        }
        call
    }

    pub(super) fn retain(&mut self, mut exists: impl FnMut(u64) -> bool) {
        self.pending.retain(|_, calls| {
            calls.retain(|call| exists(call.sequence));
            !calls.is_empty()
        });
        self.linked
            .retain(|&result, call| exists(result) && exists(*call));
    }
}
