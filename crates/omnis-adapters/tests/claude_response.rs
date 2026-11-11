use std::io::Write;

use omnis_adapters::{ClaudeAdapter, ProviderAdapter};
use omnis_core::{NativeTrajectoryItem, import_trajectory, native_trajectory_items};
use omnis_ir::{CanonicalSnapshot, EventKind, Provider, SessionRef};
use serde_json::{Value, json};
use uuid::Uuid;

fn message(index: u128, parent: u128, role: &str, content: Value) -> Value {
    let mut row = json!({
        "type": role, "uuid": Uuid::from_u128(index),
        "parentUuid": (parent != 0).then(|| Uuid::from_u128(parent)),
        "message": {"role":role}
    });
    row["message"]["content"] = content;
    row
}

fn block(index: u128, parent: u128, request: &str, content: Value) -> Value {
    let mut row = message(index, parent, "assistant", content);
    row["message"]["id"] = json!("response");
    row["requestId"] = json!(request);
    row
}

fn call(id: &str) -> Value {
    json!({"type":"tool_use","id":id,"name":"Read","input":{}})
}

fn result(id: &str, output: &str, failed: bool) -> Value {
    json!({"type":"tool_result","tool_use_id":id,"content":output,"is_error":failed})
}

fn read(rows: &[Value]) -> CanonicalSnapshot {
    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let id = Uuid::from_u128(99);
    let mut file = std::fs::File::create(project.join(format!("{id}.jsonl"))).unwrap();
    for row in rows {
        writeln!(file, "{row}").unwrap();
    }
    ClaudeAdapter::with_root(root.path())
        .read_session(&SessionRef::new(Provider::Claude, id.to_string()))
        .unwrap()
}

fn tool_outcomes(snapshot: &CanonicalSnapshot) -> Vec<(String, bool)> {
    native_trajectory_items(&import_trajectory(snapshot))
        .into_iter()
        .filter_map(|item| match item {
            NativeTrajectoryItem::Tool {
                output, is_error, ..
            } => Some((output, is_error)),
            NativeTrajectoryItem::Message { .. } => None,
        })
        .collect()
}

#[test]
fn selected_result_wins_and_only_the_missing_result_block_is_recovered() {
    let snapshot = read(&[
        message(1, 0, "user", json!("Check both")),
        block(2, 1, "request", json!([call("a"), call("b")])),
        message(
            3,
            2,
            "user",
            json!([
                result("a", "stale failure", true), result("b", "second", false),
                {"type":"text", "text":"abandoned request"}, result("unknown", "orphan", false)
            ]),
        ),
        message(4, 2, "user", json!([result("a", "selected", false)])),
        message(5, 4, "assistant", json!("Finished")),
    ]);
    assert_eq!(
        tool_outcomes(&snapshot),
        [("selected".into(), false), ("second".into(), false)]
    );
    assert!(
        snapshot
            .events
            .iter()
            .all(|e| e.payload["text"] != "abandoned request"
                && e.payload["content"] != "stale failure"
                && e.payload["content"] != "orphan")
    );
}

#[test]
fn retries_and_writers_with_the_same_message_id_keep_the_selected_response() {
    let mut other_writer = block(3, 1, "selected", json!("other writer"));
    other_writer["agentId"] = json!("other");
    other_writer["message"]["model"] = json!("other-model");
    let mut selected = block(4, 1, "selected", json!("selected answer"));
    selected["message"]["model"] = json!("selected-model");
    let snapshot = read(&[
        message(1, 0, "user", json!("Question")),
        block(2, 1, "abandoned", json!("abandoned retry")),
        other_writer,
        selected,
    ]);
    let replies: Vec<_> = snapshot
        .events
        .iter()
        .filter(|e| e.kind == EventKind::MessageAssistant)
        .map(|e| e.payload["text"].as_str().unwrap())
        .collect();
    assert_eq!(replies, ["selected answer"]);
    let models: Vec<_> = snapshot
        .events
        .iter()
        .filter_map(|e| e.payload["model"].as_str())
        .collect();
    assert_eq!(models, ["selected-model"]);
}

#[test]
fn sibling_text_keeps_its_position_before_the_call_and_result() {
    let snapshot = read(&[
        message(1, 0, "user", json!("Check status")),
        block(2, 1, "r", json!("Checking")),
        block(3, 1, "r", json!([call("a")])),
        message(4, 3, "user", json!([result("a", "complete", false)])),
    ]);
    assert_eq!(tool_outcomes(&snapshot), [("complete".into(), false)]);
    assert_eq!(snapshot.events[1].payload["text"], "Checking");
}

#[test]
fn direct_results_of_an_unnamed_response_survive_progress_links() {
    let snapshot = read(&[
        message(1, 0, "user", json!("Inspect")),
        message(2, 1, "assistant", json!([call("a"), call("b")])),
        json!({"type":"progress","uuid":Uuid::from_u128(3),"parentUuid":Uuid::from_u128(2)}),
        message(4, 3, "user", json!([result("b", "other result", false)])),
        message(5, 2, "user", json!([result("a", "selected", false)])),
    ]);
    assert_eq!(
        tool_outcomes(&snapshot),
        [("selected".into(), false), ("other result".into(), false)]
    );
}

#[test]
fn ambiguous_or_foreign_orphans_cannot_answer_an_active_call() {
    let mut foreign = message(6, 1, "user", json!([result("unique", "foreign", true)]));
    foreign["agentId"] = json!("other");
    let snapshot = read(&[
        message(1, 0, "user", json!("Inspect")),
        block(2, 1, "old", json!([call("reused")])),
        message(3, 1, "user", json!([result("future", "too early", true)])),
        block(
            4,
            1,
            "new",
            json!([call("reused"), call("unique"), call("future")]),
        ),
        message(5, 1, "user", json!([result("reused", "ambiguous", true)])),
        foreign,
        message(7, 4, "assistant", json!("Waiting for results")),
    ]);
    assert!(
        !snapshot
            .events
            .iter()
            .any(|e| matches!(e.kind, EventKind::ToolFailed | EventKind::ToolCompleted))
    );
}
