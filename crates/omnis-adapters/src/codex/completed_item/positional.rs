//! Decode positional completed messages before validating their envelope and contents.

use serde_json::Value;

fn tag(value: &Value) -> Option<&str> {
    value.get("type").or_else(|| value.get(0))?.as_str()
}

pub(in super::super) fn prepare_container(record: &mut Value) -> bool {
    let Some(payload) = record.get_mut("payload") else {
        return true;
    };
    if tag(payload) != Some("item_completed") {
        return true;
    }
    let item = payload.get("item").or_else(|| payload.get(3));
    let fields: &[&str] = match item.and_then(tag) {
        Some("UserMessage") => &["type", "id", "client_id", "content"],
        Some("AgentMessage") => &[
            "type",
            "id",
            "content",
            "phase",
            "memory_citation",
            "delivery",
            "questions",
        ],
        _ => return true,
    };
    name_fields(
        payload,
        &[
            "type",
            "thread_id",
            "turn_id",
            "item",
            "started_at_ms",
            "completed_at_ms",
        ],
    ) && name_fields(&mut payload["item"], fields)
}

fn name_fields(value: &mut Value, names: &[&str]) -> bool {
    let Some(parts) = value.as_array_mut() else {
        return true;
    };
    if parts.len() > names.len() {
        return false;
    }
    *value = Value::Object(
        names
            .iter()
            .zip(std::mem::take(parts))
            .map(|(&name, part)| (name.to_owned(), part))
            .collect(),
    );
    true
}
