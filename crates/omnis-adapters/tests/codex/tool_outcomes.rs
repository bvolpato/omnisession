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
