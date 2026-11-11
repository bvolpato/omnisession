use super::{event, message, read_records_with_preview, visible_text};
use serde_json::json;

#[test]
fn an_ordered_same_text_steer_keeps_its_own_rollback_checkpoint() {
    let turn = "first";
    let mut input = message("user", "repeat");
    input["metadata"] = json!({"user_input_order":2});
    input["payload"]["id"] = "msg_input_01948226-0000-7004-8000-000000000004".into();
    input["payload"]["internal_chat_message_metadata_passthrough"] =
        json!({"turn_id":turn,"content_item_kinds":["user.text"]});
    let records = vec![
        event("task_started", json!({"turn_id":turn})),
        event(
            "user_message",
            json!({"message":"repeat","client_id":"input-1"}),
        ),
        event("agent_message", json!({"message":"first answer"})),
        input,
        message("assistant", "second answer"),
    ];
    for preview in [false, true] {
        let mut rows = records.clone();
        for wanted in [
            vec!["repeat", "first answer", "repeat", "second answer"],
            vec!["repeat", "first answer"],
            vec![],
        ] {
            assert_eq!(
                visible_text(&read_records_with_preview(rows.clone(), preview).unwrap()),
                wanted
            );
            rows.push(event("thread_rolled_back", json!({"num_turns":1})));
        }
    }
}

#[test]
fn a_late_matching_answer_keeps_the_displayed_pair_once_and_preserves_the_input_boundary() {
    let mut ordered = message("user", "repeat");
    ordered["metadata"] = json!({"user_input_order":2});
    ordered["payload"]["id"] = "msg_input_01948226-0000-7004-8000-000000000004".into();
    let base = vec![
        event("task_started", json!({"turn_id":"first"})),
        event(
            "user_message",
            json!({"message":"repeat","client_id":"input-1"}),
        ),
        event("agent_message", json!({"message":"same answer"})),
        ordered,
        message("assistant", "same answer"),
    ];
    for preview in [false, true] {
        let mut rows = base.clone();
        for wanted in [
            vec!["repeat", "same answer"],
            vec!["repeat", "same answer"],
            vec![],
        ] {
            assert_eq!(
                visible_text(&read_records_with_preview(rows.clone(), preview).unwrap()),
                wanted
            );
            rows.push(event("thread_rolled_back", json!({"num_turns":1})));
        }
    }
}
