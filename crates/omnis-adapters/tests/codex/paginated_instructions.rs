use super::{event, message, page_user, read_with_history_mode, turn_item, visible_text};
use serde_json::{Value, json};

fn raw(payload: Value) -> Value {
    let mut record = json!({"type":"response_item"});
    record["payload"] = payload;
    record
}

fn text_user(parts: &[&str]) -> Value {
    let content: Vec<_> = parts
        .iter()
        .map(|text| json!({"type":"input_text","text":text}))
        .collect();
    raw(json!({"type":"message","role":"user","content":content}))
}

fn active_after(rows: &[Value], displayed: &[&str], active: &[&str]) {
    for preview in [false, true] {
        let mut rows = rows.to_vec();
        rows.push(event("thread_rolled_back", json!({"num_turns":1})));
        let snapshot = read_with_history_mode(rows, preview, Some("paginated")).unwrap();
        assert_eq!(visible_text(&snapshot), displayed);
        let history = omnis_core::import_conversation(&snapshot);
        assert_eq!(
            history
                .messages
                .iter()
                .map(|m| m.text.as_str())
                .collect::<Vec<_>>(),
            active
        );
        let public = serde_json::to_string(&snapshot).unwrap();
        assert!(!public.contains("private-marker"));
    }
}

fn verify_last_input(tail: Value, counts: bool) {
    let mut input = message("user", "first request");
    input["payload"]["id"] = "u1".into();
    input["payload"]["internal_chat_message_metadata_passthrough"] = json!({"turn_id":"a"});
    let rows = [
        event("task_started", json!({"turn_id":"a"})),
        page_user("a", "u1", "first request"),
        input,
        turn_item(
            "a",
            &json!({"type":"AgentMessage","id":"a1","content":[{"type":"Text","text":"first answer"}]}),
        ),
        tail,
    ];
    let displayed = ["first request", "first answer"];
    active_after(&rows, &displayed, if counts { &displayed } else { &[] });
}

#[test]
fn native_and_legacy_agent_instructions_count_without_disclosing_their_contents() {
    let communication = json!({"author":"/root","recipient":"/root/reader","content":"private-marker","trigger_turn":false});
    verify_last_input(
        raw(
            json!({"type":"agent_message","author":"/root","recipient":"/root/reader", "content":[{"type":"input_text","text":"private-marker"}]}),
        ),
        true,
    );
    verify_last_input(
        json!({"type":"inter_agent_communication","payload":communication}),
        true,
    );
    verify_last_input(
        json!({"type":"inter_agent_communication_metadata","payload":{"trigger_turn":true}}),
        false,
    );
    let mut legacy = message("assistant", &communication.to_string());
    legacy["payload"]["content"][0]["type"] = "output_text".into();
    verify_last_input(legacy, true);
    let mut invalid = communication;
    invalid["author"] = "invalid/path".into();
    verify_last_input(message("assistant", &invalid.to_string()), false);
}

#[test]
fn one_context_fragment_excludes_the_entire_raw_user_message_from_rollback() {
    let hook = r#"<hook_prompt extra="value" hook_run_id="id">private-marker</hook_prompt>"#;
    let mut literal_hook = text_user(&[hook]);
    literal_hook["payload"]["internal_chat_message_metadata_passthrough"] =
        json!({"content_item_kinds":["user.text"]});
    let mut ordinary = text_user(&["private-marker"]);
    ordinary["payload"]["internal_chat_message_metadata_passthrough"] =
        json!({"content_item_kinds":["hooks.additional_context"]});
    for (tail, counts) in [
        (
            text_user(&["preface", "<ENVIRONMENT_CONTEXT>cfg</ENVIRONMENT_CONTEXT>"]),
            false,
        ),
        (
            text_user(&["preface", "<environment_context>unfinished"]),
            true,
        ),
        (text_user(&["preface", hook]), false),
        (literal_hook, false),
        (ordinary, true),
        (text_user(&[]), true),
        (text_user(&[""]), true),
        (
            raw(
                json!({"type":"message","role":"user","content":[{"type":"input_image","image_url":"private-marker"}]}),
            ),
            true,
        ),
    ] {
        verify_last_input(tail, counts);
    }
}

#[test]
fn a_failed_local_attachment_matches_its_diagnostic_by_path_and_input_text() {
    let turn = "01948226-0000-7001-8000-000000000001";
    let display = turn_item(
        turn,
        &json!({"type":"UserMessage","id":"01948226-0000-7003-8000-000000000003","content":[
        {"type":"text","text":"inspect"},{"type":"local_image","path":"private-marker.png"}]}),
    );
    for (path, expected) in [
        ("private-marker.png", vec![]),
        ("different.png", vec!["inspect"]),
    ] {
        let diagnostic = format!("Codex could not read the local image at `{path}`: file missing");
        let mut input = text_user(&["inspect", &diagnostic]);
        input["payload"]["id"] = "msg_input_01948226-0000-7002-8000-000000000002".into();
        input["payload"]["internal_chat_message_metadata_passthrough"] = json!({"turn_id":turn});
        active_after(
            &[
                event("task_started", json!({"turn_id":turn})),
                display.clone(),
                input,
            ],
            &["inspect"],
            &expected,
        );
    }
}

