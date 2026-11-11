//! Historical display items in paginated Codex rollouts.

use omnis_ir::EventKind;
use serde_json::{Map, Value, json};

use super::compact_tool_value;

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
