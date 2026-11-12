use super::*;

fn model_result(output: &str) -> Value {
    json!({"type":"response_item","payload":{"type":"function_call_output","call_id":"cmd","output":output}})
}

fn command(turn: &str, status: &str) -> Value {
    turn_item(
        turn,
        &json!({"type":"CommandExecution","id":"cmd","command":["example"],"cwd":"file:///workspace","parsed_cmd":[],"source":"agent","aggregated_output":"display result","status":status,"exit_code":0}),
    )
}

fn results(snapshot: &CanonicalSnapshot) -> Vec<(&EventKind, &str)> {
    snapshot
        .events
        .iter()
        .filter_map(|event| Some((&event.kind, event.payload["output"].as_str()?)))
        .collect()
}

#[test]
fn model_result_before_a_generated_turn_survives_a_repeated_display_completion() {
    let rows = [
        model_result("native output"),
        event("task_started", json!({"turn_id":"rollout-2"})),
        command("rollout-2", "completed"),
        command("rollout-2", "completed"),
    ];
    for preview in [false, true] {
        for end in [3, 4] {
            let snapshot = read_records_with_preview(rows[..end].to_vec(), preview).unwrap();
            assert_eq!(
                results(&snapshot),
                [(&EventKind::ToolCompleted, "native output")]
            );
        }
    }
}

#[test]
fn a_rollback_keeps_the_published_command_and_only_its_admitted_inputs_model_result() {
    let turn = "rollout-4";
    let mut first = message("user", "first");
    first["payload"]["id"] = "u1".into();
    first["payload"]["internal_chat_message_metadata_passthrough"] = json!({"turn_id": turn});
    for same_input in [false, true] {
        for repeat in [false, true] {
            let mut rows = vec![
                event("task_started", json!({"turn_id": turn})),
                first.clone(),
                page_user(turn, "u1", "first"),
                command(turn, "completed"),
            ];
            let second = message("user", "second");
            if !same_input {
                rows.push(second.clone());
            }
            rows.extend([call(None), model_result("model reply")]);
            if same_input {
                rows.push(second);
            }
            if repeat {
                rows.push(command(turn, "completed"));
            }
            rows.push(event("thread_rolled_back", json!({"num_turns": 1})));
            for preview in [false, true] {
                let snapshot = read_records_with_preview(rows.clone(), preview).unwrap();
                assert_eq!(visible_text(&snapshot), ["first"]);
                let (kind, output, source) = if same_input {
                    (
                        EventKind::ToolCompleted,
                        "model reply",
                        "response_item.function_call_output",
                    )
                } else {
                    (
                        EventKind::CommandExecuted,
                        "display result",
                        "event_msg.item_completed",
                    )
                };
                assert_eq!(results(&snapshot), [(&kind, output)]);
                assert_eq!(
                    snapshot
                        .events
                        .iter()
                        .find(|e| e.kind == kind)
                        .unwrap()
                        .source
                        .raw_record_type
                        .as_deref(),
                    Some(source)
                );
                assert_eq!(
                    snapshot
                        .events
                        .iter()
                        .filter(|e| e.kind == EventKind::ToolCalled)
                        .count(),
                    usize::from(same_input)
                );
            }
        }
    }
}

#[test]
fn corrected_display_status_keeps_model_output_and_reused_call_ids_are_separate() {
    let rows = vec![
        event("task_started", json!({"turn_id":"first"})),
        page_user("first", "request", "first request"),
        command("first", "completed"),
        model_result("first output"),
        command("first", "failed"),
        event("task_complete", json!({"turn_id":"first"})),
        event("task_started", json!({"turn_id":"second"})),
        page_user("second", "request", "second request"),
        model_result("second output"),
        command("second", "failed"),
        command("second", "completed"),
    ];
    for preview in [false, true] {
        let snapshot = read_records_with_preview(rows.clone(), preview).unwrap();
        assert_eq!(
            results(&snapshot),
            [
                (&EventKind::ToolFailed, "first output"),
                (&EventKind::ToolCompleted, "second output"),
            ]
        );
        let mut rolled_back = rows.clone();
        rolled_back.push(event("thread_rolled_back", json!({"num_turns":1})));
        let snapshot = read_records_with_preview(rolled_back, preview).unwrap();
        assert_eq!(
            results(&snapshot),
            [(&EventKind::ToolFailed, "first output")]
        );
    }
}

#[test]
fn late_tool_mirrors_keep_their_named_owner_in_either_order() {
    let mut raw = model_result("native output");
    raw["payload"]["internal_chat_message_metadata_passthrough"] = json!({"turn_id":"first"});
    let display = command("first", "completed");
    for (early, late) in [(&raw, &display), (&display, &raw)] {
        let mut rows = vec![
            event("task_started", json!({"turn_id":"first"})),
            page_user("first", "u1", "keep"),
            early.clone(),
            event("task_started", json!({"turn_id":"second"})),
            late.clone(),
            page_user("second", "u2", "discard"),
        ];
        for _ in 0..2 {
            for preview in [false, true] {
                let snapshot = read_records_with_preview(rows.clone(), preview).unwrap();
                assert_eq!(
                    results(&snapshot),
                    [(&EventKind::ToolCompleted, "native output")]
                );
            }
            rows.push(event("thread_rolled_back", json!({"num_turns":1})));
        }
    }
}

#[test]
fn an_unstamped_model_result_before_a_new_generated_turn_rolls_back_with_its_display() {
    let mut rows = vec![
        call(None),
        model_result("old output"),
        event("task_started", json!({"turn_id":"rollout-2"})),
        page_user("rollout-2", "u1", "first request"),
        command("rollout-2", "completed"),
        call(None),
        model_result("new output"),
        event("task_complete", json!({"turn_id":"rollout-2"})),
        event("task_started", json!({"turn_id":"rollout-5"})),
        page_user("rollout-5", "u2", "second request"),
        command("rollout-5", "completed"),
    ];
    let both = [
        (&EventKind::ToolCompleted, "old output"),
        (&EventKind::ToolCompleted, "new output"),
    ];
    for expected in [&both[..], &both[..1], &both[..0]] {
        for preview in [false, true] {
            let snapshot = read_records_with_preview(rows.clone(), preview).unwrap();
            assert_eq!(results(&snapshot), expected);
        }
        rows.push(event("thread_rolled_back", json!({"num_turns":1})));
    }
}

fn call(turn: Option<&str>) -> Value {
    let mut result = json!({"type":"response_item","payload":{"type":"function_call","call_id":"cmd","name":"example","arguments":"{}"}});
    if let Some(turn) = turn {
        result["payload"]["internal_chat_message_metadata_passthrough"] = json!({"turn_id":turn});
    }
    result
}

#[test]
fn a_late_native_call_and_its_unstamped_output_keep_the_calling_turn() {
    let first_call = call(None);
    let late_call = call(Some("first"));
    for after_start in [false, true] {
        let mut rows = vec![
            event("task_started", json!({"turn_id":"first"})),
            page_user("first", "u1", "keep"),
        ];
        if !after_start {
            rows.push(first_call.clone());
        }
        rows.push(event("task_started", json!({"turn_id":"second"})));
        rows.push(page_user("second", "u2", "discard"));
        if after_start {
            rows.push(late_call.clone());
        }
        rows.extend([
            model_result("original output"),
            command("first", "completed"),
            event("thread_rolled_back", json!({"num_turns":1})),
        ]);
        for preview in [false, true] {
            let snap = read_records_with_preview(rows.clone(), preview).unwrap();
            assert_eq!(
                results(&snap),
                [(&EventKind::ToolCompleted, "original output")]
            );
            assert_eq!(
                snap.events
                    .iter()
                    .filter(|event| event.kind == EventKind::ToolCalled)
                    .count(),
                1
            );
            assert_eq!(visible_text(&snap), ["keep"]);
        }
    }
}

