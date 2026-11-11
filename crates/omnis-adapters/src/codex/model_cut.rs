//! Codex can accept an assistant reply or function call before writing the associated user input.

use serde_json::Value;

pub(super) struct HarnessMetadata {
    order: Option<u64>,
    pub(super) inherited: bool,
    pub(super) client_authored: bool,
}

pub(super) fn harness_metadata(value: &Value) -> HarnessMetadata {
    let (order, inherited, client_authored) = if value.is_array() {
        super::checkpoint_wire::active_metadata(value).unwrap_or_default()
    } else {
        (
            value["user_input_order"].as_u64(),
            value["inherited_user_message"] == true,
            value["client_authored"] == true,
        )
    };
    HarnessMetadata {
        order,
        inherited,
        client_authored,
    }
}

pub(super) fn input_order(metadata: &Value) -> Option<u64> {
    let metadata = harness_metadata(metadata);
    metadata.order.filter(|_| !metadata.inherited)
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
