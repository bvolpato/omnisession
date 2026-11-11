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
