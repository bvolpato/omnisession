use std::time::Duration;

use serde::{Deserialize, de::IgnoredAny};

pub(super) const FIELDS: &[&str] = &[
    "type",
    "id",
    "server",
    "tool",
    "arguments",
    "connectorId",
    "mcpAppResourceUri",
    "mcpAppUi",
    "linkId",
    "appName",
    "actionName",
    "pluginId",
    "readOnlyHint",
    "status",
    "result",
    "error",
    "duration",
];

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Call<'a> {
    id: &'a str,
    server: &'a str,
    tool: &'a str,
    arguments: IgnoredAny,
    #[serde(default)]
    connector_id: Option<&'a str>,
    #[serde(default)]
    mcp_app_resource_uri: Option<&'a str>,
    #[serde(default, borrow)]
    mcp_app_ui: Option<AppUi<'a>>,
    #[serde(default)]
    link_id: Option<&'a str>,
    #[serde(default)]
    app_name: Option<&'a str>,
    #[serde(default)]
    action_name: Option<&'a str>,
    #[serde(default)]
    plugin_id: Option<&'a str>,
    #[serde(default)]
    read_only_hint: Option<bool>,
    status: Status,
    #[serde(default)]
    result: Option<ToolResult>,
    #[serde(default, borrow)]
    error: Option<ToolError<'a>>,
    #[serde(default)]
    duration: Option<Duration>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
enum Status {
    InProgress,
    Completed,
    Failed,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AppUi<'a> {
    resource_uri: &'a str,
    preferred_model_display_mode: DisplayMode,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
enum DisplayMode {
    Inline,
    Fullscreen,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ToolResult {
    content: Vec<IgnoredAny>,
    #[serde(default)]
    structured_content: Option<IgnoredAny>,
    #[serde(default)]
    is_error: Option<bool>,
    #[serde(default, rename = "_meta")]
    meta: Option<IgnoredAny>,
}

#[derive(Deserialize)]
struct ToolError<'a> {
    message: &'a str,
}
