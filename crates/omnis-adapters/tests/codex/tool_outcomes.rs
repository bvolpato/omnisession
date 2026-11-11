use super::*;

fn model_result(output: &str) -> Value {
    json!({"type":"response_item","payload":{"type":"function_call_output","call_id":"cmd","output":output}})
}

fn command(turn: &str, status: &str) -> Value {
    turn_item(
        turn,
        &json!({"type":"CommandExecution","id":"cmd","command":["example"],"aggregated_output":"display result","status":status,"exit_code":0}),
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
