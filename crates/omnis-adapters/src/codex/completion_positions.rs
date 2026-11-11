//! Keep published turn slots separate from the physical model results they can mirror.

use super::tool_history::PendingCall;
use super::tool_occurrences::{Match, Receipt};
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

#[derive(Clone, Copy)]
pub(super) struct Projection {
    pub(super) scope: CompletionScope,
    pub(super) original: Option<u64>,
    pub(super) peer: Option<u64>,
    receipt: Option<Receipt>,
    pending: Option<u64>,
    deferred_output: Option<[u8; 32]>,
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
    fn call_identity_for_result(&self, result: u64) -> Option<&[u8; 32]> {
        self.calls
            .result_calls
            .get(&result)
            .and_then(|seq| self.calls.source_keys.get(seq))
    }

    fn match_display(
        &self,
        builder: &mut EventBuilder,
        (turn, id): (&str, &str),
        item: &Value,
        receipt: Option<Receipt>,
        candidate: Option<PendingCall>,
        last_result: Option<u64>,
    ) -> Match {
        let matches = super::completed_item::model_output_matcher(item);
        let mut output_matches = |seq| {
            builder
                .event_at_mut(seq)
                .is_some_and(|event| matches(&event.payload))
        };
        let known = self
            .occurrences
            .find(turn, id, receipt, &mut output_matches);
        let Some(candidate) = candidate else {
            return known;
        };
        let Some(next) = candidate.sequence() else {
            return Match::Multiple;
        };
        match known {
            Match::One(old) => {
                let earlier = self.call_identity_for_result(old);
                if earlier.is_some() && earlier != self.calls.source_keys.get(&next) {
                    return known;
                }
                if matches!(candidate, PendingCall::Unique(_))
                    && item["type"] == "CommandExecution"
                    && let Some(expected) =
                        super::completed_item::model_body(&item["aggregated_output"])
                    && !output_matches(old)
                {
                    return Match::AwaitOutput(expected);
                }
                Match::Multiple
            }
            Match::Missing
                if matches!(candidate, PendingCall::Overlapping(_))
                    || (receipt.is_some() && last_result.is_some_and(output_matches)) =>
            {
                Match::Multiple
            }
            other => other,
        }
    }

    fn reject_deferred_output(
        &mut self,
        builder: &mut EventBuilder,
        turn: &str,
        id: &str,
        scope: CompletionScope,
        output: &Value,
    ) -> bool {
        let Some(slot) = self
            .completed
            .get_mut(turn)
            .and_then(|slots| slots.get_mut(&(scope, id.to_owned())))
        else {
            return false;
        };
        let Some(expected) = slot.deferred_output.take() else {
            return false;
        };
        if super::completed_item::model_body(output) == Some(expected) {
            return false;
        }
        slot.ambiguous = true;
        if let Some(event) = builder.event_at_mut(slot.sequence)
            && let Some(fields) = event.payload.as_object_mut()
            && let Some(display_id) = fields.remove("id")
        {
            fields.insert("item_id".into(), display_id);
        }
        self.items
            .outcomes
            .take(id, false, Some(slot.sequence), Some(turn), None);
        true
    }

    pub(super) fn completion_position(
        &mut self,
        builder: &mut EventBuilder,
        turn: &str,
        id: &str,
        kind: CompletedKind,
        payload: &Value,
    ) -> Projection {
        let display = payload["type"] == "item_completed";
        let receipt = Receipt::from_item(&payload["item"]);
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
        let candidate = self
            .calls
            .pending_occurrence(id, turn, last_result, generated);
        let pending = candidate.and_then(PendingCall::sequence);
        let known = self.match_display(
            builder,
            (turn, id),
            &payload["item"],
            receipt,
            candidate,
            last_result,
        );
        let prior = match known {
            Match::One(seq) => Some(seq),
            _ => None,
        };
        let rejected = !display
            && previous.is_some_and(|p| p.deferred_output.is_some())
            && self.reject_deferred_output(
                builder,
                turn,
                id,
                if self.paginated {
                    CompletionScope::Published
                } else {
                    historical
                },
                &payload["output"],
            );
        let ambiguous = rejected
            || (display && kind == CompletedKind::Tool && matches!(known, Match::Multiple));
        let wait_for_call = if display {
            ambiguous || (pending.is_some() && kind == CompletedKind::Tool && prior.is_none())
        } else {
            rejected || previous.is_some_and(|p| p.ambiguous)
        };
        let mut projection = Projection {
            scope: historical,
            original: None,
            peer: None,
            detach: None,
            deferred_output: if let Match::AwaitOutput(expected) = known {
                Some(expected)
            } else {
                None
            },
            wait_for_call,
            pending,
            receipt,
            ambiguous,
        };
        if !self.paginated {
            projection.original = prior.or(history).filter(|_| display && !wait_for_call);
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
            projection.peer = prior
                .or(last_result)
                .filter(|_| kind == CompletedKind::Tool && !wait_for_call);
            projection.detach = old_model.filter(|_| {
                !wait_for_call
                    && old_model == last_result
                    && (prior.is_none() || prior == old_model)
            });
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
                if let Some(receipt) = position.receipt {
                    self.occurrences
                        .observe(self.aliases.resolve(&turn), id, model, Some(receipt));
                }
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
            if let Some(receipt) = position.receipt {
                self.occurrences
                    .observe(self.aliases.resolve(&turn), id, model, Some(receipt));
            }
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
                        receipt: None,
                        deferred_output: None,
                        ambiguous: false,
                        sequence: model,
                        model: None,
                    },
                );
        }
    }

    pub(super) fn remember_completion(
        &mut self,
        builder: &mut EventBuilder,
        (turn, id): (&str, &str),
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
        let key = (projected.scope, id.to_owned());
        let matched_slot = peer.and_then(|s| {
            positions
                .get(if self.paginated { &published } else { &key })
                .filter(|p| p.sequence == s)
        });
        let receipt = projected.receipt.or_else(|| matched_slot?.receipt);
        let physical = builder
            .event_at_mut(sequence)
            .is_some_and(|e| model_output(e));
        if let Some(model) = if physical { Some(sequence) } else { peer } {
            self.occurrences
                .observe(self.aliases.resolve(turn), id, model, receipt);
        }
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
        if !self.paginated
            && projected.receipt.is_some()
            && physical
            && positions.get(&key).is_some_and(|p| p.sequence > sequence)
        {
            return;
        }
        positions.insert(
            key,
            super::CompletedPosition {
                kind,
                receipt,
                ambiguous: projected.ambiguous,
                deferred_output: projected.deferred_output,
                sequence,
                model: peer,
            },
        );
    }
}
