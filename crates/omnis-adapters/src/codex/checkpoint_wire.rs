//! Codex 0.167.1 decodes `CompactedItemWire` in `history/src/rollout_payload.rs` before selecting a window.
//! Validate the `protocol/src/models.rs` response items in place. Unknown top-level tags deserialize as `ResponseItem::Other`.

use serde::Deserialize;
use serde_json::Value;

mod metadata;

pub(super) fn valid(payload: &Value, paginated: bool) -> bool {
    if !payload["message"].is_string()
        || !optional(&payload["window_number"], Value::is_u64)
        || !optional(&payload["window_id"], |id| id.is_string() || id.is_u64())
        || !optional_strings(
            payload,
            &[
                "first_window_id",
                "previous_window_id",
                "compaction_response_id",
            ],
        )
        || !optional(
            &payload["latest_token_usage_record"],
            parses::<metadata::TokenUsageRecord>,
        )
        || !optional(&payload["guardian_history"], |history| {
            array(history, |entry| {
                response_item(entry)
                    && optional(
                        &entry["guardian_metadata"],
                        parses::<metadata::CodexHarnessMetadata>,
                    )
            })
        })
        || !optional(
            &payload["retained_context"],
            parses::<metadata::RetainedContext>,
        )
        || !optional(
            &payload["mcp_resource_origins"],
            parses::<metadata::McpResourceOriginCheckpoint>,
        )
        || !optional(&payload["resume_metadata"], resume_metadata)
    {
        return false;
    }
    let replacement = &payload["replacement_history"];
    let metadata = &payload["replacement_history_metadata"];
    let Some(items) = replacement.as_array() else {
        return replacement.is_null() && metadata.is_null();
    };
    optional(metadata, |metadata| {
        metadata.as_array().is_some_and(|entries| {
            entries.len() == items.len()
                && entries.iter().zip(items).all(|(entry, item)| {
                    (!paginated && legacy_ghost(item))
                        || parses::<metadata::CodexHarnessMetadata>(entry)
                })
        })
    }) && items.iter().all(response_item)
}

// The legacy recorder removes a retired ghost and its aligned metadata before decoding the checkpoint.
pub(super) fn legacy_ghost(item: &Value) -> bool {
    item["type"] == "ghost_snapshot"
}

pub(super) fn active_metadata(value: &Value) -> Option<(Option<u64>, bool, bool)> {
    let metadata = metadata::CodexHarnessMetadata::deserialize(value).ok()?;
    Some((
        metadata.user_input_order,
        metadata.inherited_user_message,
        metadata.client_authored,
    ))
}

fn parses<T: serde::de::DeserializeOwned>(value: &Value) -> bool {
    T::deserialize(value).is_ok()
}

