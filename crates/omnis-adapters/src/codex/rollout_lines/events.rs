use serde::Deserialize;
use serde_json::Value;

use super::super::{checkpoint_wire, completed_item};
use super::Wire;

mod messages;
mod rate_limits;

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum AbortReason {
    Interrupted,
    Replaced,
    ReviewEnded,
    BudgetLimited,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum CollaborationMode {
    Plan,
    #[serde(
        alias = "code",
        alias = "pair_programming",
        alias = "execute",
        alias = "custom"
    )]
    Default,
}

fn field<'a>(value: &'a Value, name: &str, index: usize) -> Option<&'a Value> {
    value.get(name).or_else(|| value.get(index))
}

fn arity(value: &Value, min: usize, max: usize) -> bool {
    value
        .as_array()
        .is_none_or(|parts| (min..=max).contains(&parts.len()))
}

fn terminal_error(value: Option<&Value>) -> Wire {
    let Some(error) = value.filter(|value| !value.is_null()) else {
        return Wire::Decoded;
    };
    if !arity(error, 1, 2) || !field(error, "message", 0).is_some_and(Value::is_string) {
        return Wire::Invalid;
    }
    if field(error, "codex_error_info", 1).is_none_or(Value::is_null) {
        Wire::Decoded
    } else {
        Wire::Undetermined
    }
}

fn completed(payload: &Value) -> Wire {
    if !completed_item::valid_envelope(payload) {
        return Wire::Invalid;
    }
    let Some(item) = field(payload, "item", 3) else {
        return Wire::Invalid;
    };
    let Some(tag) = field(item, "type", 0) else {
        return Wire::Invalid;
    };
    match tag.as_str() {
        // prepare_event has already validated and normalized these display items.
        Some("UserMessage" | "AgentMessage" | "Plan" | "FunctionCallOutput") => Wire::Decoded,
        Some("ContextCompaction") => {
            Wire::checked(arity(item, 2, 2) && field(item, "id", 1).is_some_and(Value::is_string))
        }
        Some(_) => Wire::Undetermined,
        None if tag.is_u64() => Wire::Undetermined,
        None => Wire::Invalid,
    }
}

// Name native struct positions before either the display reader or the model reader sees the event.
pub(super) fn prepare(payload: &mut Value) -> bool {
    let Some(parts) = payload.as_array() else {
        return true;
    };
    let (min, fields): (usize, &[&str]) = match parts.first().and_then(Value::as_str) {
        Some("user_message") => return messages::prepare(payload, true),
        Some("agent_message") => return messages::prepare(payload, false),
        Some("thread_rolled_back") => (2, &["type", "num_turns"]),
        Some("token_count") => (3, &["type", "info", "rate_limits"]),
        Some("task_started" | "turn_started") => (
            7,
            &[
                "type",
                "turn_attribution",
                "turn_id",
                "root_turn_id",
                "trace_id",
                "started_at",
                "model_context_window",
                "collaboration_mode_kind",
            ],
        ),
        Some("task_complete" | "turn_complete") => (
            4,
            &[
                "type",
                "root_turn_id",
                "turn_id",
                "last_agent_message",
                "error",
                "started_at",
                "completed_at",
                "duration_ms",
                "time_to_first_token_ms",
            ],
        ),
        Some("turn_aborted") => (
            4,
            &[
                "type",
                "root_turn_id",
                "turn_id",
                "reason",
                "error",
                "started_at",
                "completed_at",
                "duration_ms",
            ],
        ),
        _ => return true,
    };
    parts.len() >= min && super::name_fields(payload, fields)
}

pub(super) fn classify(payload: &Value) -> Wire {
    let get = |name: &str| payload.get(name);
    let optional =
        |name, check: fn(&Value) -> bool| get(name).is_none_or(|v| v.is_null() || check(v));
    let Some(tag) = field(payload, "type", 0) else {
        return Wire::Invalid;
    };
    match tag.as_str() {
        Some("agent_message") => Wire::checked(messages::agent(payload)),
        Some("user_message") => Wire::checked(messages::user(payload)),
        Some("item_completed") => completed(payload),
        Some("thread_rolled_back") => Wire::checked(
            get("num_turns")
                .and_then(Value::as_u64)
                .is_some_and(|n| u32::try_from(n).is_ok()),
        ),
        Some("token_count") => Wire::checked(
            optional("info", checkpoint_wire::token_usage_info)
                && rate_limits::valid_token_count(payload),
        ),
        Some("task_started" | "turn_started") => Wire::checked(
            get("turn_id").is_some_and(Value::is_string)
                && optional("turn_attribution", checkpoint_wire::turn_attribution)
                && optional("root_turn_id", Value::is_string)
                && optional("trace_id", Value::is_string)
                && optional("started_at", Value::is_i64)
                && optional("model_context_window", Value::is_i64)
                && get("collaboration_mode_kind")
                    .is_none_or(|v| CollaborationMode::deserialize(v).is_ok()),
        ),
        Some(tag @ ("task_complete" | "turn_complete" | "turn_aborted")) => {
            let valid = optional("root_turn_id", Value::is_string)
                && optional("started_at", Value::is_i64)
                && optional("completed_at", Value::is_i64)
                && optional("duration_ms", Value::is_i64)
                && if tag == "turn_aborted" {
                    optional("turn_id", Value::is_string)
                        && get("reason").is_some_and(|v| AbortReason::deserialize(v).is_ok())
                } else {
                    get("turn_id").is_some_and(Value::is_string)
                        && optional("last_agent_message", Value::is_string)
                        && optional("time_to_first_token_ms", Value::is_i64)
                };
            if valid {
                terminal_error(get("error"))
            } else {
                Wire::Invalid
            }
        }
        Some(_) => Wire::Undetermined,
        None if tag.is_u64() => Wire::Undetermined,
        None => Wire::Invalid,
    }
}
