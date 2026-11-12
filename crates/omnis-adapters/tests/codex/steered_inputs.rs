use super::{event, message, read_records_with_preview, visible_text};
use serde_json::{Value, json};

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
    for (preview, early) in [(false, false), (true, false), (false, true), (true, true)] {
        let mut rows = records.clone();
        if early {
            let mut answer = message("assistant", "first answer");
            answer["payload"]["internal_chat_message_metadata_passthrough"] =
                json!({"turn_id":turn});
            rows.insert(1, answer);
        }
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

fn completed_message(turn: &str, kind: &str, id: &str, text: &str) -> Value {
    let part = if kind == "UserMessage" {
        "text"
    } else {
        "Text"
    };
    event(
        "item_completed",
        json!({"thread_id":super::SESSION_ID, "turn_id":turn,
            "item":{"type":kind,"id":id,"content":[{"type":part,"text":text}]}
        }),
    )
}

fn accepted(turn: &str, id: &str, text: &str) -> [Value; 2] {
    let mut raw = message("user", text);
    raw["payload"]["id"] = id.into();
    raw["payload"]["internal_chat_message_metadata_passthrough"] = json!({"turn_id":turn});
    [raw, completed_message(turn, "UserMessage", id, text)]
}

#[test]
fn rollback_restores_each_revision_to_its_surviving_instruction() {
    let t = "first";
    let answer = |text| completed_message(t, "AgentMessage", "answer", text);
    let request = |text| completed_message(t, "UserMessage", "original", text);
    let rollback = || event("thread_rolled_back", json!({"num_turns":1}));
    let mut records = vec![event("task_started", json!({"turn_id":t}))];
    records.extend(accepted(t, "original", "original question"));
    records.push(answer("original answer"));
    records.extend(accepted(t, "s1", "first steer"));
    records.extend([
        answer("partial"),
        request("clarified question"),
        answer("revised answer"),
    ]);
    records.extend(accepted(t, "s2", "second steer"));
    records.extend([request("temporary question"), answer("temporary answer")]);
    for preview in [false, true] {
        let mut rows = records.clone();
        for wanted in [
            vec![
                "temporary question",
                "temporary answer",
                "first steer",
                "second steer",
            ],
            vec!["clarified question", "revised answer", "first steer"],
            vec!["original question", "original answer"],
            vec![],
        ] {
            assert_eq!(
                visible_text(&read_records_with_preview(rows.clone(), preview).unwrap()),
                wanted
            );
            rows.push(rollback());
        }
    }
}

#[test]
fn a_late_revision_belongs_to_the_latest_input_of_the_named_turn() {
    let t = "first";
    let answer = |turn, text| completed_message(turn, "AgentMessage", "answer", text);
    let mut rows = vec![event("task_started", json!({"turn_id":t}))];
    rows.extend(accepted(t, "u", "first question"));
    rows.push(answer(t, "initial"));
    rows.extend(accepted(t, "s", "earlier steer"));
    rows.extend([
        event("task_complete", json!({"turn_id":t})),
        event("task_started", json!({"turn_id":"other"})),
    ]);
    rows.extend(accepted("other", "u", "newer request"));
    rows.extend([answer("other", "newer answer"), answer(t, "late revision")]);
    for preview in [false, true] {
        let mut rows = rows.clone();
        rows.push(event("thread_rolled_back", json!({"num_turns":1})));
        assert_eq!(
            visible_text(&read_records_with_preview(rows.clone(), preview).unwrap()),
            ["first question", "late revision", "earlier steer"]
        );
        rows.push(event("thread_rolled_back", json!({"num_turns":1})));
        assert_eq!(
            visible_text(&read_records_with_preview(rows, preview).unwrap()),
            ["first question", "initial"]
        );
    }
}

#[test]
fn a_message_revision_needs_an_admitted_steer_for_undo() {
    let t = "first";
    let answer = |text| completed_message(t, "AgentMessage", "a", text);
    let mut base = vec![event("task_started", json!({"turn_id":t}))];
    base.extend(accepted(t, "original", "first input"));
    base.push(answer("first answer"));
    let [receipt, display] = accepted(t, "steer", "steer");
    base.push(display);
    for (receipt, wanted) in [(None, "revised"), (Some(receipt), "first answer")] {
        let admitted = receipt.is_some();
        let mut rows = base.clone();
        rows.extend(receipt);
        rows.extend([
            answer("revised"),
            event("thread_rolled_back", json!({"num_turns":1})),
        ]);
        for preview in [false, true] {
            let snapshot = read_records_with_preview(rows.clone(), preview).unwrap();
            assert_eq!(visible_text(&snapshot), ["first input", wanted]);
            assert_eq!(
                omnis_core::trajectory_search_document(&snapshot).source_complete,
                admitted
            );
        }
    }
}

#[test]
fn a_late_answer_revision_preserves_its_first_display_and_owner() {
    let turn = "first";
    let mut raw = message("assistant", "model answer");
    raw["payload"]["id"] = "answer".into();
    raw["payload"]["internal_chat_message_metadata_passthrough"] = json!({"turn_id":turn});
    for (original, retained) in [
        (None, None),
        (
            Some(event("agent_message", json!({"message":"model answer"}))),
            Some(("model answer", "event_msg")),
        ),
        (
            Some(completed_message(
                turn,
                "AgentMessage",
                "answer",
                "shown answer",
            )),
            Some(("shown answer", "event_msg.item_completed")),
        ),
    ] {
        let mut rows = vec![event("task_started", json!({"turn_id":turn}))];
        rows.extend(accepted(turn, "owner", "owner request"));
        rows.push(raw.clone());
        rows.extend(original);
        rows.extend(accepted(turn, "steer", "steer"));
        rows.extend([
            completed_message(turn, "AgentMessage", "answer", "discarded revision"),
            event("thread_rolled_back", json!({"num_turns":1})),
        ]);
        for preview in [false, true] {
            let snapshot = read_records_with_preview(rows.clone(), preview).unwrap();
            let mut expected = vec!["owner request"];
            if let Some((text, source)) = retained {
                expected.push(text);
                let answer = snapshot
                    .events
                    .iter()
                    .find(|event| event.payload["text"] == text)
                    .unwrap();
                assert_eq!(answer.source.raw_record_type.as_deref(), Some(source));
            }
            assert_eq!(visible_text(&snapshot), expected);
        }
    }
}
