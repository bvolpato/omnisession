use super::{event, message, page_user, read_with_history_mode, turn_item, visible_text};
use serde_json::{Value, json};

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
fn paginated_rollback_uses_the_surviving_workspace_and_keeps_its_display() {
    let with_directory = |turn, path, copied| {
        // Migration can write the original turn context before the generated start marker.
        let mut rows = vec![json!({"type":"turn_context","payload":{"turn_id":turn,"cwd":path}})];
        rows.extend(exchange(turn, turn, "answer", copied));
        rows
    };
    let first = with_directory("first", "/workspace/one", true);
    let second = with_directory("second", "/workspace/two", true);
    let typed = with_directory("typed", "/workspace/three", false);
    let rollback = || event("thread_rolled_back", json!({"num_turns":1}));
    let mut ambiguous = second.clone();
    let user = ambiguous
        .iter_mut()
        .find(|r| r["payload"]["role"] == "user")
        .unwrap();
    user["payload"]
        .as_object_mut()
        .unwrap()
        .remove("internal_chat_message_metadata_passthrough");
    let mut steer = second.clone();
    steer.pop();
    let mut input = message("user", "steer");
    input["payload"]["id"] = "steer".into();
    input["payload"]["internal_chat_message_metadata_passthrough"] = json!({"turn_id":"second"});
    steer.extend([input, page_user("second", "steer", "steer")]);
    for (rows, directory, kept) in [
        (
            [first.clone(), second.clone(), vec![rollback()]].concat(),
            "/workspace/one",
            vec!["first"],
        ),
        (
            [first.clone(), second.clone(), typed, vec![rollback()]].concat(),
            "/workspace/three",
            vec!["first", "typed"],
        ),
        (
            [first.clone(), second, vec![rollback(), rollback()]].concat(),
            "/workspace/original",
            vec![],
        ),
        (
            [first.clone(), ambiguous, vec![rollback()]].concat(),
            "",
            vec!["first"],
        ),
        (
            [first, steer, vec![rollback()]].concat(),
            "/workspace/two",
            vec!["first", "second"],
        ),
    ] {
        for preview in [false, true] {
            let snapshot =
                read_with_history_mode(rows.clone(), preview, Some("paginated")).unwrap();
            assert_eq!(
                snapshot.workspace.current_dir,
                std::path::Path::new(directory)
            );
            assert!(visible_text(&snapshot).contains(&"second"));
            let active: Vec<_> = omnis_core::import_conversation(&snapshot)
                .messages
                .into_iter()
                .filter(|m| m.role == omnis_core::HandoffRole::User)
                .map(|m| m.text)
                .collect();
            assert_eq!(active, kept);
        }
    }
}

