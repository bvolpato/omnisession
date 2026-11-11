//! Keep published turn slots separate from the physical model results they can mirror.

use super::paginated_rollback::json_digest;
use super::{CodexHistory, CompletedKind, CompletionScope, EventBuilder, model_result_kind};
use omnis_ir::{EventKind, OmniEvent, ReplayPolicy};
use serde_json::{Value, json};

pub(super) fn model_output(event: &OmniEvent) -> bool {
    event
        .source
        .raw_record_type
        .as_deref()
        .is_some_and(|raw| raw.starts_with("response_item.") && raw.ends_with("_output"))
}

pub(super) fn restore_model_output(event: &mut OmniEvent) {
    if let Some(fields) = event.payload.as_object_mut() {
        fields.remove("omnisession_codex_tool_display");
    }
    event.kind = model_result_kind(&event.payload);
}

pub(super) fn pair(builder: &mut EventBuilder, display: u64, model: u64) -> bool {
    let Some(slot) = builder.event_at_mut(display) else {
        return false;
    };
    if slot.source.raw_record_type.as_deref() != Some("event_msg.item_completed")
        || !super::completed_item::model_receipt(&slot.payload)
    {
        return false;
    }
    let failed = slot.kind == EventKind::ToolFailed;
    let mut display_payload = std::mem::take(&mut slot.payload);
    let paired = builder
        .event_at_mut(model)
        .filter(|event| model_output(event))
        .is_some_and(|result| {
            if !super::completed_item::equivalent_model_receipt(&display_payload, &result.payload) {
                // A distinct published item keeps its native ID without claiming the recorded call.
                if let Some(fields) = display_payload.as_object_mut()
                    && let Some(id) = fields.remove("id")
                {
                    fields.insert("item_id".into(), id);
                }
                return false;
            }
            super::completed_item::annotate_model_receipt(&mut result.payload, &display_payload);
            result.kind = if failed {
                EventKind::ToolFailed
            } else {
                model_result_kind(&result.payload)
            };
            true
        });
    let slot = builder
        .event_at_mut(display)
        .expect("retained display slot");
    if paired {
        slot.kind = EventKind::ProviderEvent;
        slot.payload = json!({});
        slot.replay_policy = ReplayPolicy::HistoricalOnly;
        slot.source.raw_record_type = Some("omnisession.codex_tool_display_mirror".into());
    } else {
        slot.payload = display_payload;
    }
    paired
}

fn command_key(item: &Value) -> Option<[u8; 32]> {
    if item["type"] != "CommandExecution" {
        return None;
    }
    Some(json_digest(&[
        &item["command"],
        &item["cwd"],
        &item["aggregated_output"],
    ]))
}

fn call_key(builder: &mut EventBuilder, sequence: u64) -> Option<[u8; 32]> {
    let call = &builder.event_at_mut(sequence)?.payload;
    Some(json_digest(&[
        &call["type"],
        &call["name"],
        &call["arguments"],
        &call["input"],
    ]))
}

#[derive(Clone, Copy)]
pub(super) struct Projection {
    pub(super) scope: CompletionScope,
    pub(super) original: Option<u64>,
    pub(super) peer: Option<u64>,
    command: Option<[u8; 32]>,
    pending: Option<u64>,
    pub(super) wait_for_call: bool,
    pub(super) ambiguous: bool,
    detach: Option<u64>,
}

impl Projection {
    pub(super) fn tool_projection(self, paginated: bool) -> super::ToolProjection {
        super::ToolProjection {
            paginated,
            peer: self.peer,
            wait_for_call: self.wait_for_call,
            ambiguous: self.ambiguous,
            matched_turn: None,
        }
    }
}

