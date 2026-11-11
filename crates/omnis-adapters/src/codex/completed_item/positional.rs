//! Decode positional completed messages before validating their envelope and contents.

use serde_json::Value;

use super::super::rollout_lines::name_fields;

fn tag(value: &Value) -> Option<&str> {
    value.get("type").or_else(|| value.get(0))?.as_str()
}

pub(in super::super) fn prepare_container(payload: &mut Value) -> bool {
    if tag(payload) != Some("item_completed") {
        return true;
    }
    let item = payload.get("item").or_else(|| payload.get(3));
    let fields: &[&str] = match item.and_then(tag) {
        Some("FunctionCallOutput") => &["type", "id", "name", "namespace", "output"],
        Some("Plan") => &["type", "id", "text"],
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
