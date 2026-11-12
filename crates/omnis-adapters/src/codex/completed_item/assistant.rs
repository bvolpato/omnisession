//! Decode the text and validate the metadata of a completed Codex `AgentMessage`.

use serde::Deserialize;
use serde_json::Value;

use super::super::checkpoint_wire::{MessagePhase, array, fields, optional, optional_phase};
use super::super::wire_enums;

pub(in super::super) fn text(part: &Value) -> Option<&str> {
    if let Some(parts) = part.as_array() {
        let [tag, text] = parts.as_slice() else {
            return None;
        };
        return (tag == "Text").then(|| text.as_str()).flatten();
    }
    (part["type"] == "Text")
        .then(|| part["text"].as_str())
        .flatten()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
enum Delivery {
    Async,
}

// The native tagged event buffers its fields. Serde then accepts both null and an empty object
// as the payload of a unit variant. Only the two metadata fields need that buffer here.
#[derive(Deserialize)]
#[serde(tag = "type")]
enum AssistantMetadata {
    #[serde(alias = "agent_message")]
    AgentMessage {
        #[serde(rename = "phase")]
        _phase: Option<MessagePhase>,
        #[serde(rename = "delivery")]
        _delivery: Option<Delivery>,
    },
}

pub(in super::super) fn prepare_metadata(item: &mut Value) -> bool {
    if item["phase"].is_null() && item["delivery"].is_null() {
        return true;
    }
    if !wire_enums::accepts::<AssistantMetadata, _>(item, ["type", "phase", "delivery"]) {
        return false;
    }
    for key in ["phase", "delivery"] {
        if let Some(value) = item.get_mut(key) {
            wire_enums::name_unit(value);
        }
    }
    true
}

pub(in super::super) fn is_async(delivery: &Value) -> bool {
    delivery == "async"
}

pub(in super::super) fn valid(item: &Value) -> bool {
    array(&item["content"], |part| text(part).is_some())
        && optional_phase(&item["phase"])
        && optional(&item["delivery"], is_async)
        && optional(&item["memory_citation"], citation)
        && optional(&item["questions"], |value| array(value, question))
}

pub(in super::super) fn question(value: &Value) -> bool {
    value.as_object().is_none_or(|map| {
        map.keys()
            .all(|k| matches!(k.as_str(), "title" | "options"))
    }) && fields(
        value,
        &[
            ("title", Value::is_string),
            ("options", |v| optional(v, |v| array(v, Value::is_string))),
        ],
    )
}

pub(in super::super) fn citation(value: &Value) -> bool {
    fields(
        value,
        &[
            ("entries", |v| array(v, entry)),
            ("rolloutIds", |v| array(v, Value::is_string)),
        ],
    )
}

fn entry(value: &Value) -> bool {
    let line = |v: &Value| v.as_u64().is_some_and(|n| u32::try_from(n).is_ok());
    fields(
        value,
        &[
            ("path", Value::is_string),
            ("lineStart", line),
            ("lineEnd", line),
            ("note", Value::is_string),
        ],
    )
}
