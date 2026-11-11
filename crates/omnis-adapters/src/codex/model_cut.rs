//! Codex can accept an assistant reply or function call before writing the associated user input.

use serde_json::Value;

pub(super) fn input_order(metadata: &Value) -> Option<u64> {
    metadata["user_input_order"]
        .as_u64()
        .filter(|_| metadata["inherited_user_message"] != true)
}

pub(super) fn acceptance_order(item: &Value, metadata: &Value) -> Option<u64> {
    match item["type"].as_str()? {
        "function_call" => input_order(metadata),
        "message" if item["role"] == "assistant" => input_order(metadata),
        _ => None,
    }
}

pub(super) fn removes_accepted(order: Option<u64>, cutoff: Option<u64>) -> bool {
    order
        .zip(cutoff)
        .is_some_and(|(accepted, cut)| accepted >= cut)
}
