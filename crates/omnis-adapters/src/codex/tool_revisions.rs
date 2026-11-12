use std::collections::{HashMap, VecDeque};

use omnis_ir::{EventKind, OmniEvent, ReplayPolicy};
use serde_json::{Value, json};

use super::{EventBuilder, model_result_kind};
use crate::support::estimated_event_bytes;

pub(super) struct PreviousDisplay {
    kind: EventKind,
    // A model result keeps its original payload when its display status changes.
    payload: Option<Value>,
    model_display: Option<Value>,
    message_source: Option<String>,
}

impl PreviousDisplay {
    pub(super) fn replace_message(
        event: &mut OmniEvent,
        kind: EventKind,
        text: &str,
        raw_type: &str,
    ) -> Option<Self> {
        // The display preserves the native text, including how it joins multipart messages.
        let priority = |wire| match wire {
            Some("event_msg.item_completed") => 2,
            Some("event_msg") => 1,
            _ => 0,
        };
        if priority(Some(raw_type)) < priority(event.source.raw_record_type.as_deref()) {
            return None;
        }
        Some(Self::replace_payload(
            event,
            kind,
            json!({ "text": text }),
            raw_type,
        ))
    }

    pub(super) fn replace_payload(
        event: &mut OmniEvent,
        kind: EventKind,
        payload: Value,
        raw_type: &str,
    ) -> Self {
        event.replay_policy = ReplayPolicy::Contextual;
        Self {
            kind: std::mem::replace(&mut event.kind, kind),
            payload: Some(std::mem::replace(&mut event.payload, payload)),
            model_display: None,
            message_source: event.source.raw_record_type.replace(raw_type.to_owned()),
        }
    }

    pub(super) fn was_displayed_message(&self) -> bool {
        matches!(
            self.message_source.as_deref(),
            Some("event_msg" | "event_msg.item_completed")
        )
    }

    pub(super) fn displayed_text(&self) -> Option<&str> {
        if !self.was_displayed_message() {
            return None;
        }
        self.payload.as_ref()?.get("text")?.as_str()
    }

    fn matches(&self, event: &OmniEvent) -> bool {
        self.kind == event.kind
            && self
                .message_source
                .as_ref()
                .is_none_or(|source| Some(source) == event.source.raw_record_type.as_ref())
            && self
                .model_display
                .as_ref()
                .is_none_or(|display| display == &event.payload["omnisession_codex_tool_display"])
            && self.payload.as_ref().is_none_or(|p| p == &event.payload)
    }

    fn restore(self, event: &mut OmniEvent) {
        if let Some(display) = self.model_display
            && let Some(fields) = event.payload.as_object_mut()
        {
            if display.is_null() {
                fields.remove("omnisession_codex_tool_display");
            } else {
                fields.insert("omnisession_codex_tool_display".into(), display);
            }
        }
        if self.message_source.is_some() || is_display(event) {
            event.kind = self.kind;
            if let Some(payload) = self.payload {
                event.payload = payload;
            }
            if let Some(source) = self.message_source {
                event.source.raw_record_type = Some(source);
            }
        } else if self.kind == EventKind::ToolFailed {
            event.kind = EventKind::ToolFailed;
        } else {
            event.kind = model_result_kind(&event.payload);
        }
    }
}

struct ItemHistory {
    first_input: u64,
    evicted_through: Option<u64>,
    unknown: bool,
}

struct Revision {
    sequence: u64,
    input: u64,
    previous: Option<PreviousDisplay>,
    bytes: usize,
}

pub(super) struct DisplayRevisions {
    items: HashMap<u64, ItemHistory>,
    changes: VecDeque<Revision>,
    bytes: usize,
    max_bytes: usize,
}

impl Default for DisplayRevisions {
    fn default() -> Self {
        Self {
            items: HashMap::new(),
            changes: VecDeque::new(),
            bytes: 0,
            max_bytes: 2 * 1024 * 1024,
        }
    }
}

