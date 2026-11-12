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

fn plan_model(turn: &str, id: &str, role: &str, text: &str, order: u64) -> Value {
    let kind = if role == "user" {
        "input_text"
    } else {
        "output_text"
    };
    json!({"type":"response_item","metadata":{"user_input_order":order},"payload":{
        "type":"message","id":id,"role":role,"internal_chat_message_metadata_passthrough":{"turn_id":turn},
        "content":[{"type":kind,"text":text}]}})
}

#[test]
fn a_displayed_plan_follows_its_model_proposal_through_rollback() {
    let turn = "planning";
    let shown = turn_item(
        turn,
        &json!({"type":"Plan","id":"planning-plan","text":"Visible proposal\n"}),
    );
    let source = "Comment.\n<proposed_plan>\nVisible proposal\n</proposed_plan>";
    for order in [None, Some(1), Some(2)] {
        let mut rows = vec![
            event(
                "task_started",
                json!({"turn_id":turn,"collaboration_mode_kind":"plan"}),
            ),
            plan_model(turn, "u1", "user", "keep", 1),
            super::page_user(turn, "u1", "keep"),
        ];
        if let Some(order) = order {
            rows.push(plan_model(turn, "msg", "assistant", source, order));
            rows.push(shown_assistant(turn, "msg", "Comment.\n"));
        }
        rows.extend([
            shown.clone(),
            plan_model(turn, "u2", "user", "steer", 2),
            super::page_user(turn, "u2", "steer"),
            event("thread_rolled_back", json!({"num_turns":1})),
        ]);
        for preview in [false, true] {
            let snapshot =
                super::read_with_history_mode(rows.clone(), preview, Some("paginated")).unwrap();
            let plan = snapshot
                .events
                .iter()
                .find(|e| e.payload["type"] == "Plan")
                .unwrap();
            assert_eq!(plan.payload["text"], "Visible proposal\n");
            let active = omnis_core::import_conversation(&snapshot)
                .messages
                .into_iter()
                .map(|m| m.text)
                .collect::<Vec<_>>();
            let expected = match order {
                None => vec!["keep", "Visible proposal\n"],
                Some(1) => vec!["keep", "Comment.\n", "Visible proposal\n"],
                _ => vec!["keep"],
            };
            assert_eq!(active, expected);
            assert_eq!(
                plan.payload["omnisession_codex_rollback"].as_str(),
                if order == Some(2) {
                    Some("withdrawn")
                } else {
                    None
                }
            );
        }
    }
}

#[test]
fn a_mixed_checkpoint_keeps_both_public_items_and_restores_the_prior_plan() {
    let t = "planning";
    let user = plan_model(t, "u1", "user", "keep", 1);
    let raw = plan_model(
        t,
        "first",
        "assistant",
        "Comment.\n<proposed_plan>\noriginal\n</proposed_plan>",
        1,
    );
    let plan = |text| turn_item(t, &json!({"type":"Plan","id":"planning-plan","text":text}));
    let mut rows = vec![
        event(
            "task_started",
            json!({"turn_id":t,"collaboration_mode_kind":"plan"}),
        ),
        user.clone(),
        super::page_user(t, "u1", "keep"),
        raw.clone(),
        shown_assistant(t, "first", "Comment.\n"),
        plan("original\n"),
        json!({"type":"compacted","payload":{"message":"","window_number":1,"replacement_history":[user["payload"],raw["payload"]]}}),
    ];
    for revised in [false, true] {
        if revised {
            rows.extend([
                plan_model(t, "u2", "user", "later", 2),
                super::page_user(t, "u2", "later"),
                plan_model(
                    t,
                    "new",
                    "assistant",
                    "<proposed_plan>\nremoved\n</proposed_plan>",
                    2,
                ),
                plan("removed\n"),
                event("thread_rolled_back", json!({"num_turns":1})),
            ]);
        }
        for preview in [false, true] {
            let snapshot =
                super::read_with_history_mode(rows.clone(), preview, Some("paginated")).unwrap();
            let active = omnis_core::import_conversation(&snapshot)
                .messages
                .into_iter()
                .map(|m| m.text)
                .collect::<Vec<_>>();
            assert_eq!(active, ["keep", "Comment.\n", "original\n"]);
            let displayed = snapshot
                .events
                .iter()
                .find(|e| e.payload["type"] == "Plan")
                .unwrap();
            assert_eq!(
                displayed.payload["text"],
                if revised { "removed\n" } else { "original\n" }
            );
        }
    }
}

#[test]
fn a_finished_paginated_turn_reports_only_surviving_undisplayed_plans() {
    let t = "planning";
    let end = event("task_complete", json!({"turn_id":t}));
    let rollback = event("thread_rolled_back", json!({"num_turns":1}));
    let plan = |text| turn_item(t, &json!({"type":"Plan","id":"planning-plan","text":text}));
    let body = "model-only proposal";
    let mut raw = plan_model(
        t,
        "reply",
        "assistant",
        &format!("Preface.\n<proposed_plan>\n{body}\n</proposed_plan>"),
        1,
    );
    for stamped in [true, false] {
        if !stamped {
            raw["payload"]
                .as_object_mut()
                .unwrap()
                .remove("internal_chat_message_metadata_passthrough");
        }
        let rows = vec![
            event(
                "task_started",
                json!({"turn_id":t,"collaboration_mode_kind":"plan"}),
            ),
            plan_model(t, "request", "user", "plan this", 1),
            super::page_user(t, "request", "plan this"),
            shown_assistant(t, "reply", "Preface.\n"),
            raw.clone(),
        ];
        let second = plan_model(
            t,
            "second",
            "assistant",
            "<proposed_plan>
second proposal
</proposed_plan>",
            1,
        );
        let usage = json!({"type":"token_usage_record","payload":{"turn_id":t,"response_id":"model-response"}});
        for (tail, missing, displayed) in [
            (vec![], false, false),
            (
                vec![
                    plan(body),
                    second.clone(),
                    plan("published revision"),
                    end.clone(),
                ],
                false,
                true,
            ),
            (
                vec![
                    plan(body),
                    usage,
                    second,
                    plan("published revision"),
                    end.clone(),
                ],
                true,
                true,
            ),
            (
                vec![event(
                    "turn_aborted",
                    json!({"turn_id":t,"reason":"interrupted"}),
                )],
                false,
                false,
            ),
            (vec![end.clone()], true, false),
            (vec![end.clone(), rollback.clone()], false, false),
            (
                vec![plan(body), plan("published revision"), end.clone()],
                false,
                true,
            ),
        ] {
            for preview in [false, true] {
                let mut input = rows.clone();
                input.extend(tail.iter().cloned());
                let snap =
                    super::read_with_history_mode(input, preview, Some("paginated")).unwrap();
                let notice = snap.events.iter().find(|event| {
                    event.source.raw_record_type.as_deref()
                        == Some("omnisession.codex_plan_display_unverified")
                });
                assert_eq!(
                    notice.and_then(|event| event.payload["omitted_events"].as_u64()),
                    missing.then_some(1)
                );
                let search = omnis_core::trajectory_search_document(&snap);
                assert_eq!(search.source_complete, !missing);
                assert!(!search.text.contains(body));
                assert_eq!(search.text.contains("published revision"), displayed);
            }
        }
    }
}
