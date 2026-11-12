use super::*;

fn context(turn: Option<&str>) -> Value {
    json!({"type":"turn_context","payload":{"turn_id":turn,"cwd":"/workspace/original"}})
}

#[test]
fn named_completion_separates_repeated_requests_without_start_events() {
    for first_id in [Some("first"), None] {
        let records = vec![
            context(first_id),
            event("user_message", json!({"message":"repeat"})),
            event("agent_message", json!({"message":"first answer"})),
            event("task_complete", json!({"turn_id":"first"})),
            context(Some("second")),
            message("user", "repeat"),
            message("assistant", "second answer"),
            event("thread_rolled_back", json!({"num_turns":1})),
        ];
        for preview in [false, true] {
            let snapshot = read_records_with_preview(records.clone(), preview).unwrap();
            assert_eq!(visible_text(&snapshot), ["repeat", "first answer"]);
        }
    }
}

#[test]
fn a_late_item_before_the_next_request_belongs_to_its_named_turn() {
    let command = turn_item(
        "first",
        &json!({"type":"CommandExecution","id":"cmd",
        "command":["example"],"cwd":"file:///workspace","parsed_cmd":[],"source":"agent","status":"completed","aggregated_output":"late result"}),
    );
    for start in [
        event("task_started", json!({"turn_id":"second"})),
        context(Some("second")),
    ] {
        // The first request and its model copy can surround the first named context.
        let records = vec![
            event("user_message", json!({"message":"keep"})),
            context(Some("first")),
            message("user", "keep"),
            start,
            event("task_complete", json!({"turn_id":"first"})),
            command.clone(),
            event("user_message", json!({"message":"discard"})),
            message("user", "discard"),
            event("thread_rolled_back", json!({"num_turns":1})),
        ];
        for preview in [false, true] {
            let mut rows = records.clone();
            let kept = read_records_with_preview(rows.clone(), preview).unwrap();
            assert_eq!(visible_text(&kept), ["keep"]);
            let results: Vec<_> = kept
                .events
                .iter()
                .filter(|event| event.kind == EventKind::CommandExecuted)
                .map(|event| event.payload["output"].as_str().unwrap())
                .collect();
            assert_eq!(results, ["late result"]);
            rows.push(event("thread_rolled_back", json!({"num_turns":1})));
            assert!(
                read_records_with_preview(rows, preview)
                    .unwrap()
                    .events
                    .is_empty()
            );
        }
    }
}