#[test]
fn generated_display_turns_claim_pending_outputs_in_occurrence_order() {
    let mut rows = vec![
        model_result("model first"),
        model_result("model second"),
        event("task_started", json!({"turn_id":"rollout-2"})),
        page_user("rollout-2", "u1", "first"),
        command("rollout-2", "failed"),
        command("rollout-2", "failed"),
        event("task_complete", json!({"turn_id":"rollout-2"})),
        event("task_started", json!({"turn_id":"rollout-5"})),
        page_user("rollout-5", "u2", "second"),
        command("rollout-5", "completed"),
    ];
    let both = [
        (&EventKind::ToolFailed, "model first"),
        (&EventKind::ToolCompleted, "model second"),
    ];
    for expected in [&both[..], &both[..1], &both[..0]] {
        for preview in [false, true] {
            let snap = read_records_with_preview(rows.clone(), preview).unwrap();
            assert_eq!(results(&snap), expected);
        }
        rows.push(event("thread_rolled_back", json!({"num_turns":1})));
    }
}

#[test]
fn generated_display_turns_identify_the_unstamped_call_and_result_together() {
    let mut rows = vec![
        call(None),
        model_result("old"),
        call(None),
        model_result("new"),
        event("task_started", json!({"turn_id":"rollout-5"})),
        page_user("rollout-5", "u1", "keep"),
        command("rollout-5", "completed"),
        event("task_complete", json!({"turn_id":"rollout-5"})),
        event("task_started", json!({"turn_id":"rollout-7"})),
        page_user("rollout-7", "u2", "discard"),
        command("rollout-7", "failed"),
    ];
    let both = [
        (&EventKind::ToolCompleted, "old"),
        (&EventKind::ToolFailed, "new"),
    ];
    for expected in [&both[..], &both[..1], &both[..0]] {
        for preview in [false, true] {
            let snap = read_records_with_preview(rows.clone(), preview).unwrap();
            assert_eq!(results(&snap), expected);
            assert_eq!(
                snap.events
                    .iter()
                    .filter(|event| event.kind == EventKind::ToolCalled)
                    .count(),
                expected.len()
            );
        }
        rows.push(event("thread_rolled_back", json!({"num_turns":1})));
    }
}

#[test]
fn conflicting_explicit_turns_cannot_share_a_tool_result() {
    let mut raw = model_result("second model");
    raw["payload"]["internal_chat_message_metadata_passthrough"] = json!({"turn_id":"second"});
    for preview in [false, true] {
        let snap = read_records_with_preview(
            vec![
                raw.clone(),
                command("first", "failed"),
                command("second", "completed"),
            ],
            preview,
        )
        .unwrap();
        assert_eq!(
            results(&snap),
            [
                (&EventKind::ToolCompleted, "second model"),
                (&EventKind::ToolFailed, "display result")
            ]
        );
    }
}

#[test]
fn a_retained_generated_turn_keeps_its_original_call_owner_after_rollback() {
    let rows = vec![
        json!({"type":"turn_context","payload":{"turn_id":"original","cwd":"/workspace"}}),
        message("user", "keep"),
        event("task_started", json!({"turn_id":"rollout-3"})),
        page_user("rollout-3", "item-1", "keep"),
        event("task_started", json!({"turn_id":"second"})),
        page_user("second", "u2", "discard"),
        event("thread_rolled_back", json!({"num_turns":1})),
        event("task_started", json!({"turn_id":"third"})),
        page_user("third", "u3", "also discard"),
        call(Some("original")),
        model_result("original result"),
        command("original", "completed"),
        event("thread_rolled_back", json!({"num_turns":1})),
    ];
    for preview in [false, true] {
        let snapshot = read_records_with_preview(rows.clone(), preview).unwrap();
        assert_eq!(visible_text(&snapshot), ["keep"]);
        assert_eq!(
            results(&snapshot),
            [(&EventKind::ToolCompleted, "original result")]
        );
        assert_eq!(
            snapshot
                .events
                .iter()
                .filter(|event| event.kind == EventKind::ToolCalled)
                .count(),
            1
        );
    }
}

#[test]
fn named_model_results_follow_their_generated_turn_when_displays_arrive_out_of_order() {
    let mut rows = Vec::new();
    for (id, text) in [("a", "first"), ("b", "second")] {
        rows.push(json!({"type":"turn_context","payload":{"turn_id":id,"cwd":"/workspace"}}));
        rows.push(message("user", text));
        rows.push(model_result(text));
    }
    for (turn, text, status) in [
        ("rollout-8", "second", "failed"),
        ("rollout-10", "first", "completed"),
    ] {
        rows.push(event("task_started", json!({"turn_id":turn})));
        rows.push(page_user(turn, "item-1", text));
        rows.push(command(turn, status));
        rows.push(event("task_complete", json!({"turn_id":turn})));
    }
    for preview in [false, true] {
        let snap = read_records_with_preview(rows.clone(), preview).unwrap();
        assert_eq!(
            results(&snap),
            [
                (&EventKind::ToolCompleted, "first"),
                (&EventKind::ToolFailed, "second")
            ]
        );
    }
}

#[test]
fn a_generated_call_keeps_the_input_that_issued_it_when_its_unstamped_result_arrives_late() {
    let mut rows = vec![
        event("task_started", json!({"turn_id":"rollout-2"})),
        page_user("rollout-2", "u1", "keep"),
        call(None),
        event("task_complete", json!({"turn_id":"rollout-2"})),
        event("task_started", json!({"turn_id":"second"})),
        page_user("second", "u2", "discard"),
        model_result("first native result"),
        command("rollout-2", "failed"),
        event("thread_rolled_back", json!({"num_turns":1})),
    ];
    for expected in [true, false] {
        for preview in [false, true] {
            let snap = read_records_with_preview(rows.clone(), preview).unwrap();
            let calls = snap
                .events
                .iter()
                .filter(|item| item.kind == EventKind::ToolCalled)
                .count();
            assert_eq!(calls, usize::from(expected));
            assert_eq!(
                results(&snap),
                if expected {
                    vec![(&EventKind::ToolFailed, "first native result")]
                } else {
                    vec![]
                }
            );
        }
        rows.push(event("thread_rolled_back", json!({"num_turns":1})));
    }
}

#[test]
fn repeating_a_command_completion_after_a_steer_keeps_the_original_owner() {
    for repeat in [false, true] {
        let mut rows = vec![
            event("task_started", json!({"turn_id":"named"})),
            page_user("named", "u1", "first"),
            call(None),
            model_result("first result"),
            command("named", "completed"),
            page_user("named", "u2", "second"),
        ];
        if repeat {
            rows.push(command("named", "completed"));
        }
        rows.push(event("thread_rolled_back", json!({"num_turns":1})));
        for preview in [false, true] {
            let snap = read_records_with_preview(rows.clone(), preview).unwrap();
            assert_eq!(visible_text(&snap), ["first"]);
            assert_eq!(
                results(&snap),
                [(&EventKind::ToolCompleted, "first result")]
            );
        }
    }
}

#[test]
fn original_and_generated_turn_ids_pair_named_calls_and_replies_in_either_order() {
    let mut assistant = message("assistant", "answer");
    assistant["payload"]["internal_chat_message_metadata_passthrough"] =
        json!({"turn_id":"original"});
    let shown = turn_item(
        "rollout-3",
        &json!({"type":"AgentMessage","id":"item-2","content":[{"type":"Text","text":"answer"}]}),
    );
    let mut raw = model_result("native");
    raw["payload"]["internal_chat_message_metadata_passthrough"] = json!({"turn_id":"original"});
    let display = command("rollout-3", "failed");
    for display_first in [false, true] {
        let mut rows = vec![
            json!({"type":"turn_context","payload":{"turn_id":"original","cwd":"/workspace"}}),
            message("user", "first"),
            event("task_started", json!({"turn_id":"rollout-3"})),
            page_user("rollout-3", "item-1", "first"),
        ];
        rows.extend(if display_first {
            [
                shown.clone(),
                assistant.clone(),
                display.clone(),
                raw.clone(),
            ]
        } else {
            [
                assistant.clone(),
                shown.clone(),
                raw.clone(),
                display.clone(),
            ]
        });
        for preview in [false, true] {
            let snap = read_records_with_preview(rows.clone(), preview).unwrap();
            assert_eq!(visible_text(&snap), ["first", "answer"]);
            assert_eq!(results(&snap), [(&EventKind::ToolFailed, "native")]);
        }
    }
}

