//! Historical display items in paginated Codex rollouts.

use omnis_ir::EventKind;
use serde_json::{Map, Value, json};

use super::{checkpoint_wire, compact_tool_value};

pub(super) mod assistant;
mod positional;

pub(super) fn prepare_event(payload: &mut Value) -> bool {
    if !positional::prepare_container(payload) {
        return false;
    }
    if payload["type"] != "item_completed" {
        return true;
    }
    let item = &payload["item"];
    match item["type"].as_str() {
        Some("UserMessage" | "AgentMessage" | "Plan" | "FunctionCallOutput")
            if !valid_envelope(payload) || !item["id"].is_string() =>
        {
            false
        }
        Some("UserMessage") => prepare_user_message(&mut payload["item"]),
        Some("AgentMessage") => {
            let item = &mut payload["item"];
            item.get_mut("content")
                .and_then(Value::as_array_mut)
                .is_some_and(|parts| parts.iter_mut().all(positional::assistant_content))
                && assistant::valid(item)
        }
        Some("Plan") => item["text"].is_string(),
        Some("FunctionCallOutput") => {
            item["name"].is_string()
                && checkpoint_wire::optional(&item["namespace"], Value::is_string)
                && payload["item"].get_mut("output").is_some_and(|output| {
                    checkpoint_wire::normalize_output(output);
                    checkpoint_wire::output(output)
                })
        }
        _ => true,
    }
}

// A known historical item with an unverified positional layout cannot supply a display record.
pub(super) fn omits_documentary_display(payload: &Value) -> bool {
    payload["type"] == "item_completed"
        && matches!(
            payload["item"].get(0).and_then(Value::as_str),
            Some(
                "CommandExecution"
                    | "FileChange"
                    | "DynamicToolCall"
                    | "McpToolCall"
                    | "WebSearch"
                    | "ImageView"
                    | "ImageGeneration"
                    | "CollabAgentToolCall"
                    | "Extension"
            )
        )
}

pub(super) fn valid_envelope(payload: &Value) -> bool {
    let field = |key: &str, index: usize| payload.get(key).or_else(|| payload.get(index));
    payload.as_array().is_none_or(|parts| parts.len() <= 6)
        && field("thread_id", 1)
            .and_then(Value::as_str)
            .is_some_and(|id| uuid::Uuid::parse_str(id).is_ok())
        && field("turn_id", 2).is_some_and(Value::is_string)
        && field("item", 3).is_some_and(|item| item.is_object() || item.is_array())
        && field("started_at_ms", 4).is_none_or(|value| value.is_null() || value.is_i64())
        && field("completed_at_ms", 5).is_none_or(Value::is_i64)
}

fn prepare_user_message(item: &mut Value) -> bool {
    (item["client_id"].is_null() || item["client_id"].is_string())
        && item
            .get_mut("content")
            .and_then(Value::as_array_mut)
            .is_some_and(|parts| parts.iter_mut().all(user_input))
}

// Codex decodes the whole UserInput list before publishing its item.
fn user_input(part: &mut Value) -> bool {
    if !positional::user_input(part) {
        return false;
    }
    match part["type"].as_str() {
        Some("text") => {
            part["text"].is_string()
                && part.get("text_elements").is_none_or(|spans| {
                    spans
                        .as_array()
                        .is_some_and(|entries| entries.iter().all(text_element))
                })
        }
        Some("image") => checkpoint_wire::image(part),
        Some("audio") => checkpoint_wire::audio(part),
        Some("local_image") => {
            part["path"].is_string() && checkpoint_wire::image_detail(&part["detail"])
        }
        Some("local_audio") => part["path"].is_string(),
        Some("skill" | "mention") => part["name"].is_string() && part["path"].is_string(),
        _ => false,
    }
}

pub(super) fn text_element(value: &Value) -> bool {
    checkpoint_wire::fields(
        value,
        &[
            ("byte_range", byte_range),
            ("placeholder", |v| v.is_null() || v.is_string()),
        ],
    )
}

fn byte_range(value: &Value) -> bool {
    let offset = |n: &Value| n.as_u64().is_some_and(|n| usize::try_from(n).is_ok());
    checkpoint_wire::fields(value, &[("start", offset), ("end", offset)])
}

