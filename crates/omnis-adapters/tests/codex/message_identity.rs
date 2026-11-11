use super::{event, message, read_records_with_preview, turn_item, visible_text};
use serde_json::{Value, json};

fn model(turn: &str, id: &str, text: &str) -> Value {
    let mut row = message("assistant", text);
    row["payload"]["id"] = id.into();
    row["payload"]["content"][0]["type"] = "output_text".into();
    row["payload"]["internal_chat_message_metadata_passthrough"] = json!({"turn_id":turn});
    row
}

fn display(turn: &str, id: &str, text: &str) -> Value {
    turn_item(
        turn,
        &json!({"type":"AgentMessage","id":id,"content":[{"type":"Text","text":text}]}),
    )
}

#[test]
fn shared_message_ids_select_display_text_in_either_order() {
    for display_first in [false, true] {
        let answer = display("first", "msg_shared", "final answer");
        let underlying = model(
            "first",
            "msg_shared",
            "final answer<oai-mem-citation>private note</oai-mem-citation>",
        );
        let pair = if display_first {
            [answer, underlying]
        } else {
            [underlying, answer]
        };
        let mut rows = vec![event("task_started", json!({"turn_id":"first"}))];
        rows.extend(pair);
        rows.push(display("first", "msg_shared", "revised final answer"));
        rows.push(event("task_complete", json!({"turn_id":"first"})));
        rows.push(event("task_started", json!({"turn_id":"second"})));
        rows.push(display(
            "second",
            "msg_shared",
            "answer in a different turn",
        ));
        rows.push(model(
            "first",
            "msg_shared",
            "late model copy for the first turn",
        ));
        for preview in [false, true] {
            let snapshot = read_records_with_preview(rows.clone(), preview).unwrap();
            assert_eq!(
                visible_text(&snapshot),
                ["revised final answer", "answer in a different turn"]
            );
        }
    }
}

#[test]
fn conflicting_native_ids_preserve_repeated_messages() {
    let rows = vec![
        event("task_started", json!({"turn_id":"turn-1"})),
        model("turn-1", "msg_a", "repeat"),
        display("turn-1", "msg_b", "repeat"),
        display("turn-1", "msg_a", "changed A"),
        model("turn-1", "msg_b", "model copy B"),
    ];
    for preview in [false, true] {
        let snapshot = read_records_with_preview(rows.clone(), preview).unwrap();
        assert_eq!(visible_text(&snapshot), ["changed A", "repeat"]);
    }
}

#[test]
fn migrated_citation_matches_the_display_without_rewriting_unpaired_raw_text() {
    const RAW: &str = "a<oai-mem-citation>private</oai-mem-citation>b<oai-mem-citation>unfinished";
    for display_first in [false, true] {
        let mut shown = display("rollout-1", "item-2", "ab");
        shown["payload"]["item"]["memory_citation"] =
            json!({"entries":[],"rolloutIds":["00000000-0000-4000-8000-000000000001"]});
        let raw = model("rollout-1", "msg_original", RAW);
        let mut rows = vec![event("task_started", json!({"turn_id":"rollout-1"}))];
        rows.extend(if display_first {
            [shown, raw]
        } else {
            [raw, shown]
        });
        rows.push(message("user", RAW));
        rows.push(model("rollout-1", "msg_unpaired", RAW));
        for preview in [false, true] {
            let snapshot = read_records_with_preview(rows.clone(), preview).unwrap();
            assert_eq!(visible_text(&snapshot), ["ab", RAW, RAW]);
        }
    }
}

