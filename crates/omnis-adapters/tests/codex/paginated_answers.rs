use super::{event, page_user, read_with_history_mode, turn_item, visible_text};
use serde_json::{Value, json};

fn model(turn: &str, id: &str, role: &str, text: &str) -> Value {
    let part = if role == "user" {
        "input_text"
    } else {
        "output_text"
    };
    json!({"type":"response_item","payload":{"type":"message","role":role,"id":id,
        "internal_chat_message_metadata_passthrough":{"turn_id":turn},"content":[{"type":part,"text":text}]}})
}

fn shown(turn: &str, id: &str, text: &str) -> Value {
    turn_item(
        turn,
        &json!({"type":"AgentMessage","id":id,"content":[{"type":"Text","text":text}]}),
    )
}

fn start(turn: &str) -> Value {
    event("task_started", json!({"turn_id":turn}))
}

fn rollback() -> Value {
    event("thread_rolled_back", json!({"num_turns":1}))
}

fn check(rows: &[Value], displayed: &[&str], active: &[&str]) {
    for preview in [false, true] {
        let snapshot = read_with_history_mode(rows.to_vec(), preview, Some("paginated")).unwrap();
        assert_eq!(visible_text(&snapshot), displayed);
        let import = omnis_core::import_conversation(&snapshot);
        assert_eq!(
            import
                .messages
                .iter()
                .map(|message| message.text.as_str())
                .collect::<Vec<_>>(),
            active
        );
    }
}

#[test]
fn an_early_answer_follows_its_native_acceptance_order_after_rollback() {
    for (answer_order, input_order, inherited_input, inherited_answer, survives) in [
        (None, None, false, false, true),
        (Some(2), Some(1), false, false, false),
        (Some(1), Some(1), false, false, false),
        (Some(0), Some(1), false, false, true),
        (Some(2), Some(1), true, false, true),
        (Some(2), Some(1), false, true, true),
    ] {
        let mut early = model("b", "reply", "assistant", "private model form");
        early["metadata"] =
            json!({"user_input_order":answer_order,"inherited_user_message":inherited_answer});
        let mut queued = model("b", "queued", "user", "queued request");
        queued["metadata"] =
            json!({"user_input_order":input_order,"inherited_user_message":inherited_input});
        let rows = [
            start("a"),
            page_user("a", "first", "first request"),
            model("a", "first", "user", "first request"),
            start("b"),
            page_user("b", "queued", "queued request"),
            shown("b", "reply", "visible answer"),
            early,
            queued,
            rollback(),
        ];
        let mut active = vec!["first request"];
        if survives {
            active.push("visible answer");
        }
        check(
            &rows,
            &["first request", "queued request", "visible answer"],
            &active,
        );
    }
}

#[test]
fn successive_rollbacks_and_display_updates_do_not_restore_a_removed_answer() {
    let mut rows = vec![
        start("a"),
        page_user("a", "keep", "keep"),
        model("a", "keep", "user", "keep"),
        start("b"),
        page_user("b", "drop", "drop"),
        model("b", "drop", "user", "drop"),
        model("b", "reply", "assistant", "raw answer"),
        rollback(),
        shown("b", "reply", "late display"),
    ];
    check(&rows, &["keep", "drop", "late display"], &["keep"]);
    rows.extend([
        start("c"),
        page_user("c", "new", "third request"),
        model("c", "new", "user", "third request"),
        shown("b", "reply", "updated display"),
        rollback(),
    ]);
    check(
        &rows,
        &["keep", "drop", "updated display", "third request"],
        &["keep"],
    );
}

#[test]
fn a_surviving_raw_reply_cannot_validate_another_turn_or_item() {
    let rows = [
        start("a"),
        page_user("a", "first", "keep"),
        model("a", "first", "user", "keep"),
        model("a", "shared", "assistant", "same"),
        model("b", "other", "assistant", "same"),
        start("b"),
        page_user("b", "queued", "withdraw"),
        model("b", "queued", "user", "withdraw"),
        shown("b", "shared", "same"),
        rollback(),
    ];
    check(&rows, &["keep", "withdraw", "same"], &["keep"]);
}

#[test]
fn generated_reply_ids_follow_the_matching_model_occurrence() {
    let mut early = model("turn", "raw-reply", "assistant", "answer to queued request");
    early["metadata"] = json!({"user_input_order":2});
    let mut queued = model("turn", "second", "user", "queued request");
    queued["metadata"] = json!({"user_input_order":2});
    let mut rows = vec![
        start("turn"),
        model("turn", "first", "user", "keep"),
        page_user("turn", "item-1", "keep"),
        early,
        shown("turn", "item-2", "answer to queued request"),
        queued,
        page_user("turn", "item-3", "queued request"),
        rollback(),
    ];
    check(
        &rows,
        &["keep", "answer to queued request", "queued request"],
        &["keep"],
    );
    rows.extend([
        start("fresh"),
        model("fresh", "new", "user", "new request"),
        page_user("fresh", "item-4", "new request"),
        model(
            "fresh",
            "new-reply",
            "assistant",
            "answer to queued request",
        ),
        shown("fresh", "item-5", "answer to queued request"),
    ]);
    check(
        &rows,
        &[
            "keep",
            "answer to queued request",
            "queued request",
            "new request",
            "answer to queued request",
        ],
        &["keep", "new request", "answer to queued request"],
    );
}

