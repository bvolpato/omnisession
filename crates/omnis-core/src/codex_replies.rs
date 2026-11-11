use std::{borrow::Cow, collections::HashSet, ops::Deref};

use omnis_ir::{CanonicalSnapshot, EventKind, OmniEvent, Provider, ReplayPolicy};
use serde_json::Value;

use super::{has_active_codex_event, restored_codex_answer};

/// One displayed reply can have several accepted versions in the model history.
#[derive(Clone, Copy)]
pub(super) struct Occurrence<'a> {
    event: &'a OmniEvent,
    spelling: Option<&'a str>,
}

impl Deref for Occurrence<'_> {
    type Target = OmniEvent;

    fn deref(&self) -> &Self::Target {
        self.event
    }
}

impl<'a> Occurrence<'a> {
    pub(super) fn recorded(event: &'a OmniEvent) -> Self {
        Self {
            event,
            spelling: None,
        }
    }

    pub(super) fn message(self) -> Cow<'a, OmniEvent> {
        match self.spelling {
            None => Cow::Borrowed(self.event),
            Some(text) => Cow::Owned(OmniEvent {
                payload: serde_json::json!({ "text": text }),
                schema_version: self.event.schema_version.clone(),
                event_id: self.event.event_id,
                thread_id: self.event.thread_id,
                branch_id: self.event.branch_id,
                sequence: self.event.sequence,
                timestamp: self.event.timestamp,
                source: self.event.source.clone(),
                kind: self.event.kind.clone(),
                raw_blob_hash: self.event.raw_blob_hash.clone(),
                sensitivity: self.event.sensitivity,
                replay_policy: self.event.replay_policy,
            }),
        }
    }
}

pub(super) fn versions<'a>(
    snapshot: &CanonicalSnapshot,
    event: &'a OmniEvent,
) -> Option<&'a [Value]> {
    if snapshot.session.provider != Provider::Codex
        || event.source.provider != Provider::Codex
        || event.kind != EventKind::MessageAssistant
        || event.replay_policy != ReplayPolicy::Contextual
        || event.source.raw_record_type.as_deref() != Some("event_msg.item_completed")
    {
        return None;
    }
    event
        .payload
        .get("omnisession_codex_active_replies")?
        .as_array()
        .map(Vec::as_slice)
}

pub(super) fn previous_spellings<'a>(
    snapshot: &CanonicalSnapshot,
    event: &'a OmniEvent,
) -> impl Iterator<Item = &'a str> {
    let versions = versions(snapshot, event)
        .into_iter()
        .flatten()
        .filter_map(|version| {
            position(&version["position"])?;
            version["text"].as_str()
        });
    let mut seen = HashSet::new();
    let current = event.payload["text"].as_str();
    restored_codex_answer(snapshot, event)
        .into_iter()
        .chain(versions)
        .filter(move |text| Some(*text) != current && !text.trim().is_empty() && seen.insert(*text))
}

fn position(value: &Value) -> Option<(u64, bool, u64, u64)> {
    let [before, ordinal, index] = value.as_array()?.as_slice() else {
        return None;
    };
    Some((before.as_u64()?, false, ordinal.as_u64()?, index.as_u64()?))
}

pub(super) fn active<'a>(
    snapshot: &CanonicalSnapshot,
    events: Vec<&'a OmniEvent>,
) -> Vec<Occurrence<'a>> {
    let recorded = |event: &'a OmniEvent| Occurrence {
        event,
        spelling: restored_codex_answer(snapshot, event),
    };
    if !events
        .iter()
        .any(|event| versions(snapshot, event).is_some())
    {
        return events
            .into_iter()
            .filter(|event| has_active_codex_event(snapshot, event))
            .map(recorded)
            .collect();
    }
    let mut ordered = Vec::with_capacity(events.len());
    for event in events {
        if let Some(versions) = versions(snapshot, event) {
            for version in versions {
                if let Some(order) = position(&version["position"]) {
                    ordered.push((
                        order,
                        Occurrence {
                            event,
                            spelling: version["text"].as_str(),
                        },
                    ));
                }
            }
        } else if has_active_codex_event(snapshot, event) {
            let order = position(&event.payload["omnisession_codex_checkpoint_position"])
                .unwrap_or((event.sequence, true, 0, 0));
            ordered.push((order, recorded(event)));
        }
    }
    ordered.sort_by_key(|(order, _)| *order);
    ordered.into_iter().map(|(_, event)| event).collect()
}
