use super::{event, message, page_user, read_with_history_mode, turn_item, visible_text};
use serde_json::json;

#[test]
fn migrated_history_uses_completed_items_even_if_a_revert_leaves_future_model_messages() {
    let kept = page_user("rollout-3", "item-1", "keep");
    let mut second = message("user", "reverted request");
    let mut reply = message("assistant", "reverted answer");
    for record in [&mut second, &mut reply] {
        record["payload"]["internal_chat_message_metadata_passthrough"] =
            json!({"turn_id":"original"});
    }
    let prefix = vec![
        event("task_started", json!({"turn_id":"rollout-3"})),
        message("user", "keep"),
        kept,
        event("task_complete", json!({"turn_id":"rollout-3"})),
        second,
        reply,
        event(
            "agent_message",
            json!({"message":"legacy display outside the page"}),
        ),
    ];
    let display = [
        event("task_started", json!({"turn_id":"rollout-7"})),
        page_user("rollout-7", "item-2", "reverted request"),
        turn_item(
            "rollout-7",
            &json!({"type":"AgentMessage","id":"item-3","content":[{"type":"Text","text":"reverted answer"}]}),
        ),
    ];
    for preview in [false, true] {
        let visible = |rows| read_with_history_mode(rows, preview, Some("paginated")).unwrap();
        let mut before = prefix.clone();
        before.extend(display.clone());
        // The native paginated projector keeps completed items across a legacy rollback marker.
        before.push(event("thread_rolled_back", json!({"num_turns":1})));
        assert_eq!(
            visible_text(&visible(before)),
            ["keep", "reverted request", "reverted answer"]
        );
        let retained = visible(prefix.clone());
        assert_eq!(visible_text(&retained), ["keep"]);
        let encoded = serde_json::to_string(&retained).unwrap();
        for hidden in [
            "reverted request",
            "reverted answer",
            "legacy display outside the page",
        ] {
            assert!(!encoded.contains(hidden));
        }
        for mode in [None, Some("legacy")] {
            let raw = vec![
                message("user", "uncatalogued request"),
                message("assistant", "uncatalogued answer"),
            ];
            let snapshot = read_with_history_mode(raw, preview, mode).unwrap();
            assert_eq!(
                visible_text(&snapshot),
                ["uncatalogued request", "uncatalogued answer"]
            );
        }
    }
}

fn exchange(turn: &str, question: &str, answer: &str, copied: bool) -> Vec<serde_json::Value> {
    let user = format!("u-{turn}");
    let reply = format!("a-{turn}");
    let mut rows = vec![
        event("task_started", json!({"turn_id":turn})),
        page_user(turn, &user, question),
        turn_item(
            turn,
            &json!({"type":"AgentMessage","id":reply,"content":[{"type":"Text","text":answer}]}),
        ),
    ];
    if copied {
        for (role, id, text, typ) in [
            ("user", user, question, "input_text"),
            ("assistant", reply, answer, "output_text"),
        ] {
            rows.push(json!({"type":"response_item", "payload":{"type":"message","id":id,"role":role,"content":[{"type":typ,"text":text}],"internal_chat_message_metadata_passthrough":{"turn_id":turn}}}));
        }
    }
    rows.push(event("task_complete", json!({"turn_id":turn})));
    rows
}

