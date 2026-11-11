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

impl CodexHistory {
    pub(super) fn completion_position(
        &mut self,
        builder: &mut EventBuilder,
        turn: &str,
        id: &str,
        kind: CompletedKind,
        display: bool,
    ) -> (CompletionScope, Option<u64>) {
        let historical = CompletionScope::Historical(kind);
        let positions = self.completed.get(turn);
        let get = |scope| positions?.get(&(scope, id.to_owned())).copied();
        let prior_model = get(historical).map(|position| position.sequence);
        if !self.paginated {
            return (historical, prior_model);
        }
        let published = get(CompletionScope::Published);
        if !display {
            // Each model output belongs to a physical call occurrence. It can fill a pending
            // display receipt once, even when subsequent calls reuse the native ID.
            if let Some(slot) = published
                && slot.kind == kind
                && builder
                    .event_at_mut(slot.sequence)
                    .is_some_and(|event| !model_output(event))
            {
                return (CompletionScope::Published, Some(slot.sequence));
            }
            return (historical, None);
        }
        let mut original = published.map(|slot| slot.sequence);
        if let Some(slot) = published
            && let Some(event) = builder.event_at_mut(slot.sequence)
        {
            if model_output(event) && (slot.kind != kind || prior_model.is_some()) {
                event.kind = model_result_kind(&event.payload);
                self.paginated_answers.forget_tool_display(slot.sequence);
                original = None;
            } else if let Some(model) = prior_model.filter(|seq| *seq != slot.sequence) {
                // A result already has a physical event. Retire the former display in place
                // so that rollback anchors keep their sequences and the result appears once.
                event.kind = EventKind::ProviderEvent;
                event.payload = json!({});
                event.replay_policy = ReplayPolicy::HistoricalOnly;
                event.source.raw_record_type = Some("omnisession.codex_superseded_display".into());
                self.paginated_answers.forget_tool_display(slot.sequence);
                original = Some(model);
            }
        }
        (CompletionScope::Published, original.or(prior_model))
    }
}
