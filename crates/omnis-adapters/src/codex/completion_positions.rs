//! Keep published turn slots separate from the physical model results they can mirror.

use super::{CodexHistory, CompletedKind, CompletionScope, EventBuilder, model_result_kind};
use omnis_ir::{EventKind, OmniEvent, ReplayPolicy};
use serde_json::json;

pub(super) fn model_output(event: &OmniEvent) -> bool {
    event
        .source
        .raw_record_type
        .as_deref()
        .is_some_and(|raw| raw.starts_with("response_item.") && raw.ends_with("_output"))
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
    let Some(result) = builder.event_at_mut(model).filter(|e| model_output(e)) else {
        return false;
    };
    result.kind = if failed {
        EventKind::ToolFailed
    } else {
        model_result_kind(&result.payload)
    };
    let slot = builder
        .event_at_mut(display)
        .expect("retained display slot");
    slot.kind = EventKind::ProviderEvent;
    slot.payload = json!({});
    slot.replay_policy = ReplayPolicy::HistoricalOnly;
    slot.source.raw_record_type = Some("omnisession.codex_tool_display_mirror".into());
    true
}

#[derive(Clone, Copy)]
pub(super) struct Projection {
    pub(super) scope: CompletionScope,
    pub(super) original: Option<u64>,
    pub(super) peer: Option<u64>,
    detach: Option<u64>,
}

impl CodexHistory {
    pub(super) fn completion_position(
        &mut self,
        builder: &mut EventBuilder,
        turn: &str,
        id: &str,
        kind: CompletedKind,
        display: bool,
    ) -> Projection {
        let historical = CompletionScope::Historical(kind);
        let positions = self.completed.get(turn);
        let get = |scope| positions?.get(&(scope, id.to_owned())).copied();
        let history = get(historical).map(|position| position.sequence);
        if !self.paginated {
            let original = history.filter(|_| display);
            let peer = history.filter(|seq| {
                !display
                    && builder.event_at_mut(*seq).is_some_and(|e| {
                        e.source.raw_record_type.as_deref() == Some("event_msg.item_completed")
                            && super::completed_item::model_receipt(&e.payload)
                    })
            });
            return Projection {
                scope: historical,
                detach: None,
                original,
                peer,
            };
        }
        let published = get(CompletionScope::Published);
        if !display {
            let peer = published
                .filter(|slot| slot.kind == CompletedKind::Tool && slot.model.is_none())
                .map(|slot| slot.sequence);
            return Projection {
                scope: historical,
                detach: None,
                original: None,
                peer,
            };
        }
        let original = published.map(|p| p.sequence);
        let old_model = published.and_then(|p| p.model);
        let last_result = history.or(old_model);
        let new_call = last_result.is_some_and(|result| self.calls.pending_after(id, turn, result));
        let peer = last_result.filter(|_| kind == CompletedKind::Tool && !new_call);
        Projection {
            scope: CompletionScope::Published,
            original,
            peer,
            detach: old_model
                .filter(|_| !new_call && old_model == last_result && old_model != peer),
        }
    }

    pub(super) fn detach_tool_display(
        &mut self,
        builder: &mut EventBuilder,
        projection: Projection,
    ) {
        if let Some(old) = projection.detach {
            if let Some(event) = builder.event_at_mut(old).filter(|e| model_output(e)) {
                event.kind = model_result_kind(&event.payload);
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
        positions.insert(
            (projected.scope, id.to_owned()),
            super::CompletedPosition {
                kind,
                sequence,
                model: peer,
            },
        );
    }
}