impl CodexHistory {
    pub(super) fn completion_position(
        &mut self,
        builder: &mut EventBuilder,
        turn: &str,
        id: &str,
        kind: CompletedKind,
        payload: &Value,
    ) -> Projection {
        let display = payload["type"] == "item_completed";
        let command = command_key(&payload["item"]);
        let historical = CompletionScope::Historical(kind);
        let positions = self.completed.get(turn);
        let get = |scope| positions?.get(&(scope, id.to_owned())).copied();
        let history = get(historical).map(|position| position.sequence);
        let published = get(CompletionScope::Published);
        let previous = if self.paginated {
            published
        } else {
            get(historical)
        };
        let old_model = published.and_then(|p| p.model);
        let last_result = history.or(old_model);
        let generated = super::is_implicit_turn(turn) && self.current_turn_id() == Some(turn);
        let candidate =
            last_result.and_then(|result| self.calls.pending_after(id, turn, result, generated));
        let multiple = matches!(candidate, Some(super::tool_history::PendingCall::Multiple));
        let pending = match candidate {
            Some(super::tool_history::PendingCall::Unique(seq)) => Some(seq),
            _ => None,
        };
        let matches_previous = command.is_some() && previous.is_some_and(|p| p.command == command);
        let unconfirmed_output = command.is_some()
            && previous.is_none_or(|p| p.command.is_none())
            && pending.is_some()
            && last_result
                .and_then(|seq| builder.event_at_mut(seq))
                .is_some_and(|event| {
                    payload["item"]["aggregated_output"].is_string()
                        && event.payload["output"] == payload["item"]["aggregated_output"]
                });
        let ambiguous = display
            && kind == CompletedKind::Tool
            && (multiple
                || unconfirmed_output
                || (matches_previous
                    && pending.is_some_and(|next| {
                        let past = last_result
                            .and_then(|r| self.calls.result_calls.get(&r))
                            .and_then(|s| call_key(builder, *s));
                        past.is_none() || past == call_key(builder, next)
                    })));
        let wait_for_call = if display {
            ambiguous || (pending.is_some() && kind == CompletedKind::Tool && !matches_previous)
        } else {
            previous.is_some_and(|p| p.ambiguous)
        };
        let mut projection = Projection {
            scope: historical,
            original: None,
            peer: None,
            detach: None,
            wait_for_call,
            pending,
            command,
            ambiguous,
        };
        if !self.paginated {
            projection.original = history.filter(|_| display && !wait_for_call);
            projection.peer = history.filter(|seq| {
                !display
                    && !wait_for_call
                    && builder.event_at_mut(*seq).is_some_and(|e| {
                        e.source.raw_record_type.as_deref() == Some("event_msg.item_completed")
                            && super::completed_item::model_receipt(&e.payload)
                    })
            });
        } else if display {
            projection.scope = CompletionScope::Published;
            projection.original = published.map(|p| p.sequence);
            projection.peer = last_result.filter(|_| kind == CompletedKind::Tool && !wait_for_call);
            projection.detach = old_model.filter(|_| !wait_for_call && old_model == last_result);
        } else {
            projection.peer = published
                .filter(|slot| {
                    !wait_for_call && slot.kind == CompletedKind::Tool && slot.model.is_none()
                })
                .map(|slot| slot.sequence);
        }
        projection
    }

    pub(super) fn detach_tool_display(
        &mut self,
        builder: &mut EventBuilder,
        projection: Projection,
        actual_peer: Option<u64>,
    ) {
        self.ambiguous_receipts += usize::from(projection.ambiguous);
        if let Some(old) = projection.detach.filter(|old| Some(*old) != actual_peer) {
            if let Some(event) = builder.event_at_mut(old).filter(|e| model_output(e)) {
                restore_model_output(event);
            }
            self.paginated_answers.forget_tool_display(old);
        }
    }

    pub(super) fn finish_unscoped_result(&mut self, id: &str) {
        let (Some(slot), Some(turn), Some(model)) = (
            self.items.tool_projection.peer.take(),
            self.items.tool_projection.matched_turn.take(),
            self.items.projected,
        ) else {
            return;
        };
        if !self.paginated {
            self.display_revisions.move_to_result(slot, model);
            if let Some(position) = self
                .completed
                .get_mut(self.aliases.resolve(&turn))
                .and_then(|items| {
                    items.get_mut(&(
                        CompletionScope::Historical(CompletedKind::Tool),
                        id.to_owned(),
                    ))
                })
                .filter(|position| position.sequence == slot)
            {
                position.sequence = model;
            }
            return;
        }
        if let Some(position) = self
            .completed
            .get_mut(self.aliases.resolve(&turn))
            .and_then(|items| items.get_mut(&(CompletionScope::Published, id.to_owned())))
            .filter(|position| position.sequence == slot)
        {
            position.model = Some(model);
            self.paginated_answers.move_tool_display(slot, model);
            self.completed
                .entry(self.aliases.resolve(&turn).to_owned())
                .or_default()
                .insert(
                    (
                        CompletionScope::Historical(CompletedKind::Tool),
                        id.to_owned(),
                    ),
                    super::CompletedPosition {
                        kind: CompletedKind::Tool,
                        command: None,
                        ambiguous: false,
                        sequence: model,
                        model: None,
                    },
                );
        }
    }

    pub(super) fn remember_completion(
        &mut self,
        turn: &str,
        id: &str,
        kind: CompletedKind,
        projected: Projection,
        sequence: u64,
        peer: Option<u64>,
    ) {
        if projected.wait_for_call
            && !projected.ambiguous
            && let Some(call) = projected.pending
        {
            let turn = self.aliases.resolve(turn);
            self.calls
                .confirm_turn(id, call, turn, self.named_turns.get(turn).copied());
        }
        if !self.paginated
            && let Some(display) = peer
        {
            self.display_revisions.move_to_result(display, sequence);
        }
        let positions = self
            .completed
            .entry(self.aliases.resolve(turn).to_owned())
            .or_default();
        let published = (CompletionScope::Published, id.to_owned());
        if self.paginated
            && projected.scope != CompletionScope::Published
            && let Some(mirror) = peer
            && let Some(slot) = positions
                .get_mut(&published)
                .filter(|slot| slot.sequence == mirror)
        {
            slot.model = Some(sequence);
            self.paginated_answers.move_tool_display(mirror, sequence);
        }
        let key = (projected.scope, id.to_owned());
        let command = projected.command.or_else(|| {
            peer.and_then(|p| {
                positions
                    .get(&key)
                    .filter(|prior| prior.sequence == p)?
                    .command
            })
        });
        positions.insert(
            key,
            super::CompletedPosition {
                kind,
                command,
                ambiguous: projected.ambiguous,
                sequence,
                model: peer,
            },
        );
    }
}
