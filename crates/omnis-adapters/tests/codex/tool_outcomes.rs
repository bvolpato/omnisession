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
        model_result("old output"),
        event("task_started", json!({"turn_id":"rollout-2"})),
        page_user("rollout-2", "u1", "first request"),
        command("rollout-2", "completed"),
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
