//! Compare the public text of a tool display with its model receipt.

use serde_json::{Map, Value};

pub(in super::super) fn equivalent_model_receipt(display: &Value, model: &Value) -> bool {
    let content = match display["type"].as_str() {
        Some("CommandExecution" | "FunctionCallOutput") => return true,
        Some("McpToolCall") => &display["output"]["content"],
        Some("DynamicToolCall") => &display["output"],
        _ => return false,
    };
    if !matches!(display["status"].as_str(), Some("failed" | "completed")) {
        return false;
    }
    let Some(parts) = content.as_array() else {
        return false;
    };
    let output = &model["output"];
    match output {
        Value::String(text) => matches!(parts.as_slice(), [part] if equivalent_text(part, text)),
        Value::Array(raw) => {
            parts.len() == raw.len()
                && parts.iter().zip(raw).all(|(part, raw)| {
                    let text = match raw["type"].as_str() {
                        Some("input_text" | "text") => raw["text"].as_str(),
                        _ => raw.as_str(),
                    };
                    text.is_some_and(|text| equivalent_text(part, text))
                })
        }
        _ => false,
    }
}

fn equivalent_text(part: &Value, text: &str) -> bool {
    part.as_str() == Some(text) && text.chars().count() <= super::super::MAX_TOOL_STRING_CHARACTERS
}

pub(in super::super) fn annotate_model_receipt(model: &mut Value, display: &Value) {
    let Some(fields) = model.as_object_mut() else {
        return;
    };
    fields.remove("omnisession_codex_tool_display");
    if !matches!(
        display["type"].as_str(),
        Some("McpToolCall" | "DynamicToolCall")
    ) {
        return;
    }
    let mut metadata: Map<_, _> = display
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(key, _)| key.as_str() != "output")
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    if let Some(structured) = display["output"].get("structuredContent") {
        metadata.insert("structuredContent".into(), structured.clone());
    }
    fields.insert(
        "omnisession_codex_tool_display".into(),
        Value::Object(metadata),
    );
}
