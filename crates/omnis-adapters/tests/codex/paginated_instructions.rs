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
