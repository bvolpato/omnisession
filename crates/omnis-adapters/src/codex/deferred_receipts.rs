use std::collections::HashMap;

use super::tool_history::DisplayCallIdentity;
use super::{CodexHistory, EventBuilder, completed_item, completion_positions};
use omnis_ir::{EventKind, ReplayPolicy};
use serde_json::{Value, json};

#[derive(Default)]
pub(super) struct DeferredReceipts(HashMap<(String, String), Vec<Revision>>);

struct Revision {
    display: u64,
    owner: u64,
    awaiting: Option<u64>,
    ambiguous: bool,
    identity: Option<DisplayCallIdentity>,
}

impl DeferredReceipts {
    pub(super) fn observe(
        &mut self,
        scope: (&str, &str),
        display: u64,
        owner: u64,
        pending: Option<u64>,
        identity: Option<DisplayCallIdentity>,
    ) {
        if self.0.is_empty() && pending.is_none() {
            return;
        }
        let key = (scope.0.to_owned(), scope.1.to_owned());
        let revisions = if pending.is_some() {
            self.0.entry(key).or_default()
        } else if let Some(revisions) = self.0.get_mut(&key) {
            revisions
        } else {
            return;
        };
        let revision = Revision {
            display,
            owner,
            awaiting: pending.or_else(|| revisions.last()?.awaiting),
            ambiguous: pending.is_some(),
            identity,
        };
        if let Some(last) = revisions.last_mut()
            && last.display == display
            && last.owner == owner
        {
            *last = revision;
        } else {
            revisions.push(revision);
        }
    }

    pub(super) fn retain(&mut self, mut keep: impl FnMut(u64, u64) -> bool) {
        self.0.retain(|_, revisions| {
            revisions.retain(|r| keep(r.display, r.owner));
            !revisions.is_empty()
        });
    }
}

impl CodexHistory {
    pub(super) fn finish_deferred_receipts(&mut self, builder: &mut EventBuilder) {
        for ((turn, id), mut revisions) in std::mem::take(&mut self.deferred_receipts).0 {
            let Some(active) = revisions.pop() else {
                continue;
            };
            for old in revisions {
                if old.ambiguous
                    && old.display != active.display
                    && let Some(event) = builder.event_at_mut(old.display)
                    && event.source.raw_record_type.as_deref() == Some("event_msg.item_completed")
                {
                    event.kind = EventKind::ProviderEvent;
                    event.replay_policy = ReplayPolicy::HistoricalOnly;
                    event.payload = json!({});
                    event.source.raw_record_type =
                        Some("omnisession.codex_tool_display_mirror".into());
                }
            }
            let Some(pending) = active.awaiting else {
                continue;
            };
            let Some(event) = builder.event_at_mut(active.display) else {
                continue;
            };
            if event.source.raw_record_type.as_deref() != Some("event_msg.item_completed")
                || !matches!(
                    event.payload["type"].as_str(),
                    Some("FunctionCallOutput" | "DynamicToolCall")
                )
            {
                continue;
            }
            let turn = self.aliases.resolve(&turn);
            let Some(end) = self
                .occurrences
                .results(turn, &id)
                .find(|seq| self.calls.result_calls.get(seq) == Some(&pending))
            else {
                self.ambiguous_receipts += usize::from(active.ambiguous);
                continue;
            };
            let display = event.payload.clone();
            let mut candidates = self.occurrences.results(turn, &id).filter(|&result| {
                if result > end {
                    return false;
                }
                let Some(call) = self.calls.result_calls.get(&result) else {
                    return false;
                };
                let compatible = match active.identity {
                    Some(identity) => {
                        self.calls.display_matches_call(identity, *call) == Some(true)
                    }
                    None => builder
                        .event_at_mut(*call)
                        .is_some_and(|e| same_function(&e.payload, &display)),
                };
                compatible
                    && builder.event_at_mut(result).is_some_and(|e| {
                        completed_item::equivalent_model_receipt(&display, &e.payload)
                    })
            });
            if let Some(model) = candidates.next()
                && candidates.next().is_none()
                && completion_positions::pair(builder, active.display, model)
            {
                if self.paginated {
                    self.paginated_answers
                        .move_tool_display(active.display, model);
                }
            } else {
                self.ambiguous_receipts += usize::from(active.ambiguous);
            }
        }
    }
}

fn same_function(call: &Value, display: &Value) -> bool {
    call["name"] == display["name"] && call["namespace"].as_str() == display["namespace"].as_str()
}