#[test]
fn a_rollback_consumes_the_suffix_then_the_checkpoint_without_publishing_private_inputs() {
    let first = exchange("first", "public request", "public reply", true);
    let mut replacement: Vec<_> = first
        .iter()
        .filter(|row| row["type"] == "response_item")
        .map(|row| row["payload"].clone())
        .collect();
    replacement.push(message("user", "unpublished checkpoint input")["payload"].clone());
    let mut rows = first;
    rows.push(json!({"type":"compacted","payload":{"message":"note","window_number":4,"replacement_history":replacement}}));
    rows.extend(exchange("suffix", "suffix request", "suffix reply", true));
    for count in 1..=3 {
        rows.push(event("thread_rolled_back", json!({"num_turns":1})));
        let snapshot = read_with_history_mode(rows.clone(), false, Some("paginated")).unwrap();
        let result = omnis_core::import_conversation(&snapshot);
        let expected: &[&str] = if count == 3 {
            &[]
        } else {
            &["public request", "public reply"]
        };
        assert_eq!(
            result
                .messages
                .iter()
                .map(|m| m.text.as_str())
                .collect::<Vec<_>>(),
            expected
        );
        assert_eq!(
            visible_text(&snapshot),
            [
                "public request",
                "public reply",
                "suffix request",
                "suffix reply"
            ]
        );
        assert!(
            !serde_json::to_string(&snapshot)
                .unwrap()
                .contains("unpublished checkpoint input")
        );
    }
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

#[test]
fn migrated_turns_match_original_inputs_by_occurrence_and_retain_ambiguous_copies_for_recovery() {
    let mut rows = Vec::new();
    for (index, turn) in ["rollout-8", "rollout-16"].into_iter().enumerate() {
        let original = format!("original-{index}");
        let mut input = message("user", "same request");
        input["payload"]["id"] =
            format!("msg_input_01948226-0000-7002-8000-00000000000{index}").into();
        input["payload"]["internal_chat_message_metadata_passthrough"] =
            json!({"turn_id":original});
        rows.extend([
            input,
            event("task_started", json!({"turn_id":turn})),
            page_user(turn, &format!("item-{index}"), "same request"),
        ]);
        let text = format!("answer {index}");
        rows.push(turn_item(
            turn,
            &json!({"type":"AgentMessage","id":"a","content":[{"type":"Text","text":text}]}),
        ));
        let mut reply = message("assistant", &text);
        reply["payload"]["id"] = "a".into();
        reply["payload"]["internal_chat_message_metadata_passthrough"] =
            json!({"turn_id":original});
        rows.extend([reply, event("task_complete", json!({"turn_id":turn}))]);
    }
    let mut ambiguous = rows.clone();
    // Neither the generated turn name nor repeated text links these distant displays uniquely.
    for index in [8, 2] {
        ambiguous.insert(index, event("token_count", json!({})));
    }
    for preview in [false, true] {
        for (records, want, uncertain) in [
            (&rows, vec!["same request", "answer 0"], false),
            (&ambiguous, vec![], true),
        ] {
            let mut records = records.clone();
            records.push(event("thread_rolled_back", json!({"num_turns":1})));
            let snapshot = read_with_history_mode(records, preview, Some("paginated")).unwrap();
            assert_eq!(
                visible_text(&snapshot),
                ["same request", "answer 0", "same request", "answer 1"]
            );
            let active = omnis_core::import_conversation(&snapshot);
            let observed: Vec<_> = active
                .messages
                .iter()
                .map(|m| m.text.as_str())
                .filter(|text| *text == "same request" || text.starts_with("answer "))
                .collect();
            assert_eq!(observed, want);
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
fn compaction_only_input_obeys_the_selected_model_window_and_keeps_the_public_display() {
    let first = exchange("first", "first request", "first answer", true);
    let second = exchange("next", "second request", "second answer", false);
    let replacement: Vec<_> = [
        first.clone(),
        exchange("next", "second request", "second answer", true),
    ]
    .concat()
    .into_iter()
    .filter(|row| row["type"] == "response_item")
    .map(|row| row["payload"].clone())
    .collect();
    let shown = [
        "first request",
        "first answer",
        "second request",
        "second answer",
    ];
    for (numbered, retained, rolled, expected) in [
        (true, 4, true, &shown[..2]),
        (true, 4, false, &shown[..]),
        (true, 2, true, &shown[..0]),
        (true, 2, false, &shown[..2]),
        (true, 0, true, &shown[..0]),
        (false, 4, true, &shown[..0]),
        // An item absent from both original and retired model records remains a typed-only display.
        (false, 0, true, &shown[2..]),
    ] {
        let mut payload =
            json!({"message":"checkpoint note","replacement_history":replacement[..retained]});
        if numbered {
            payload["window_number"] = 1.into();
        }
        let mut rows = [first.clone(), second.clone()].concat();
        rows.push(json!({"type":"compacted","payload":payload}));
        if rolled {
            rows.push(event("thread_rolled_back", json!({"num_turns":1})));
        }
        for preview in [false, true] {
            let snapshot =
                read_with_history_mode(rows.clone(), preview, Some("paginated")).unwrap();
            assert_eq!(visible_text(&snapshot), shown);
            if !rolled && retained == 2 {
                let mut recovery = Vec::new();
                omnis_core::write_recovery_handoff(&snapshot, &mut recovery).unwrap();
                let recovery = String::from_utf8(recovery).unwrap();
                assert!(recovery.contains("active compaction checkpoint"));
                assert!(!recovery.contains("rollback"));
                let fidelity = omnis_core::build_semantic_handoff_report_for_snapshot(
                    &snapshot,
                    omnis_ir::Provider::Claude,
                    true,
                );
                assert!(
                    fidelity
                        .warnings
                        .iter()
                        .any(|warning| warning.contains("compaction checkpoint"))
                );
                assert!(
                    !fidelity
                        .warnings
                        .iter()
                        .any(|warning| warning.contains("rollback"))
                );
            }
            let active = omnis_core::import_conversation(&snapshot);
            assert_eq!(
                active
                    .messages
                    .iter()
                    .map(|m| m.text.as_str())
                    .collect::<Vec<_>>(),
                expected,
                "numbered={numbered} retained={retained} rolled={rolled} preview={preview}"
            );
        }
    }
}

fn positional_sidecars(mut rows: Vec<Value>) -> Vec<Value> {
    let defaults = json!([
        [],
        [],
        false,
        null,
        null,
        false,
        null,
        null,
        false,
        null,
        null,
        false,
        false,
        null,
        null
    ]);
    let checkpoint = rows
        .iter_mut()
        .find(|row| row["type"] == "compacted")
        .unwrap();
    for sidecar in checkpoint["payload"]["replacement_history_metadata"]
        .as_array_mut()
        .unwrap()
    {
        let mut tuple = defaults.clone();
        tuple[10] = sidecar["user_input_order"].clone();
        tuple[12] = (sidecar["inherited_user_message"] == true).into();
        *sidecar = tuple;
    }
    rows
}

#[test]
fn checkpoint_uses_input_ownership_and_acceptance_order_across_its_suffix() {
    let turn = "original";
    let model = |id, role, text, order| {
        let kind = if role == "user" {
            "input_text"
        } else {
            "output_text"
        };
        json!({"type":"response_item", "metadata":{"user_input_order":order}, "payload":{
            "type":"message","id":id,"role":role,"content":[{"type":kind,"text":text}],
            "internal_chat_message_metadata_passthrough":{"turn_id":turn}}})
    };
    let first = model(
        "msg_input_01948226-0000-7002-8000-000000000002",
        "user",
        "initial request",
        1,
    );
    let queued = model(
        "msg_input_01948226-0000-7003-8000-000000000003",
        "user",
        "queued request",
        2,
    );
    let reply = turn_item(
        turn,
        &json!({"type":"AgentMessage","id":"reply","content":[{"type":"Text","text":"queued answer"}]}),
    );
    for (in_checkpoint, raw_order, stored_order, inherited, count, wanted) in [
        (
            true,
            1,
            2,
            false,
            0,
            vec!["initial request", "queued answer", "queued request"],
        ),
        (true, 1, 2, false, 1, vec!["initial request"]),
        (
            true,
            2,
            1,
            false,
            1,
            vec!["initial request", "queued answer"],
        ),
        (false, 1, 2, false, 1, vec!["initial request"]),
        (
            true,
            2,
            2,
            true,
            1,
            vec!["initial request", "queued answer"],
        ),
    ] {
        let early = model("reply", "assistant", "queued answer", raw_order);
        let mut rows = vec![
            event("task_started", json!({"turn_id":turn})),
            first.clone(),
            page_user(turn, "item-1", "initial request"),
            early.clone(),
            reply.clone(),
        ];
        let mut retained = vec![first.clone(), early];
        retained[1]["metadata"]["user_input_order"] = stored_order.into();
        if inherited {
            retained[1]["metadata"]["inherited_user_message"] = true.into();
        }
        if in_checkpoint {
            rows.extend([queued.clone(), page_user(turn, "item-2", "queued request")]);
            retained.push(queued.clone());
        }
        rows.push(json!({"type":"compacted", "payload":{"message":"", "window_number":1,
            "replacement_history":retained.iter().map(|r| &r["payload"]).collect::<Vec<_>>(),
            "replacement_history_metadata":retained.iter().map(|r| &r["metadata"]).collect::<Vec<_>>()}}));
        if !in_checkpoint {
            rows.extend([queued.clone(), page_user(turn, "item-2", "queued request")]);
        }
        rows.push(event("thread_rolled_back", json!({"num_turns":count})));
        for records in [rows.clone(), positional_sidecars(rows)] {
            for preview in [false, true] {
                let snapshot =
                    read_with_history_mode(records.clone(), preview, Some("paginated")).unwrap();
                let active = omnis_core::import_conversation(&snapshot);
                assert_eq!(
                    active
                        .messages
                        .iter()
                        .map(|row| row.text.as_str())
                        .collect::<Vec<_>>(),
                    wanted,
                    "in_checkpoint={in_checkpoint} raw={raw_order} stored={stored_order} inherited={inherited} count={count} preview={preview}"
                );
            }
        }
    }
}

#[test]
fn rollback_preserves_an_answer_when_a_numbered_window_has_no_instruction() {
    let mut rows = exchange("old", "old request", "old answer", true);
    rows.push(
        json!({"type":"compacted", "payload":{"message":"", "window_number":2,
        "replacement_history":[{"type":"compaction", "encrypted_content":"synthetic"}]}}),
    );
    let mut after = exchange("after", "unused", "still active", true);
    after.retain(|r| {
        r["payload"]["role"] != "user" && r["payload"]["item"]["type"] != "UserMessage"
    });
    rows.extend(after);
    rows.push(event("thread_rolled_back", json!({"num_turns":3})));
    for preview in [false, true] {
        let snapshot = read_with_history_mode(rows.clone(), preview, Some("paginated")).unwrap();
        let active = omnis_core::import_conversation(&snapshot);
        assert_eq!(
            active
                .messages
                .iter()
                .map(|row| row.text.as_str())
                .collect::<Vec<_>>(),
            ["still active"]
        );
        assert!(visible_text(&snapshot).contains(&"old request"));
    }
}

#[test]
fn a_withdrawn_checkpoint_owner_keeps_its_later_display_reply_out_of_the_handoff() {
    let task = exchange("first", "first request", "first answer", true);
    let second = exchange("second", "second request", "unused reply", true);
    let mut retained: Vec<_> = task
        .iter()
        .filter(|r| r["type"] == "response_item")
        .map(|r| r["payload"].clone())
        .collect();
    retained.push(
        second
            .iter()
            .find(|r| r["payload"]["role"] == "user")
            .unwrap()["payload"]
            .clone(),
    );
    let mut rows = task;
    rows.extend(second.into_iter().filter(|r| {
        r["payload"]["type"] == "task_started" || r["payload"]["item"]["type"] == "UserMessage"
    }));
    rows.push(json!({"type":"compacted","payload":{"message":"", "window_number":3,"replacement_history":retained}}));
    rows.push(turn_item("second", &json!({"type":"AgentMessage","id":"late-b","content":[{"type":"Text","text":"reply to second"}]})));
    rows.extend(exchange(
        "separate",
        "separate request",
        "separate reply",
        false,
    ));
    rows.push(event("thread_rolled_back", json!({"num_turns":1})));
    for preview in [false, true] {
        let snapshot = read_with_history_mode(rows.clone(), preview, Some("paginated")).unwrap();
        let active = omnis_core::import_conversation(&snapshot);
        assert_eq!(
            active
                .messages
                .iter()
                .map(|row| row.text.as_str())
                .collect::<Vec<_>>(),
            [
                "first request",
                "first answer",
                "separate request",
                "separate reply"
            ]
        );
        let mut recovery = Vec::new();
        omnis_core::write_recovery_handoff(&snapshot, &mut recovery).unwrap();
        assert!(
            String::from_utf8(recovery)
                .unwrap()
                .contains("### Previous Codex display (Assistant, withdrawn from model context)\n\n> reply to second")
        );
    }
}

#[test]
fn unnumbered_compaction_follows_its_native_turn_when_a_steer_rolls_back() {
    let request = |turn: &str, text: &str| {
        let mut item = message("user", text);
        item["payload"]["id"] = format!("input-{text}").into();
        item["payload"]["internal_chat_message_metadata_passthrough"] = json!({"turn_id":turn});
        let shown = page_user(turn, &format!("input-{text}"), text);
        [item, shown]
    };
    let checkpoint = |item: &serde_json::Value, numbered| {
        let mut body = json!({"message":"", "replacement_history":[item["payload"]]});
        if numbered {
            body["window_number"] = 1.into();
        }
        json!({"type":"compacted", "payload":body})
    };
    let first = request("first", "first request");
    let second = request("second", "second request");
    let old = request("older", "earlier request");
    let start = |turn| event("task_started", json!({"turn_id":turn}));
    let end = |turn| event("task_complete", json!({"turn_id":turn}));
    for (prior, numbered, next_turn, expected) in [
        (
            false,
            false,
            false,
            vec!["earlier request", "first request", "second request"],
        ),
        (true, false, false, vec!["first request", "second request"]),
        (false, true, false, vec!["second request"]),
        (false, false, true, vec!["second request"]),
    ] {
        let mut rows = vec![start("older")];
        rows.extend(old.clone());
        rows.extend([end("older"), start("first")]);
        rows.extend(first.clone());
        if prior {
            rows.push(checkpoint(&first[0], true));
        }
        rows.extend([end("first"), start("second")]);
        rows.extend(second.clone());
        rows.push(checkpoint(&second[0], numbered));
        if next_turn {
            rows.extend([end("second"), start("third")]);
        }
        let steer_turn = if next_turn { "third" } else { "second" };
        rows.extend(request(steer_turn, "queued request"));
        rows.extend([
            end(steer_turn),
            event("thread_rolled_back", json!({"num_turns":1})),
        ]);
        for preview in [false, true] {
            let snapshot =
                read_with_history_mode(rows.clone(), preview, Some("paginated")).unwrap();
            let active = omnis_core::import_conversation(&snapshot);
            assert_eq!(
                active
                    .messages
                    .iter()
                    .map(|m| m.text.as_str())
                    .collect::<Vec<_>>(),
                expected,
                "prior={prior} numbered={numbered} next_turn={next_turn} preview={preview}"
            );
        }
    }
}

#[test]
fn a_damaged_checkpoint_preserves_the_last_verified_window() {
    let first = exchange("first", "first request", "first reply", true);
    let second = exchange("second", "second request", "second reply", true);
    let user = |rows: &[Value]| {
        rows.iter()
            .find(|row| row["payload"]["role"] == "user")
            .unwrap()["payload"]
            .clone()
    };
    let checkpoint = |items: Value| json!({"type":"compacted", "payload":{"message":"", "window_number":1, "replacement_history":items}});
    let a = user(&first);
    let b = user(&second);
    let mut rows = [first, second].concat();
    rows.push(checkpoint(json!([a])));
    for (extra, accepted) in [
        (Value::Null, false),
        (json!(42), false),
        (json!({"content":"private checkpoint field"}), false),
        (
            json!({"type":"message", "role":"user", "content":"invalid"}),
            false,
        ),
        (
            json!({"type":"message", "role":"assistant", "content":[{"type":"output_text", "text":"private checkpoint field", "annotations":{}}]}),
            false,
        ),
        (
            json!({"type":"message", "role":"assistant", "content":[{"type":"not_a_content_item"}]}),
            false,
        ),
        (
            json!({"type":"message", "role":"assistant", "content":[], "phase":"unknown"}),
            false,
        ),
        (
            json!({"type":"function_call", "name":"private checkpoint field", "arguments":{}}),
            false,
        ),
        (json!({"type":"reasoning"}), false),
        (json!({"type":"compaction"}), false),
        (
            json!({"type":"future_extension", "id":42, "role":"user", "content":"private checkpoint field"}),
            true,
        ),
        (
            json!({"type":"reasoning", "summary":[], "encrypted_content":"private checkpoint field"}),
            true,
        ),
        (
            json!({"type":"compaction", "encrypted_content":"private checkpoint field"}),
            true,
        ),
    ] {
        let mut records = rows.clone();
        records.push(checkpoint(json!([b, extra])));
        for preview in [false, true] {
            let snapshot =
                read_with_history_mode(records.clone(), preview, Some("paginated")).unwrap();
            let conversation = omnis_core::import_conversation(&snapshot);
            let active: Vec<_> = conversation
                .messages
                .iter()
                .map(|message| message.text.as_str())
                .collect();
            assert_eq!(
                active,
                [if accepted {
                    "second request"
                } else {
                    "first request"
                }],
                "{extra}"
            );
            assert_eq!(conversation.truncated, !accepted);
            let search = omnis_core::trajectory_search_document(&snapshot);
            assert_eq!(search.source_complete, accepted);
            assert!(
                search.text.contains("first request") && search.text.contains("second request")
            );
            assert!(!search.text.contains("private checkpoint field"));
            assert_eq!(
                snapshot.workspace.current_dir,
                std::path::Path::new("/workspace/original")
            );
        }
    }
}

fn assert_checkpoint_selection(key: &str, value: Value, accepted: bool) {
    let rows = exchange("first", "request", "answer", true);
    let checkpoint = |role: &str, number: u64| {
        let item = rows
            .iter()
            .find(|row| row["payload"]["role"] == role)
            .unwrap();
        json!({"type":"compacted","payload":{"message":"","window_number":number,"replacement_history":[item["payload"]]}})
    };
    let mut latest = checkpoint("assistant", 2);
    latest["payload"]
        .as_object_mut()
        .unwrap()
        .insert(key.into(), value);
    let previous = checkpoint("user", 1);
    let mut records = rows;
    records.extend([previous, latest]);
    let snapshot = read_with_history_mode(records, false, Some("paginated")).unwrap();
    let conversation = omnis_core::import_conversation(&snapshot);
    let active: Vec<_> = conversation
        .messages
        .iter()
        .map(|message| message.text.as_str())
        .collect();
    assert_eq!(
        active,
        [if accepted { "answer" } else { "request" }],
        "{key}"
    );
    assert_eq!(conversation.truncated, !accepted, "{key}");
    assert!(
        !omnis_core::trajectory_search_document(&snapshot)
            .text
            .contains("private catalog field")
    );
}

#[test]
fn checkpoint_selection_validates_typed_resume_metadata() {
    let thread = "00000000-0000-4000-8000-000000000001";
    let catalog = |tool: Value| json!({"dynamic_tools":{"thread_id":thread,"tools":[tool]}});
    let function = json!({"type":"function","name":"inspect","description":"private catalog field","inputSchema":null});
    let mut complete =
        catalog(json!({"type":"namespace","name":"test","description":"","tools":[function]}));
    complete["multi_agent_version"] = json!("v1");
    complete["last_started_turn_id"] = json!("first");
    complete["previous_turn_settings"] = json!({"model":"","comp_hash":null,"realtime_active":false,"cyber_access_program":"standard","future":true});
    complete["turn_attribution"] =
        json!({"turn_id":"first","initiating_agent_path":"/root/worker","future":123});
    for (metadata, accepted) in [
        (Value::Null, true),
        (json!({"previous_turn_settings":null}), true),
        (complete, true),
        (
            json!([
                [
                    thread,
                    [["namespace", "test", "", [["function", "inspect", "", null]]]]
                ],
                null,
                null,
                null,
                ["", null, null, false]
            ]),
            true,
        ),
        (json!([null, null, null, null]), false),
        (json!({"previous_turn_settings":{}}), false),
        (json!({"previous_turn_settings":{"model":null}}), false),
        (json!({"previous_turn_settings":{"model":42}}), false),
        (
            json!({"previous_turn_settings":{"model":"","realtime_active":1}}),
            false,
        ),
        (json!({"multi_agent_version":"future"}), false),
        (
            json!({"turn_attribution":{"turn_id":"","initiating_agent_path":"relative"}}),
            false,
        ),
        (
            json!({"dynamic_tools":{"thread_id":"invalid","tools":[]}}),
            false,
        ),
        (
            catalog(json!({"type":"function","name":"missing_schema","description":""})),
            false,
        ),
        (
            catalog(
                json!({"type":"function","name":"bad_flag","description":"","inputSchema":{},"deferLoading":null}),
            ),
            false,
        ),
    ] {
        assert_checkpoint_selection("resume_metadata", metadata, accepted);
    }
}

#[test]
fn checkpoint_selection_validates_private_records_before_using_their_window() {
    let id = "00000000-0000-4000-8000-000000000001";
    let usage = json!({"input_tokens":-1,"cached_input_tokens":0,"output_tokens":1,"reasoning_output_tokens":0,"total_tokens":0});
    let record = json!({"thread_id":id, "turn_id":"t", "session_id":id, "root_turn_id":"r", "response_id":"",
        "usage":usage, "turn_token_usage":usage, "thread_token_usage":usage, "future":true});
    let mut missing_id = record.clone();
    missing_id.as_object_mut().unwrap().remove("response_id");
    let mut invalid_total = record.clone();
    invalid_total["thread_token_usage"]["total_tokens"] = Value::Null;
    let message = json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"private catalog field"}]});
    for (key, value, accepted) in [
        ("latest_token_usage_record", record, true),
        ("latest_token_usage_record", Value::Null, true),
        ("latest_token_usage_record", missing_id, false),
        ("latest_token_usage_record", invalid_total, false),
        ("first_window_id", json!(true), false),
        ("previous_window_id", json!(1), false),
        ("compaction_response_id", json!([]), false),
        ("guardian_history", json!([message]), true),
        ("guardian_history", json!([null]), false),
        (
            "mcp_resource_origins",
            json!({"origins":[],"turns":[]}),
            true,
        ),
        (
            "mcp_resource_origins",
            json!({"origins":[{}],"turns":[]}),
            false,
        ),
        (
            "retained_context",
            json!({"verified_answers":[],"incomplete":false}),
            true,
        ),
        (
            "retained_context",
            json!({"verified_answers":[],"incomplete":false,"user_messages":[{}]}),
            false,
        ),
        (
            "replacement_history_metadata",
            json!([{"guardian_review_ids":["r"],"mcp_attribution":"opaque"}]),
            true,
        ),
        (
            "replacement_history_metadata",
            json!([{"guardian_review_ids":[42]}]),
            false,
        ),
        (
            "replacement_history_metadata",
            json!([{"sender_user_messages":{"receiver_turn_id":"t","text":"private catalog field"}}]),
            false,
        ),
    ] {
        assert_checkpoint_selection(key, value, accepted);
    }
}
#[test]
fn opaque_checkpoint_items_cannot_confirm_legacy_tool_records() {
    let call = json!({"type":"computer_call", "call_id":"legacy", "name":"view", "arguments":"{}"});
    let result =
        json!({"type":"computer_call_output", "call_id":"legacy", "output":"unconfirmed result"});
    let snapshot = read_with_history_mode(vec![
        json!({"type":"response_item", "payload":call}),
        json!({"type":"response_item", "payload":result}),
        json!({"type":"compacted", "payload":{"message":"", "window_number":1, "replacement_history":[call, result]}}),
    ], false, Some("paginated")).unwrap();
    assert_eq!(omnis_core::import_trajectory(&snapshot).tool_events, 0);
    let search = omnis_core::trajectory_search_document(&snapshot);
    assert!(search.source_complete && search.text.contains("unconfirmed result"));
}

