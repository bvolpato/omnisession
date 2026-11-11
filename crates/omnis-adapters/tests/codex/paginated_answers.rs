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
fn invalid_agent_message_revisions_preserve_the_last_published_reply() {
    let mut rows = vec![
        start("turn"),
        page_user("turn", "u", "request"),
        model("turn", "a", "assistant", "published reply"),
        shown("turn", "a", "published reply"),
    ];
    for (field, value) in [
        (
            "content",
            json!([{"type":"Text","text":"partial invalid edit"},{"type":"Text","text":7}]),
        ),
        ("phase", json!("incomplete")),
        ("delivery", json!("unknown")),
        (
            "questions",
            json!([{"title":"question","options":[],"unknown":0}]),
        ),
        (
            "memory_citation",
            json!({"entries":[{"path":"f","lineStart":-1,"lineEnd":1,"note":"x"}],"rolloutIds":[]}),
        ),
    ] {
        let mut edit = shown("turn", "a", "partial invalid edit");
        edit["payload"]["item"][field] = value;
        rows.push(edit);
    }
    check(
        &rows,
        &["request", "published reply"],
        &["request", "published reply"],
    );
    let snapshot = read_with_history_mode(rows.clone(), false, Some("paginated")).unwrap();
    assert!(
        !serde_json::to_string(&snapshot)
            .unwrap()
            .contains("partial invalid edit")
    );
    rows.push(turn_item("turn", &json!({"type":"AgentMessage","id":"a","content":[["Text","valid "],{"type":"Text","text":"revision","future":true}],"phase":"partial_answer","future":12,"questions":[["title",null]],"memory_citation":[[["f",0,1,"note"]],["demo"]]})));
    check(
        &rows,
        &["request", "valid revision"],
        &["request", "valid revision"],
    );
    let user = json!(["UserMessage", "u2", null, [{"type":"text","text":"next request","text_elements":[]}]]);
    let positional_reply = json!(["AgentMessage", "a", [["Text", "last revision"]]]);
    rows.push(turn_item("turn", &user));
    rows.push(json!({"type":"event_msg","payload":["item_completed",super::SESSION_ID,"turn",positional_reply]}));
    check(
        &rows,
        &["request", "last revision", "next request"],
        &["request", "last revision", "next request"],
    );
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
fn unstamped_reply_needs_a_unique_public_id_and_spelling() {
    for (order, ambiguous) in [(1, false), (2, false), (2, true)] {
        let mut raw = model("a", "shared", "assistant", "public reply");
        raw["payload"]
            .as_object_mut()
            .unwrap()
            .remove("internal_chat_message_metadata_passthrough");
        raw["metadata"] = json!({"user_input_order":order});
        let mut queued = model("a", "later", "user", "later");
        queued["metadata"] = json!({"user_input_order":2});
        let mut rows = vec![
            start("a"),
            model("a", "keep", "user", "keep"),
            page_user("a", "keep", "keep"),
            raw,
            shown("a", "shared", "public reply"),
        ];
        if ambiguous {
            rows.push(shown("b", "shared", "public reply"));
        }
        rows.extend([queued, page_user("a", "later", "later"), rollback()]);
        let snap = read_with_history_mode(rows, false, Some("paginated")).unwrap();
        let statuses = snap
            .events
            .iter()
            .filter(|event| event.payload["text"] == "public reply")
            .map(|event| event.payload["omnisession_codex_rollback"].as_str())
            .collect::<Vec<_>>();
        if ambiguous {
            assert_eq!(statuses, [Some("unverified"), Some("unverified")]);
        } else {
            assert_eq!(statuses, [(order == 2).then_some("withdrawn")]);
        }
    }
}

#[test]
fn an_unidentified_withdrawn_raw_only_marks_its_neighbor_uncertain() {
    let mut rows = vec![
        start("a"),
        model("a", "u", "user", "keep"),
        page_user("a", "u", "keep"),
    ];
    let mut raw = model("a", "unused", "assistant", "same answer");
    let payload = raw["payload"].as_object_mut().unwrap();
    payload.remove("id");
    payload.remove("internal_chat_message_metadata_passthrough");
    raw["metadata"] = json!({"user_input_order":2});
    let mut later = model("a", "q", "user", "queued");
    later["metadata"] = json!({"user_input_order":2});
    rows.extend([
        raw,
        shown("a", "typed", "same answer"),
        later,
        page_user("a", "q", "queued"),
        rollback(),
        start("fresh"),
        model("fresh", "new", "user", "new"),
        page_user("fresh", "new", "new"),
        shown("fresh", "fresh-reply", "same answer"),
    ]);
    let snap = read_with_history_mode(rows, false, Some("paginated")).unwrap();
    let flags = snap
        .events
        .iter()
        .filter(|e| e.payload["text"] == "same answer")
        .map(|e| e.payload["omnisession_codex_rollback"].as_str())
        .collect::<Vec<_>>();
    assert_eq!(flags, [Some("unverified"), None]);
}

#[test]
fn a_checkpoint_can_witness_a_generated_reply_without_a_previous_model_copy() {
    let first = model("a", "u", "user", "keep");
    let reply = model("a", "native", "assistant", "public reply");
    for (raw_copy, second_display, checkpoint_turn, checkpoint_text, active) in [
        (true, false, "a", "public reply", true),
        (false, false, "a", "public reply", true),
        (false, true, "a", "public reply", false),
        (false, false, "b", "public reply", false),
        (false, false, "a", "model-only reply", false),
    ] {
        let mut rows = vec![start("a"), first.clone(), page_user("a", "u", "keep")];
        if raw_copy {
            rows.push(reply.clone());
        }
        rows.push(shown("a", "item-2", "public reply"));
        if second_display {
            rows.push(shown("a", "item-3", "public reply"));
        }
        let replacement = model(checkpoint_turn, "native", "assistant", checkpoint_text);
        rows.push(json!({"type":"compacted","payload":{"message":"","window_number":1,"replacement_history":[first["payload"],replacement["payload"]]}}));
        let snap = read_with_history_mode(rows, false, Some("paginated")).unwrap();
        let active_text = omnis_core::import_conversation(&snap)
            .messages
            .into_iter()
            .map(|m| m.text)
            .collect::<Vec<_>>();
        if active {
            assert_eq!(active_text, ["keep", "public reply"]);
        } else {
            assert_eq!(active_text, ["keep"]);
        }
        assert!(
            !serde_json::to_string(&snap)
                .unwrap()
                .contains("model-only reply")
        );
    }
}

#[test]
fn rollback_transfers_only_the_previously_displayed_surviving_reply_version() {
    for (with_checkpoint, original_model_copy) in [(false, true), (true, true), (true, false)] {
        let first = model("turn", "first", "user", "keep");
        let old = model("turn", "reply", "assistant", "original public reply");
        let mut rows = vec![
            start("turn"),
            first.clone(),
            page_user("turn", "first", "keep"),
        ];
        if original_model_copy {
            rows.push(old.clone());
        }
        rows.push(shown("turn", "reply", "original public reply"));
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
fn a_verified_post_compaction_reply_follows_its_request_only_in_active_history() {
    #[derive(Clone, Copy, Debug, PartialEq)]
    enum Revision {
        Published,
        FirstPublication,
        DisplayOnly,
        AfterRollback,
        Withdrawn,
    }
    use Revision::{AfterRollback, DisplayOnly, FirstPublication, Published, Withdrawn};

    let t = "turn";
    let first = model(t, "first", "user", "request one");
    let checkpoints = [
        json!({"window_number":1,"message":"","replacement_history":[first["payload"]]}),
        json!({"message":"source summary"}),
    ];
    for checkpoint in checkpoints {
        for revision in [
            Published,
            FirstPublication,
            DisplayOnly,
            AfterRollback,
            Withdrawn,
        ] {
            let mut rows = vec![
                start(t),
                first.clone(),
                page_user(t, "first", "request one"),
                model(t, "answer", "assistant", "original body"),
            ];
            let mut public = vec!["request one"];
            if revision != FirstPublication {
                rows.push(shown(t, "answer", "original body"));
                public.push("revised answer");
            }
            rows.push(json!({"type":"compacted","payload":checkpoint}));
            if revision == AfterRollback {
                rows.extend([
                    model(t, "removed", "user", "discarded steer"),
                    page_user(t, "removed", "discarded steer"),
                    rollback(),
                ]);
                public.push("discarded steer");
            }
            rows.extend([
                model(t, "second", "user", "request two"),
                page_user(t, "second", "request two"),
            ]);
            public.push("request two");
            let revised = model(t, "answer", "assistant", "revised answer");
            let display = shown(t, "answer", "revised answer");
            match revision {
                Published => rows.extend([display, revised]),
                DisplayOnly => rows.push(display),
                Withdrawn => rows.extend([revised, rollback(), display]),
                FirstPublication | AfterRollback => rows.extend([revised, display]),
            }
            if revision == FirstPublication {
                public.push("revised answer");
            }

            let mut expected = vec!["request one"];
            if checkpoint["message"] == "source summary" {
                expected.push("source summary");
            }
            if revision != Withdrawn {
                expected.push("request two");
            }
            if !matches!(revision, DisplayOnly | Withdrawn) {
                expected.push("revised answer");
            }

            for preview in [false, true] {
                let snapshot =
                    read_with_history_mode(rows.clone(), preview, Some("paginated")).unwrap();
                assert_eq!(
                    visible_text(&snapshot),
                    public,
                    "{revision:?}, preview={preview}"
                );
                if revision == FirstPublication {
                    assert!(
                        !serde_json::to_string(&snapshot)
                            .unwrap()
                            .contains("original body")
                    );
                }
                let active = omnis_core::import_conversation(&snapshot);
                let messages: Vec<_> = active
                    .messages
                    .iter()
                    .map(|message| message.text.lines().last().unwrap())
                    .collect();
                assert_eq!(
                    messages, expected,
                    "{checkpoint}, {revision:?}, preview={preview}"
                );
            }
        }
    }
}

#[test]
fn a_migrated_reply_uses_the_final_confirmed_turn() {
    let generated = "rollout-2";
    let request = |turn, number, text| {
        model(
            turn,
            &format!("msg_input_01948226-0000-7000-8000-{number:012x}"),
            "user",
            text,
        )
    };
    let first = request("original", 1, "first request");
    let mut rows = vec![
        first.clone(),
        start(generated),
        page_user(generated, "item-1", "first request"),
        model("original", "reply", "assistant", "original reply"),
        shown(generated, "reply", "original reply"),
        json!({"type":"compacted","payload":{"message":"","window_number":1,"replacement_history":[first["payload"]]}}),
        request("original", 2, "second request"),
        page_user(generated, "item-3", "second request"),
        model("original", "reply", "assistant", "revised reply"),
        shown(generated, "reply", "revised reply"),
    ];
    check(
        &rows,
        &["first request", "revised reply", "second request"],
        &["first request", "second request", "revised reply"],
    );
    rows.extend([
        start("other"),
        model("other", "temporary", "user", "temporary request"),
        page_user("other", "temporary", "temporary request"),
        rollback(),
        start(generated),
        request("other", 3, "other request"),
        page_user(generated, "item-8", "other request"),
    ]);
    check(
        &rows,
        &[
            "first request",
            "revised reply",
            "second request",
            "temporary request",
            "other request",
        ],
        &["first request", "second request", "other request"],
    );
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
        rows.extend([rollback(), turn_item("running", &json!({"type":"CommandExecution","id":"cmd","command":["example"],"cwd":"file:///workspace","parsed_cmd":[],"source":"agent","status":"completed","aggregated_output":"later display"}))]);
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
                &json!({"type":"CommandExecution","id":"cmd","command":["example"],"cwd":"file:///workspace","parsed_cmd":[],"source":"agent","status":"completed","aggregated_output":"late displayed receipt"}),
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
            &json!({"type":"CommandExecution","id":"reused","command":["example"],"cwd":"file:///workspace","parsed_cmd":[],"source":"agent","status":"completed","aggregated_output":"ambiguous receipt"}),
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