/// Select the historical fields from a completed Codex turn item. The caller handles messages.
pub(super) fn historical_item(item: &Value) -> Option<(EventKind, Value)> {
    let item_type = item["type"].as_str()?;
    let fields: &[&str] = match item_type {
        "Plan" => &["text"],
        "CommandExecution" => &["command", "cwd", "exit_code"],
        "FileChange" => &["changes", "stdout", "stderr"],
        "FunctionCallOutput" => &["name", "namespace"],
        "DynamicToolCall" => &["namespace", "tool", "arguments", "error"],
        "McpToolCall" => &["server", "tool", "arguments", "error"],
        "WebSearch" => &["query", "action", "results"],
        "ImageView" => &["path"],
        "ImageGeneration" => &["revised_prompt", "saved_path"],
        "CollabAgentToolCall" => &["tool", "sender_thread_id", "receiver_thread_ids"],
        "Extension" => match item["kind"].as_str()? {
            "web.search" => &["kind", "query", "action", "results"],
            "image_gen.generation" => &["kind", "revisedPrompt", "savedPath", "failure"],
            "clock.sleep" => &["kind", "durationMs"],
            _ => return None,
        },
        _ => return None,
    };
    let id = item["id"].as_str().filter(|id| !id.is_empty())?;
    let mut payload = Map::from_iter([
        ("type".to_owned(), Value::from(item_type)),
        ("id".to_owned(), Value::from(id)),
    ]);
    for field in fields.iter().copied().chain(["status"]) {
        if let Some(value) = item.get(field).filter(|value| !value.is_null()) {
            payload.insert(field.to_owned(), compact_tool_value(value, Some(field)));
        }
    }
    // These output fields can carry either display text or model-only media.
    let output = match item_type {
        "CommandExecution" => item
            .get("aggregated_output")
            .map(|v| compact_tool_value(v, None)),
        "FunctionCallOutput" => item.get("output").and_then(text_output),
        "DynamicToolCall" => item.get("content_items").and_then(text_output),
        "McpToolCall" => item.get("result").and_then(|result| {
            let mut output = Map::new();
            if let Some(parts) = result.get("content").and_then(text_output) {
                output.insert("content".to_owned(), parts);
            }
            if let Some(value) = result.get("structuredContent").filter(|v| !v.is_null()) {
                output.insert(
                    "structuredContent".to_owned(),
                    compact_tool_value(value, None),
                );
            }
            (!output.is_empty()).then_some(Value::Object(output))
        }),
        _ => None,
    };
    if let Some(value) = output {
        payload.insert("output".to_owned(), value);
    }
    let kind = match (item_type, item["status"].as_str()) {
        ("Plan", _) => EventKind::PlanUpdated,
        (_, Some("failed" | "declined" | "interrupted")) => EventKind::ToolFailed,
        (_, Some("in_progress" | "inProgress")) => EventKind::ToolCalled,
        ("CommandExecution", _) => EventKind::CommandExecuted,
        _ => EventKind::ToolCompleted,
    };
    Some((kind, Value::Object(payload)))
}

fn text_output(output: &Value) -> Option<Value> {
    if output.is_string() {
        return Some(compact_tool_value(output, None));
    }
    let parts = output.as_array()?;
    let mut shown = Vec::new();
    for part in parts.iter().take(super::MAX_TOOL_ARRAY_ITEMS) {
        let text = match part["type"].as_str() {
            Some("text" | "input_text" | "inputText") => part["text"].as_str(),
            Some("resource") => part["resource"]["text"].as_str(),
            _ => None,
        };
        shown.push(text.map_or_else(
            || json!({"content_omitted": true}),
            |text| Value::String(super::compact_tool_string(text)),
        ));
    }
    if parts.len() > shown.len() {
        shown.push(json!({"omitted_items": parts.len() - shown.len()}));
    }
    Some(Value::Array(shown))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completed_command_preserves_its_outcome_and_ignores_runtime_state() {
        let item = json!({
            "type":"CommandExecution","id":"command","command":["example"],"cwd":"file:///workspace",
            "source":"agent","parsed_cmd":[],"aggregated_output":"missing input","exit_code":1,"status":"failed"
        });
        let (kind, payload) = historical_item(&item).unwrap();
        assert_eq!(kind, EventKind::ToolFailed);
        assert_eq!(
            payload,
            json!({
                "type":"CommandExecution","id":"command","command":["example"],"cwd":"file:///workspace",
                "output":"missing input","exit_code":1,"status":"failed"
            })
        );
    }

    #[test]
    fn output_preserves_text_and_omits_media_and_server_metadata() {
        let item = json!({
            "type":"McpToolCall","id":"call","tool":"lookup","server":"fixture","arguments":{},
            "status":"completed","result":{"content":[
                {"type":"text","text":"visible result"},
                {"type":"image","data":"fixture bytes","mimeType":"image/png"}
            ],"_meta":{"fixture":"private metadata"}}
        });
        let (kind, payload) = historical_item(&item).unwrap();
        assert_eq!(kind, EventKind::ToolCompleted);
        assert_eq!(
            payload["output"],
            json!({"content":["visible result",{"content_omitted":true}]})
        );
    }
}
