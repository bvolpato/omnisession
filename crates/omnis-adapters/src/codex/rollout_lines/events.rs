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
        // prepare_record has already validated and normalized these display items.
        Some("UserMessage" | "AgentMessage" | "Plan" | "FunctionCallOutput") => Wire::Decoded,
        Some("ContextCompaction") => {
            Wire::checked(arity(item, 2, 2) && field(item, "id", 1).is_some_and(Value::is_string))
        }
        Some(_) => Wire::Undetermined,
        None if tag.is_u64() => Wire::Undetermined,
        None => Wire::Invalid,
    }
}

pub(super) fn classify(payload: &Value) -> Wire {
    let get = |name: &str, index: usize| field(payload, name, index);
    let optional = |name, index, check: fn(&Value) -> bool| {
        get(name, index).is_none_or(|v| v.is_null() || check(v))
    };
    let Some(tag) = get("type", 0) else {
        return Wire::Invalid;
    };
    match tag.as_str() {
        Some("agent_message") => Wire::checked(messages::agent(payload)),
        Some("user_message") => Wire::checked(messages::user(payload)),
        Some("item_completed") => completed(payload),
        Some("thread_rolled_back") => Wire::checked(
            arity(payload, 2, 2)
                && get("num_turns", 1)
                    .and_then(Value::as_u64)
                    .is_some_and(|n| u32::try_from(n).is_ok()),
        ),
        Some("token_count") => Wire::checked(
            arity(payload, 3, 3)
                && optional("info", 1, checkpoint_wire::token_usage_info)
                && optional("rate_limits", 2, rate_limits::valid),
        ),
        Some("task_started" | "turn_started") => Wire::checked(
            arity(payload, 7, 8)
                && get("turn_id", 2).is_some_and(Value::is_string)
                && optional("turn_attribution", 1, checkpoint_wire::turn_attribution)
                && optional("root_turn_id", 3, Value::is_string)
                && optional("trace_id", 4, Value::is_string)
                && optional("started_at", 5, Value::is_i64)
                && optional("model_context_window", 6, Value::is_i64)
                && get("collaboration_mode_kind", 7)
                    .is_none_or(|v| CollaborationMode::deserialize(v).is_ok()),
        ),
        Some(tag @ ("task_complete" | "turn_complete" | "turn_aborted")) => {
            let valid = optional("root_turn_id", 1, Value::is_string)
                && optional("started_at", 5, Value::is_i64)
                && optional("completed_at", 6, Value::is_i64)
                && optional("duration_ms", 7, Value::is_i64)
                && if tag == "turn_aborted" {
                    arity(payload, 4, 8)
                        && optional("turn_id", 2, Value::is_string)
                        && get("reason", 3).is_some_and(|v| AbortReason::deserialize(v).is_ok())
                } else {
                    arity(payload, 4, 9)
                        && get("turn_id", 2).is_some_and(Value::is_string)
                        && optional("last_agent_message", 3, Value::is_string)
                        && optional("time_to_first_token_ms", 8, Value::is_i64)
                };
            if valid {
                terminal_error(get("error", 4))
            } else {
                Wire::Invalid
            }
        }
        Some(_) => Wire::Undetermined,
        None if tag.is_u64() => Wire::Undetermined,
        None => Wire::Invalid,
    }
}