#[test]
fn rolling_back_a_steer_restores_the_previous_legacy_tool_status() {
    let mut rows = vec![
        event("task_started", json!({"turn_id":"named"})),
        page_user("named", "u1", "first"),
        call(None),
        model_result("model receipt"),
        command("named", "failed"),
        page_user("named", "u2", "second"),
        command("named", "completed"),
    ];
    let rollback = event("thread_rolled_back", json!({"num_turns":1}));
    for preview in [false, true] {
        let mut rolled = rows.clone();
        rolled.push(rollback.clone());
        for (mode, kind) in [
            (None, EventKind::ToolFailed),
            (Some("paginated"), EventKind::ToolCompleted),
        ] {
            let snapshot = read_with_history_mode(rolled.clone(), preview, mode).unwrap();
            assert_eq!(results(&snapshot), [(&kind, "model receipt")]);
        }
    }
    rows.extend([
        page_user("named", "u3", "third"),
        command("named", "failed"),
        rollback.clone(),
    ]);
    assert_eq!(
        results(&read_records(rows.clone()).unwrap()),
        [(&EventKind::ToolCompleted, "model receipt")]
    );
    rows.push(rollback);
    assert_eq!(
        results(&read_records(rows).unwrap()),
        [(&EventKind::ToolFailed, "model receipt")]
    );
}

#[test]
fn legacy_display_revisions_keep_the_earlier_payload_or_a_later_model_receipt() {
    let mut correction = command("named", "completed");
    correction["payload"]["item"]["aggregated_output"] = json!("revised display");
    for include_model in [false, true] {
        let mut rows = vec![
            event("task_started", json!({"turn_id":"named"})),
            page_user("named", "u1", "first"),
            call(None),
            command("named", "failed"),
            page_user("named", "u2", "second"),
            correction.clone(),
        ];
        if include_model {
            rows.push(model_result("model receipt"));
        }
        rows.push(event("thread_rolled_back", json!({"num_turns":1})));
        let snapshot = read_records(rows).unwrap();
        let expected = if include_model {
            "model receipt"
        } else {
            "display result"
        };
        assert_eq!(results(&snapshot), [(&EventKind::ToolFailed, expected)]);
    }
}

#[test]
fn checkpoint_tools_require_the_same_call_and_result_and_follow_the_call_cutoff() {
    let turn = "original";
    let mut user = message("user", "first request");
    user["payload"]["id"] = "first".into();
    user["payload"]["internal_chat_message_metadata_passthrough"] = json!({"turn_id": turn});
    let call = json!({"type":"function_call", "call_id":"call-a", "name":"lookup", "arguments":"{\"count\":1}"});
    let result =
        json!({"type":"function_call_output", "call_id":"call-a", "output":"published result"});
    let mut other_call = call.clone();
    other_call["arguments"] = "{\"count\":2}".into();
    let mut other_result = result.clone();
    other_result["output"] = "replacement-only result".into();
    let published = [
        event("task_started", json!({"turn_id":turn})),
        user.clone(),
        page_user(turn, "first", "first request"),
        json!({"type":"response_item", "payload":call}),
        json!({"type":"response_item", "payload":result}),
        turn_item(
            turn,
            &json!({"type":"FunctionCallOutput", "id":"call-a", "name":"lookup", "output":"published result"}),
        ),
    ];
    // The result precedes the steer on disk. With matching acceptance orders its call still withdraws it.
    for (items, rollback, expected_calls, expected_results) in [
        (vec![call.clone(), result.clone()], false, 1, 1),
        (vec![call.clone(), other_result], false, 1, 0),
        (vec![other_call, result.clone()], false, 0, 0),
        (vec![result.clone()], false, 0, 0),
        (vec![call.clone(), result], true, 0, 0),
    ] {
        let mut retained = vec![user["payload"].clone()];
        retained.extend(items);
        let mut metadata = vec![json!({}); retained.len()];
        if rollback {
            metadata[1] = json!({"user_input_order":2});
            retained.push(message("user", "queued request")["payload"].clone());
            metadata.push(json!({"user_input_order":2}));
        }
        let mut rows = published.to_vec();
        rows.push(
            json!({"type":"compacted", "payload":{"message":"", "window_number":2,
            "replacement_history":retained, "replacement_history_metadata":metadata}}),
        );
        if rollback {
            rows.push(event("thread_rolled_back", json!({"num_turns":1})));
        }
        for preview in [false, true] {
            let snapshot =
                read_with_history_mode(rows.clone(), preview, Some("paginated")).unwrap();
            let markdown = omnis_core::render_markdown_export(&snapshot);
            let active = markdown.split("## Previous Codex display").next().unwrap();
            assert_eq!(active.matches("### Tool call").count(), expected_calls);
            assert_eq!(active.matches("### Tool result").count(), expected_results);
            assert!(!markdown.contains("replacement-only result"));
            let import = omnis_core::import_trajectory(&snapshot);
            let structured = import
                .items
                .iter()
                .filter(|item| item.tool.is_some())
                .count();
            assert_eq!(structured, if expected_results == 1 { 2 } else { 0 });
        }
    }
}

#[test]
fn checkpoint_tool_results_with_a_reused_call_id_follow_the_scoped_call() {
    let call = |turn: &str| json!({"type":"function_call", "call_id":"reuse", "name":"lookup", "arguments":"{}", "internal_chat_message_metadata_passthrough":{"turn_id":turn}});
    let result = json!({"type":"function_call_output", "call_id":"reuse", "output":"same result"});
    let mut records = Vec::new();
    for turn in ["first", "second"] {
        records.extend([
            event("task_started", json!({"turn_id":turn})),
            json!({"type":"response_item","payload":call(turn)}),
            json!({"type":"response_item","payload":result}),
            event("task_complete", json!({"turn_id":turn})),
        ]);
    }
    for (stamp, expected) in [(true, [true, true, false, false]), (false, [false; 4])] {
        let mut retained = call("first");
        if !stamp {
            retained
                .as_object_mut()
                .unwrap()
                .remove("internal_chat_message_metadata_passthrough");
        }
        let mut rows = records.clone();
        rows.push(json!({"type":"compacted","payload":{"message":"", "window_number":1, "replacement_history":[retained,result]}}));
        let snapshot = read_with_history_mode(rows, false, Some("paginated")).unwrap();
        let active: Vec<_> = snapshot
            .events
            .iter()
            .filter(|e| matches!(e.kind, EventKind::ToolCalled | EventKind::ToolCompleted))
            .map(|e| e.payload.get("omnisession_codex_checkpoint").is_none())
            .collect();
        assert_eq!(active, expected, "checkpoint has turn stamp: {stamp}");
    }
}

