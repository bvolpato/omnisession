//! Codex 0.167.1 decodes `CompactedItemWire` in `history/src/rollout_payload.rs` before selecting a window.
//! Validate the `protocol/src/models.rs` response items in place. Unknown top-level tags deserialize as `ResponseItem::Other`.

use serde_json::Value;

pub(super) fn valid(payload: &Value) -> bool {
    if !payload["message"].is_string()
        || !optional(&payload["window_number"], Value::is_u64)
        || !optional(&payload["window_id"], |id| id.is_string() || id.is_u64())
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
                && entries.iter().all(|entry| {
                    entry.is_object()
                        && optional(&entry["user_input_order"], Value::is_u64)
                        && [
                            "client_authored",
                            "inherited_user_message",
                            "compaction_output",
                            "harness_authored_configuration",
                        ]
                        .iter()
                        .all(|key| entry.get(key).is_none_or(Value::is_boolean))
                })
        })
    }) && items.iter().all(response_item)
}

fn optional(value: &Value, valid: impl FnOnce(&Value) -> bool) -> bool {
    value.is_null() || valid(value)
}

fn array(value: &Value, valid: impl Fn(&Value) -> bool) -> bool {
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

fn image(part: &Value) -> bool {
    (part["image_url"].is_string() || part["file_id"].is_string())
        && optional(&part["detail"], |v| {
            matches!(v.as_str(), Some("auto" | "low" | "high" | "original"))
        })
}

fn message_part(part: &Value) -> bool {
    match part["type"].as_str() {
        Some("input_text") => part["text"].is_string(),
        Some("input_image") => image(part),
        Some("input_audio") => part["audio_url"].is_string() || part["file_id"].is_string(),
        Some("output_text") => {
            part["text"].is_string()
                && optional(&part["annotations"], Value::is_array)
                && optional(&part["logprobs"], Value::is_array)
        }
        _ => false,
    }
}

fn output(value: &Value) -> bool {
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

fn response_item(item: &Value) -> bool {
    let Some(tag) = item["type"].as_str() else {
        return false;
    };
    let valid = match tag {
        "message" => {
            item["role"].is_string()
                && array(&item["content"], message_part)
                && optional_strings(item, &["encrypted_content", "status"])
                && optional(&item["phase"], |v| {
                    matches!(
                        v.as_str(),
                        Some("commentary" | "partial_answer" | "final_answer")
                    )
                })
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
