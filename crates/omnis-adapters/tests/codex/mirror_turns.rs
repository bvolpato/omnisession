use super::{event, message, page_user, read_records_with_preview, turn_item, visible_text};
use serde_json::json;

#[test]
fn a_reply_separates_repeated_requests_in_an_unmarked_rollout() {
    let rows = vec![
        event("user_message", json!({"message":"repeat"})),
        event("agent_message", json!({"message":"first answer"})),
        message("user", "repeat"),
        message("assistant", "second answer"),
    ];
    for preview in [false, true] {
        let mut records = rows.clone();
        for expected in [
            vec!["repeat", "first answer", "repeat", "second answer"],
            vec!["repeat", "first answer"],
            vec![],
        ] {
            assert_eq!(
                visible_text(&read_records_with_preview(records.clone(), preview).unwrap()),
                expected
            );
            records.push(event("thread_rolled_back", json!({"num_turns":1})));
        }
    }
}

#[test]
fn a_generated_display_turn_matches_its_original_named_model_turn() {
    let rows = vec![
        json!({"type":"turn_context","payload":{"turn_id":"original","cwd":"/workspace/original"}}),
        message("user", "request"),
        message("assistant", "reply"),
        event("task_started", json!({"turn_id":"rollout-4"})),
        page_user("rollout-4", "item-1", "request"),
        turn_item(
            "rollout-4",
            &json!({"type":"AgentMessage","id":"item-2","content":[{"type":"Text","text":"reply"}]}),
        ),
    ];
    for preview in [false, true] {
        let mut records = rows.clone();
        assert_eq!(
            visible_text(&read_records_with_preview(records.clone(), preview).unwrap()),
            ["request", "reply"]
        );
        records.push(event("thread_rolled_back", json!({"num_turns":1})));
        assert!(visible_text(&read_records_with_preview(records, preview).unwrap()).is_empty());
    }
}

#[test]
fn a_generated_turn_cannot_collect_model_text_from_two_different_original_turns() {
    let mut elsewhere = message("assistant", "same text");
    elsewhere["payload"]["internal_chat_message_metadata_passthrough"] = json!({"turn_id":"other"});
    let rows = vec![
        json!({"type":"turn_context","payload":{"turn_id":"original","cwd":"/workspace/original"}}),
        message("user", "request"),
        elsewhere,
        event("task_started", json!({"turn_id":"rollout-4"})),
        page_user("rollout-4", "item-1", "request"),
        turn_item(
            "rollout-4",
            &json!({"type":"AgentMessage","id":"item-2","content":[{"type":"Text","text":"same text"}]}),
        ),
    ];
    for preview in [false, true] {
        assert_eq!(
            visible_text(&read_records_with_preview(rows.clone(), preview).unwrap()),
            ["request", "same text", "same text"]
        );
    }
}

#[test]
fn generated_turns_can_follow_several_original_model_contexts() {
    let pairs = [
        ("a", "rollout-7", "first question", "first answer"),
        ("b", "rollout-9", "second question", "second answer"),
    ];
    let mut rows = Vec::new();
    for (original, _, question, answer) in pairs {
        rows.push(json!({"type":"turn_context","payload":{"turn_id":original,"cwd":"/workspace"}}));
        rows.push(message("user", question));
        rows.push(message("assistant", answer));
    }
    let mut legacy = rows.clone();
    for (_, generated, question, answer) in pairs {
        legacy.push(event("user_message", json!({"message":question})));
        legacy.push(event("agent_message", json!({"message":answer})));
        rows.push(event("task_started", json!({"turn_id":generated})));
        rows.push(page_user(generated, "item-1", question));
        rows.push(turn_item(
            generated,
            &json!({"type":"AgentMessage","id":"item-2","content":[{"type":"Text","text":answer}]}),
        ));
        rows.push(event("task_complete", json!({"turn_id":generated})));
    }
    for source in [rows, legacy] {
        for preview in [false, true] {
            let mut records = source.clone();
            for expected in [
                vec![
                    "first question",
                    "first answer",
                    "second question",
                    "second answer",
                ],
                vec!["first question", "first answer"],
                vec![],
            ] {
                assert_eq!(
                    visible_text(&read_records_with_preview(records.clone(), preview).unwrap()),
                    expected
                );
                records.push(event("thread_rolled_back", json!({"num_turns":1})));
            }
        }
    }
}

#[test]
fn native_user_input_and_its_independently_generated_display_id_share_a_checkpoint() {
    const TURN: &str = "01948226-0000-7001-8000-000000000001";
    let mut raw = message("user", "keep this native request");
    raw["payload"]["id"] = "msg_01948226-0000-7002-8000-000000000002".into();
    raw["payload"]["internal_chat_message_metadata_passthrough"] = json!({"turn_id": TURN});
    let shown = page_user(
        TURN,
        "01948226-0000-7003-8000-000000000003",
        "keep this native request",
    );
    for pair in [[raw.clone(), shown.clone()], [shown, raw]] {
        let mut rows = vec![event("task_started", json!({"turn_id":TURN}))];
        rows.extend(pair);
        for preview in [false, true] {
            let mut records = rows.clone();
            assert_eq!(
                visible_text(&read_records_with_preview(records.clone(), preview).unwrap()),
                ["keep this native request"]
            );
            records.push(event("thread_rolled_back", json!({"num_turns":1})));
            assert!(visible_text(&read_records_with_preview(records, preview).unwrap()).is_empty());
        }
    }
}

#[test]
fn independent_user_ids_do_not_join_unrelated_inputs() {
    let turn = "first";
    let mut raw = message("user", "repeat");
    raw["payload"]["id"] = "caller-stable-id".into();
    let rows = vec![
        event("task_started", json!({"turn_id":turn})),
        raw,
        page_user(turn, "01948226-0000-7003-8000-000000000003", "repeat"),
    ];
    let mut input = message("user", "repeat");
    input["payload"]["id"] = "msg_input_01948226-0000-7004-8000-000000000004".into();
    let mut separate = vec![event("task_started", json!({"turn_id":turn})), input];
    separate.push(event("task_complete", json!({"turn_id":turn})));
    separate.push(event("task_started", json!({"turn_id":"second"})));
    separate.push(page_user(
        "second",
        "01948226-0000-7005-8000-000000000005",
        "repeat",
    ));
    for preview in [false, true] {
        for records in [&rows, &separate] {
            assert_eq!(
                visible_text(&read_records_with_preview(records.clone(), preview).unwrap()),
                ["repeat", "repeat"]
            );
        }
    }
}
