//! Compare full source output before the reader shortens documentary payloads.

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use super::super::paginated_rollback::json_digest;

mod shell;
mod sources;
pub(in crate::codex) use sources::ShellSources;

const WITNESS: &str = "omnisession_codex_source_receipt";
type Key = [u8; 32];

pub(in super::super) fn clear_source_receipt(payload: &mut Value) {
    if let Some(fields) = payload.as_object_mut() {
        fields.remove(WITNESS);
    }
}

pub(in super::super) fn with_source_receipt(mut payload: Value, source: &Value) -> Value {
    clear_source_receipt(&mut payload);
    let model = matches!(
        source["type"].as_str(),
        Some(
            "function_call_output"
                | "custom_tool_call_output"
                | "local_shell_call_output"
                | "computer_call_output"
        )
    );
    let (body, structured) = if model {
        (model_body(&source["output"]), None)
    } else {
        display_keys(source)
    };
    if body.is_some() || structured.is_some() {
        let mut witness =
            json!({"body":body.map(hex::encode),"structured":structured.map(hex::encode)});
        if model && let Some(shell) = shell::output(&source["output"]) {
            witness["shell"] = shell;
        } else if source["type"] == "CommandExecution"
            && let Some(text) = source["aggregated_output"].as_str()
        {
            witness["shell_sizes"] = shell::display_sizes(text);
        }
        if model && let Some((field, key)) = mcp_writer_body(&source["output"]) {
            witness[field] = hex::encode(key).into();
            if field == "mcp_text"
                && let Some(text) = source["output"].as_str()
                && let Some((header, body)) = text.split_once("\nOutput:\n")
                && let Some(mut cut) = shell::truncation(body)
            {
                cut["header"] = (header.len() + "\nOutput:\n".len()).into();
                witness["mcp_cut"] = cut;
            }
        }
        if let Some(key) = sanitized_mcp_key(source) {
            witness["sanitized"] = hex::encode(key).into();
        }
        payload[WITNESS] = witness;
    }
    payload
}

fn display_keys(source: &Value) -> (Option<Key>, Option<Key>) {
    match source["type"].as_str() {
        Some("CommandExecution") => (model_body(&source["aggregated_output"]), None),
        Some("FunctionCallOutput") => (model_body(&source["output"]), None),
        Some("DynamicToolCall") => {
            let cancelled = source["status"] == "failed"
                && source["success"] == false
                && source["content_items"]
                    .as_array()
                    .is_some_and(Vec::is_empty)
                && source["error"] == "dynamic tool call was cancelled before receiving a response";
            let body = if cancelled {
                model_body(&source["error"])
            } else {
                parts_key(&source["content_items"], dynamic_part)
            };
            (body, None)
        }
        Some("McpToolCall") => {
            let result = &source["result"];
            if result.is_null() && source["status"] == "failed" {
                return (model_body(&source["error"]["message"]), None);
            }
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
            (
                parts.as_array().and_then(|p| mcp_parts_key(p, true)),
                structured,
            )
        }
        _ => (None, None),
    }
}

pub(in super::super) fn model_body(output: &Value) -> Option<Key> {
    if let Some(text) = output.as_str() {
        return Some(single_key(0, Sha256::digest(text).into()));
    }
    parts_key(output, model_part)
}

fn model_part(part: &Value) -> Option<(u8, Key)> {
    match part["type"].as_str() {
        Some("input_text" | "text") => data_key(0, &part["text"]),
        Some("input_audio") => {
            data_key(1, &part["audio_url"]).or_else(|| data_key(5, &part["file_id"]))
        }
        Some("encrypted_content") => data_key(2, &part["encrypted_content"]),
        Some("input_image") => image_key(&part["image_url"], &part["detail"]).or_else(|| {
            let (_, data) = data_key(4, &part["file_id"])?;
            Some((4, image_with_detail(data, part["detail"].as_str())))
        }),
        _ => data_key(0, part),
    }
}

fn dynamic_part(part: &Value) -> Option<(u8, Key)> {
    match part["type"].as_str()? {
        "inputText" => data_key(0, &part["text"]),
        "inputAudio" => data_key(1, &part["audioUrl"]),
        "inputImage" => {
            let key = Sha256::digest(part["imageUrl"].as_str()?).into();
            Some((3, image_with_detail(key, Some("high"))))
        }
        _ => None,
    }
}

fn sanitized_mcp_key(source: &Value) -> Option<Key> {
    if source["type"] != "McpToolCall" {
        return None;
    }
    let parts = source["result"]["content"].as_array()?;
    parts
        .iter()
        .any(|part| part["type"] == "image" && part["_meta"]["codex/imageDetail"] == "original")
        .then(|| mcp_parts_key(parts, false))
        .flatten()
}