#[test]
fn message_only_compaction_rebuilds_users_from_the_selected_checkpoint_and_suffix() {
    let first = exchange("first", "selected request", "previous reply", true);
    let suffix = exchange("second", "subsequent request", "subsequent reply", true);
    let only_displayed = exchange("typed", "display only", "displayed reply", false);
    let selected = first
        .iter()
        .find(|row| row["payload"]["role"] == "user")
        .unwrap();
    let legacy = json!({"type":"compacted","payload":{"replacement_history":null,"message":"Surviving source summary."}});
    for (replacement, keep_first) in [(json!([selected["payload"]]), true), (json!([]), false)] {
        let mut rows = first.clone();
        rows.push(json!({"type":"compacted","payload":{"window_number":1,"message":"","replacement_history":replacement}}));
        rows.push(
            json!({"type":"compacted","payload":{"message":"malformed","replacement_history":{}}}),
        );
        rows.extend(suffix.clone());
        rows.extend(only_displayed.clone());
        rows.push(message("user", "private model-only request"));
        rows.push(legacy.clone());
        rows.extend(exchange(
            "after",
            "next public request",
            "next public answer",
            false,
        ));
        for preview in [false, true] {
            let snapshot =
                read_with_history_mode(rows.clone(), preview, Some("paginated")).unwrap();
            let imported = omnis_core::import_conversation(&snapshot);
            let active: Vec<_> = imported
                .messages
                .iter()
                .map(|item| item.text.as_str())
                .collect();
            assert_eq!(active.contains(&"selected request"), keep_first);
            assert!(
                active.contains(&"subsequent request") && active.contains(&"next public answer")
            );
            assert!(
                active
                    .iter()
                    .any(|text| text.contains("Surviving source summary."))
            );
            for absent in [
                "previous reply",
                "subsequent reply",
                "display only",
                "private model-only request",
                "malformed",
            ] {
                assert!(!active.contains(&absent), "{absent} was active");
            }
            let mut recovery = Vec::new();
            omnis_core::write_recovery_handoff(&snapshot, &mut recovery).unwrap();
            let recovery = String::from_utf8(recovery).unwrap();
            assert!(recovery.contains("previous reply") && recovery.contains("display only"));
            assert!(!recovery.contains("private model-only request"));
        }
    }
}

