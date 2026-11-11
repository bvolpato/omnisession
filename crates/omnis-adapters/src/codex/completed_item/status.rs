//! Status enums used by documentary Codex turn items in protocol/src/items.rs.

use serde::Deserialize;
use serde_json::Value;

use super::super::wire_enums;

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum DynamicStatus {
    InProgress,
    Completed,
    Failed,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
enum McpStatus {
    InProgress,
    Completed,
    Failed,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum CollabStatus {
    InProgress,
    Completed,
    Failed,
    Interrupted,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum PatchStatus {
    Completed,
    Failed,
    Declined,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum ToolStatus {
    DynamicToolCall {
        #[serde(rename = "status")]
        _status: DynamicStatus,
    },
    McpToolCall {
        #[serde(rename = "status")]
        _status: McpStatus,
    },
    CollabAgentToolCall {
        #[serde(rename = "status")]
        _status: CollabStatus,
    },
    FileChange {
        #[serde(rename = "status")]
        _status: Option<PatchStatus>,
    },
}

pub(super) fn prepare(item: &mut Value) -> bool {
    if !wire_enums::accepts::<ToolStatus, _>(item, ["type", "status"]) {
        return false;
    }
    if let Some(status) = item.get_mut("status") {
        wire_enums::name_unit(status);
    }
    true
}