#[test]
fn indistinguishable_generated_replies_stay_in_recovery_after_a_partial_rollback() {
    let mut early = model("turn", "a", "assistant", "repeated reply");
    early["metadata"] = json!({"user_input_order":1});
    let mut late = model("turn", "b", "assistant", "repeated reply");
    late["metadata"] = json!({"user_input_order":2});
    let mut queued = model("turn", "q", "user", "queued");
    queued["metadata"] = json!({"user_input_order":2});
    let rows = vec![
        start("turn"),
        model("turn", "first", "user", "keep"),
        page_user("turn", "first", "keep"),
        early,
        shown("turn", "item-1", "repeated reply"),
        late,
        queued,
        page_user("turn", "q", "queued"),
        rollback(),
    ];
    let snap = read_with_history_mode(rows, false, Some("paginated")).unwrap();
    let reply = snap
        .events
        .iter()
        .find(|e| e.payload["text"] == "repeated reply")
        .unwrap();
    assert_eq!(reply.payload["omnisession_codex_rollback"], "unverified");
}

#[test]
fn rollback_transfers_only_the_previously_displayed_surviving_reply_version() {
    for with_checkpoint in [false, true] {
        let first = model("turn", "first", "user", "keep");
        let old = model("turn", "reply", "assistant", "original public reply");
        let mut rows = vec![
            start("turn"),
            first.clone(),
            page_user("turn", "first", "keep"),
            old.clone(),
            shown("turn", "reply", "original public reply"),
        ];
        if with_checkpoint {
            rows.push(json!({"type":"compacted","payload":{"message":"","window_number":1,"replacement_history":[first["payload"],old["payload"]]}}));
        }
        rows.extend([
            model("turn", "later", "user", "withdrawn request"),
            page_user("turn", "later", "withdrawn request"),
            model("turn", "reply", "assistant", "withdrawn revised reply"),
            shown("turn", "reply", "withdrawn revised reply"),
            rollback(),
        ]);
        check(
            &rows,
            &["keep", "withdrawn revised reply", "withdrawn request"],
            &["keep", "original public reply"],
        );
        let snap = read_with_history_mode(rows, false, Some("paginated")).unwrap();
        let markdown = omnis_core::render_markdown_export(&snap);
        let (active, previous) = markdown.split_once("## Previous Codex display").unwrap();
        assert!(!active.contains("withdrawn revised reply"));
        assert!(previous.contains("withdrawn revised reply"));
    }
}

#[test]
fn a_surviving_model_only_revision_never_becomes_public_text() {
    let rows = [
        start("turn"),
        model("turn", "first", "user", "keep"),
        page_user("turn", "first", "keep"),
        model("turn", "reply", "assistant", "never publicly displayed"),
        model("turn", "later", "user", "withdrawn request"),
        page_user("turn", "later", "withdrawn request"),
        model("turn", "reply", "assistant", "withdrawn revised reply"),
        shown("turn", "reply", "withdrawn revised reply"),
        rollback(),
    ];
    let snap = read_with_history_mode(rows.to_vec(), false, Some("paginated")).unwrap();
    let history = omnis_core::import_conversation(&snap);
    assert_eq!(
        history
            .messages
            .iter()
            .map(|m| m.text.as_str())
            .collect::<Vec<_>>(),
        ["keep"]
    );
    assert!(
        !serde_json::to_string(&snap)
            .unwrap()
            .contains("never publicly displayed")
    );
}

#[test]
fn rollback_uses_native_call_acceptance_and_keeps_orphaned_receipts_in_recovery() {
    for (kind, order, inherited, late_receipt, call_kept, receipt_kept) in [
        ("function_call", 2, false, false, false, false),
        ("function_call", 1, false, false, true, true),
        ("function_call", 2, true, false, true, true),
        ("custom_tool_call", 2, false, false, true, true),
        ("custom_tool_call", 2, false, true, true, false),
    ] {
        let call = json!({"type":"response_item","metadata":{"user_input_order":order,"inherited_user_message":inherited},
            "payload":{"type":kind,"call_id":"cmd","name":"fixture_lookup","arguments":"{}","input":"fixture"}});
        let receipt = json!({"type":"response_item","metadata":{"user_input_order":5},
            "payload":{"type":format!("{kind}_output"),"call_id":"cmd","output":"original receipt"}});
        let mut queued = model("running", "u2", "user", "queued request");
        queued["metadata"] = json!({"user_input_order":2});
        let mut rows = vec![
            start("running"),
            page_user("running", "u1", "keep"),
            model("running", "u1", "user", "keep"),
            call,
        ];
        if !late_receipt {
            rows.push(receipt.clone());
        }
        rows.extend([page_user("running", "u2", "queued request"), queued]);
        if late_receipt {
            rows.push(receipt);
        }
        rows.extend([rollback(), turn_item("running", &json!({"type":"CommandExecution","id":"cmd","status":"completed","aggregated_output":"later display"}))]);
        for preview in [false, true] {
            let snapshot =
                read_with_history_mode(rows.clone(), preview, Some("paginated")).unwrap();
            let trajectory = omnis_core::import_trajectory(&snapshot);
            assert_eq!(
                trajectory.tool_events,
                usize::from(call_kept) + usize::from(receipt_kept)
            );
            let markdown = omnis_core::render_markdown_export(&snapshot);
            let (active, recovery) = markdown.split_once("## Previous Codex display").unwrap();
            assert_eq!(active.contains("fixture_lookup"), call_kept);
            assert_eq!(active.contains("original receipt"), receipt_kept);
            assert_eq!(recovery.contains("original receipt"), !receipt_kept);
            assert!(!active.contains("queued request"));
        }
    }
}