#[test]
fn consecutive_message_only_compactions_keep_a_retained_summary_in_its_source_role() {
    use omnis_core::HandoffRole::{Assistant, User};

    let compaction = |text: &str| json!({"type":"compacted","payload":{"message":text}});
    let mut rows = exchange("first", "current request", "previous answer", true);
    rows.push(compaction("First summary."));
    rows.extend(exchange(
        "second",
        "follow-up request",
        "earlier follow-up reply",
        true,
    ));
    rows.push(compaction("Latest summary."));
    for rolled in [false, true] {
        let mut records = rows.clone();
        if rolled {
            records.push(event("thread_rolled_back", json!({"num_turns":1})));
        }
        let snapshot = read_with_history_mode(records, false, Some("paginated")).unwrap();
        let imported = omnis_core::import_conversation(&snapshot);
        let texts: Vec<_> = imported
            .messages
            .iter()
            .filter(|item| !item.text.contains("Codex replaced"))
            .map(|item| (item.role, item.text.lines().last().unwrap()))
            .collect();
        let mut expected = vec![
            (User, "current request"),
            (Assistant, "First summary."),
            (User, "follow-up request"),
        ];
        if !rolled {
            expected.push((Assistant, "Latest summary."));
        }
        assert_eq!(texts, expected);
    }
}