fn mcp_parts_key(parts: &[Value], original_supported: bool) -> Option<Key> {
    let parts = parts.iter().filter(|part| {
        !(part["type"] == "text"
            && part["text"] == ""
            && part["_meta"]["codex/encryptedContent"] != true)
    });
    keys_for_parts(parts, |part| mcp_part(part, original_supported))
}

fn mcp_cut_agrees(source: &str, cut: &Value) -> bool {
    let Some(header) = cut["header"].as_u64().and_then(|n| usize::try_from(n).ok()) else {
        return false;
    };
    shell::matching_edges(&shell::display_sizes(source), cut, source, header, true) == Some(true)
}

fn mcp_part(part: &Value, original_supported: bool) -> Option<(u8, Key)> {
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
            let detail = if detail == "original" && !original_supported {
                "high"
            } else {
                detail
            };
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
    keys_for_parts(parts.as_array()?, part_key)
}

fn keys_for_parts<'a>(
    parts: impl IntoIterator<Item = &'a Value>,
    part_key: impl Fn(&Value) -> Option<(u8, Key)>,
) -> Option<Key> {
    let mut hash = Sha256::new();
    for part in parts {
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

// McpToolOutput::response_payload prepends a separate text part for content items.
// Structured content instead gets a single text body with a newline after the header.
fn mcp_writer_body(output: &Value) -> Option<(&'static str, Key)> {
    if let Some(text) = output.as_str() {
        let (time, tail) = text.split_once("\nOutput:")?;
        if !mcp_time(time) {
            return None;
        }
        let body = if tail.is_empty() {
            ""
        } else {
            tail.strip_prefix('\n')?
        };
        return Some(("mcp_text", single_key(0, Sha256::digest(body).into())));
    }
    let (header, body) = output.as_array()?.split_first()?;
    if header["type"] != "input_text"
        || !mcp_time(header["text"].as_str()?.strip_suffix("\nOutput:")?)
    {
        return None;
    }
    Some(("mcp_parts", keys_for_parts(body, model_part)?))
}

fn mcp_time(line: &str) -> bool {
    let Some(seconds) = line
        .strip_prefix("Wall time: ")
        .and_then(|s| s.strip_suffix(" seconds"))
    else {
        return false;
    };
    if seconds.len() > 25 || !seconds.as_bytes().first().is_some_and(u8::is_ascii_digit) {
        return false;
    }
    seconds.parse::<f64>().is_ok_and(|value| {
        value <= std::time::Duration::MAX.as_secs_f64() && format!("{value:.4}") == seconds
    })
}

fn body_matches(witness: &Value, body: Option<&str>, structured: Option<&str>, mcp: bool) -> bool {
    let exact = witness["body"].as_str();
    body.is_some_and(|key| exact == Some(key) || (mcp && witness["mcp_parts"] == key))
        || structured.is_some_and(|key| exact == Some(key) || (mcp && witness["mcp_text"] == key))
}

pub(in super::super) fn model_output_matcher(
    source: &Value,
) -> (Option<Key>, impl Fn(&Value) -> bool + '_) {
    let (body_key, structured) = display_keys(source);
    let body = body_key.map(hex::encode);
    let structured = structured.map(hex::encode);
    let mcp = source["type"] == "McpToolCall";
    let sanitized = sanitized_mcp_key(source).map(hex::encode);
    let command = command_display(source);
    (body_key, move |model: &Value| {
        if let Some(command) = &command
            && let Some(witness) = model[WITNESS].get("shell")
        {
            return shell::agrees(command, witness, source["aggregated_output"].as_str());
        }
        if structured.is_some()
            && let Some(cut) = model[WITNESS].get("mcp_cut")
            && mcp_cut_agrees(&source["result"]["structuredContent"].to_string(), cut)
        {
            return true;
        }
        body_matches(&model[WITNESS], body.as_deref(), structured.as_deref(), mcp)
            || body_matches(&model[WITNESS], sanitized.as_deref(), None, mcp)
    })
}

fn command_display(item: &Value) -> Option<Value> {
    if item["type"] != "CommandExecution" {
        return None;
    }
    let (_, payload) = super::historical_item(item)?;
    Some(with_source_receipt(payload, item))
}

pub(in super::super) fn shell_output_conflicts(item: &Value, model: &Value) -> bool {
    model[WITNESS]
        .get("shell")
        .zip(command_display(item))
        .is_some_and(|(w, d)| {
            !shell::precedes_terminal_display(&d, w)
                && !shell::agrees(&d, w, item["aggregated_output"].as_str())
        })
}

pub(in super::super) fn pending_shell_conflicts(
    display: &Value,
    output: &Value,
    sources: &ShellSources,
) -> bool {
    shell::output(output).is_some_and(|w| {
        !shell::precedes_terminal_display(display, &w) && !sources.agrees(display, &w)
    })
}

pub(in super::super) fn pending_shell_agrees(
    display: &Value,
    output: &Value,
    sources: &ShellSources,
) -> Option<bool> {
    Some(sources.agrees(display, &shell::output(output)?))
}

pub(in super::super) fn deferred_model_receipt(
    display: &Value,
    model: &Value,
    sources: &ShellSources,
) -> bool {
    if display["type"] == "CommandExecution" && model[WITNESS].get("shell").is_none() {
        return false;
    }
    equivalent_model_receipt(display, model, sources)
}

pub(in super::super) fn pending_command_output_matches(
    display: &Value,
    output: &Value,
    sources: &ShellSources,
) -> bool {
    if let Some(agrees) = pending_shell_agrees(display, output, sources) {
        return agrees;
    }
    model_body(output)
        .map(hex::encode)
        .is_some_and(|key| display[WITNESS]["body"] == key)
}

pub(in super::super) fn equivalent_model_receipt(
    display: &Value,
    model: &Value,
    sources: &ShellSources,
) -> bool {
    match display["type"].as_str() {
        Some("CommandExecution") => model[WITNESS]
            .get("shell")
            .is_none_or(|w| sources.agrees(display, w)),
        Some("FunctionCallOutput") => body_matches(
            &model[WITNESS],
            display[WITNESS]["body"].as_str(),
            None,
            false,
        ),
        Some("DynamicToolCall" | "McpToolCall")
            if matches!(display["status"].as_str(), Some("completed" | "failed")) =>
        {
            if display["type"] == "McpToolCall"
                && let Some(cut) = model[WITNESS].get("mcp_cut")
                && sources.mcp_agrees(display, cut)
            {
                return true;
            }
            body_matches(
                &model[WITNESS],
                display[WITNESS]["body"].as_str(),
                display[WITNESS]["structured"].as_str(),
                display["type"] == "McpToolCall",
            ) || body_matches(
                &model[WITNESS],
                display[WITNESS]["sanitized"].as_str(),
                None,
                true,
            )
        }
        _ => false,
    }
}

pub(in super::super) fn annotate_model_receipt(model: &mut Value, display: &Value) {
    let structured_matches = body_matches(
        &model[WITNESS],
        None,
        display[WITNESS]["structured"].as_str(),
        true,
    );
    let truncated_structured =
        display[WITNESS]["structured"].is_string() && model[WITNESS]["mcp_cut"].is_object();
    let transport_error = display["type"] == "McpToolCall"
        && display["output"].is_null()
        && display["error"]["message"].is_string();
    let repeated_error = transport_error
        || display["error"]
            .as_str()
            .is_some_and(|error| model["output"] == error);
    let fuller_command = display["type"] == "CommandExecution"
        && model[WITNESS]["shell"]["cut"].is_object()
        && display[WITNESS]["body"] != model[WITNESS]["shell"]["body"]
        && display[WITNESS]["body"] != model[WITNESS]["shell"]["timeout_body"];
    let Some(fields) = model.as_object_mut() else {
        return;
    };
    fields.remove("omnisession_codex_tool_display");
    if fuller_command {
        fields.insert(
            "omnisession_codex_tool_display".into(),
            json!({"type":"CommandExecution","output":display["output"]}),
        );
        return;
    }
    if !matches!(
        display["type"].as_str(),
        Some("McpToolCall" | "DynamicToolCall" | "FunctionCallOutput")
    ) {
        return;
    }
    let mut metadata: Map<_, _> = display
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(key, _)| {
            key.as_str() != "output"
                && key.as_str() != WITNESS
                && !(key.as_str() == "error" && repeated_error)
        })
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    if truncated_structured && let Some(content) = display["output"].get("content") {
        metadata.insert("content".into(), content.clone());
    }
    let retained = if structured_matches {
        "content"
    } else {
        "structuredContent"
    };
    if display["type"] == "McpToolCall"
        && let Some(value) = display["output"].get(retained)
    {
        metadata.insert(retained.into(), value.clone());
    }
    fields.insert(
        "omnisession_codex_tool_display".into(),
        Value::Object(metadata),
    );
}
