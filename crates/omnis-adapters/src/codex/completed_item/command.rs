//! Validate the command item before publishing its output or applying a revision.
#![expect(
    dead_code,
    reason = "Serde validates the private fields without publishing them"
)]

use std::time::Duration;

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::Deserialize;
use serde_json::Value;
use url::Url;

use super::super::rollout_lines::name_fields;

pub(super) fn prepare(item: &mut Value) -> bool {
    let Ok(CommandItem::CommandExecution(command)) = CommandItem::deserialize(&*item) else {
        return false;
    };
    if !valid_path_uri(command.cwd) {
        return false;
    }
    let status = match command.status {
        Status::InProgress => "in_progress",
        Status::Completed => "completed",
        Status::Failed => "failed",
        Status::Declined => "declined",
    };
    if !name_fields(
        item,
        &[
            "type",
            "id",
            "plugin_id",
            "script_path",
            "process_id",
            "command",
            "cwd",
            "parsed_cmd",
            "source",
            "interaction_input",
            "status",
            "aggregated_output",
            "exit_code",
            "duration",
        ],
    ) {
        return false;
    }
    item["status"] = status.into();
    true
}

pub(super) fn valid_path_uri(cwd: &str) -> bool {
    let Ok(url) = Url::parse(cwd) else {
        return false;
    };
    if url.scheme() != "file"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return false;
    }
    if !url.path().contains("%00") {
        return true;
    }
    // Codex uses this reserved URI for native paths that have no ordinary URI representation.
    url.as_str()
        .strip_prefix("file:///%00/bad/path/")
        .is_some_and(|data| !data.is_empty() && URL_SAFE_NO_PAD.decode(data).is_ok())
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum CommandItem<'a> {
    CommandExecution(#[serde(borrow)] Execution<'a>),
}

// Field order and defaults follow CommandExecutionItem in Codex 0.167.1 protocol/src/items.rs.
#[derive(Deserialize)]
struct Execution<'a> {
    id: &'a str,
    #[serde(default)]
    plugin_id: Option<&'a str>,
    #[serde(default)]
    script_path: Option<&'a str>,
    #[serde(default)]
    process_id: Option<&'a str>,
    command: Vec<&'a str>,
    cwd: &'a str,
    #[serde(borrow)]
    parsed_cmd: Vec<ParsedCommand<'a>>,
    source: Source,
    #[serde(default)]
    interaction_input: Option<&'a str>,
    status: Status,
    #[serde(default)]
    aggregated_output: Option<&'a str>,
    #[serde(default)]
    exit_code: Option<i32>,
    #[serde(default)]
    duration: Option<Duration>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ParsedCommand<'a> {
    Read {
        cmd: &'a str,
        name: &'a str,
        path: &'a str,
    },
    ListFiles {
        cmd: &'a str,
        path: Option<&'a str>,
    },
    Search {
        cmd: &'a str,
        query: Option<&'a str>,
        path: Option<&'a str>,
    },
    Unknown {
        cmd: &'a str,
    },
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Source {
    Agent,
    UserShell,
    UnifiedExecStartup,
    UnifiedExecInteraction,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Status {
    InProgress,
    Completed,
    Failed,
    Declined,
}
