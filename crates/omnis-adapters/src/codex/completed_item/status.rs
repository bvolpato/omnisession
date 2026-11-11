//! Wire schemas for the tool items projected into documentary history.
#![expect(
    dead_code,
    reason = "Serde validates fields that the public projection omits"
)]

use std::{collections::HashMap, time::Duration};

use serde::{Deserialize, de::IgnoredAny};
use serde_json::Value;

use super::super::{
    rollout_lines::{name_fields, name_numeric_tag},
    wire_enums::name_unit,
};

mod collab;
mod mcp;

#[derive(Deserialize)]
#[serde(tag = "type")]
enum ToolItem<'a> {
    DynamicToolCall(#[serde(borrow)] Dynamic<'a>),
    FileChange(#[serde(borrow)] Patch<'a>),
    McpToolCall(#[serde(borrow)] mcp::Call<'a>),
    CollabAgentToolCall(#[serde(borrow)] collab::Call<'a>),
}

pub(super) fn prepare(item: &mut Value) -> bool {
    let Ok(decoded) = ToolItem::deserialize(&*item) else {
        return false;
    };
    let fields: &[&str] = match decoded {
        ToolItem::DynamicToolCall(_) => &[
            "type",
            "id",
            "namespace",
            "tool",
            "arguments",
            "status",
            "content_items",
            "success",
            "error",
            "duration",
        ],
        ToolItem::FileChange(_) => &[
            "type",
            "id",
            "changes",
            "status",
            "auto_approved",
            "stdout",
            "stderr",
        ],
        ToolItem::McpToolCall(_) => mcp::FIELDS,
        ToolItem::CollabAgentToolCall(call) => {
            if call.reasoning_effort == Some("") {
                return false;
            }
            collab::FIELDS
        }
    };
    if !name_fields(item, fields) {
        return false;
    }
    if let Some(status) = item.get_mut("status") {
        name_unit(status);
    }
    match item["type"].as_str() {
        Some("DynamicToolCall") => item["content_items"].as_array_mut().is_none_or(|items| {
            items.iter_mut().all(|part| {
                normalize_tagged(
                    part,
                    &[
                        ("inputText", &["type", "text"]),
                        ("inputImage", &["type", "imageUrl"]),
                        ("inputAudio", &["type", "audioUrl"]),
                    ],
                )
            })
        }),
        Some("FileChange") => item["changes"].as_object_mut().is_some_and(|changes| {
            changes.values_mut().all(|part| {
                normalize_tagged(
                    part,
                    &[
                        ("add", &["type", "content"]),
                        ("delete", &["type", "content"]),
                        ("update", &["type", "unified_diff", "move_path"]),
                    ],
                )
            })
        }),
        Some("McpToolCall") => {
            item.get_mut("result").is_none_or(|result| {
                name_fields(
                    result,
                    &["content", "structuredContent", "isError", "_meta"],
                )
            }) && item
                .get_mut("error")
                .is_none_or(|error| name_fields(error, &["message"]))
        }
        Some("CollabAgentToolCall") => {
            name_unit(&mut item["tool"]);
            true
        }
        _ => false,
    }
}

fn normalize_tagged(part: &mut Value, variants: &[(&'static str, &[&str])]) -> bool {
    name_numeric_tag(part, |index| {
        variants
            .get(usize::try_from(index).ok()?)
            .map(|entry| entry.0)
    });
    let kind = part
        .get("type")
        .or_else(|| part.get(0))
        .and_then(Value::as_str);
    variants
        .iter()
        .find(|(name, _)| Some(*name) == kind)
        .is_some_and(|(_, fields)| name_fields(part, fields))
}

#[derive(Deserialize)]
struct Dynamic<'a> {
    id: &'a str,
    #[serde(default)]
    namespace: Option<&'a str>,
    tool: &'a str,
    arguments: IgnoredAny,
    status: DynamicStatus,
    #[serde(default, borrow)]
    content_items: Option<Vec<DynamicContent<'a>>>,
    #[serde(default)]
    success: Option<bool>,
    #[serde(default)]
    error: Option<&'a str>,
    #[serde(default)]
    duration: Option<Duration>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum DynamicContent<'a> {
    #[serde(rename = "inputText")]
    Text { text: &'a str },
    #[serde(rename = "inputImage")]
    Image {
        #[serde(rename = "imageUrl")]
        image_url: &'a str,
    },
    #[serde(rename = "inputAudio")]
    Audio {
        #[serde(rename = "audioUrl")]
        audio_url: &'a str,
    },
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum DynamicStatus {
    InProgress,
    Completed,
    Failed,
}

#[derive(Deserialize)]
struct Patch<'a> {
    id: &'a str,
    #[serde(borrow)]
    changes: HashMap<&'a str, Change<'a>>,
    #[serde(default)]
    status: Option<PatchStatus>,
    #[serde(default)]
    auto_approved: Option<bool>,
    #[serde(default)]
    stdout: Option<&'a str>,
    #[serde(default)]
    stderr: Option<&'a str>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Change<'a> {
    Add {
        content: &'a str,
    },
    Delete {
        content: &'a str,
    },
    Update {
        unified_diff: &'a str,
        move_path: Option<&'a str>,
    },
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum PatchStatus {
    Completed,
    Failed,
    Declined,
}
