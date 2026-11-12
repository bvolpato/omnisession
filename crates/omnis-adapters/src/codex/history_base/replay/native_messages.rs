use serde_json::Value;

use crate::codex::{display_text, is_hook_fragment};

pub(super) fn requires_native(record: &Value) -> bool {
    let payload = &record["payload"];
    match (record["type"].as_str(), payload["type"].as_str()) {
        (Some("response_item"), Some("message")) => {
            let Some(parts) = payload["content"].as_array() else {
                return false;
            };
            match payload["role"].as_str() {
                Some("assistant") => {
                    parts
                        .iter()
                        .filter_map(|part| part["text"].as_str())
                        .any(|text| {
                            display_text::visit_uncited(text, |_| {})
                                || display_text::without_plan(text).is_some()
                        })
                }
                Some("user") => {
                    parts
                        .iter()
                        .filter_map(|part| part["text"].as_str())
                        .any(|text| {
                            is_hook_fragment(text) || crate::codex::attachments::is_diagnostic(text)
                        })
                        || !payload["id"].is_string()
                            && (parts
                                .iter()
                                .filter(|part| {
                                    matches!(
                                        part["type"].as_str(),
                                        Some("input_audio" | "input_image")
                                    )
                                })
                                .nth(1)
                                .is_some()
                                || parts.iter().any(|part| part["type"] == "input_audio"))
                }
                _ => false,
            }
        }
        (Some("event_msg"), Some("user_message")) => {
            payload["message"]
                .as_str()
                .is_some_and(|text| text.trim().is_empty())
                && [
                    "images",
                    "file_ids",
                    "local_images",
                    "audio",
                    "local_audio",
                    "audio_references",
                ]
                .iter()
                .any(|&key| {
                    payload[key]
                        .as_array()
                        .is_some_and(|items| !items.is_empty())
                })
        }
        (Some("event_msg"), Some("task_started" | "turn_started")) => {
            payload["collaboration_mode_kind"] == "plan"
        }
        _ => false,
    }
}
