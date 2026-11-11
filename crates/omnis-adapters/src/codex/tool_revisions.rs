use std::collections::{HashMap, VecDeque};

use omnis_ir::{EventKind, OmniEvent, ReplayPolicy};
use serde_json::{Value, json};

use super::{EventBuilder, model_result_kind};
use crate::support::estimated_event_bytes;

pub(super) struct PreviousDisplay {
    kind: EventKind,
    // A model result keeps its original payload when its display status changes.
    payload: Option<Value>,
}

struct ToolDisplay {
    first_input: u64,
    evicted_through: Option<u64>,
    unknown: bool,
}

struct Revision {
    tool: u64,
    input: u64,
    previous: Option<PreviousDisplay>,
    bytes: usize,
}

pub(super) struct ToolRevisions {
    tools: HashMap<u64, ToolDisplay>,
    changes: VecDeque<Revision>,
    bytes: usize,
    max_bytes: usize,
}

impl Default for ToolRevisions {
    fn default() -> Self {
        Self {
            tools: HashMap::new(),
            changes: VecDeque::new(),
            bytes: 0,
            max_bytes: 2 * 1024 * 1024,
        }
    }
}

impl ToolRevisions {
    pub(super) fn before(
        &self,
        builder: &mut EventBuilder,
        sequence: Option<u64>,
        input: u64,
    ) -> Option<PreviousDisplay> {
        let sequence = sequence?;
        let history = self.tools.get(&sequence)?;
        if history.first_input >= input
            || history.evicted_through.is_some_and(|at| at >= input)
            || self
                .changes
                .iter()
                .rev()
                .take_while(|r| r.input >= input)
                .any(|r| r.tool == sequence)
        {
            return None;
        }
        let event = builder.event_at_mut(sequence)?;
        Some(PreviousDisplay {
            kind: event.kind.clone(),
            payload: is_display(event).then(|| event.payload.clone()),
        })
    }

    pub(super) fn observe(
        &mut self,
        builder: &mut EventBuilder,
        sequence: u64,
        input: u64,
        previous: Option<PreviousDisplay>,
    ) {
        let Some(event) = builder.event_at_mut(sequence) else {
            return;
        };
        if !matches!(
            event.kind,
            EventKind::CommandExecuted
                | EventKind::ToolCalled
                | EventKind::ToolCompleted
                | EventKind::ToolFailed
        ) {
            return;
        }
        let history = self.tools.entry(sequence).or_insert(ToolDisplay {
            first_input: input,
            evicted_through: None,
            unknown: false,
        });
        let unknown = std::mem::take(&mut history.unknown);
        let Some(previous) = previous else { return };
        if !unknown
            && previous.kind == event.kind
            && previous
                .payload
                .as_ref()
                .is_none_or(|p| p == &event.payload)
        {
            return;
        }
        let size = size_of::<Revision>();
        let mut bytes = size + previous.payload.as_ref().map_or(0, estimated_event_bytes);
        let previous = if unknown || bytes > self.max_bytes {
            bytes = size;
            None
        } else {
            Some(previous)
        };
        while self.changes.len() >= 256 || self.bytes + bytes > self.max_bytes {
            let Some(oldest) = self.changes.pop_front() else {
                break;
            };
            self.bytes -= oldest.bytes;
            self.tools.get_mut(&oldest.tool).unwrap().evicted_through = Some(oldest.input);
        }
        self.bytes += bytes;
        self.changes.push_back(Revision {
            tool: sequence,
            input,
            previous,
            bytes,
        });
    }

    pub(super) fn rollback(&mut self, builder: &mut EventBuilder, cut: u64) {
        while self
            .changes
            .back()
            .is_some_and(|change| change.input >= cut)
        {
            let change = self.changes.pop_back().unwrap();
            self.bytes -= change.bytes;
            if let Some(event) = builder.event_at_mut(change.tool) {
                self.tools.get_mut(&change.tool).unwrap().unknown = change.previous.is_none();
                if let Some(previous) = change.previous {
                    if is_display(event) {
                        event.kind = previous.kind;
                        if let Some(payload) = previous.payload {
                            event.payload = payload;
                        }
                    } else if previous.kind == EventKind::ToolFailed {
                        event.kind = EventKind::ToolFailed;
                    } else {
                        event.kind = model_result_kind(&event.payload);
                    }
                }
            }
        }
        for history in self.tools.values_mut() {
            if history.evicted_through.is_some_and(|at| at >= cut) {
                history.evicted_through = None;
                history.unknown = true;
            }
        }
        self.prune(builder, 0);
    }

