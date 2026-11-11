use super::{
    event, message, page_user, published_text, read_with_history_mode, turn_item, visible_text,
};
use omnis_ir::EventKind;
use serde_json::{Value, json};

fn user(id: &str, text: &str, order: u64) -> Value {
    let mut raw = message("user", text);
    raw["payload"]["id"] = id.into();
    raw["payload"]["internal_chat_message_metadata_passthrough"] = json!({"turn_id":"t"});
    raw["metadata"] = json!({"user_input_order":order});
    raw
}

fn question(text: &str) -> Value {
    turn_item(
        "t",
        &json!({"type":"AgentMessage","id":"async-call","phase":"final_answer","delivery":"async",
        "questions":[{"title":text,"options":null}],"content":[{"type":"Text","text":text}]}),
    )
}

fn call(order: u64) -> Value {
    json!({"type":"response_item","metadata":{"user_input_order":order},"payload":{"type":"function_call",
        "id":"fc_1","call_id":"async-call","name":"request_user_input_async","arguments":"{\"questions\":[{\"title\":\"Which workspace?\",\"options\":null}]}"}})
}

fn receipt() -> Value {
    json!({"type":"response_item","payload":{"type":"function_call_output","id":"fco_2",
        "call_id":"async-call","output":"{\"accepted\":true}"}})
}

#[test]
fn an_async_question_and_receipt_keep_separate_histories_in_either_order() {
    for mode in [None, Some("paginated")] {
        for question_first in [false, true] {
            let mut rows = vec![
                event("task_started", json!({"turn_id":"t"})),
                user("user", "choose a workspace", 1),
                page_user("t", "user", "choose a workspace"),
                call(1),
            ];
            let ask = question("Which workspace?");
            rows.extend(if question_first {
                [ask, receipt()]
            } else {
                [receipt(), ask]
            });
            rows.push(question("Which workspace should I use?"));
            let snap = read_with_history_mode(rows, false, mode).unwrap();
            assert_eq!(
                visible_text(&snap),
                ["choose a workspace", "Which workspace should I use?"]
            );
            let results = snap
                .events
                .iter()
                .filter(|e| e.kind == EventKind::ToolCompleted)
                .collect::<Vec<_>>();
            assert_eq!(results.len(), 1);
            assert_eq!(results[0].payload["output"], r#"{"accepted":true}"#);
            assert_eq!(omnis_core::import_trajectory(&snap).tool_events, 2);
        }
    }
}

#[test]
fn an_async_question_follows_its_native_calls_rollback_even_if_displayed_early() {
    for order in [1, 2] {
        let queued = user("b", "queued", 2);
        let rows = vec![
            event("task_started", json!({"turn_id":"t"})),
            user("a", "first", 1),
            page_user("t", "a", "first"),
            call(order),
            question("Which workspace?"),
            receipt(),
            queued,
            page_user("t", "b", "queued"),
            event("thread_rolled_back", json!({"num_turns":1})),
        ];
        let snap = read_with_history_mode(rows, false, Some("paginated")).unwrap();
        let active = omnis_core::import_trajectory(&snap);
        assert_eq!(active.tool_events, if order == 1 { 2 } else { 0 });
        let conversation = omnis_core::import_conversation(&snap);
        assert_eq!(
            conversation
                .messages
                .iter()
                .any(|m| m.text == "Which workspace?"),
            order == 1
        );
        assert_eq!(visible_text(&snap), ["first", "Which workspace?", "queued"]);
    }
}

#[test]
fn an_async_question_survives_an_exact_checkpoint_copy_of_its_call() {
    let first = user("a", "first", 1);
    let original = call(1);
    for (altered, revised) in [(false, false), (false, true), (true, false)] {
        let mut retained = original["payload"].clone();
        if altered {
            retained["arguments"] = r#"{"questions":[{"title":"model-only suggestion"}]}"#.into();
        }
        let mut rows = vec![
            event("task_started", json!({"turn_id":"t"})),
            first.clone(),
            page_user("t", "a", "first"),
            original.clone(),
            question("Which workspace?"),
            receipt(),
            json!({"type":"compacted","payload":{"window_number":1,"message":"","replacement_history":[first["payload"],retained,receipt()["payload"]]}}),
        ];
        if revised {
            rows.push(question("Later display"));
        }
        rows.push(page_user("fresh", "new", "next request"));
        for preview in [false, true] {
            let snap = read_with_history_mode(rows.clone(), preview, Some("paginated")).unwrap();
            let expected_question = if revised {
                "Later display"
            } else {
                "Which workspace?"
            };
            assert_eq!(
                published_text(&snap),
                ["first", expected_question, "next request"]
            );
            let active = omnis_core::import_conversation(&snap);
            let messages: Vec<_> = active
                .messages
                .iter()
                .map(|message| message.text.as_str())
                .collect();
            if altered {
                assert_eq!(messages, ["first", "next request"]);
            } else {
                assert_eq!(messages, ["first", "Which workspace?", "next request"]);
            }
        }
    }
}
