use super::*;

fn hook_request(texts: &[&str], metadata: Value) -> Value {
    let content: Vec<_> = texts
        .iter()
        .map(|text| json!({"type":"input_text","text":text}))
        .collect();
    let mut record = json!({"type":"response_item","payload":{
        "type":"message","id":"msg-hook","role":"user","content":content
    }});
    record["payload"]["internal_chat_message_metadata_passthrough"] = metadata;
    record
}

const HOOK: &str = r#"<hook_prompt hook_run_id="run-a">Private &amp; more.</hook_prompt>"#;

#[test]
fn a_compacted_multipart_request_with_a_hook_keeps_its_original_turn() {
    let mixed = hook_request(
        &["first ", HOOK, "second"],
        json!({"content_item_kinds":["user.text","hooks.additional_context","user.text"]}),
    );
    let hidden = hook_request(
        &[HOOK],
        json!({"content_item_kinds":["hooks.additional_context"]}),
    );
    let mut records = vec![
        message("user", "keep"),
        page_user("discard", "request", "first second"),
        json!({"type":"compacted","payload":{"message":"","replacement_history":[mixed["payload"],hidden["payload"]]}}),
    ];
    let snapshot = read_records(records.clone()).unwrap();
    let imported = omnis_core::import_conversation(&snapshot);
    assert_eq!(
        imported
            .messages
            .iter()
            .map(|m| m.text.as_str())
            .collect::<Vec<_>>(),
        ["first \nsecond"]
    );
    records.push(event("thread_rolled_back", json!({"num_turns":1})));
    for preview in [false, true] {
        assert_eq!(
            visible_text(&read_records_with_preview(records.clone(), preview).unwrap()),
            ["keep"]
        );
    }
}

#[test]
fn stop_hook_copies_do_not_reach_preview_transfer_or_rollback_history() {
    let raw = hook_request(
        &[
            HOOK,
            "<hook_prompt hook_run_id='run-b'>Other private text.</hook_prompt>",
        ],
        Value::Null,
    );
    let display = completed(&json!({"type":"HookPrompt","id":"msg-hook","fragments":[
        {"text":"Private & more.","hookRunId":"run-a"}, {"text":"Other private text.","hookRunId":"run-b"}
    ]}));
    for hook in [
        vec![raw.clone()],
        vec![raw.clone(), display.clone()],
        vec![display, raw],
    ] {
        let mut records = vec![
            message("user", "keep"),
            message("assistant", "kept answer"),
            message("user", "discard"),
        ];
        records.extend(hook);
        for preview in [false, true] {
            let snapshot = read_records_with_preview(records.clone(), preview).unwrap();
            assert_eq!(visible_text(&snapshot), ["keep", "kept answer", "discard"]);
            let omission = snapshot
                .events
                .iter()
                .find(|event| {
                    event.source.raw_record_type.as_deref()
                        == Some("omnisession.codex_hook_input_omitted")
                })
                .unwrap();
            assert_eq!(omission.payload["content_omitted"].as_bool(), Some(true));
            assert!(!omission.payload.to_string().contains("Private"));
            let imported = omnis_core::import_conversation(&snapshot);
            assert_eq!(
                imported
                    .messages
                    .iter()
                    .map(|message| message.text.as_str())
                    .collect::<Vec<_>>(),
                ["keep", "kept answer", "discard"]
            );
            let mut rolled_back = records.clone();
            rolled_back.push(event("thread_rolled_back", json!({"num_turns":1})));
            assert_eq!(
                visible_text(&read_records_with_preview(rolled_back, preview).unwrap()),
                ["keep", "kept answer"]
            );
        }
    }
}

#[test]
fn hook_attribution_preserves_real_user_parts_and_literal_xml() {
    let hook = json!({"provenance":{"type":"harness","hook":true}});
    let user = json!({"provenance":{"type":"user"}});
    let private = "private hook text";
    let hidden = hook_request(&[private], json!({"content_item_metadata":[hook]}));
    let mixed = hook_request(
        &["from the user", private],
        json!({"content_item_metadata":[user,hook]}),
    );
    let typed = hook_request(
        &[HOOK],
        json!({"content_item_metadata":[user],"content_item_kinds":["user.text"]}),
    );
    let kind_only = hook_request(&[HOOK], json!({"content_item_kinds":["user.text"]}));
    let mut admitted = hook_request(&[HOOK], Value::Null);
    admitted["payload"]["id"] = json!("msg_input_00000000-0000-4000-8000-000000000001");
    let categorized = hook_request(
        &[private],
        json!({"content_item_kinds":["hooks.additional_context"]}),
    );
    for (records, expected) in [
        (vec![hidden, mixed, categorized], "from the user"),
        (vec![typed], HOOK),
        (vec![kind_only], HOOK),
        (vec![admitted], HOOK),
    ] {
        assert_eq!(visible_text(&read_records(records).unwrap()), [expected]);
    }
    for literal in [
        "please explain <hook_prompt hook_run_id=\"run-a\">text</hook_prompt>",
        "<hook_prompt>missing a run ID</hook_prompt>",
        "<hook_prompt hook_run_id=\" \">blank run ID</hook_prompt>",
        "<hook_prompt hook_run_id=\"abc\">example</hook_prompt> with a suffix",
    ] {
        assert_eq!(
            visible_text(&read_records(vec![message("user", literal)]).unwrap()),
            [literal]
        );
    }
    assert_eq!(
        visible_text(
            &read_records(vec![hook_request(&["explain this XML", HOOK], Value::Null)]).unwrap()
        ),
        [format!("explain this XML\n{HOOK}")]
    );
}