#[test]
fn checkpoint_retains_only_a_published_client_authored_standalone_receipt() {
    let item = json!({"type":"function_call_output", "id":"fco_observation", "name":"observation", "namespace":"fixture", "output":"published receipt"});
    let author = json!({"client_authored":true});
    let model = |item: &Value, metadata: &Value| json!({"type":"response_item", "payload":item, "metadata":metadata});
    let public = |item: &Value| {
        let mut typed = item.clone();
        typed["type"] = "FunctionCallOutput".into();
        turn_item("source", &typed)
    };
    let raw = model(&item, &author);
    let shown = public(&item);
    let published = vec![raw.clone(), shown.clone()];
    let retained = vec![item.clone()];
    let metadata = vec![author.clone()];
    let mut cases = vec![
        (published.clone(), retained.clone(), metadata.clone(), true),
        (
            vec![
                raw.clone(),
                shown.clone(),
                turn_item(
                    "source",
                    &json!({"type":"ImageView","id":"fco_observation","path":"file:///fixture/revision"}),
                ),
                shown.clone(),
            ],
            retained.clone(),
            metadata.clone(),
            true,
        ),
        (
            vec![shown.clone()],
            retained.clone(),
            metadata.clone(),
            false,
        ),
        (vec![raw.clone()], retained.clone(), metadata.clone(), false),
        (
            vec![model(&item, &Value::Null), shown.clone()],
            retained.clone(),
            metadata.clone(),
            false,
        ),
        (published.clone(), retained.clone(), vec![json!({})], false),
        (
            vec![raw.clone(), raw.clone(), shown.clone()],
            retained.clone(),
            metadata.clone(),
            false,
        ),
        (
            published.clone(),
            vec![item.clone(), item.clone()],
            vec![author.clone(); 2],
            false,
        ),
    ];
    for (field, value) in [
        ("id", "fco_other"),
        ("name", "changed"),
        ("namespace", "different"),
        ("output", "private replacement"),
    ] {
        let mut other = item.clone();
        other[field] = value.into();
        cases.push((
            published.clone(),
            vec![other.clone()],
            metadata.clone(),
            false,
        ));
        cases.push((
            vec![raw.clone(), public(&other)],
            retained.clone(),
            metadata.clone(),
            false,
        ));
    }
    for (original, copies, metadata, expected) in cases {
        let mut rows = vec![event("task_started", json!({"turn_id":"source"}))];
        rows.extend(original);
        rows.push(json!({"type":"compacted", "payload":{"window_number":1, "message":"", "replacement_history":copies, "replacement_history_metadata":metadata}}));
        for preview in [false, true] {
            let snapshot =
                read_with_history_mode(rows.clone(), preview, Some("paginated")).unwrap();
            let trajectory = omnis_core::import_trajectory(&snapshot);
            assert_eq!(trajectory.tool_events, usize::from(expected), "{rows:?}");
            assert!(trajectory.items.iter().all(|item| item.tool.is_none()));
            let markdown = omnis_core::render_markdown_export(&snapshot);
            let active = markdown.split("## Previous Codex display").next().unwrap();
            assert_eq!(active.contains("published receipt"), expected);
            assert!(!active.contains("private replacement"));
        }
    }
}

#[test]
fn a_malformed_function_result_cannot_replace_the_previous_output_or_block_its_repair() {
    let displayed = |ordinal: u64, output: Value| {
        let mut record = turn_item(
            "turn",
            &json!({"type":"FunctionCallOutput","id":"result","name":"lookup","output":output}),
        );
        record["timestamp"] = "2026-01-05T09:00:00.000Z".into();
        record["ordinal"] = ordinal.into();
        record
    };
    for bad in [
        json!({"type":"input_text"}),
        json!({"type":"input_image","file_id":"file","detail":"medium"}),
        json!({"type":"input_audio","file_id":"file"}),
        json!({"type":"encrypted_content","encrypted_content":[1]}),
    ] {
        let mut rows = vec![
            displayed(1, "first result".into()),
            displayed(
                2,
                json!([{"type":"input_text","text":"unpublished fragment"},bad]),
            ),
        ];
        for expected in [json!("first result"), json!(["repaired result"])] {
            let snap = read_with_history_mode(rows.clone(), false, Some("paginated")).unwrap();
            let outputs = snap
                .events
                .iter()
                .filter(|e| e.payload["id"] == "result")
                .map(|e| &e.payload["output"])
                .collect::<Vec<_>>();
            assert_eq!(outputs, [&expected]);
            assert!(
                snap.events
                    .iter()
                    .any(|e| e.payload["native_history_unverified"] == true)
            );
            assert!(
                !serde_json::to_string(&snap)
                    .unwrap()
                    .contains("unpublished fragment")
            );
            rows.push(displayed(2, json!([["input_text", "repaired result"]])));
        }
    }
}

#[test]
fn raw_function_results_retain_visible_text_and_omit_private_media() {
    let mut raw = model_result("unused");
    raw["payload"]["output"] = json!([
        [0,"visible text"],
        {"type":"input_image","file_id":"hidden-image"},
        [2,"hidden-audio"],
        [3,"hidden-ciphertext"]
    ]);
    for mode in [None, Some("paginated")] {
        let snap = read_with_history_mode(vec![raw.clone()], false, mode).unwrap();
        let public = serde_json::to_string(&snap).unwrap();
        assert!(public.contains("visible text"));
        assert!(
            !["hidden-image", "hidden-audio", "hidden-ciphertext"]
                .iter()
                .any(|text| public.contains(text))
        );
    }
}

#[test]
fn command_revisions_require_a_decodable_native_item() {
    let original = command("turn", "completed");
    let mut damaged = original["payload"]["item"].clone();
    damaged["type"] = json!(6);
    damaged["status"] = json!("unknown_status");
    damaged["aggregated_output"] = json!("unpublished output");
    let mut bad_path = damaged.clone();
    bad_path["type"] = json!("CommandExecution");
    bad_path["status"] = json!("completed");
    bad_path["cwd"] = json!("file:///workspace?query");
    let mut bad_parse = bad_path.clone();
    bad_parse["cwd"] = json!("file:///workspace");
    bad_parse["parsed_cmd"] = json!([{"type":"read","cmd":"example","name":"example"}]);
    for invalid in [damaged, bad_path, bad_parse] {
        let mut rows = vec![original.clone(), turn_item("turn", &invalid)];
        for expected in ["display result", "verified revision"] {
            for preview in [false, true] {
                let snap =
                    read_with_history_mode(rows.clone(), preview, Some("paginated")).unwrap();
                assert_eq!(results(&snap), [(&EventKind::CommandExecuted, expected)]);
                let search = omnis_core::trajectory_search_document(&snap);
                assert!(!search.source_complete && search.text.contains(expected));
                let trajectory = omnis_core::import_trajectory(&snap);
                assert!(
                    trajectory
                        .items
                        .iter()
                        .any(|item| item.text.contains(expected))
                );
                assert!(!search.text.contains("unpublished output"));
                assert!(
                    trajectory
                        .items
                        .iter()
                        .all(|item| !item.text.contains("unpublished output"))
                );
            }
            let mut repair = turn_item(
                "turn",
                &json!([6,"cmd",null,null,null,["example"],"file:///workspace",[[3,"example"]],{"agent":{}},null,{"completed":{}},"verified revision",0,{"secs":1,"nanos":0}]),
            );
            repair["timestamp"] = json!("2026-01-05T09:00:00.000Z");
            repair["ordinal"] = json!(2);
            rows.push(repair);
        }
    }
}

#[test]
fn a_tool_revision_must_decode_the_status_of_its_native_item() {
    for (tag, initial, invalid) in [
        (7, json!({"in_progress":{}}), "inProgress"),
        (17, json!({"inProgress":{}}), "in_progress"),
    ] {
        let item = |status, text| {
            json!({"type":tag, "id":"tool", "server":"fixture",
            "tool":"lookup", "arguments":{"input":text}, "status":status})
        };
        let records = vec![
            turn_item("turn", &item(initial, "published argument")),
            turn_item("turn", &item(json!(invalid), "rejected argument")),
        ];
        for preview in [false, true] {
            let snap = read_with_history_mode(records.clone(), preview, Some("paginated")).unwrap();
            assert!(
                snap.events
                    .iter()
                    .any(|event| event.kind == EventKind::ToolCalled)
            );
            let search = omnis_core::trajectory_search_document(&snap);
            assert!(!search.source_complete && search.text.contains("published argument"));
            assert!(!search.text.contains("rejected argument"));
        }
    }
}

