use super::{
    event, message, page_user, read_records_with_preview, read_with_history_mode, turn_item,
    visible_text,
};
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

#[test]
fn an_early_answer_confirmed_after_compaction_stays_in_its_original_window() {
    let turn = "named";
    let rows = vec![
        event("task_started", json!({"turn_id":turn})),
        model_answer(turn, "reply", "earlier answer"),
        page_user(turn, "u1", "earlier request"),
        json!({"type":"compacted","payload":{"message":"","replacement_history":[]}}),
        answer(turn, "reply", "earlier answer"),
        page_user("new", "u2", "fresh request"),
    ];
    for preview in [false, true] {
        let snapshot = read_records_with_preview(rows.clone(), preview).unwrap();
        assert_eq!(
            visible_text(&snapshot),
            ["earlier request", "earlier answer", "fresh request"]
        );
        let imported = omnis_core::import_conversation(&snapshot).messages;
        assert_eq!(
            imported.iter().map(|m| m.text.as_str()).collect::<Vec<_>>(),
            ["fresh request"]
        );
    }
}

#[test]
fn a_paginated_answer_published_after_compaction_keeps_its_model_position() {
    let old = model_answer("first", "reply", "published answer");
    for retained in [false, true] {
        let replacement = if retained {
            json!([old["payload"]])
        } else {
            json!([])
        };
        let rows = vec![
            old.clone(),
            json!({"type":"compacted","payload":{"message":"","replacement_history":replacement}}),
            page_user("new", "question", "fresh question"),
            answer("first", "reply", "published answer"),
            answer("new", "reply", "fresh typed answer"),
        ];
        for preview in [false, true] {
            let snapshot =
                read_with_history_mode(rows.clone(), preview, Some("paginated")).unwrap();
            let displayed: Vec<_> = snapshot
                .events
                .iter()
                .filter(|e| e.source.raw_record_type.as_deref() == Some("event_msg.item_completed"))
                .filter_map(|e| e.payload["text"].as_str())
                .collect();
            assert_eq!(
                displayed,
                ["fresh question", "published answer", "fresh typed answer"]
            );
            let active = omnis_core::import_conversation(&snapshot);
            let texts: Vec<_> = active.messages.iter().map(|m| m.text.as_str()).collect();
            let expected = if retained {
                vec!["published answer", "fresh question", "fresh typed answer"]
            } else {
                vec!["fresh question", "fresh typed answer"]
            };
            assert_eq!(texts, expected);
            assert!(
                omnis_core::trajectory_search_document(&snapshot)
                    .text
                    .contains("published answer")
            );
            if !retained {
                assert!(omnis_core::needs_recovery_handoff(&snapshot, false));
                let mut recovery = Vec::new();
                omnis_core::write_recovery_handoff(&snapshot, &mut recovery).unwrap();
                assert!(
                    String::from_utf8(recovery)
                        .unwrap()
                        .contains("published answer")
                );
                let mut sampled = snapshot;
                sampled
                    .events
                    .retain(|e| e.source.raw_record_type.as_deref() != Some("compacted"));
                assert!(omnis_core::needs_recovery_handoff(&sampled, false));
                assert!(
                    omnis_core::render_session_history_preview(&sampled)
                        .contains("published answer")
                );
                assert!(
                    omnis_core::import_conversation(&sampled)
                        .messages
                        .iter()
                        .all(|m| m.text != "published answer")
                );
            }
        }
    }
}

#[test]
fn a_new_physical_segment_only_reuses_completed_checkpoint_matches() {
    let checkpoint = json!({"type":"compacted","payload":{"message":"","replacement_history":[]}});
    let header = json!({"type":"session_meta","payload":{"history_mode":"paginated"}});
    let rows = vec![
        model_answer("old", "published", "old published answer"),
        model_answer("shared", "reused", "private input from the first file"),
        checkpoint,
        answer("old", "published", "old published answer"),
        header,
        answer("shared", "reused", "fresh item in the next file"),
    ];
    let snapshot = read_with_history_mode(rows, false, Some("paginated")).unwrap();
    let active = omnis_core::import_conversation(&snapshot);
    assert_eq!(
        active
            .messages
            .iter()
            .map(|m| m.text.as_str())
            .collect::<Vec<_>>(),
        ["fresh item in the next file"]
    );
    assert!(visible_text(&snapshot).contains(&"old published answer"));
    assert!(!visible_text(&snapshot).contains(&"private input from the first file"));
}
