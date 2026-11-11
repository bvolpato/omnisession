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

#[derive(Clone, Copy)]
pub(super) enum PendingCall {
    Unique(u64),
    Overlapping(u64),
    Multiple,
}

impl PendingCall {
    pub(super) fn sequence(self) -> Option<u64> {
        match self {
            Self::Unique(seq) | Self::Overlapping(seq) => Some(seq),
            Self::Multiple => None,
        }
    }
}

#[derive(Default)]
pub(super) struct NativeCalls {
    pub(super) result_calls: HashMap<u64, u64>,
    pub(super) source_keys: HashMap<u64, [u8; 32]>,
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
        payload: &serde_json::Value,
    ) {
        let arguments = (payload["name"] == "exec_command")
            .then(|| payload["arguments"].as_str())
            .flatten()
            .and_then(|text| {
                serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(text).ok()
            })
            .map(|mut fields| {
                fields.remove("yield_time_ms");
                serde_json::Value::Object(fields)
            });
        self.source_keys.insert(
            sequence,
            super::paginated_rollback::json_digest(&[
                &payload["type"],
                &payload["name"],
                &payload["namespace"],
                arguments.as_ref().unwrap_or(&payload["arguments"]),
                &payload["input"],
                &payload["action"],
            ]),
        );
        self.pending
            .entry(id.to_owned())
            .or_default()
            .push_back(ScopedRecord {
                sequence,
                turn,
                owner,
            });
    }

    pub(super) fn pending_occurrence(
        &self,
        id: &str,
        turn: &str,
        result: Option<u64>,
        generated: bool,
    ) -> Option<PendingCall> {
        let mut matching = self.pending.get(id)?.iter().filter(|call| {
            call.turn.as_deref() == Some(turn) || (generated && call.turn.is_none())
        });
        let first = matching.next()?.sequence;
        if matching.next().is_some() {
            return Some(PendingCall::Multiple);
        }
        let result = result?;
        if first > result {
            return Some(PendingCall::Unique(first));
        }
        self.result_calls
            .contains_key(&result)
            .then_some(PendingCall::Overlapping(first))
    }

    pub(super) fn confirm_turn(&mut self, id: &str, sequence: u64, turn: &str, owner: Option<u64>) {
        if let Some(call) = self.pending.get_mut(id).and_then(|queue| {
            queue
                .iter_mut()
                .find(|call| call.sequence == sequence && call.turn.is_none())
        }) {
            call.turn = Some(turn.to_owned());
            call.owner = owner;
        }
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
        self.source_keys.retain(|&seq, _| exists(seq));
        self.result_calls
            .retain(|&result, call| exists(result) && exists(*call));
        self.pending.retain(|_, calls| {
            calls.retain(|call| exists(call.sequence));
            !calls.is_empty()
        });
        self.linked
            .retain(|&result, call| exists(result) && exists(*call));
    }
}