#[test]
fn damaged_dynamic_tool_revisions_leave_the_published_output_and_ordinal_available() {
    let original = json!({"type":"DynamicToolCall","id":"call","tool":"lookup","arguments":{},
        "status":"completed","success":true,"content_items":[{"type":"inputText","text":"verified receipt"}]});
    let mut bad = original.clone();
    bad["success"] = json!("yes");
    bad["content_items"][0]["text"] = json!("unpublished replacement");
    let mut prefix = vec![completed(&original), completed(&bad)];
    for expected in ["verified receipt", "repaired receipt"] {
        for preview in [false, true] {
            let snap = read_with_history_mode(prefix.clone(), preview, Some("paginated")).unwrap();
            let search = omnis_core::trajectory_search_document(&snap);
            assert!(
                search.text.contains(expected)
                    && !search.text.contains("unpublished replacement")
                    && !search.source_complete
            );
            let transfer = omnis_core::import_trajectory(&snap);
            assert!(
                transfer
                    .items
                    .iter()
                    .any(|item| item.text.contains(expected))
            );
            assert!(
                transfer
                    .items
                    .iter()
                    .all(|item| !item.text.contains("unpublished replacement"))
            );
        }
        let mut repair = completed(
            &json!([7,"call",null,"lookup",{},{"completed":{}},[[0,"repaired receipt"]],true]),
        );
        repair["timestamp"] = json!("2026-01-05T09:00:00.000Z");
        repair["ordinal"] = json!(2);
        prefix.push(repair);
    }
}

#[test]
fn documentary_tool_revisions_validate_private_fields_before_publishing_output() {
    let changes = json!({"example.rs":[0,"kept patch"]});
    for (item, field, bad) in [
        (
            json!({"type":"FileChange","id":"patch","changes":changes,"stdout":"kept patch"}),
            "auto_approved",
            json!(0),
        ),
        (
            json!({"type":"McpToolCall","id":"mcp","server":"fixture","tool":"lookup","arguments":"kept arguments","status":"completed","result":[[],{},false]}),
            "result",
            json!({"content":"bad array"}),
        ),
        (
            json!({"type":"CollabAgentToolCall","id":"collab","tool":{"send_input":{}},"status":"completed","sender_thread_id":SESSION_ID,"receiver_agents":[[SESSION_ID]]}),
            "receiver_agents",
            json!([{"thread_id":"invalid"}]),
        ),
    ] {
        let mut invalid = item.clone();
        invalid[field] = bad;
        invalid["status"] = json!("failed");
        let snapshot = read_with_history_mode(
            vec![completed(&item), completed(&invalid)],
            false,
            Some("paginated"),
        )
        .unwrap();
        assert!(
            snapshot
                .events
                .iter()
                .any(|event| event.kind == EventKind::ToolCompleted
                    && event.payload["id"] == item["id"]),
            "{item}"
        );
        assert!(
            !snapshot
                .events
                .iter()
                .any(|event| event.kind == EventKind::ToolFailed)
        );
        assert!(!omnis_core::trajectory_search_document(&snapshot).source_complete);
    }
}

#[test]
fn an_invalid_search_cannot_replace_a_command_or_take_its_repair_ordinal() {
    let mut original = command("turn", "completed");
    original["payload"]["item"]["id"] = json!("");
    let bad = turn_item(
        "turn",
        &json!({"type":"WebSearch","id":"","query":"unpublished query","action":false}),
    );
    let mut rows = vec![original.clone(), bad];
    for expected in ["display result", "repaired query"] {
        for preview in [false, true] {
            let snap = read_with_history_mode(rows.clone(), preview, Some("paginated")).unwrap();
            let search = omnis_core::trajectory_search_document(&snap);
            assert!(
                search.text.contains(expected)
                    && !search.text.contains("unpublished query")
                    && !search.source_complete
            );
            let trajectory = omnis_core::import_trajectory(&snap);
            assert!(
                trajectory
                    .items
                    .iter()
                    .any(|item| item.text.contains(expected))
            );
            assert!(
                trajectory
                    .items
                    .iter()
                    .all(|item| !item.text.contains("unpublished query"))
            );
        }
        let mut repaired = turn_item(
            "turn",
            &json!([10, "", "repaired query", [1, "https://example.invalid/"]]),
        );
        repaired["timestamp"] = json!("2026-01-05T09:00:00.000Z");
        repaired["ordinal"] = json!(2);
        rows.push(repaired);
    }
}

#[test]
fn malformed_image_and_extension_revisions_leave_the_previous_display() {
    for (published, revision) in [
        (
            json!([11, "view", "file:///verified.png"]),
            json!({"type":"ImageView","id":"view","path":"file:///damaged?query"}),
        ),
        (
            json!([13, "image", "completed", "verified prompt", "inline"]),
            json!({"type":"ImageGeneration","id":"image","status":"completed","revised_prompt":"damaged","result":false}),
        ),
        (
            json!({"type":"Extension","kind":"image_gen.generation","id":"image","status":"completed","revisedPrompt":"verified prompt","fileId":"file"}),
            json!({"type":"Extension","kind":"image_gen.generation","id":"image","status":"completed","revisedPrompt":"damaged","fileId":false}),
        ),
        (
            json!([12, "web.search", "web", "verified query", null]),
            json!({"type":"Extension","kind":"web.search","id":"web","query":"damaged","action":false}),
        ),
        (
            json!([12, "clock.sleep", "timer", 27]),
            json!({"type":"Extension","kind":"clock.sleep","id":"timer","durationMs":"damaged"}),
        ),
    ] {
        let records = vec![completed(&published), completed(&revision)];
        let snapshot = read_with_history_mode(records, false, Some("paginated")).unwrap();
        let search = omnis_core::trajectory_search_document(&snapshot);
        assert!(
            !search.source_complete && !search.text.contains("damaged"),
            "{revision}"
        );
        assert!(
            search.text.contains("verified") || search.text.contains("27"),
            "{published}"
        );
    }
}

#[test]
fn replacing_a_published_slot_preserves_one_physical_model_result() {
    for command_first in [true, false] {
        let message = page_user("turn", "cmd", "shared slot request");
        let display = command("turn", "completed");
        let mut records = vec![event("task_started", json!({"turn_id":"turn"}))];
        if !command_first {
            records.push(message.clone());
        }
        records.extend([call(Some("turn")), model_result("recorded result"), display]);
        if command_first {
            records.push(message);
        }
        for preview in [false, true] {
            let snapshot =
                read_with_history_mode(records.clone(), preview, Some("paginated")).unwrap();
            assert_eq!(
                results(&snapshot),
                [(&EventKind::ToolCompleted, "recorded result")]
            );
            let search = omnis_core::trajectory_search_document(&snapshot);
            assert!(search.source_complete);
            for text in [search.text, omnis_core::render_semantic_handoff(&snapshot)] {
                assert_eq!(text.matches("recorded result").count(), 1, "{text}");
                assert_eq!(
                    text.contains("shared slot request"),
                    command_first,
                    "{text}"
                );
                assert!(!text.contains("display result"), "{text}");
            }
        }
    }
}

#[test]
fn image_view_replacing_a_command_keeps_its_path_and_the_physical_receipt() {
    let image = json!({"type":"ImageView","id":"cmd","path":"file:///fixture/published-image.png"});
    let rows = vec![
        event("task_started", json!({"turn_id":"turn"})),
        page_user("turn", "request", "inspect fixture"),
        call(Some("turn")),
        model_result("physical receipt"),
        command("turn", "failed"),
        turn_item("turn", &image),
    ];
    for preview in [false, true] {
        let snapshot = read_with_history_mode(rows.clone(), preview, Some("paginated")).unwrap();
        let search = omnis_core::trajectory_search_document(&snapshot);
        assert!(search.source_complete);
        assert!(search.text.contains("published-image.png"));
        assert_eq!(
            results(&snapshot),
            [(&EventKind::ToolCompleted, "physical receipt")]
        );
    }
}