#[test]
fn an_unstamped_model_id_can_match_its_only_pending_display_item() {
    let mut unstamped = model("", "msg_stable", "model annotation");
    unstamped["payload"]
        .as_object_mut()
        .unwrap()
        .remove("internal_chat_message_metadata_passthrough");
    for records in [
        vec![
            unstamped.clone(),
            event("task_started", json!({"turn_id":"rollout-2"})),
            display("rollout-2", "msg_stable", "visible"),
        ],
        vec![display("explicit", "msg_stable", "visible"), unstamped],
    ] {
        for preview in [false, true] {
            let snapshot = read_records_with_preview(records.clone(), preview).unwrap();
            assert_eq!(visible_text(&snapshot), ["visible"]);
        }
    }
}

#[test]
fn citation_only_model_text_does_not_reappear_after_a_blank_display() {
    let raw = model(
        "first",
        "msg_empty",
        "<oai-mem-citation>hidden</oai-mem-citation>",
    );
    let shown = display("first", "msg_empty", "");
    for records in [vec![raw.clone(), shown.clone()], vec![shown, raw]] {
        let snapshot = read_records_with_preview(records, false).unwrap();
        assert!(visible_text(&snapshot).iter().all(|text| text.is_empty()));
    }
}

#[test]
fn unfinished_citations_match_unannotated_display_only_in_a_compatible_turn() {
    const RAW: &str = "Answer.<oai-mem-citation>private unfinished suffix";
    let raw = model("turn-1", "msg_native", RAW);
    for shown in [
        display("turn-1", "item-2", "Answer."),
        event("agent_message", json!({"message":"Answer."})),
    ] {
        for (first, second) in [(&shown, &raw), (&raw, &shown)] {
            let records = vec![
                event("task_started", json!({"turn_id":"turn-1"})),
                first.clone(),
                second.clone(),
            ];
            for preview in [true, false] {
                let snapshot = read_records_with_preview(records.clone(), preview).unwrap();
                assert_eq!(visible_text(&snapshot), ["Answer."]);
                let imported = omnis_core::import_conversation(&snapshot);
                assert_eq!(imported.messages.len(), 1);
                assert_eq!(imported.messages[0].text, "Answer.");
                assert!(
                    !serde_json::to_string(&snapshot)
                        .unwrap()
                        .contains("private unfinished")
                );
            }
        }
    }
    let unscoped = vec![
        message("assistant", RAW),
        event("agent_message", json!({"message":"Answer."})),
    ];
    assert_eq!(
        visible_text(&read_records_with_preview(unscoped, false).unwrap()),
        [RAW, "Answer."]
    );
    let separate = vec![
        event("task_started", json!({"turn_id":"turn-1"})),
        model("turn-1", "msg_other", RAW),
        display("turn-1", "msg_distinct", "Answer."),
        message("user", RAW),
        event("task_complete", json!({"turn_id":"turn-1"})),
        event("task_started", json!({"turn_id":"turn-2"})),
        display("turn-2", "item-3", "Answer."),
        model("turn-2", "msg_only", "raw-only<oai-mem-citation>retained"),
    ];
    let snapshot = read_records_with_preview(separate, false).unwrap();
    assert_eq!(
        visible_text(&snapshot),
        [
            RAW,
            "Answer.",
            RAW,
            "Answer.",
            "raw-only<oai-mem-citation>retained"
        ]
    );
}

#[test]
fn unannotated_displays_follow_the_source_order_when_cited_and_plain_replies_look_alike() {
    let raw = vec![
        model("turn-1", "msg_plain", "Done."),
        model("turn-1", "msg_cited", "Done.<oai-mem-citation>private"),
    ];
    let shown = vec![
        display("turn-1", "item-1", "Done."),
        display("turn-1", "item-2", "Done."),
    ];
    for (first, second) in [(&raw, &shown), (&shown, &raw)] {
        let mut records = vec![event("task_started", json!({"turn_id":"turn-1"}))];
        records.extend(first.iter().chain(second).cloned());
        records.push(display("turn-1", "item-2", "Updated second reply."));
        let snapshot = read_records_with_preview(records, false).unwrap();
        assert_eq!(visible_text(&snapshot), ["Done.", "Updated second reply."]);
    }
}
