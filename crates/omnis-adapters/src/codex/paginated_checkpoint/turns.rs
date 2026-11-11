//! Select a Codex replay checkpoint from native turn boundaries and rollback markers.

use std::collections::VecDeque;

use serde_json::Value;
use sha2::{Digest, Sha256};

use super::super::paginated_rollback::communication;
use super::{MAX_CANONICAL_EVENTS, PaginatedCheckpoint, model_instruction};

type Turn = [u8; 32];

#[derive(Clone, Copy)]
enum Marker {
    Start(Turn),
    Scope(Turn),
    Input,
    Checkpoint { numbered: bool, replacement: bool },
    Rollback(usize),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(in super::super) enum CheckpointSelection {
    Selected(u64),
    Original,
    Unverified,
}

#[derive(Default)]
pub(in super::super) struct CheckpointTurns {
    markers: VecDeque<(u64, Marker)>,
    preceding_start: Option<Turn>,
    newest_checkpoint: Option<(u64, bool)>,
    evicted: bool,
}

#[derive(Default)]
struct Segment {
    turn: Option<Turn>,
    has_input: bool,
    checkpoint: Option<u64>,
}

impl Segment {
    fn finish(&self, pending: &mut usize) -> Option<u64> {
        if *pending == 0 {
            self.checkpoint
        } else {
            *pending -= usize::from(self.has_input);
            None
        }
    }
}

impl CheckpointTurns {
    pub(in super::super) fn observe(&mut self, record: &Value, ordinal: u64) {
        let payload = &record["payload"];
        let turn = || {
            payload["turn_id"]
                .as_str()
                .map(|id| Sha256::digest(id.as_bytes()).into())
        };
        let marker = match record["type"].as_str() {
            Some("compacted") if PaginatedCheckpoint::valid_metadata(payload) => {
                Marker::Checkpoint {
                    numbered: payload["window_number"].is_u64() || payload["window_id"].is_u64(),
                    replacement: payload["replacement_history"].is_array(),
                }
            }
            Some("inter_agent_communication") if communication(payload) => Marker::Input,
            Some("response_item") if model_instruction(payload) => Marker::Input,
            Some("turn_context") => {
                let Some(id) = turn() else { return };
                Marker::Scope(id)
            }
            Some("event_msg") => match payload["type"].as_str() {
                Some("task_started" | "turn_started") => {
                    let Some(id) = turn() else { return };
                    self.preceding_start = Some(id);
                    Marker::Start(id)
                }
                Some("task_complete" | "turn_complete" | "turn_aborted") => {
                    let Some(id) = turn() else { return };
                    // A stop hook can finish an older turn after a newer turn has started.
                    if self.preceding_start.is_some_and(|current| current != id) {
                        return;
                    }
                    Marker::Scope(id)
                }
                Some("user_message") => Marker::Input,
                Some("thread_rolled_back") => {
                    let count = payload["num_turns"].as_u64().unwrap_or(0);
                    if count == 0 {
                        return;
                    }
                    Marker::Rollback(usize::try_from(count).unwrap_or(usize::MAX))
                }
                _ => return,
            },
            _ => return,
        };
        if let Marker::Checkpoint {
            numbered,
            replacement,
        } = marker
        {
            self.newest_checkpoint = Some((ordinal, numbered && replacement));
        }
        if matches!(
            (self.markers.back(), marker),
            (Some((_, Marker::Input)), Marker::Input)
        ) {
            return;
        }
        if self.markers.len() == MAX_CANONICAL_EVENTS {
            self.markers.pop_front();
            self.evicted = true;
        }
        self.markers.push_back((ordinal, marker));
    }

    pub(in super::super) fn selected(&self) -> CheckpointSelection {
        match self.newest_checkpoint {
            None => return CheckpointSelection::Original,
            Some((ordinal, true)) => return CheckpointSelection::Selected(ordinal),
            Some((_, false)) => {}
        }
        let mut pending = 0usize;
        let mut segment = Segment::default();
        for &(ordinal, marker) in self.markers.iter().rev() {
            match marker {
                Marker::Checkpoint { replacement, .. } => {
                    if replacement {
                        segment.checkpoint.get_or_insert(ordinal);
                    }
                }
                Marker::Start(id) if segment.turn.is_none_or(|turn| turn == id) => {
                    if let Some(ordinal) = segment.finish(&mut pending) {
                        return CheckpointSelection::Selected(ordinal);
                    }
                    segment = Segment::default();
                }
                Marker::Scope(id) => {
                    segment.turn.get_or_insert(id);
                }
                Marker::Input => segment.has_input = true,
                Marker::Rollback(count) => pending = pending.saturating_add(count),
                Marker::Start(_) => {}
            }
        }
        if self.evicted {
            CheckpointSelection::Unverified
        } else {
            segment
                .finish(&mut pending)
                .map_or(CheckpointSelection::Original, CheckpointSelection::Selected)
        }
    }
}
