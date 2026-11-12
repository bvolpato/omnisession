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

pub(super) fn unattribute_display(payload: &mut Value) {
    if let Some(fields) = payload.as_object_mut()
        && let Some(id) = fields.remove("id")
    {
        fields.insert("item_id".into(), id);
    }
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
                unattribute_display(&mut display_payload);
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
    pub(super) display_identity: Option<super::tool_history::DisplayCallIdentity>,
    pub(super) reconsider: Option<u64>,
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

fn completed_display_match(
    builder: &mut EventBuilder,
    item: &Value,
    known: Match,
    last_result: Option<u64>,
) -> Match {
    let chosen = match known {
        Match::One(seq) => Some(seq),
        Match::Missing => last_result,
        Match::DifferentCommand => return Match::Multiple,
        _ => return known,
    };
    if chosen
        .and_then(|seq| builder.event_at_mut(seq))
        .is_some_and(|event| super::completed_item::shell_output_conflicts(item, &event.payload))
    {
        Match::Multiple
    } else {
        known
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
        if item["type"] == "FunctionCallOutput" {
            return self.match_function_output(builder, (turn, id), item, candidate);
        }
        let (body, matches) = super::completed_item::model_output_matcher(item);
        let mut output_matches = |seq| {
            builder
                .event_at_mut(seq)
                .is_some_and(|event| matches(&event.payload))
        };
        let identity = super::tool_history::DisplayCallIdentity::from_item(item);
        let call_matches = |seq: &u64| self.calls.display_matches_call(identity?, *seq);
        let input_agrees = |seq| self.calls.result_calls.get(&seq).and_then(call_matches);
        let known = self
            .occurrences
            .find(turn, id, receipt, input_agrees, &mut output_matches);
        if let Match::One(old) = known
            && self
                .calls
                .result_calls
                .get(&old)
                .is_some_and(|call| self.calls.overlapping.contains(call))
            && !output_matches(old)
        {
            return Match::Multiple;
        }
        let Some(candidate) = candidate else {
            return completed_display_match(builder, item, known, last_result);
        };
        let Some(next) = candidate.sequence() else {
            return Match::Multiple;
        };
        let next_matches = call_matches(&next);
        if matches!(candidate, PendingCall::First(_)) {
            return if next_matches == Some(false) {
                Match::Multiple
            } else {
                known
            };
        }
        if matches!(candidate, PendingCall::Unique(_))
            && next_matches == Some(true)
            && item["type"] == "DynamicToolCall"
            && let Some(expected) = body
            && self
                .occurrences
                .results(turn, id)
                .all(|seq| input_agrees(seq) == Some(false) || !output_matches(seq))
        {
            return Match::AwaitOutput(expected);
        }
        if matches!(candidate, PendingCall::Overlapping(_))
            && matches!(known, Match::DifferentCommand)
        {
            return Match::Multiple;
        }
        if matches!(known, Match::DifferentCommand) {
            return if next_matches == Some(true) {
                Match::Missing
            } else {
                Match::Multiple
            };
        }
        match known {
            Match::One(old) => {
                if input_agrees(old) == Some(true) && next_matches == Some(false) {
                    return known;
                }
                let earlier = self.call_identity_for_result(old);
                if !matches!(
                    identity,
                    Some(super::tool_history::DisplayCallIdentity::Dynamic(_))
                ) && !self.calls.commands.contains_key(&next)
                    && earlier.is_some()
                    && earlier != self.calls.source_keys.get(&next)
                {
                    return known;
                }
                if matches!(candidate, PendingCall::Unique(_))
                    && item["type"] == "CommandExecution"
                    && let Some(expected) = body
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

    pub(super) fn reject_pending_display(
        &mut self,
        builder: &mut EventBuilder,
        turn: Option<&str>,
        id: &str,
        call: &super::tool_history::ScopedRecord,
        output: &Value,
    ) -> bool {
        let scope = if self.paginated {
            CompletionScope::Published
        } else {
            CompletionScope::Historical(CompletedKind::Tool)
        };
        let key = (scope, id.to_owned());
        let turn = turn
            .filter(|turn| {
                self.completed
                    .get(self.aliases.resolve(turn))
                    .is_some_and(|slots| slots.contains_key(&key))
            })
            .or_else(|| {
                self.items
                    .outcomes
                    .pending_display_turn(id, turn, call.owner)
            });
        let Some(slot) = turn
            .and_then(|turn| self.completed.get_mut(self.aliases.resolve(turn)))
            .and_then(|slots| slots.get_mut(&key))
        else {
            return false;
        };
        let Some(event) = builder.event_at_mut(slot.sequence) else {
            return false;
        };
        if slot.ambiguous
            || slot.model.is_some()
            || event.source.raw_record_type.as_deref() != Some("event_msg.item_completed")
            || !matches!(
                event.payload["type"].as_str(),
                Some("CommandExecution" | "DynamicToolCall")
            )
        {
            return false;
        }
        let compatible = slot
            .display_identity
            .and_then(|identity| self.calls.display_matches_call(identity, call.sequence));
        let shell_agrees = event.payload["type"] != "CommandExecution"
            || super::completed_item::pending_shell_agrees(&event.payload, output) != Some(false);
        if shell_agrees
            && compatible.unwrap_or_else(|| {
                event.payload["type"] == "CommandExecution"
                    && super::completed_item::pending_command_output_matches(&event.payload, output)
            })
        {
            return false;
        }
        slot.ambiguous = true;
        unattribute_display(&mut event.payload);
        self.items
            .outcomes
            .take(id, false, Some(slot.sequence), None, None);
        self.ambiguous_receipts += 1;
        true
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
        if builder.event_at_mut(slot.sequence).is_some_and(|event| {
            event.payload["type"] == "CommandExecution"
                && super::completed_item::pending_shell_agrees(&event.payload, output) == Some(true)
        }) || super::completed_item::model_body(output) == Some(expected)
        {
            return false;
        }
        slot.ambiguous = true;
        if let Some(event) = builder.event_at_mut(slot.sequence) {
            unattribute_display(&mut event.payload);
        }
        self.items
            .outcomes
            .take(id, false, Some(slot.sequence), Some(turn), None);
        true
    }

    fn match_function_output(
        &self,
        builder: &mut EventBuilder,
        (turn, id): (&str, &str),
        item: &Value,
        candidate: Option<PendingCall>,
    ) -> Match {
        let (_, matches_output) = super::completed_item::model_output_matcher(item);
        let matches_name = |event: &OmniEvent| {
            event.payload["name"] == item["name"]
                && event.payload["namespace"].as_str() == item["namespace"].as_str()
        };
        let mut best = 0;
        let mut selected = Match::Missing;
        let mut count = 0;
        for result in self.occurrences.results(turn, id) {
            let Some(&call) = self.calls.result_calls.get(&result) else {
                continue;
            };
            count += 1;
            let name = builder
                .event_at_mut(call)
                .is_some_and(|event| matches_name(event));
            let body = builder
                .event_at_mut(result)
                .is_some_and(|event| matches_output(&event.payload));
            let score = u8::from(name) + u8::from(body);
            if score > best {
                best = score;
                selected = Match::One(result);
            } else if score == best && count > 1 {
                selected = Match::Multiple;
            }
        }
        if let Some(pending) = candidate {
            let Some(call) = pending.sequence() else {
                return Match::Multiple;
            };
            let pending_name = builder
                .event_at_mut(call)
                .is_some_and(|event| matches_name(event));
            return match (best, pending_name) {
                (2, true) | (1, false) => Match::Multiple,
                (2, false) => selected,
                _ => Match::Missing,
            };
        }
        selected
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
        let item = &payload["item"];
        let receipt = Receipt::from_item(item);
        let historical = CompletionScope::Historical(kind);
        let positions = self.completed.get(turn);
        let get = |scope| positions?.get(&(scope, id.to_owned())).copied();
        let history = get(historical).map(|position| position.sequence);
        let published = get(CompletionScope::Published);
        let legacy_output = get(CompletionScope::LegacyOutput).filter(|_| !self.paginated);
        let previous_scope = if self.paginated {
            CompletionScope::Published
        } else {
            historical
        };
        let previous = get(previous_scope);
        let old_model = published.and_then(|p| p.model);
        let last_result = history.or(old_model);
        let generated = super::is_implicit_turn(turn) && self.current_turn_id() == Some(turn);
        let candidate = self
            .calls
            .pending_occurrence(id, turn, last_result, generated);
        let pending = candidate.and_then(PendingCall::after_result);
        let known = self.match_display(builder, (turn, id), item, receipt, candidate, last_result);
        let prior = match known {
            Match::One(seq) => Some(seq),
            _ => None,
        };
        let rejected = !display
            && previous.is_some_and(|p| p.deferred_output.is_some())
            && self.reject_deferred_output(builder, turn, id, previous_scope, &payload["output"]);
        let ambiguous = rejected
            || (display && kind == CompletedKind::Tool && matches!(known, Match::Multiple));
        let wait_for_call = if display {
            ambiguous || (pending.is_some() && kind == CompletedKind::Tool && prior.is_none())
        } else {
            rejected || previous.is_some_and(|p| p.ambiguous)
        };
        let mut projection = Projection {
            reconsider: pending.filter(|_| {
                ambiguous
                    && matches!(
                        item["type"].as_str(),
                        Some("FunctionCallOutput" | "DynamicToolCall")
                    )
            }),
            display_identity: super::tool_history::DisplayCallIdentity::from_item(item),
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
            return self.legacy_projection(
                builder,
                projection,
                payload,
                history,
                legacy_output,
                prior,
            );
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

    fn legacy_projection(
        &self,
        builder: &mut EventBuilder,
        mut projection: Projection,
        payload: &Value,
        history: Option<u64>,
        legacy_output: Option<super::CompletedPosition>,
        prior: Option<u64>,
    ) -> Projection {
        let display = payload["type"] == "item_completed";
        let item = &payload["item"];
        let wait_for_call = projection.wait_for_call;
        if !display && legacy_output.is_some_and(|slot| Some(slot.sequence) == history) {
            projection.wait_for_call = true;
            projection.pending = None;
            return projection;
        }
        if let Some(slot) = legacy_output.filter(|_| item["type"] == "FunctionCallOutput") {
            projection.scope = CompletionScope::LegacyOutput;
            projection.original = Some(slot.sequence);
            if let Some(result) = prior {
                projection.pending = self.calls.result_calls.get(&result).copied();
            }
            return projection;
        }
        projection.original = prior.or(history).filter(|_| display && !wait_for_call);
        projection.peer = history.filter(|seq| {
            !display
                && !wait_for_call
                && builder.event_at_mut(*seq).is_some_and(|e| {
                    e.source.raw_record_type.as_deref() == Some("event_msg.item_completed")
                        && super::completed_item::model_receipt(&e.payload)
                })
        });
        projection
    }

    pub(super) fn detach_tool_display(
        &mut self,
        builder: &mut EventBuilder,
        projection: Projection,
        actual_peer: Option<u64>,
    ) {
        self.ambiguous_receipts +=
            usize::from(projection.ambiguous && projection.reconsider.is_none());
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
                        display_identity: None,
                        pending_output_call: None,
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

    pub(super) fn finish_legacy_outputs(&self, builder: &mut EventBuilder) {
        for (turn, items) in &self.completed {
            for ((scope, id), position) in items {
                if *scope != CompletionScope::LegacyOutput || position.ambiguous {
                    continue;
                }
                let pending = position
                    .pending_output_call
                    .filter(|&call| builder.event_at_mut(call).is_some());
                let model = match pending {
                    Some(call) => self
                        .occurrences
                        .results(turn, id)
                        .find(|seq| self.calls.result_calls.get(seq) == Some(&call)),
                    None => position.model,
                };
                if let Some(model) = model {
                    pair(builder, position.sequence, model);
                }
            }
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
        if projected.scope == CompletionScope::LegacyOutput {
            if let Some(slot) = positions.get_mut(&key) {
                slot.ambiguous = projected.ambiguous;
                slot.pending_output_call = projected.pending;
            }
            return;
        }
        if !self.paginated && peer.is_none() && !projected.wait_for_call {
            let distinct = if physical {
                projected.peer.map(|display| (display, sequence))
            } else {
                projected
                    .original
                    .filter(|seq| builder.event_at_mut(*seq).is_some_and(|e| model_output(e)))
                    .map(|model| (sequence, model))
            };
            if let Some((display, model)) = distinct
                && builder
                    .event_at_mut(display)
                    .is_some_and(|e| e.payload["type"] == "FunctionCallOutput")
                && let Some(mut position) = positions.get(&key).copied()
            {
                position.sequence = display;
                position.model = Some(model);
                positions.insert((CompletionScope::LegacyOutput, id.to_owned()), position);
            }
        }
        positions.insert(
            key,
            super::CompletedPosition {
                display_identity: projected.display_identity,
                pending_output_call: None,
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