#[test]
fn a_literal_file_error_matches_only_its_visible_text() {
    let shown_error = "Codex could not read the local image at `figure.png`: not found";
    for (model_error, expected) in [
        (shown_error, vec![]),
        (
            "Codex could not read the local image at `figure.png`: access denied",
            vec![shown_error],
        ),
    ] {
        let mut input = text_user(&[model_error]);
        input["payload"]["internal_chat_message_metadata_passthrough"] = json!({"turn_id":"a"});
        active_after(
            &[
                event("task_started", json!({"turn_id":"a"})),
                page_user("a", "item-1", shown_error),
                input,
            ],
            &[shown_error],
            &expected,
        );
    }
}

#[test]
fn a_literal_and_an_attachment_cannot_both_confirm_the_same_raw_input() {
    let error = "Codex could not read the local image at `figure.png`: not found";
    let image = turn_item(
        "a",
        &json!({"type":"UserMessage","id":"item-1","content":[{"type":"local_image","path":"figure.png"}]}),
    );
    let mut input = text_user(&[error]);
    input["payload"]["internal_chat_message_metadata_passthrough"] = json!({"turn_id":"a"});
    let rows = [
        event("task_started", json!({"turn_id":"a"})),
        image,
        input,
        page_user("a", "item-2", error),
        event("thread_rolled_back", json!({"num_turns":1})),
    ];
    for preview in [false, true] {
        let snapshot = read_with_history_mode(rows.to_vec(), preview, Some("paginated")).unwrap();
        let statuses: Vec<_> = snapshot
            .events
            .iter()
            .filter(|e| e.kind == omnis_ir::EventKind::MessageUser)
            .map(|e| e.payload["omnisession_codex_rollback"].as_str())
            .collect();
        assert_eq!(statuses, [Some("unverified"), Some("unverified")]);
        assert!(
            omnis_core::import_conversation(&snapshot)
                .messages
                .is_empty()
        );
    }
}

#[test]
fn a_second_rollback_counts_only_instructions_retained_by_the_first() {
    let make_user = |id: &str, text: &str, order| {
        let mut user = message("user", text);
        user["payload"]["id"] = id.into();
        user["payload"]["internal_chat_message_metadata_passthrough"] = json!({"turn_id":"a"});
        user["metadata"] = json!({"user_input_order":order});
        user
    };
    let body = json!({"author":"/root","recipient":"/root/reader","content":"private-marker","trigger_turn":false});
    let mut assistant = message("assistant", &body.to_string());
    assistant["payload"]["content"][0]["type"] = "output_text".into();
    let typed = raw(
        json!({"type":"agent_message","author":"/root","recipient":"/root/reader","content":[{"type":"input_text","text":"private-marker"}]}),
    );
    for (mut instruction, order, inherited, first_survives) in [
        (assistant.clone(), 2, false, false),
        (assistant.clone(), 1, false, true),
        (assistant, 2, true, true),
        (typed, 2, false, true),
    ] {
        instruction["metadata"] =
            json!({"user_input_order":order,"inherited_user_message":inherited});
        let rows = [
            event("task_started", json!({"turn_id":"a"})),
            make_user("u1", "first request", 1),
            page_user("a", "u1", "first request"),
            instruction,
            make_user("u2", "queued request", 2),
            page_user("a", "u2", "queued request"),
            event("thread_rolled_back", json!({"num_turns":1})),
        ];
        let display = ["first request", "queued request"];
        active_after(
            &rows,
            &display,
            if first_survives { &display[..1] } else { &[] },
        );
    }
}

#[test]
fn a_legacy_display_needs_a_matching_model_input_across_a_rollback() {
    let exchange = |turn: &str, text: &str, model: bool, raw_first: bool| {
        let mut rows = vec![event("task_started", json!({"turn_id":turn}))];
        let display = event("user_message", json!({"message":text}));
        let input = message("user", text);
        if model && raw_first {
            rows.push(input.clone());
        }
        rows.push(display);
        if model && !raw_first {
            rows.push(input);
        }
        rows.push(event("task_complete", json!({"turn_id":turn})));
        rows
    };
    for (model, raw_first, extra) in [
        (false, true, false),
        (true, true, false),
        (true, false, false),
        (false, true, true),
    ] {
        let mut rows = exchange("a", "first request", true, raw_first);
        rows.extend(exchange("b", "second request", model, raw_first));
        if extra {
            rows.extend(exchange("c", "third request", true, raw_first));
        }
        rows.push(event("thread_rolled_back", json!({"num_turns":1})));
        for preview in [false, true] {
            let snapshot = read_with_history_mode(rows.clone(), preview, None).unwrap();
            let published = ["first request", "second request"];
            assert_eq!(
                visible_text(&snapshot),
                if extra {
                    &published[..]
                } else {
                    &published[..1]
                }
            );
            assert_eq!(
                snapshot
                    .events
                    .iter()
                    .any(|event| event.payload["native_history_unverified"] == true),
                !model
            );
        }
    }
}