#[test]
fn a_late_display_uses_the_function_calls_scope_and_keeps_uncertain_matches_in_recovery() {
    for (input_turn, display_turn, expected) in [
        ("running", "running", Some("withdrawn")),
        ("running", "other", None),
        ("running", "rollout-5", Some("unverified")),
        ("rollout-5", "rollout-5", Some("withdrawn")),
    ] {
        let mut queued = model("running", "u2", "user", "queued request");
        queued["metadata"] = json!({"user_input_order":2});
        let rows = vec![
            start(input_turn),
            page_user(input_turn, "u1", "keep"),
            model("running", "u1", "user", "keep"),
            json!({"type":"response_item","metadata":{"user_input_order":2},
                "payload":{"type":"function_call","call_id":"cmd","name":"fixture_lookup","arguments":"{}",
                    "internal_chat_message_metadata_passthrough":{"turn_id":"running"}}}),
            page_user(input_turn, "u2", "queued request"),
            queued,
            rollback(),
            turn_item(
                display_turn,
                &json!({"type":"CommandExecution","id":"cmd","status":"completed","aggregated_output":"late displayed receipt"}),
            ),
        ];
        let snapshot = read_with_history_mode(rows, false, Some("paginated")).unwrap();
        let shown = snapshot
            .events
            .iter()
            .find(|e| e.payload["output"] == "late displayed receipt")
            .unwrap();
        assert_eq!(
            shown.payload["omnisession_codex_rollback"].as_str(),
            expected
        );
        let markdown = omnis_core::render_markdown_export(&snapshot);
        let (active, recovery) = markdown.split_once("## Previous Codex display").unwrap();
        assert_eq!(
            active.contains("late displayed receipt"),
            expected.is_none()
        );
        assert_eq!(
            recovery.contains("late displayed receipt"),
            expected.is_some()
        );
    }
}

#[test]
fn web_searches_without_ids_follow_the_physical_model_boundary() {
    let search = |name| {
        json!({"type":"response_item","metadata":{"user_input_order":3},
        "payload":{"type":"web_search_call","action":{"type":"search","query":name}}})
    };
    let mut queued = model("running", "u2", "user", "queued request");
    queued["metadata"] = json!({"user_input_order": 2});
    let rows = vec![
        start("running"),
        page_user("running", "u1", "keep"),
        model("running", "u1", "user", "keep"),
        page_user("running", "u2", "queued request"),
        search("first query"),
        queued,
        search("second query"),
        rollback(),
    ];
    let snapshot = read_with_history_mode(rows, false, Some("paginated")).unwrap();
    let markdown = omnis_core::render_markdown_export(&snapshot);
    let (active, recovery) = markdown.split_once("## Previous Codex display").unwrap();
    assert!(active.contains("first query"));
    assert!(!active.contains("second query"));
    assert!(recovery.contains("second query"));
}

#[test]
fn reused_call_ids_with_unproven_turn_aliases_leave_the_display_unverified() {
    let call = |turn, order| {
        json!({"type":"response_item","metadata":{"user_input_order":order},
        "payload":{"type":"function_call","call_id":"reused","name":"lookup","arguments":"{}",
            "internal_chat_message_metadata_passthrough":{"turn_id":turn}}})
    };
    let mut queued = model("named", "q", "user", "queued request");
    queued["metadata"] = json!({"user_input_order":2});
    let rows = vec![
        start("named"),
        page_user("named", "first", "keep"),
        model("named", "first", "user", "keep"),
        call("named", 1),
        call("rollout-7", 2),
        page_user("named", "q", "queued request"),
        queued,
        rollback(),
        turn_item(
            "named",
            &json!({"type":"CommandExecution","id":"reused","status":"completed","aggregated_output":"ambiguous receipt"}),
        ),
    ];
    let snapshot = read_with_history_mode(rows, false, Some("paginated")).unwrap();
    let shown = snapshot
        .events
        .iter()
        .find(|e| e.payload["output"] == "ambiguous receipt")
        .unwrap();
    assert_eq!(shown.payload["omnisession_codex_rollback"], "unverified");
}