    pub(super) fn prune(&mut self, builder: &mut EventBuilder, oldest_input: u64) {
        self.tools
            .retain(|&sequence, _| builder.event_at_mut(sequence).is_some());
        self.changes.retain(|change| {
            let keep = change.input >= oldest_input && self.tools.contains_key(&change.tool);
            if !keep {
                self.bytes -= change.bytes;
            }
            keep
        });
    }

    pub(super) fn finish(&self, builder: &mut EventBuilder) {
        let mut omitted = 0;
        for (&sequence, tool) in &self.tools {
            if tool.unknown
                && let Some(event) = builder.event_at_mut(sequence)
            {
                omitted += 1;
                if !is_display(event) {
                    event.kind = model_result_kind(&event.payload);
                }
            }
        }
        if omitted > 0 {
            builder.retain_events(|event| {
                !is_display(event)
                    || !self
                        .tools
                        .get(&event.sequence)
                        .is_some_and(|tool| tool.unknown)
            });
            builder.push(
                EventKind::ProviderEvent,
                json!({"omitted_events": omitted, "omitted_tool_events": omitted, "event_kind": "tool",
                    "reason": "The earlier tool display exceeded the retained rollback history."}),
                None, ReplayPolicy::HistoricalOnly, Some("omnisession.codex_tool_revision_limit".into()), None,
            );
        }
    }
}

fn is_display(event: &OmniEvent) -> bool {
    event.source.raw_record_type.as_deref() == Some("event_msg.item_completed")
}

#[cfg(test)]
mod tests {
    use super::*;
    use omnis_ir::Provider;

    #[test]
    fn rollback_past_the_revision_budget_reports_loss_and_keeps_raw_results() {
        for model in [false, true] {
            let mut builder = EventBuilder::new(Provider::Codex, "example");
            let (body, source) = if model {
                ("model receipt", "response_item.function_call_output")
            } else {
                ("one", "event_msg.item_completed")
            };
            let payload = json!({"output": body});
            let mut history = ToolRevisions {
                max_bytes: size_of::<Revision>()
                    + if model {
                        0
                    } else {
                        estimated_event_bytes(&payload)
                    },
                ..ToolRevisions::default()
            };
            builder.push(
                EventKind::ToolFailed,
                payload,
                None,
                ReplayPolicy::HistoricalOnly,
                Some(source.into()),
                None,
            );
            history.observe(&mut builder, 0, 1, None);
            for (input, kind, text) in [
                (2, EventKind::ToolCompleted, "two"),
                (3, EventKind::ToolFailed, "new"),
                (4, EventKind::ToolFailed, "new"),
            ] {
                let previous = history.before(&mut builder, Some(0), input);
                let event = builder.event_at_mut(0).unwrap();
                event.kind = kind;
                if !model {
                    event.payload["output"] = text.into();
                }
                history.observe(&mut builder, 0, input, previous);
            }
            history.rollback(&mut builder, 3);
            let retained = builder.event_at_mut(0).unwrap();
            assert_eq!(retained.kind, EventKind::ToolCompleted);
            assert_eq!(retained.payload["output"], if model { body } else { "two" });
            history.rollback(&mut builder, 2);
            history.finish(&mut builder);
            if model {
                let event = builder.event_at_mut(0).unwrap();
                assert_eq!(event.kind, EventKind::ToolCompleted);
                assert_eq!(event.payload["output"], body);
            } else {
                assert!(builder.event_at_mut(0).is_none());
            }
            let notice = builder.event_at_mut(1).unwrap();
            assert_eq!(notice.kind, EventKind::ProviderEvent);
            assert_eq!(notice.payload["omitted_tool_events"], 1);
        }
    }
}
