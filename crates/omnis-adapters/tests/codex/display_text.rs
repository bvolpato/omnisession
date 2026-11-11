use super::{event, message, read_records_with_preview, turn_item};
use omnis_ir::{EventKind, ReplayPolicy};
use serde_json::{Value, json};

fn raw_assistant(id: &str, parts: &[&str]) -> Value {
    json!({"type":"response_item","payload":{"type":"message","role":"assistant","id":id,
        "content":parts.iter().map(|text| json!({"type":"output_text","text":text})).collect::<Vec<_>>()}})
}

fn shown_assistant(turn: &str, id: &str, text: &str) -> Value {
    turn_item(
        turn,
        &json!({"type":"AgentMessage","id":id,"content":[{"type":"Text","text":text}]}),
    )
}

#[test]
fn plan_mode_replies_keep_the_native_plan_and_only_its_surrounding_assistant_text() {
    let raw = "Before.<oai-mem-citation>outside citation</oai-mem-citation>\n  <proposed_plan>\nold\n</proposed_plan>\n<proposed_plan>\nnew<oai-mem-citation>inside citation</oai-mem-citation>\n</proposed_plan>\nAfter.";
    let shown = "Before.\nAfter.";
    for display_first in [false, true] {
        let mut rows = vec![
            event("task_started", json!({"turn_id":"first"})),
            json!({"type":"turn_context","payload":{"turn_id":"first","collaboration_mode":{"mode":"plan"}}}),
            message("user", "plans please"),
        ];
        let model = raw_assistant("msg_1", &[raw]);
        let display = shown_assistant("first", "item-2", shown);
        rows.extend(if display_first {
            [display, model]
        } else {
            [model, display]
        });
        rows.push(turn_item(
            "first",
            &json!({"type":"Plan","id":"item-3","text":"new\n"}),
        ));
        rows.push(event("task_complete", json!({"turn_id":"first"})));
        rows.push(event("task_started", json!({"turn_id":"second"})));
        let literal = "Literal:\n<proposed_plan>\nexample\n</proposed_plan>";
        rows.push(raw_assistant("msg_4", &[literal]));
        for preview in [false, true] {
            let snapshot = read_records_with_preview(rows.clone(), preview).unwrap();
            assert!(omnis_core::trajectory_search_document(&snapshot).source_complete);
            let text: Vec<_> = snapshot
                .events
                .iter()
                .filter(|v| v.kind == EventKind::MessageAssistant)
                .map(|v| v.payload["text"].as_str().unwrap())
                .collect();
            assert_eq!(text, [shown, "new\n", literal]);
            let plan = snapshot
                .events
                .iter()
                .find(|event| event.payload["type"] == "Plan")
                .unwrap();
            assert_eq!(plan.replay_policy, ReplayPolicy::Contextual);
            assert_eq!(plan.payload["turn_id"], "first");
            assert_eq!(plan.payload["id"], "item-3");
            let imported = omnis_core::import_conversation(&snapshot);
            assert_eq!(
                imported
                    .messages
                    .iter()
                    .map(|message| message.text.as_str())
                    .collect::<Vec<_>>(),
                ["plans please", shown, "new\n", literal]
            );
            assert!(
                !omnis_core::trajectory_search_document(&snapshot)
                    .text
                    .contains("inside citation")
            );
        }
    }
}

#[test]
fn unfinished_plan_markup_keeps_only_the_displayed_proposal() {
    let literal = "<proposed_plan> on this line is an example";
    let rows = vec![
        event(
            "task_started",
            json!({"turn_id":"first","collaboration_mode_kind":"plan"}),
        ),
        raw_assistant("msg_1", &["\r\n <proposed_plan>\r\n- unfinished"]),
        turn_item(
            "first",
            &json!({"type":"Plan","id":"first-plan","text":"- unfinished"}),
        ),
        raw_assistant("msg_2", &[literal]),
    ];
    for preview in [false, true] {
        let raw_only = read_records_with_preview(rows[..2].to_vec(), preview).unwrap();
        assert!(super::visible_text(&raw_only).is_empty());
        assert!(!omnis_core::trajectory_search_document(&raw_only).source_complete);
        assert!(omnis_core::import_conversation(&raw_only).truncated);
        let notice = raw_only
            .events
            .iter()
            .find(|event| omnis_core::reports_omitted_events(event))
            .unwrap();
        assert_eq!(notice.replay_policy, ReplayPolicy::HistoricalOnly);
        assert!(
            !serde_json::to_string(&raw_only)
                .unwrap()
                .contains("- unfinished")
        );
        let snapshot = read_records_with_preview(rows.clone(), preview).unwrap();
        assert_eq!(super::visible_text(&snapshot), ["- unfinished", literal]);
        assert!(omnis_core::trajectory_search_document(&snapshot).source_complete);
        assert_eq!(snapshot.events[0].kind, EventKind::MessageAssistant);
        assert_eq!(snapshot.events[0].payload["type"], "Plan");
    }
}