#[test]
fn a_request_replacing_a_command_keeps_its_slot_when_the_model_result_arrives() {
    for late_result in [false, true] {
        let mut records = vec![
            event("task_started", json!({"turn_id":"turn"})),
            call(Some("turn")),
        ];
        let result = model_result("physical output");
        if !late_result {
            records.push(result.clone());
        }
        records.extend([
            command("turn", "completed"),
            page_user("turn", "second", "last objective"),
            page_user("turn", "cmd", "revised first slot"),
        ]);
        if late_result {
            records.push(result);
        }
        for preview in [false, true] {
            let snapshot =
                read_with_history_mode(records.clone(), preview, Some("paginated")).unwrap();
            assert_eq!(
                omnis_core::plan_semantic_handoff(&snapshot)
                    .objective
                    .as_deref(),
                Some("last objective")
            );
            assert_eq!(
                visible_text(&snapshot),
                ["revised first slot", "last objective"]
            );
            assert_eq!(
                results(&snapshot),
                [(&EventKind::ToolCompleted, "physical output")]
            );
            let search = omnis_core::trajectory_search_document(&snapshot);
            assert!(search.source_complete);
            for text in [search.text, omnis_core::render_semantic_handoff(&snapshot)] {
                assert!(text.contains("last objective") && text.contains("physical output"));
            }
        }
    }
}

#[test]
fn a_published_file_change_keeps_its_patch_when_it_replaces_a_command_receipt() {
    let change =
        json!({"type":"FileChange","id":"cmd","changes":{"fixture.rs":[0,"added fixture line"]}});
    let records = vec![
        event("task_started", json!({"turn_id":"turn"})),
        call(Some("turn")),
        model_result("physical result"),
        command("turn", "failed"),
        turn_item("turn", &change),
    ];
    for preview in [false, true] {
        let snapshot = read_with_history_mode(records.clone(), preview, Some("paginated")).unwrap();
        let search = omnis_core::trajectory_search_document(&snapshot);
        assert!(search.source_complete);
        assert!(search.text.contains("fixture.rs") && search.text.contains("added fixture line"));
        let trajectory = omnis_core::import_trajectory(&snapshot);
        assert!(trajectory.items.iter().any(
            |item| item.text.contains("fixture.rs") && item.text.contains("added fixture line")
        ));
        let handoff = omnis_core::render_semantic_handoff(&snapshot);
        assert_eq!(handoff.matches("physical result").count(), 1);
        assert_eq!(
            results(&snapshot),
            [(&EventKind::ToolCompleted, "physical result")]
        );
    }
}

#[test]
fn reused_call_ids_assign_display_status_to_the_matching_result_occurrence() {
    for late_result in [false, true] {
        for first_status in ["completed", "failed"] {
            let mut first = command("turn", first_status);
            first["payload"]["item"]["command"] = json!(["first"]);
            let mut second = command("turn", "failed");
            second["payload"]["item"]["command"] = json!(["second"]);
            let mut second_call = call(Some("turn"));
            second_call["payload"]["name"] = json!("second");
            let output = model_result("second result");
            let mut rows = vec![
                event("task_started", json!({"turn_id":"turn"})),
                call(Some("turn")),
                model_result("first result"),
                first,
                second_call,
            ];
            if !late_result {
                rows.push(output.clone());
            }
            rows.push(second);
            if late_result {
                rows.push(output);
            }
            for preview in [false, true] {
                let snap =
                    read_with_history_mode(rows.clone(), preview, Some("paginated")).unwrap();
                let first_kind = if first_status == "failed" {
                    EventKind::ToolFailed
                } else {
                    EventKind::ToolCompleted
                };
                assert_eq!(
                    results(&snap),
                    [
                        (&first_kind, "first result"),
                        (&EventKind::ToolFailed, "second result")
                    ]
                );
                let search = omnis_core::trajectory_search_document(&snap);
                for text in [search.text, omnis_core::render_semantic_handoff(&snap)] {
                    assert!(text.contains("first result") && text.contains("second result"));
                }
            }
        }
    }
}

#[test]
fn restoring_a_published_command_reuses_its_physical_result() {
    let failed = command("turn", "failed");
    let image = turn_item(
        "turn",
        &json!({"type":"ImageView","id":"cmd","path":"file:///fixture/published-image.png"}),
    );
    let rows = vec![
        event("task_started", json!({"turn_id":"turn"})),
        call(Some("turn")),
        model_result("only output"),
        failed.clone(),
        image,
        failed,
    ];
    for preview in [false, true] {
        let snap = read_with_history_mode(rows.clone(), preview, Some("paginated")).unwrap();
        assert_eq!(results(&snap), [(&EventKind::ToolFailed, "only output")]);
        for text in [
            omnis_core::trajectory_search_document(&snap).text,
            omnis_core::render_semantic_handoff(&snap),
        ] {
            assert_eq!(text.matches("only output").count(), 1);
            assert!(!text.contains("published-image.png"));
        }
    }
}