#[test]
fn a_reselected_original_keeps_only_surviving_legacy_summaries() {
    let original = exchange("first", "original request", "older answer", true);
    for (first_summary, retained) in [
        ("First source summary.", true),
        (
            "<environment_context>First source summary.</environment_context>",
            false,
        ),
    ] {
        let mut records = original.clone();
        records.push(json!({"type":"compacted","payload":{"window_number":1,"message":"","replacement_history":[]}}));
        for summary in [first_summary, "Latest summary."] {
            records.push(json!({"type":"compacted","payload":{"message":summary}}));
        }
        for count in 1..=3 {
            records.push(event("thread_rolled_back", json!({"num_turns":1})));
            let snapshot =
                read_with_history_mode(records.clone(), false, Some("paginated")).unwrap();
            let imported = omnis_core::import_conversation(&snapshot);
            let active: Vec<_> = imported
                .messages
                .iter()
                .map(|item| item.text.as_str())
                .collect();
            assert_eq!(
                active.contains(&"original request"),
                count <= if retained { 2 } else { 1 }
            );
            assert_eq!(
                active
                    .iter()
                    .any(|text| text.contains("First source summary.")),
                retained && count == 1
            );
            assert!(
                !active
                    .iter()
                    .any(|text| text.contains("older answer") || text.contains("Latest summary."))
            );
        }
    }
}