#[test]
fn display_copy_preserves_multipart_tokens_in_search_and_transfer() {
    for display_first in [false, true] {
        let model = raw_assistant("msg_1", &["Use --dry", "-run."]);
        let display = shown_assistant("first", "item-2", "Use --dry-run.");
        let pair = if display_first {
            [display, model]
        } else {
            [model, display]
        };
        let mut rows = vec![
            event("task_started", json!({"turn_id":"first"})),
            message("user", "How?"),
        ];
        rows.extend(pair);
        for preview in [false, true] {
            let snapshot = read_records_with_preview(rows.clone(), preview).unwrap();
            assert_eq!(super::visible_text(&snapshot), ["How?", "Use --dry-run."]);
            let search = omnis_core::trajectory_search_document(&snapshot);
            assert!(search.text.contains("--dry-run"));
            let messages = omnis_core::import_conversation(&snapshot).messages;
            assert_eq!(messages.last().unwrap().text, "Use --dry-run.");
        }
    }
}

#[test]
fn a_stamped_default_turn_keeps_literal_tags_during_a_different_plan_turn() {
    let literal = "<proposed_plan>\nexample\n</proposed_plan>";
    let mut late = raw_assistant("msg_default", &[literal]);
    late["payload"]["internal_chat_message_metadata_passthrough"] = json!({"turn_id":"default"});
    let rows = vec![
        event("task_started", json!({"turn_id":"default"})),
        event(
            "task_started",
            json!({"turn_id":"plan","collaboration_mode_kind":"plan"}),
        ),
        late,
    ];
    for preview in [false, true] {
        assert_eq!(
            super::visible_text(&read_records_with_preview(rows.clone(), preview).unwrap()),
            [literal]
        );
    }
}

#[test]
fn a_plan_keeps_its_position_entire_text_and_separate_identity() {
    let body = "0123456789".repeat(5_000) + "end of plan";
    let plan =
        |id: &str, text: &str| turn_item("first", &json!({"type":"Plan","id":id,"text":text}));
    let mut rows = vec![
        event("task_started", json!({"turn_id":"first"})),
        plan("plan-1", "superseded"),
        shown_assistant("first", "comment", "same text"),
        raw_assistant("comment", &["same text"]),
        plan("plan-2", "same text"),
        plan("plan-1", &body),
    ];
    rows.push(json!({"type":"response_item","payload":{"type":"function_call","name":"update_plan","call_id":"tool","arguments":"{}"}}));
    for preview in [false, true] {
        let snapshot = read_records_with_preview(rows.clone(), preview).unwrap();
        assert_eq!(
            super::visible_text(&snapshot),
            [body.as_str(), "same text", "same text"]
        );
        assert_eq!(snapshot.events[0].payload["id"], "plan-1");
        assert_eq!(snapshot.events[2].payload["id"], "plan-2");
        assert_eq!(snapshot.events[3].kind, EventKind::ToolCalled);
    }
}

#[test]
fn a_plan_display_confirms_only_the_matching_proposal_in_its_turn() {
    let begin = |turn| {
        event(
            "task_started",
            json!({"turn_id":turn,"collaboration_mode_kind":"plan"}),
        )
    };
    let display =
        |turn, text| turn_item(turn, &json!({"type":"Plan","id":"some-plan","text":text}));
    let raw = raw_assistant("model", &["<proposed_plan>\nsource-only\n</proposed_plan>"]);
    let rows = vec![
        begin("first"),
        message("user", "first input"),
        display("first", "source-only\n"),
        begin("second"),
        message("user", "second input"),
        display("second", "different"),
        raw.clone(),
    ];
    for preview in [false, true] {
        let snap = read_records_with_preview(rows.clone(), preview).unwrap();
        assert!(!omnis_core::trajectory_search_document(&snap).source_complete);
        let mut confirmed = rows.clone();
        confirmed.push(display("second", "source-only"));
        assert!(
            omnis_core::trajectory_search_document(
                &read_records_with_preview(confirmed, preview).unwrap()
            )
            .source_complete
        );
        let mut rolled_back = rows.clone();
        rolled_back.push(event("thread_rolled_back", json!({"num_turns":1})));
        assert!(
            omnis_core::trajectory_search_document(
                &read_records_with_preview(rolled_back, preview).unwrap()
            )
            .source_complete
        );
        let earlier = vec![
            begin("first"),
            display("first", "source-only\n"),
            raw.clone(),
        ];
        assert!(
            omnis_core::trajectory_search_document(
                &read_records_with_preview(earlier, preview).unwrap()
            )
            .source_complete
        );
    }
}