#[test]
fn failed_published_tool_receipts_transfer_the_matching_raw_output_once() {
    for (item, detail) in [
        (
            json!({"type":"DynamicToolCall","id":"cmd","tool":"lookup","namespace":"fixture-namespace","arguments":{},"status":"failed","success":false,"content_items":[{"type":"inputText","text":"lookup receipt"}]}),
            "fixture-namespace",
        ),
        (
            json!({"type":"McpToolCall","id":"cmd","tool":"lookup","server":"fixture-server","arguments":{},"status":"failed","result":{"content":[{"type":"text","text":"lookup receipt"}],"structuredContent":{"code":"fixture-code"},"isError":true}}),
            "fixture-code",
        ),
    ] {
        for mode in [None, Some("paginated")] {
            for display_first in [false, true] {
                for raw_text in ["lookup receipt", "different physical output"] {
                    let mut raw_call = call(Some("turn"));
                    raw_call["payload"]["name"] = "lookup".into();
                    let mut rows = vec![event("task_started", json!({"turn_id":"turn"})), raw_call];
                    let raw = model_result(raw_text);
                    let display = turn_item("turn", &item);
                    rows.extend(if display_first {
                        [display, raw]
                    } else {
                        [raw, display]
                    });
                    for preview in [false, true] {
                        let snap = read_with_history_mode(rows.clone(), preview, mode).unwrap();
                        let search = omnis_core::trajectory_search_document(&snap);
                        assert!(
                            search.source_complete && search.text.contains(detail),
                            "{mode:?} {display_first} {raw_text}: {}",
                            search.text
                        );
                        let handoff = omnis_core::render_semantic_handoff(&snap);
                        assert_eq!(handoff.matches("lookup receipt").count(), 1, "{handoff}");
                        let matched = raw_text == "lookup receipt";
                        if !matched {
                            assert!(handoff.contains(raw_text), "{handoff}");
                        }
                        let imported = omnis_core::import_trajectory(&snap);
                        if matched {
                            assert!(omnis_core::native_trajectory_items(&imported).iter().any(|item| matches!(item, omnis_core::NativeTrajectoryItem::Tool {output, is_error:true,..} if output==raw_text)));
                        } else {
                            assert_eq!(results(&snap), [(&EventKind::ToolCompleted, raw_text)]);
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn a_command_correction_during_a_later_call_uses_the_proven_occurrence() {
    for indistinguishable in [false, true] {
        let mut first = command("turn", "completed");
        first["payload"]["item"]["command"] = json!(["first"]);
        first["payload"]["item"]["aggregated_output"] = json!("first physical");
        let mut revision = first.clone();
        revision["payload"]["item"]["status"] = json!("failed");
        let first_call = call(Some("turn"));
        let mut next_call = first_call.clone();
        if !indistinguishable {
            next_call["payload"]["name"] = json!("second");
        }
        let mut rows = vec![
            event("task_started", json!({"turn_id":"turn"})),
            first_call,
            model_result("first physical"),
            first,
            next_call,
            revision,
        ];
        for end in [false, true] {
            if end {
                rows.push(model_result("second physical"));
            }
            for preview in [false, true] {
                let snap =
                    read_with_history_mode(rows.clone(), preview, Some("paginated")).unwrap();
                let search = omnis_core::trajectory_search_document(&snap);
                assert_eq!(search.source_complete, !indistinguishable);
                let raw: Vec<_> = snap
                    .events
                    .iter()
                    .filter(|e| {
                        e.source.raw_record_type.as_deref()
                            == Some("response_item.function_call_output")
                    })
                    .map(|e| (&e.kind, &e.payload["output"]))
                    .collect();
                assert_eq!(
                    raw[0].0,
                    if indistinguishable {
                        &EventKind::ToolCompleted
                    } else {
                        &EventKind::ToolFailed
                    }
                );
                if end {
                    assert_eq!(raw[1].0, &EventKind::ToolCompleted);
                }
                assert!(omnis_core::render_semantic_handoff(&snap).contains("first physical"));
            }
        }
    }
}

#[test]
fn mcp_structured_results_match_the_serialized_model_body_and_keep_display_text() {
    let structured = json!({"code":"missing_file", "line":12});
    let body = serde_json::to_string(&structured).unwrap();
    let display = turn_item(
        "turn",
        &json!({"type":"McpToolCall","id":"cmd","tool":"lookup","server":"fixture","arguments":{},"status":"failed","result":{"content":[{"type":"text","text":"No such file"}],"structuredContent":structured,"isError":true}}),
    );
    for preview in [false, true] {
        for mode in [None, Some("paginated")] {
            for display_first in [false, true] {
                let mut rows = vec![
                    event("task_started", json!({"turn_id":"turn"})),
                    call(Some("turn")),
                ];
                let raw = model_result(&body);
                rows.extend(if display_first {
                    [display.clone(), raw]
                } else {
                    [raw, display.clone()]
                });
                let snap = read_with_history_mode(rows, preview, mode).unwrap();
                assert_eq!(results(&snap), [(&EventKind::ToolFailed, body.as_str())]);
                let search = omnis_core::trajectory_search_document(&snap);
                assert!(search.source_complete && search.text.contains("No such file"));
                assert!(omnis_core::native_trajectory_items(&omnis_core::import_trajectory(&snap)).iter().any(|item| matches!(item,omnis_core::NativeTrajectoryItem::Tool {is_error:true,output,..} if output==&body)));
            }
        }
    }
}

#[test]
fn a_long_dynamic_result_compares_the_original_bytes_before_shortening_them() {
    let edge = "ü".repeat(16_384);
    let raw_text = format!("{edge}a{edge}");
    let mut lookup = call(Some("turn"));
    lookup["payload"]["name"] = "lookup".into();
    for middle in ["a", "z"] {
        let display = turn_item(
            "turn",
            &json!({"type":"DynamicToolCall","id":"cmd","tool":"lookup","arguments":{},"status":"failed","success":false,"content_items":[{"type":"inputText","text":format!("{edge}{middle}{edge}")}]}),
        );
        let rows = vec![
            event("task_started", json!({"turn_id":"turn"})),
            lookup.clone(),
            model_result(&raw_text),
            display,
        ];
        for preview in [false, true] {
            let snap = read_with_history_mode(rows.clone(), preview, Some("paginated")).unwrap();
            let raw = snap
                .events
                .iter()
                .find(|e| {
                    e.source.raw_record_type.as_deref()
                        == Some("response_item.function_call_output")
                })
                .unwrap();
            assert_eq!(raw.kind == EventKind::ToolFailed, middle == "a");
            assert!(
                raw.payload["output"]
                    .as_str()
                    .unwrap()
                    .contains("characters omitted")
            );
            let outcomes = omnis_core::plan_semantic_handoff(&snap).tool_outcomes;
            assert_eq!(outcomes.len(), if middle == "a" { 1 } else { 2 });
            assert!(snap.events.iter().all(|event| {
                !event
                    .payload
                    .to_string()
                    .contains("omnisession_codex_source_receipt")
            }));
        }
    }
}

#[test]
fn repeated_commands_with_the_same_inputs_can_be_distinguished_by_their_original_output() {
    let shown = |text: &str, status: &str| {
        let mut item = command("turn", status);
        item["payload"]["item"]["aggregated_output"] = json!(text);
        item
    };
    let rows = vec![
        event("task_started", json!({"turn_id":"turn"})),
        call(Some("turn")),
        model_result("first native output"),
        shown("first native output", "completed"),
        call(Some("turn")),
        model_result("second native output"),
        shown("second native output", "completed"),
        shown("first native output", "failed"),
    ];
    for preview in [false, true] {
        let snapshot = read_with_history_mode(rows.clone(), preview, Some("paginated")).unwrap();
        assert_eq!(
            results(&snapshot),
            [
                (&EventKind::ToolFailed, "first native output"),
                (&EventKind::ToolCompleted, "second native output")
            ]
        );
        assert!(omnis_core::trajectory_search_document(&snapshot).source_complete);
    }
}

#[test]
fn stock_tool_writer_formats_preserve_failures_only_for_equivalent_outputs() {
    let header = "Wall time: 0.0420 seconds\nOutput:";
    let near_header = "Wall time: 0.042 seconds\nOutput:";
    let diagnostic = "No such file";
    let cancel = "dynamic tool call was cancelled before receiving a response";
    let image = "data:image/png;base64,AA==";
    let text_part = |text| json!({"type":"input_text","text":text});
    let image_part = |detail| json!({"type":"input_image","image_url":image,"detail":detail});
    let mcp = json!({"type":"McpToolCall","id":"cmd","server":"fixture","tool":"lookup","arguments":{},"status":"failed","result":{"content":[{"type":"text","text":diagnostic}],"isError":true}});
    let mut structured = mcp.clone();
    structured["result"]["structuredContent"] = json!({"code":"missing_file"});
    let dynamic = json!({"type":"DynamicToolCall","id":"cmd","tool":"lookup","arguments":{},"status":"failed","success":false,"content_items":[{"type":"inputText","text":diagnostic},{"type":"inputImage","imageUrl":image}]});
    let cancelled = json!({"type":"DynamicToolCall","id":"cmd","tool":"lookup","arguments":{},"status":"failed","success":false,"content_items":[],"error":cancel});
    let mut lookup = call(Some("turn"));
    lookup["payload"]["name"] = "lookup".into();
    for (item, body, other) in [
        (
            mcp,
            json!([text_part(header), text_part(diagnostic)]),
            json!([text_part(near_header), text_part(diagnostic)]),
        ),
        (
            structured,
            json!(format!("{header}\n{{\"code\":\"missing_file\"}}")),
            json!(format!("{near_header}\n{{\"code\":\"missing_file\"}}")),
        ),
        (
            dynamic,
            json!([text_part(diagnostic), image_part("high")]),
            json!([text_part(diagnostic), image_part("low")]),
        ),
        (
            cancelled,
            json!(cancel),
            json!("different cancellation reason"),
        ),
    ] {
        for (output, matched) in [(body, true), (other, false)] {
            for mode in [None, Some("paginated")] {
                for display_first in [false, true] {
                    let mut records = vec![
                        event("task_started", json!({"turn_id":"turn"})),
                        lookup.clone(),
                    ];
                    let mut raw = model_result("");
                    raw["payload"]["output"] = output.clone();
                    let display = turn_item("turn", &item);
                    records.extend(if display_first {
                        [display, raw]
                    } else {
                        [raw, display]
                    });
                    for preview in [false, true] {
                        let snapshot =
                            read_with_history_mode(records.clone(), preview, mode).unwrap();
                        let result = snapshot
                            .events
                            .iter()
                            .find(|event| {
                                event.source.raw_record_type.as_deref()
                                    == Some("response_item.function_call_output")
                            })
                            .unwrap();
                        assert_eq!(
                            result.kind == EventKind::ToolFailed,
                            matched,
                            "{item} {output} {mode:?} {display_first}"
                        );
                        let search = omnis_core::trajectory_search_document(&snapshot);
                        assert!(search.source_complete);
                        let outcomes = omnis_core::plan_semantic_handoff(&snapshot).tool_outcomes;
                        assert_eq!(outcomes.len(), if matched { 1 } else { 2 });
                        let media = item["content_items"].as_array().is_some_and(|parts| {
                            parts.iter().any(|part| part["type"] == "inputImage")
                        });
                        if matched && media {
                            assert!(
                                omnis_core::import_trajectory(&snapshot)
                                    .items
                                    .iter()
                                    .any(|item| item.text.contains("Historical tool failure")
                                        && item.text.contains(diagnostic))
                            );
                        }
                        if matched && !media {
                            assert!(
                                omnis_core::native_trajectory_items(
                                    &omnis_core::import_trajectory(&snapshot)
                                )
                                .iter()
                                .any(|item| matches!(
                                    item,
                                    omnis_core::NativeTrajectoryItem::Tool { is_error: true, .. }
                                ))
                            );
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn transport_errors_and_original_mcp_images_follow_stock_model_output_conversion() {
    let diagnostic = "fixture unavailable";
    let header = json!({"type":"input_text","text":"Wall time: 0.0420 seconds\nOutput:"});
    let text = |value| json!({"type":"input_text","text":value});
    let image = |detail| json!({"type":"input_image","image_url":"data:image/png;base64,AA==","detail":detail});
    let error = json!({"type":"McpToolCall","id":"cmd","tool":"lookup","server":"fixture","arguments":{},"status":"failed","result":null,"error":{"message":diagnostic}});
    let pictured = json!({"type":"McpToolCall","id":"cmd","tool":"lookup","server":"fixture","arguments":{},"status":"failed","result":{"isError":true,"content":[{"type":"text","text":diagnostic},{"type":"image","mimeType":"image/png","data":"AA==","_meta":{"codex/imageDetail":"original"}}]}});
    for (item, output, matched) in [
        (&error, json!([header, text(diagnostic)]), true),
        (&error, json!([header, text("different message")]), false),
        (
            &pictured,
            json!([header, text(diagnostic), image("original")]),
            true,
        ),
        (
            &pictured,
            json!([header, text(diagnostic), image("high")]),
            true,
        ),
        (
            &pictured,
            json!([header, text(diagnostic), image("low")]),
            false,
        ),
    ] {
        for mode in [None, Some("paginated")] {
            for display_first in [false, true] {
                let display = turn_item("turn", item);
                let mut raw = model_result("");
                raw["payload"]["output"] = output.clone();
                let mut records = vec![
                    event("task_started", json!({"turn_id":"turn"})),
                    call(Some("turn")),
                ];
                records.extend(if display_first {
                    [display, raw]
                } else {
                    [raw, display]
                });
                for preview in [false, true] {
                    let snapshot = read_with_history_mode(records.clone(), preview, mode).unwrap();
                    let raw = snapshot
                        .events
                        .iter()
                        .find(|e| {
                            e.source.raw_record_type.as_deref()
                                == Some("response_item.function_call_output")
                        })
                        .unwrap();
                    assert_eq!(raw.kind == EventKind::ToolFailed, matched);
                    let outcomes = omnis_core::plan_semantic_handoff(&snapshot).tool_outcomes;
                    assert_eq!(outcomes.len(), if matched { 1 } else { 2 });
                    let search = omnis_core::trajectory_search_document(&snapshot);
                    assert!(search.source_complete && search.text.contains(diagnostic));
                    assert!(
                        omnis_core::import_trajectory(&snapshot)
                            .items
                            .iter()
                            .any(|i| i.text.contains(diagnostic))
                    );
                }
            }
        }
    }
}

#[test]
fn a_revised_published_function_output_keeps_its_correction_and_the_raw_receipt() {
    let shown = |output| {
        turn_item(
            "turn",
            &json!({"type":"FunctionCallOutput","id":"cmd","name":"lookup","output":output}),
        )
    };
    for mode in [None, Some("paginated")] {
        for model_first in [false, true] {
            let mut records = vec![
                event("task_started", json!({"turn_id":"turn"})),
                call(Some("turn")),
            ];
            let raw = model_result("model receipt");
            let original = shown("model receipt");
            records.extend(if model_first {
                [raw, original]
            } else {
                [original, raw]
            });
            for revise in [false, true] {
                if revise {
                    records.push(shown("corrected publication"));
                }
                for preview in [false, true] {
                    let snapshot = read_with_history_mode(records.clone(), preview, mode).unwrap();
                    for text in [
                        omnis_core::trajectory_search_document(&snapshot).text,
                        omnis_core::render_semantic_handoff(&snapshot),
                    ] {
                        assert_eq!(
                            text.matches("model receipt").count(),
                            1,
                            "{mode:?} {model_first} {text}"
                        );
                        assert_eq!(
                            text.contains("corrected publication"),
                            revise,
                            "{mode:?} {model_first} {text}"
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn published_function_identity_survives_result_pairing_and_revision() {
    let shown = |name, namespace| {
        turn_item(
            "turn",
            &json!({
                "type":"FunctionCallOutput","id":"cmd","name":name,"namespace":namespace,"output":"fixed receipt"
            }),
        )
    };
    for mode in [None, Some("paginated")] {
        for model_first in [false, true] {
            let mut records = vec![
                event("task_started", json!({"turn_id":"turn"})),
                call(Some("turn")),
            ];
            let raw = model_result("fixed receipt");
            let initial = shown("lookup", "initialspace");
            records.extend(if model_first {
                [raw, initial]
            } else {
                [initial, raw]
            });
            records.push(shown("correctedfixture", "revisedspace"));
            for preview in [false, true] {
                let snapshot = read_with_history_mode(records.clone(), preview, mode).unwrap();
                let search = omnis_core::trajectory_search_document(&snapshot);
                let handoff = omnis_core::render_semantic_handoff(&snapshot);
                for text in [&search.text, &handoff] {
                    assert!(
                        text.contains("correctedfixture") && text.contains("revisedspace"),
                        "{mode:?} {model_first} {text}"
                    );
                    assert_eq!(text.matches("fixed receipt").count(), 1, "{text}");
                    assert!(!text.contains("initialspace"), "{text}");
                }
            }
        }
    }
}

#[test]
fn corrected_function_body_releases_the_previous_display_identity() {
    let display = |name, namespace, output| {
        turn_item(
            "turn",
            &json!({
                "type":"FunctionCallOutput","id":"cmd","name":name,"namespace":namespace,"output":output
            }),
        )
    };
    for mode in [None, Some("paginated")] {
        for raw_first in [true, false] {
            let mut records = vec![
                event("task_started", json!({"turn_id":"turn"})),
                {
                    let mut input = message("user", "fixture objective");
                    input["payload"]["id"] = "u".into();
                    input["payload"]["internal_chat_message_metadata_passthrough"] =
                        json!({"turn_id":"turn"});
                    input
                },
                page_user("turn", "u", "fixture objective"),
                call(Some("turn")),
            ];
            let raw = model_result("rawalpha");
            let original = display("lookup", "initialspace", "rawalpha");
            records.extend(if raw_first {
                [raw, original]
            } else {
                [original, raw]
            });
            records.push(display("revisedname", "revisedspace", "displaybeta"));
            for rolled in [false, true] {
                if rolled {
                    records.push(event("thread_rolled_back", json!({"num_turns":1})));
                }
                for preview in [false, true] {
                    let snapshot = read_with_history_mode(records.clone(), preview, mode).unwrap();
                    let search = omnis_core::trajectory_search_document(&snapshot);
                    let handoff = omnis_core::render_semantic_handoff(&snapshot);
                    for (text, retains_history) in
                        [(&search.text, mode.is_some()), (&handoff, false)]
                    {
                        assert!(
                            !text.contains("initialspace"),
                            "{mode:?} {raw_first} {rolled} {text}"
                        );
                        for term in ["rawalpha", "revisedspace", "revisedname", "displaybeta"] {
                            assert_eq!(
                                text.contains(term),
                                !rolled || retains_history,
                                "{mode:?} {raw_first} {rolled} {term} {text}"
                            );
                        }
                    }
                }
            }
        }
    }
}