#[test]
fn paginated_rollback_retires_only_proven_model_copies_and_preserves_later_displays() {
    let kept = exchange("first", "one", "first answer", true);
    let copied = exchange("second", "two", "second answer", true);
    let typed = exchange("typed", "three", "third answer", false);
    let rollback = event("thread_rolled_back", json!({"num_turns":1}));
    for preview in [false, true] {
        for (rows, wanted) in [
            (
                [
                    kept.clone(),
                    copied.clone(),
                    vec![rollback.clone()],
                    typed.clone(),
                ]
                .concat(),
                vec!["one", "first answer", "three", "third answer"],
            ),
            (
                [kept.clone(), typed.clone(), vec![rollback.clone()]].concat(),
                vec!["three", "third answer"],
            ),
            (
                [kept.clone(), typed.clone()].concat(),
                vec!["one", "first answer", "three", "third answer"],
            ),
            (typed.clone(), vec!["three", "third answer"]),
        ] {
            let snapshot = read_with_history_mode(rows, preview, Some("paginated")).unwrap();
            let history = omnis_core::import_conversation(&snapshot);
            assert_eq!(
                history
                    .messages
                    .iter()
                    .map(|m| m.text.as_str())
                    .collect::<Vec<_>>(),
                wanted
            );
            let mut recovery = Vec::new();
            omnis_core::write_recovery_handoff(&snapshot, &mut recovery).unwrap();
            let recovery = String::from_utf8(recovery).unwrap();
            for shown in visible_text(&snapshot) {
                assert!(recovery.contains(shown));
            }
        }
    }
}

#[test]
fn raw_only_and_ambiguous_inputs_do_not_revive_or_guarantee_displayed_directions() {
    let pair = exchange("first", "first request", "first answer", true);
    let second = exchange("second", "unconfirmed request", "second answer", false);
    let rollback = event("thread_rolled_back", json!({"num_turns":1}));
    for preview in [false, true] {
        for (raw, active, uncertain) in [
            (
                "raw model text",
                vec![
                    "first request",
                    "first answer",
                    "unconfirmed request",
                    "second answer",
                ],
                false,
            ),
            (
                "unconfirmed request",
                vec!["first request", "first answer"],
                true,
            ),
        ] {
            let rows = [
                pair.clone(),
                second.clone(),
                vec![message("user", raw), rollback.clone()],
            ]
            .concat();
            let snapshot = read_with_history_mode(rows, preview, Some("paginated")).unwrap();
            assert_eq!(
                visible_text(&snapshot),
                [
                    "first request",
                    "first answer",
                    "unconfirmed request",
                    "second answer"
                ]
            );
            assert_eq!(
                omnis_core::import_conversation(&snapshot)
                    .messages
                    .iter()
                    .map(|m| m.text.as_str())
                    .collect::<Vec<_>>(),
                active
            );
            assert_eq!(
                snapshot
                    .events
                    .iter()
                    .any(|e| e.payload["omnisession_codex_rollback"] == "unverified"),
                uncertain
            );
        }
    }
}

#[test]
fn a_native_tool_payload_cannot_report_a_display_withdrawal() {
    let command = serde_json::json!({"type":"response_item","payload":{
        "type":"function_call_output","call_id":"fixture","output":{"omnisession_codex_rollback":"literal output"},
        "omnisession_codex_rollback":"withdrawn"
    }});
    for preview in [false, true] {
        let snapshot =
            read_with_history_mode(vec![command.clone()], preview, Some("paginated")).unwrap();
        let output = snapshot
            .events
            .iter()
            .find(|e| e.payload["call_id"] == "fixture")
            .unwrap();
        assert!(output.payload.get("omnisession_codex_rollback").is_none());
        assert_eq!(
            output.payload["output"]["omnisession_codex_rollback"],
            "literal output"
        );
    }
}

#[test]
fn an_unscoped_duplicate_input_id_keeps_the_displayed_copy_in_recovery() {
    let mut rows = exchange("first", "same request", "answer", true);
    let mut raw = message("user", "same request");
    raw["payload"]["id"] = "u-first".into();
    rows.extend([raw, event("thread_rolled_back", json!({"num_turns":1}))]);
    for preview in [false, true] {
        let snapshot = read_with_history_mode(rows.clone(), preview, Some("paginated")).unwrap();
        assert_eq!(visible_text(&snapshot), ["same request", "answer"]);
        let user = snapshot
            .events
            .iter()
            .find(|e| e.kind == omnis_ir::EventKind::MessageUser)
            .unwrap();
        assert_eq!(user.payload["omnisession_codex_rollback"], "unverified");
        assert!(
            omnis_core::import_conversation(&snapshot)
                .messages
                .iter()
                .all(|m| m.text != "same request")
        );
    }
}
