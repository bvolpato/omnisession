use super::{event, message, page_user, read_records_with_preview, turn_item, visible_text};
use serde_json::{Value, json};

fn model_answer(turn: &str, id: &str, text: &str) -> Value {
    let mut record = message("assistant", text);
    record["payload"]["id"] = id.into();
    record["payload"]["internal_chat_message_metadata_passthrough"] = json!({"turn_id":turn});
    record
}

fn answer(turn: &str, id: &str, text: &str) -> Value {
    turn_item(
        turn,
        &json!({"type":"AgentMessage","id":id,"content":[{"type":"Text","text":text}]}),
    )
}

fn check_then_rollback(rows: &[Value], full: &[&str], retained: &[&str]) {
    for preview in [false, true] {
        let mut rows = rows.to_vec();
        for expected in [full, retained] {
            let snapshot = read_records_with_preview(rows.clone(), preview).unwrap();
            assert_eq!(visible_text(&snapshot), expected);
            assert!(
                snapshot
                    .events
                    .windows(2)
                    .all(|events| events[0].sequence < events[1].sequence)
            );
            rows.push(event("thread_rolled_back", json!({"num_turns":1})));
        }
    }
}

#[test]
fn generated_turn_displays_an_early_model_answer_after_its_request() {
    let generated = "rollout-7";
    check_then_rollback(
        &[
            event("task_started", json!({"turn_id":"rollout-3"})),
            page_user("rollout-3", "item-1", "keep"),
            event("task_complete", json!({"turn_id":"rollout-3"})),
            model_answer("original", "msg_b", "early answer"),
            event("task_started", json!({"turn_id":generated})),
            page_user(generated, "item-2", "discard"),
            answer(generated, "msg_b", "early answer"),
        ],
        &["keep", "discard", "early answer"],
        &["keep"],
    );
}

#[test]
fn each_early_answer_keeps_its_first_display_position_and_input_after_an_update() {
    let t = "named";
    check_then_rollback(
        &[
            event("task_started", json!({"turn_id":t})),
            model_answer(t, "first", "model first"),
            page_user(t, "u1", "first input"),
            answer(t, "first", "first answer"),
            model_answer(t, "a", "model a"),
            model_answer(t, "b", "model b"),
            page_user(t, "u2", "steer"),
            answer(t, "b", "shown b"),
            answer(t, "a", "shown a"),
            answer(t, "first", "updated first answer"),
        ],
        &[
            "first input",
            "updated first answer",
            "steer",
            "shown b",
            "shown a",
        ],
        &["first input", "updated first answer"],
    );
}

#[test]
fn a_legacy_displayed_answer_follows_the_steer_that_owns_it() {
    let turn = "original";
    check_then_rollback(
        &[
            event("task_started", json!({"turn_id":turn})),
            message("user", "keep"),
            event("user_message", json!({"message":"keep"})),
            event("agent_message", json!({"message":"first answer"})),
            model_answer(turn, "msg_second", "early answer"),
            message("user", "steer"),
            event("user_message", json!({"message":"steer"})),
            event("agent_message", json!({"message":"early answer"})),
        ],
        &["keep", "first answer", "steer", "early answer"],
        &["keep", "first answer"],
    );
}