impl DisplayRevisions {
    pub(super) fn before(
        &self,
        builder: &mut EventBuilder,
        sequence: Option<u64>,
        input: u64,
    ) -> Option<PreviousDisplay> {
        let sequence = sequence?;
        let history = self.items.get(&sequence)?;
        if history.first_input >= input
            || history.evicted_through.is_some_and(|at| at >= input)
            || self
                .changes
                .iter()
                .rev()
                .any(|r| r.sequence == sequence && r.input >= input)
        {
            return None;
        }
        let event = builder.event_at_mut(sequence)?;
        if is_message(event) {
            return None;
        }
        Some(PreviousDisplay {
            kind: event.kind.clone(),
            payload: is_display(event).then(|| event.payload.clone()),
            model_display: super::completion_positions::model_output(event)
                .then(|| event.payload["omnisession_codex_tool_display"].clone()),
            message_source: None,
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
        self.save(sequence, input, input, event, previous);
    }

    pub(super) fn observe_message(
        &mut self,
        builder: &mut EventBuilder,
        previous: Option<(u64, PreviousDisplay)>,
        input: Option<u64>,
        owners: &HashMap<u64, u64>,
    ) {
        let (Some((sequence, previous)), Some(input)) = (previous, input) else {
            return;
        };
        let owner = owners.get(&sequence).copied().unwrap_or(sequence);
        if owner >= input {
            if let Some(history) = self.items.get_mut(&sequence) {
                history.unknown = false;
            }
        } else if let Some(event) = builder.event_at_mut(sequence) {
            self.save(sequence, input, owner, event, Some(previous));
        }
    }

    fn save(
        &mut self,
        sequence: u64,
        input: u64,
        first_input: u64,
        event: &OmniEvent,
        previous: Option<PreviousDisplay>,
    ) {
        let history = self.items.entry(sequence).or_insert(ItemHistory {
            first_input,
            evicted_through: None,
            unknown: false,
        });
        let unknown = std::mem::take(&mut history.unknown);
        let Some(previous) = previous else { return };
        if (previous.message_source.is_some()
            && (history.evicted_through.is_some_and(|at| at >= input)
                || self
                    .changes
                    .iter()
                    .rev()
                    .any(|r| r.sequence == sequence && r.input == input)))
            || (!unknown && previous.matches(event))
        {
            return;
        }
        let size = size_of::<Revision>();
        let mut bytes = size
            + previous.payload.as_ref().map_or(0, estimated_event_bytes)
            + previous
                .model_display
                .as_ref()
                .filter(|display| !display.is_null())
                .map_or(0, estimated_event_bytes)
            + previous.message_source.as_ref().map_or(0, String::len);
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
            let history = self.items.get_mut(&oldest.sequence).unwrap();
            history.evicted_through = Some(
                history
                    .evicted_through
                    .map_or(oldest.input, |at| at.max(oldest.input)),
            );
        }
        self.bytes += bytes;
        self.changes.push_back(Revision {
            sequence,
            input,
            previous,
            bytes,
        });
    }

    pub(super) fn move_to_result(&mut self, display: u64, result: u64) {
        if let Some(history) = self.items.remove(&display) {
            self.items.insert(result, history);
            for change in &mut self.changes {
                if change.sequence == display {
                    change.sequence = result;
                }
            }
        }
    }

    pub(super) fn rollback(&mut self, builder: &mut EventBuilder, cut: u64) {
        // A late item can target an older named turn after the current turn changed.
        let mut retained = VecDeque::new();
        while let Some(change) = self.changes.pop_back() {
            if change.input < cut {
                retained.push_front(change);
                continue;
            }
            self.bytes -= change.bytes;
            if let Some(event) = builder.event_at_mut(change.sequence) {
                self.items.get_mut(&change.sequence).unwrap().unknown = change.previous.is_none();
                if let Some(previous) = change.previous {
                    previous.restore(event);
                }
            }
        }
        self.changes = retained;
        for history in self.items.values_mut() {
            if history.evicted_through.is_some_and(|at| at >= cut) {
                history.evicted_through = None;
                history.unknown = true;
            }
        }
        self.prune(builder, 0);
    }

    pub(super) fn prune(&mut self, builder: &mut EventBuilder, oldest_input: u64) {
        self.items
            .retain(|&sequence, _| builder.event_at_mut(sequence).is_some());
        self.changes.retain(|change| {
            let keep = change.input >= oldest_input && self.items.contains_key(&change.sequence);
            if !keep {
                self.bytes -= change.bytes;
            }
            keep
        });
    }

    pub(super) fn finish(&self, builder: &mut EventBuilder) {
        let mut omitted = 0;
        let mut messages = 0;
        for (&sequence, history) in &self.items {
            if history.unknown
                && let Some(event) = builder.event_at_mut(sequence)
            {
                if is_message(event) {
                    messages += 1;
                    continue;
                }
                omitted += 1;
                if !is_display(event) {
                    super::completion_positions::restore_model_output(event);
                }
            }
        }
        if omitted + messages > 0 {
            builder.retain_events(|event| {
                (!is_display(event) && !is_message(event))
                    || !self
                        .items
                        .get(&event.sequence)
                        .is_some_and(|history| history.unknown)
            });
        }
        if messages > 0 {
            builder.push(
                EventKind::ProviderEvent,
                json!({"omitted_events": messages, "event_kind": "message",
                    "reason": "The earlier message exceeded the retained rollback history."}),
                None,
                ReplayPolicy::HistoricalOnly,
                Some("omnisession.codex_message_revision_limit".into()),
                None,
            );
        }
        if omitted > 0 {
            builder.push(
                EventKind::ProviderEvent,
                json!({"omitted_events": omitted, "omitted_tool_events": omitted, "event_kind": "tool",
                    "reason": "The earlier tool display exceeded the retained rollback history."}),
                None, ReplayPolicy::HistoricalOnly, Some("omnisession.codex_tool_revision_limit".into()), None,
            );
        }
    }
}

fn is_message(event: &OmniEvent) -> bool {
    matches!(
        event.kind,
        EventKind::MessageUser | EventKind::MessageAssistant
    )
}

fn is_display(event: &OmniEvent) -> bool {
    event.source.raw_record_type.as_deref() == Some("event_msg.item_completed")
}

#[cfg(test)]
mod tests {
    use super::*;
    use omnis_ir::Provider;

