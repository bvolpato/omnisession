//! Compare full source output before the reader shortens documentary payloads.

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use super::super::paginated_rollback::json_digest;

const WITNESS: &str = "omnisession_codex_source_receipt";
type Key = [u8; 32];

pub(in super::super) fn clear_source_receipt(payload: &mut Value) {
    if let Some(fields) = payload.as_object_mut() {
        fields.remove(WITNESS);
    }
}

pub(in super::super) fn with_source_receipt(mut payload: Value, source: &Value) -> Value {
    clear_source_receipt(&mut payload);
    let (body, structured) = match source["type"].as_str() {
        Some(
            "function_call_output"
            | "custom_tool_call_output"
            | "local_shell_call_output"
            | "computer_call_output",
        ) => (model_body(&source["output"]), None),
        _ => display_keys(source),
    };
    if body.is_some() || structured.is_some() {
        payload[WITNESS] =
            json!({"body":body.map(hex::encode),"structured":structured.map(hex::encode)});
    }
    payload
}

fn display_keys(source: &Value) -> (Option<Key>, Option<Key>) {
    match source["type"].as_str() {
        Some("CommandExecution") => (model_body(&source["aggregated_output"]), None),
        Some("FunctionCallOutput") => (model_body(&source["output"]), None),
        Some("DynamicToolCall") => (parts_key(&source["content_items"], dynamic_part), None),
        Some("McpToolCall") => {
            let result = &source["result"];
            let parts = &result["content"];
            let encrypted = parts.as_array().is_some_and(|parts| {
                parts.iter().any(|part| {
                    part["type"] == "text" && part["_meta"]["codex/encryptedContent"] == true
                })
            });
            // Codex serializes structuredContent as model text unless encrypted content takes precedence.
            let structured = result
                .get("structuredContent")
                .filter(|value| !encrypted && !value.is_null())
                .map(|value| single_key(0, json_digest(value)));
            (parts_key(parts, mcp_part), structured)
        }
        _ => (None, None),
    }
}

fn model_body(output: &Value) -> Option<Key> {
    if let Some(text) = output.as_str() {
        return Some(single_key(0, Sha256::digest(text).into()));
    }
    parts_key(output, |part| match part["type"].as_str() {
        Some("input_text" | "text") => data_key(0, &part["text"]),
        Some("input_audio") => data_key(1, &part["audio_url"]),
        Some("encrypted_content") => data_key(2, &part["encrypted_content"]),
        Some("input_image") => image_key(&part["image_url"], &part["detail"]),
        _ => data_key(0, part),
    })
}

fn dynamic_part(part: &Value) -> Option<(u8, Key)> {
    match part["type"].as_str()? {
        "inputText" => data_key(0, &part["text"]),
        "inputAudio" => data_key(1, &part["audioUrl"]),
        "inputImage" => image_key(&part["imageUrl"], &Value::Null),
        _ => None,
    }
}

fn mcp_part(part: &Value) -> Option<(u8, Key)> {
    match part["type"].as_str() {
        Some("text") if part["text"].is_string() => {
            let encrypted = part["_meta"]["codex/encryptedContent"] == true;
            data_key(if encrypted { 2 } else { 0 }, &part["text"])
        }
        Some("image" | "audio") if part["data"].is_string() => {
            let data = part["data"].as_str()?;
            let mut bytes = Sha256::new();
            if !data.starts_with("data:") {
                let mime = part
                    .get("mimeType")
                    .or_else(|| part.get("mime_type"))
                    .and_then(Value::as_str)
                    .unwrap_or("application/octet-stream");
                bytes.update(b"data:");
                bytes.update(mime);
                bytes.update(b";base64,");
            }
            bytes.update(data);
            let key: Key = bytes.finalize().into();
            if part["type"] == "audio" {
                return Some((1, key));
            }
            let detail = part["_meta"]["codex/imageDetail"]
                .as_str()
                .filter(|v| matches!(*v, "auto" | "original" | "low" | "high"))
                .unwrap_or("high");
            Some((3, image_with_detail(key, Some(detail))))
        }
        _ => Some((0, json_digest(part))),
    }
}

fn data_key(kind: u8, value: &Value) -> Option<(u8, Key)> {
    Some((kind, Sha256::digest(value.as_str()?).into()))
}

fn image_key(url: &Value, detail: &Value) -> Option<(u8, Key)> {
    Some((
        3,
        image_with_detail(Sha256::digest(url.as_str()?).into(), detail.as_str()),
    ))
}

fn image_with_detail(data: Key, detail: Option<&str>) -> Key {
    let mut hash = Sha256::new();
    hash.update(data);
    hash.update(detail.unwrap_or_default());
    hash.finalize().into()
}

fn parts_key(parts: &Value, part_key: impl Fn(&Value) -> Option<(u8, Key)>) -> Option<Key> {
    let mut hash = Sha256::new();
    for part in parts.as_array()? {
        let (kind, key) = part_key(part)?;
        hash.update([kind]);
        hash.update(key);
    }
    Some(hash.finalize().into())
}

fn single_key(kind: u8, part: Key) -> Key {
    let mut hash = Sha256::new();
    hash.update([kind]);
    hash.update(part);
    hash.finalize().into()
}

pub(in super::super) fn display_matches_model(source: &Value, model: &Value) -> bool {
    model[WITNESS]["body"].as_str().is_some_and(|model| {
        let (body, structured) = display_keys(source);
        [body, structured]
            .into_iter()
            .flatten()
            .any(|key| hex::encode(key) == model)
    })
}

pub(in super::super) fn equivalent_model_receipt(display: &Value, model: &Value) -> bool {
    match display["type"].as_str() {
        Some("CommandExecution" | "FunctionCallOutput") => true,
        Some("DynamicToolCall" | "McpToolCall")
            if matches!(display["status"].as_str(), Some("completed" | "failed")) =>
        {
            model[WITNESS]["body"].as_str().is_some_and(|key| {
                ["body", "structured"]
                    .iter()
                    .any(|field| display[WITNESS][*field].as_str() == Some(key))
            })
        }
        _ => false,
    }
}

pub(in super::super) fn annotate_model_receipt(model: &mut Value, display: &Value) {
    let structured_matches = model[WITNESS]["body"]
        .as_str()
        .is_some_and(|body| display[WITNESS]["structured"] == body);
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
        .filter(|(key, _)| key.as_str() != "output" && key.as_str() != WITNESS)
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    let retained = if structured_matches {
        "content"
    } else {
        "structuredContent"
    };
    if let Some(value) = display["output"].get(retained) {
        metadata.insert(retained.into(), value.clone());
    }
    fields.insert(
        "omnisession_codex_tool_display".into(),
        Value::Object(metadata),
    );
}