#[test]
fn legacy_private_model_inputs_stay_out_of_exports_and_have_their_own_rollback_position() {
    let body = json!({"author":"/root","recipient":"/root/reader","content":"private-marker","trigger_turn":false}).to_string();
    let mut private = message("assistant", &body);
    private["payload"]["content"][0]["type"] = "output_text".into();
    let mut queued = message("user", "queued request");
    queued["metadata"] = json!({"user_input_order":2});
    let rollback = event("thread_rolled_back", json!({"num_turns":1}));
    for (order, uncertain) in [(1, true), (2, false)] {
        private["metadata"] = json!({"user_input_order":order});
        let rows = [
            message("user", "first request"),
            private.clone(),
            queued.clone(),
            rollback.clone(),
            rollback.clone(),
        ];
        for preview in [false, true] {
            let public = read_with_history_mode(rows[..3].to_vec(), preview, None).unwrap();
            assert_eq!(visible_text(&public), ["first request", "queued request"]);
            let mut recovery = Vec::new();
            omnis_core::write_recovery_handoff(&public, &mut recovery).unwrap();
            assert!(
                !String::from_utf8(recovery)
                    .unwrap()
                    .contains("private-marker")
            );
            assert!(!omnis_core::render_markdown_export(&public).contains("private-marker"));
            let snapshot = read_with_history_mode(rows.to_vec(), preview, None).unwrap();
            assert!(visible_text(&snapshot).is_empty());
            assert_eq!(
                snapshot
                    .events
                    .iter()
                    .any(|e| e.payload["native_history_unverified"] == true),
                uncertain
            );
        }
    }
    for (count, uncertain) in [(2, true), (3, false), (20, false)] {
        let reset = read_with_history_mode(
            vec![
                message("user", "first"),
                queued.clone(),
                private.clone(),
                event("thread_rolled_back", json!({"num_turns":count})),
            ],
            false,
            None,
        )
        .unwrap();
        assert!(visible_text(&reset).is_empty());
        assert_eq!(
            reset
                .events
                .iter()
                .any(|e| e.payload["native_history_unverified"] == true),
            uncertain
        );
    }
    let independently_published = turn_item(
        "a",
        &json!({"type":"AgentMessage","id":"public","content":[{"type":"Text","text":body}]}),
    );
    let public =
        read_with_history_mode(vec![private, independently_published], false, None).unwrap();
    assert_eq!(visible_text(&public), [body.as_str()]);
}

#[test]
fn an_unstamped_checkpoint_user_needs_one_exact_original_copy() {
    let first = "01948226-0000-7001-8000-000000000001";
    let second = "01948226-0000-7001-8000-000000000002";
    let id = "msg_input_01948226-0000-7002-8000-000000000002";
    let model = |turn: &str, text: &str| {
        let mut input = message("user", text);
        input["payload"]["id"] = id.into();
        input["payload"]["internal_chat_message_metadata_passthrough"] = json!({"turn_id":turn});
        input
    };
    let source = model(first, "original request");
    let display = page_user(first, "item-1", "original request");
    let mut unstamped = source["payload"].clone();
    unstamped
        .as_object_mut()
        .unwrap()
        .remove("internal_chat_message_metadata_passthrough");
    let mut changed = unstamped.clone();
    changed["content"][0]["text"] = "different request".into();
    let wrong_turn = model(second, "original request")["payload"].clone();
    let pair = vec![source, display.clone()];
    let mut reused = pair.clone();
    reused.extend([
        model(second, "another request"),
        page_user(second, "item-2", "another request"),
    ]);
    let mut extra_display = pair.clone();
    extra_display.push(page_user(second, id, "another request"));
    for (origin, retained, active) in [
        (pair.clone(), vec![unstamped.clone()], true),
        (pair.clone(), vec![changed], false),
        (pair.clone(), vec![wrong_turn], false),
        (pair, vec![unstamped.clone(), unstamped.clone()], false),
        (reused, vec![unstamped.clone()], false),
        (extra_display, vec![unstamped.clone()], false),
        (vec![display], vec![unstamped], false),
    ] {
        let mut records = origin;
        records.push(json!({"type":"compacted","payload":{"message":"","window_number":1,"replacement_history":retained}}));
        for preview in [false, true] {
            let snapshot =
                read_with_history_mode(records.clone(), preview, Some("paginated")).unwrap();
            assert!(visible_text(&snapshot).contains(&"original request"));
            assert_eq!(
                omnis_core::import_conversation(&snapshot)
                    .messages
                    .iter()
                    .any(|m| m.text == "original request"),
                active
            );
        }
    }
}