    #[test]
    fn an_earlier_message_that_exceeds_the_revision_budget_is_reported_and_omitted() {
        let mut builder = EventBuilder::new(Provider::Codex, "example");
        let source = "response_item.message";
        builder.push(
            EventKind::MessageAssistant,
            json!({"text":"old reply"}),
            None,
            ReplayPolicy::Contextual,
            Some(source.into()),
            None,
        );
        let mut history = DisplayRevisions {
            max_bytes: size_of::<Revision>(),
            ..DisplayRevisions::default()
        };
        let previous = PreviousDisplay::replace_message(
            builder.event_at_mut(0).unwrap(),
            EventKind::MessageAssistant,
            "withdrawn reply",
            "event_msg.item_completed",
        )
        .unwrap();
        history.observe_message(&mut builder, Some((0, previous)), Some(1), &HashMap::new());
        history.rollback(&mut builder, 1);
        history.finish(&mut builder);
        assert!(builder.event_at_mut(0).is_none());
        let notice = builder.event_at_mut(1).unwrap();
        assert_eq!(notice.payload["omitted_events"], 1);
        assert_eq!(notice.payload["event_kind"], "message");
    }

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
            let mut history = DisplayRevisions {
                max_bytes: size_of::<Revision>()
                    + if model {
                        0
                    } else {
                        estimated_event_bytes(&payload)
                    },
                ..DisplayRevisions::default()
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