// Serde accepts named fields and positional arrays for the structs in compaction_resume_metadata.rs.
type WireField = (&'static str, fn(&Value) -> bool);

pub(super) fn fields(value: &Value, schema: &[WireField]) -> bool {
    match value {
        Value::Object(_) => schema.iter().all(|(name, valid)| valid(&value[*name])),
        Value::Array(parts) => {
            parts.len() == schema.len()
                && parts
                    .iter()
                    .zip(schema)
                    .all(|(part, (_, valid))| valid(part))
        }
        _ => false,
    }
}

fn optional_text(value: &Value) -> bool {
    optional(value, Value::is_string)
}

fn resume_metadata(metadata: &Value) -> bool {
    fields(
        metadata,
        &[
            ("dynamic_tools", |v| optional(v, dynamic_catalog)),
            ("multi_agent_version", |v| {
                optional(v, |v| matches!(v.as_str(), Some("disabled" | "v1" | "v2")))
            }),
            ("last_started_turn_id", optional_text),
            ("turn_attribution", |v| optional(v, turn_attribution)),
            ("previous_turn_settings", |v| optional(v, previous_settings)),
        ],
    )
}

fn previous_settings(settings: &Value) -> bool {
    fields(
        settings,
        &[
            ("model", Value::is_string),
            ("cyber_access_program", |v| {
                optional(v, |v| {
                    matches!(
                        v.as_str(),
                        Some("standard" | "daybreak_blue" | "daybreak_red")
                    )
                })
            }),
            ("comp_hash", optional_text),
            ("realtime_active", |v| optional(v, Value::is_boolean)),
        ],
    )
}

pub(super) fn turn_attribution(attribution: &Value) -> bool {
    fields(
        attribution,
        &[
            ("turn_id", Value::is_string),
            ("turn_trigger", optional_text),
            ("parent_turn_id", optional_text),
            ("initiating_agent_path", |v| {
                optional(v, |v| {
                    v.as_str()
                        .is_some_and(super::paginated_rollback::agent_path)
                })
            }),
            ("root_turn_id", optional_text),
        ],
    )
}

fn dynamic_catalog(catalog: &Value) -> bool {
    fields(
        catalog,
        &[
            ("thread_id", |v| {
                v.as_str()
                    .is_some_and(|id| uuid::Uuid::parse_str(id).is_ok())
            }),
            ("tools", |v| array(v, |tool| dynamic_tool(tool, true))),
        ],
    )
}

fn dynamic_tool(tool: &Value, allow_namespace: bool) -> bool {
    let field = |name: &str, index: usize| tool.get(name).or_else(|| tool.get(index));
    if !field("name", 1).is_some_and(Value::is_string)
        || !field("description", 2).is_some_and(Value::is_string)
    {
        return false;
    }
    match field("type", 0).and_then(Value::as_str) {
        Some("function") => {
            field("inputSchema", 3).is_some()
                && field("deferLoading", 4).is_none_or(Value::is_boolean)
                && tool.as_array().is_none_or(|fields| fields.len() <= 5)
        }
        Some("namespace") if allow_namespace => {
            tool.as_array().is_none_or(|fields| fields.len() == 4)
                && field("tools", 3)
                    .is_some_and(|tools| array(tools, |child| dynamic_tool(child, false)))
        }
        _ => false,
    }
}

pub(super) fn optional(value: &Value, valid: impl FnOnce(&Value) -> bool) -> bool {
    value.is_null() || valid(value)
}

pub(super) fn array(value: &Value, valid: impl Fn(&Value) -> bool) -> bool {
    value
        .as_array()
        .is_some_and(|items| items.iter().all(valid))
}

fn optional_strings(item: &Value, keys: &[&str]) -> bool {
    keys.iter()
        .all(|key| optional(&item[key], Value::is_string))
}

fn strings(item: &Value, keys: &[&str]) -> bool {
    keys.iter().all(|key| item[key].is_string())
}

pub(super) fn image(part: &Value) -> bool {
    (part["image_url"].is_string() || part["file_id"].is_string()) && image_detail(&part["detail"])
}

pub(super) fn image_detail(detail: &Value) -> bool {
    #[derive(Deserialize)]
    #[serde(rename_all = "lowercase")]
    enum ImageDetail {
        Auto,
        Low,
        High,
        Original,
    }
    optional(detail, parses::<ImageDetail>)
}

pub(super) fn optional_phase(value: &Value) -> bool {
    optional(value, parses::<metadata::MessagePhase>)
}

pub(super) fn audio(part: &Value) -> bool {
    part["audio_url"].is_string() || part["file_id"].is_string()
}

pub(super) fn message_part(part: &Value) -> bool {
    match part["type"].as_str() {
        Some("input_text") => part["text"].is_string(),
        Some("input_image") => image(part),
        Some("input_audio") => audio(part),
        Some("output_text") => {
            part["text"].is_string()
                && optional(&part["annotations"], Value::is_array)
                && optional(&part["logprobs"], Value::is_array)
        }
        _ => false,
    }
}

// Serde accepts positional text, audio and encrypted fields. The flattened image reference requires a map.
pub(super) fn normalize_output(output: &mut Value) {
    const VARIANTS: [&str; 4] = [
        "input_text",
        "input_image",
        "input_audio",
        "encrypted_content",
    ];
    let Some(parts) = output.as_array_mut() else {
        return;
    };
    for part in parts {
        let tag = part.get("type").or_else(|| part.get(0));
        let ordinal = tag
            .and_then(Value::as_u64)
            .and_then(|n| usize::try_from(n).ok())
            .and_then(|n| VARIANTS.get(n))
            .copied();
        if part.is_object() {
            if let Some(name) = ordinal {
                part["type"] = Value::from(name);
            }
            continue;
        }
        let tag = ordinal.or_else(|| tag.and_then(Value::as_str));
        let (name, field) = match tag {
            Some("input_text") => ("input_text", "text"),
            Some("input_audio") => ("input_audio", "audio_url"),
            Some("encrypted_content") => ("encrypted_content", "encrypted_content"),
            _ => continue,
        };
        if let Some(fields) = part.as_array_mut()
            && fields.len() == 2
            && fields[1].is_string()
        {
            *part = serde_json::json!({"type": name, field: fields[1].take()});
        }
    }
}

pub(super) fn prepare_response_output(item: &mut Value) -> bool {
    if !matches!(
        item["type"].as_str(),
        Some("function_call_output" | "custom_tool_call_output")
    ) {
        return true;
    }
    item.get_mut("output").is_some_and(|body| {
        normalize_output(body);
        output(body)
    })
}

pub(super) fn output(value: &Value) -> bool {
    value.is_string()
        || array(value, |part| match part["type"].as_str() {
            Some("input_text") => part["text"].is_string(),
            Some("input_image") => image(part),
            Some("input_audio") => part["audio_url"].is_string(),
            Some("encrypted_content") => part["encrypted_content"].is_string(),
            _ => false,
        })
}

fn web_search(action: &Value) -> bool {
    match action["type"].as_str() {
        Some("search") => {
            optional_strings(action, &["query"])
                && optional(&action["queries"], |v| array(v, Value::is_string))
        }
        Some("open_page") => optional_strings(action, &["url"]),
        Some("find_in_page") => optional_strings(action, &["url", "pattern"]),
        // Codex preserves an unknown action as WebSearchAction::Other.
        other => other.is_some(),
    }
}

fn local_shell(item: &Value) -> bool {
    let action = &item["action"];
    matches!(
        item["status"].as_str(),
        Some("completed" | "in_progress" | "incomplete")
    ) && optional_strings(item, &["encrypted_content", "call_id"])
        && action["type"] == "exec"
        && array(&action["command"], Value::is_string)
        && optional(&action["timeout_ms"], Value::is_u64)
        && optional_strings(action, &["working_directory", "user"])
        && optional(&action["env"], |env| {
            env.as_object()
                .is_some_and(|e| e.values().all(Value::is_string))
        })
}

pub(super) fn response_record(record: &Value) -> bool {
    response_item(&record["payload"])
        && optional(
            &record["metadata"],
            parses::<metadata::CodexHarnessMetadata>,
        )
}

pub(super) fn token_usage_info(value: &Value) -> bool {
    parses::<metadata::TokenUsageInfo>(value)
}

fn response_item(item: &Value) -> bool {
    let Some(tag) = item["type"].as_str() else {
        return false;
    };
    let valid = match tag {
        "message" => {
            item["role"].is_string()
                && array(&item["content"], message_part)
                && optional_strings(item, &["encrypted_content", "status"])
                && optional_phase(&item["phase"])
        }
        "agent_message" => {
            strings(item, &["author", "recipient"])
                && array(&item["content"], |part| match part["type"].as_str() {
                    Some("input_text") => part["text"].is_string(),
                    Some("encrypted_content") => part["encrypted_content"].is_string(),
                    _ => false,
                })
        }
        "reasoning" => {
            array(&item["summary"], |part| {
                part["type"] == "summary_text" && part["text"].is_string()
            }) && optional_strings(item, &["encrypted_content"])
                && optional(&item["content"], |v| {
                    array(v, |part| {
                        matches!(part["type"].as_str(), Some("reasoning_text" | "text"))
                            && part["text"].is_string()
                    })
                })
        }
        "function_call" => {
            strings(item, &["name", "arguments", "call_id"])
                && optional_strings(item, &["namespace", "status", "encrypted_content"])
                && optional(&item["encrypted_function_args"], |v| {
                    array(v, Value::is_string)
                })
        }
        "custom_tool_call" => {
            strings(item, &["name", "input", "call_id"])
                && optional_strings(item, &["namespace", "status", "encrypted_content"])
        }
        "function_call_output" => {
            output(&item["output"]) && optional_strings(item, &["call_id", "name", "namespace"])
        }
        "custom_tool_call_output" => {
            output(&item["output"])
                && item["call_id"].is_string()
                && optional_strings(item, &["name"])
        }
        "local_shell_call" => local_shell(item),
        "web_search_call" => {
            optional(&item["action"], web_search)
                && optional_strings(item, &["status", "encrypted_content"])
        }
        "tool_search_call" => {
            item["execution"].is_string()
                && item.get("arguments").is_some()
                && optional_strings(item, &["call_id", "status", "encrypted_content"])
        }
        "tool_search_output" => {
            strings(item, &["status", "execution"])
                && item["tools"].is_array()
                && optional_strings(item, &["call_id"])
        }
        "image_generation_call" => {
            strings(item, &["status", "result"])
                && optional_strings(item, &["revised_prompt", "encrypted_content"])
        }
        "compaction" | "compaction_summary" => item["encrypted_content"].is_string(),
        "context_compaction" => optional_strings(item, &["encrypted_content"]),
        "additional_tools" => {
            return item["role"].is_string()
                && item["tools"].is_array()
                && optional_strings(item, &["id"]);
        }
        "configuration_update" => {
            return item["reasoning"]["effort"]
                .as_str()
                .is_some_and(|effort| !effort.is_empty());
        }
        // CompactionTrigger, Other and future tags have no fields that Codex reads.
        _ => return true,
    };
    valid
        && optional_strings(item, &["id"])
        && optional(
            &item["internal_chat_message_metadata_passthrough"],
            |metadata| {
                metadata.is_object()
                    && optional_strings(metadata, &["turn_id"])
                    && optional(&metadata["create_time"], Value::is_number)
            },
        )
}
