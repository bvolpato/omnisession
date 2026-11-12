//! Decode positional completed messages before validating their envelope and contents.

use serde_json::Value;

use super::super::rollout_lines::{name_fields, name_numeric_tag};

// TurnItem in Codex 0.167.1 protocol/src/items.rs. Serde uses declaration order for integer tags.
const TURN_ITEMS: &[&str] = &[
    "UserMessage",
    "FunctionCallOutput",
    "HookPrompt",
    "AgentMessage",
    "Plan",
    "Reasoning",
    "CommandExecution",
    "DynamicToolCall",
    "CollabAgentToolCall",
    "SubAgentActivity",
    "WebSearch",
    "ImageView",
    "Extension",
    "ImageGeneration",
    "EnteredReviewMode",
    "ExitedReviewMode",
    "FileChange",
    "McpToolCall",
    "ContextCompaction",
];
const USER_INPUTS: &[&str] = &[
    "text",
    "image",
    "local_image",
    "audio",
    "local_audio",
    "skill",
    "mention",
];

fn tag(value: &Value) -> Option<&Value> {
    value.get("type").or_else(|| value.get(0))
}

fn name_tag(value: &mut Value, variants: &[&str]) {
    name_numeric_tag(value, |index| {
        variants.get(usize::try_from(index).ok()?).copied()
    });
}

pub(super) fn user_input(part: &mut Value) -> bool {
    name_tag(part, USER_INPUTS);
    let fields: &[&str] = match tag(part).and_then(Value::as_str) {
        Some("text") => &["type", "text", "text_elements"],
        Some("local_image") => &["type", "path", "detail"],
        Some("local_audio") => &["type", "path"],
        Some("skill" | "mention") => &["type", "name", "path"],
        // Both reference types flatten their fields and require an object.
        Some("image" | "audio") => return part.is_object(),
        _ => return false,
    };
    name_fields(part, fields)
}

pub(super) fn assistant_content(part: &mut Value) -> bool {
    name_tag(part, &["Text"]);
    tag(part).is_some_and(|tag| tag == "Text") && name_fields(part, &["type", "text"])
}

pub(in super::super) fn prepare_container(payload: &mut Value) -> bool {
    if tag(payload).is_none_or(|tag| tag != "item_completed") {
        return true;
    }
    if !name_fields(
        payload,
        &[
            "type",
            "thread_id",
            "turn_id",
            "item",
            "started_at_ms",
            "completed_at_ms",
        ],
    ) {
        return false;
    }
    let Some(item) = payload.get_mut("item") else {
        return false;
    };
    name_tag(item, TURN_ITEMS);
    let fields: &[&str] = match tag(item).and_then(Value::as_str) {
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
        Some("ContextCompaction") => &["type", "id"],
        _ => return true,
    };
    name_fields(item, fields)
}
