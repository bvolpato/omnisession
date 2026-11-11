use serde::Deserialize;
use serde_json::Value;

use super::super::{checkpoint_wire, completed_item};

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum AbortReason {
    Interrupted,
    Replaced,
    ReviewEnded,
    BudgetLimited,
}

pub(super) fn valid(payload: &Value) -> bool {
    let field = |name: &str, index: usize| payload.get(name).or_else(|| payload.get(index));
    let optional = |name, index, check: fn(&Value) -> bool| {
        field(name, index).is_none_or(|v| v.is_null() || check(v))
    };
    match field("type", 0).and_then(Value::as_str) {
        Some("item_completed") => completed_item::valid_envelope(payload),
        Some("thread_rolled_back") => field("num_turns", 1)
            .and_then(Value::as_u64)
            .is_some_and(|n| u32::try_from(n).is_ok()),
        Some("token_count") => optional("info", 1, checkpoint_wire::token_usage_info),
        Some("task_started" | "turn_started") => {
            field("turn_id", 2).is_some_and(Value::is_string)
                && optional("root_turn_id", 3, Value::is_string)
                && optional("trace_id", 4, Value::is_string)
                && optional("started_at", 5, Value::is_i64)
                && optional("model_context_window", 6, Value::is_i64)
        }
        Some(tag @ ("task_complete" | "turn_complete" | "turn_aborted")) => {
            optional("root_turn_id", 1, Value::is_string)
                && optional("started_at", 5, Value::is_i64)
                && optional("completed_at", 6, Value::is_i64)
                && optional("duration_ms", 7, Value::is_i64)
                && if tag == "turn_aborted" {
                    optional("turn_id", 2, Value::is_string)
                        && field("reason", 3).is_some_and(|v| AbortReason::deserialize(v).is_ok())
                } else {
                    field("turn_id", 2).is_some_and(Value::is_string)
                        && optional("last_agent_message", 3, Value::is_string)
                        && optional("time_to_first_token_ms", 8, Value::is_i64)
                }
        }
        tag => tag.is_some(),
    }
}
