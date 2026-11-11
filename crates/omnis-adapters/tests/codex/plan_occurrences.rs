use super::{event, page_user, published_text, read_with_history_mode, turn_item};
use serde_json::{Value, json};

const TURN: &str = "planning";

fn begin() -> Value {
    event(
        "task_started",
        json!({"turn_id":TURN,"collaboration_mode_kind":"plan"}),
    )
}

fn model(id: &str, role: &str, text: &str, order: u64) -> Value {
    let kind = if role == "user" {
        "input_text"
    } else {
        "output_text"
    };
    json!({"type":"response_item","metadata":{"user_input_order":order},"payload":{
        "type":"message","id":id,"role":role,"internal_chat_message_metadata_passthrough":{"turn_id":TURN},
        "content":[{"type":kind,"text":text}]}})
}

fn shown(text: &str) -> Value {
    turn_item(
        TURN,
        &json!({"type":"AgentMessage","id":"reply","content":[{"type":"Text","text":text}]}),
    )
}

fn plan(text: &str) -> Value {
    turn_item(TURN, &json!({"type":"Plan","id":"item-9","text":text}))
}

fn checkpoint(user: &Value, reply: &Value) -> Value {
    json!({"type":"compacted","payload":{"message":"","window_number":1,
        "replacement_history":[user["payload"],reply["payload"]]}})
}

fn check(rows: &[Value], published: &[&str], active: &[&str]) {
    for preview in [false, true] {
        let snapshot = read_with_history_mode(rows.to_vec(), preview, Some("paginated")).unwrap();
        assert_eq!(published_text(&snapshot), published);
        let messages = omnis_core::import_conversation(&snapshot).messages;
        assert_eq!(
            messages.iter().map(|m| m.text.as_str()).collect::<Vec<_>>(),
            active
        );
    }
}

#[test]
fn accepted_revisions_of_a_comment_and_plan_keep_both_native_positions() {
    let first = model("u1", "user", "initial", 1);
    let reply = model(
        "reply",
        "assistant",
        "First.\n<proposed_plan>\nFirst plan\n</proposed_plan>",
        1,
    );
    let mut rows = vec![
        begin(),
        first.clone(),
        page_user(TURN, "u1", "initial"),
        reply.clone(),
        shown("First.\n"),
        plan("First plan\n"),
        checkpoint(&first, &reply),
        model("u2", "user", "steer", 2),
        page_user(TURN, "u2", "steer"),
        model(
            "reply",
            "assistant",
            "Revised.\n<proposed_plan>\nRevised plan\n</proposed_plan>",
            2,
        ),
        shown("Revised.\n"),
        plan("Revised plan\n"),
    ];
    let published = ["initial", "Revised.\n", "Revised plan\n", "steer"];
    check(
        &rows,
        &published,
        &[
            "initial",
            "First.\n",
            "First plan\n",
            "steer",
            "Revised.\n",
            "Revised plan\n",
        ],
    );
    rows.push(event("thread_rolled_back", json!({"num_turns":1})));
    check(&rows, &published, &["initial", "First.\n", "First plan\n"]);
}

#[test]
fn a_late_publication_can_confirm_the_two_parts_of_a_checkpoint_reply_independently() {
    let first = model("u1", "user", "initial", 1);
    let reply = model(
        "reply",
        "assistant",
        "Deferred.\n<proposed_plan>\nDeferred plan\n</proposed_plan>",
        1,
    );
    let mut rows = vec![
        begin(),
        first.clone(),
        page_user(TURN, "u1", "initial"),
        checkpoint(&first, &reply),
        model("u2", "user", "fresh", 2),
        page_user(TURN, "u2", "fresh"),
    ];
    check(&rows, &["initial", "fresh"], &["initial", "fresh"]);
    rows.push(plan("Deferred plan"));
    check(
        &rows,
        &["initial", "fresh", "Deferred plan"],
        &["initial", "Deferred plan", "fresh"],
    );
    rows.push(shown("Deferred.\n"));
    check(
        &rows,
        &["initial", "fresh", "Deferred plan", "Deferred.\n"],
        &["initial", "Deferred.\n", "Deferred plan", "fresh"],
    );
}

#[test]
fn a_new_model_reply_with_the_same_spelling_keeps_its_later_position() {
    let first = model("u1", "user", "initial", 1);
    let reply = model(
        "reply",
        "assistant",
        "Repeated.\n<proposed_plan>\nRepeated plan\n</proposed_plan>",
        1,
    );
    let mut later = reply.clone();
    later["metadata"]["user_input_order"] = 2.into();
    let rows = [
        begin(),
        first.clone(),
        page_user(TURN, "u1", "initial"),
        checkpoint(&first, &reply),
        model("u2", "user", "fresh", 2),
        page_user(TURN, "u2", "fresh"),
        later,
        shown("Repeated.\n"),
        plan("Repeated plan"),
    ];
    let published = ["initial", "fresh", "Repeated.\n", "Repeated plan"];
    check(&rows, &published, &published);
}
