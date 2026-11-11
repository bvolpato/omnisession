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

fn compaction(index: u128, parent: u128, retained: &[u128]) -> [Value; 2] {
    let id = Uuid::from_u128;
    let header = json!({"type":"system","subtype":"compact_boundary","uuid":id(index),"logicalParentUuid":id(parent),
        "compactMetadata":{"preservedMessages":{"anchorUuid":id(index+1),"uuids":retained.iter().copied().map(id).collect::<Vec<_>>()}}});
    let mut summary = message(index + 1, index, "user", json!(format!("Summary {index}")));
    summary["isCompactSummary"] = json!(true);
    [header, summary]
}

#[test]
fn late_parallel_results_follow_retained_calls_after_compaction() {
    let mut records = vec![
        message(1, 0, "user", json!("Read both")),
        block(2, 1, "request", json!([call("a"), call("b")])),
    ];
    records.extend(compaction(3, 2, &[2]));
    records.extend([
        message(5, 2, "user", json!([result("b", "second", false)])),
        message(6, 2, "user", json!([result("a", "first", false)])),
        message(7, 6, "assistant", json!("Finished")),
    ]);
    assert_eq!(
        tool_outcomes(&read(&records)),
        [("first".into(), false), ("second".into(), false)]
    );
}

#[test]
fn a_later_compaction_keeps_earlier_rewind_cutoffs_out_of_search_and_recovery() {
    let mut records = vec![
        message(1, 0, "user", json!("Initial task")),
        message(2, 1, "assistant", json!("Retained result")),
        message(3, 2, "user", json!("Abandoned request")),
    ];
    records.extend(compaction(4, 3, &[2, 3]));
    records.push(message(6, 2, "user", json!("Replacement request")));
    for retained in [&[][..], &[6][..]] {
        let mut rows = records.clone();
        rows.extend(compaction(7, 6, retained));
        let snapshot = read(&rows);
        let mut recovery = Vec::new();
        omnis_core::write_recovery_handoff(&snapshot, &mut recovery).unwrap();
        for text in [
            omnis_core::trajectory_search_document(&snapshot).text,
            String::from_utf8(recovery).unwrap(),
        ] {
            assert!(text.contains("Retained result"));
            assert!(text.contains("Replacement request"));
            assert!(!text.contains("Abandoned request"));
        }
    }
}

#[test]
fn compacted_calls_do_not_recover_results_the_summary_omitted() {
    let mut records = vec![
        message(1, 0, "user", json!("Check")),
        block(2, 1, "request", json!([call("a"), call("b")])),
        message(
            3,
            2,
            "user",
            json!([result("a", "stalebeforecompaction", true)]),
        ),
    ];
    records.extend(compaction(4, 3, &[2]));
    records.push(message(
        6,
        2,
        "user",
        json!([result("b", "current", false)]),
    ));
    let snapshot = read(&records);
    assert!(
        import_trajectory(&snapshot)
            .items
            .iter()
            .any(|item| item.text.contains("current"))
    );
    assert!(
        !import_trajectory(&snapshot)
            .items
            .iter()
            .any(|item| item.text.contains("stalebeforecompaction"))
    );
}

fn linked_result(index: u128, parent: u128, source: u128, id: &str, output: &str) -> Value {
    let mut row = message(index, parent, "user", json!([result(id, output, false)]));
    row["sourceToolAssistantUUID"] = json!(Uuid::from_u128(source));
    row
}

#[test]
fn result_source_identifies_the_selected_response_chunk_after_a_retry() {
    let snapshot = read(&[
        message(1, 0, "user", json!("Inspect")),
        block(2, 1, "abandoned", json!([call("reused")])),
        block(3, 1, "selected", json!([call("primary")])),
        block(4, 1, "selected", json!([call("reused")])),
        message(5, 3, "user", json!([result("primary", "first", false)])),
        linked_result(6, 5, 4, "reused", "second"),
        linked_result(7, 6, 3, "primary", "discarded first result"),
        message(8, 5, "assistant", json!("Finished")),
    ]);
    assert_eq!(
        tool_outcomes(&snapshot),
        [("first".into(), false), ("second".into(), false)]
    );
    assert!(
        snapshot
            .events
            .iter()
            .all(|e| e.payload["content"] != "discarded first result")
    );
}

#[test]
fn a_conflicting_result_source_cannot_attach_a_discarded_result() {
    for (parent, source, id) in [
        (3, 2, "reused"),  // The source belongs to a discarded retry.
        (2, 3, "reused"),  // Two different assistants both issued this call ID.
        (3, 4, "unique"),  // The source is a progress record.
        (1, 99, "unique"), // The source is missing.
        (1, 3, "split"),   // This assistant did not issue the call.
        (1, 5, "unique"),  // The source belongs to a different writer.
    ] {
        let mut foreign = block(5, 1, "selected", json!([call("unique")]));
        foreign["agentId"] = json!("other");
        let snapshot = read(&[
            message(1, 0, "user", json!("Inspect")),
            block(2, 1, "abandoned", json!([call("reused")])),
            block(3, 1, "selected", json!([call("reused"), call("unique")])),
            json!({"type":"progress", "uuid":Uuid::from_u128(4), "parentUuid":Uuid::from_u128(3)}),
            foreign,
            block(8, 1, "selected", json!([call("split")])),
            linked_result(6, parent, source, id, "discarded"),
            message(7, 3, "assistant", json!("Waiting")),
        ]);
        assert!(
            snapshot
                .events
                .iter()
                .all(|e| e.payload["content"] != "discarded"),
            "parent {parent}, source {source}, id {id}"
        );
    }
}

#[test]
fn a_repeated_source_uuid_cannot_disambiguate_a_reused_tool_id() {
    let snapshot = read(&[
        message(1, 0, "user", json!("Inspect")),
        block(2, 1, "abandoned", json!([call("same")])),
        block(2, 1, "selected", json!([call("same")])),
        linked_result(3, 1, 2, "same", "ambiguous"),
        message(4, 2, "assistant", json!("Waiting")),
    ]);
    assert!(
        snapshot
            .events
            .iter()
            .all(|e| e.payload["content"] != "ambiguous")
    );
}

#[test]
fn split_response_results_have_the_same_owners_after_compaction_in_either_order() {
    for compacted in [false, true] {
        for last in [2, 3] {
            for sibling_parent in [1, 2] {
                let mut rows = vec![
                    message(1, 0, "user", json!("Inspect both")),
                    block(2, 1, "request", json!([call("a")])),
                    block(3, sibling_parent, "request", json!([call("b")])),
                ];
                if compacted {
                    rows.extend(compaction(4, 3, &[2, 3]));
                }
                let first = 5 - last;
                let outcome = |row, owner| {
                    let id = if owner == 2 { "a" } else { "b" };
                    linked_result(row, owner, owner, id, id)
                };
                rows.extend([
                    outcome(6, first),
                    outcome(7, last),
                    message(8, 7, "assistant", json!("Finished")),
                ]);
                assert_eq!(
                    tool_outcomes(&read(&rows)),
                    [("a".into(), false), ("b".into(), false)],
                    "compacted={compacted}, last owner={last}, second block parent={sibling_parent}"
                );
            }
        }
    }
}

#[test]
fn a_rewind_removes_the_withdrawn_turn_and_its_response_blocks_from_history() {
    for sibling_parent in [2, 3] {
        for listed_block in [3, 4] {
            let mut rows = vec![
                message(1, 0, "user", json!("Initial task")),
                message(2, 1, "user", json!("Shared question")),
                block(3, 2, "abandoned", json!("Retired block one")),
                block(4, sibling_parent, "abandoned", json!("Retired block two")),
                message(5, 4, "user", json!("Retired follow-up")),
                block(6, 5, "child", json!("Retired reply")),
            ];
            rows.extend(compaction(7, 6, &[2, listed_block]));
            rows.push(block(9, 2, "new", json!("Replacement")));
            for _ in 0..2 {
                let snap = read(&rows);
                let mut recovery = Vec::new();
                omnis_core::write_recovery_handoff(&snap, &mut recovery).unwrap();
                for text in [
                    omnis_core::trajectory_search_document(&snap).text,
                    String::from_utf8(recovery).unwrap(),
                ] {
                    assert!(text.contains("Shared question"));
                    assert!(text.contains("Replacement"));
                    assert!(
                        !text.contains("Retired"),
                        "listed={listed_block}, parent={sibling_parent}: {text}"
                    );
                }
                rows.extend(compaction(10, 9, &[9]));
            }
        }
    }
}

#[test]
fn stale_selected_result_cannot_hide_a_verified_retry() {
    for (stale_last, stale_source, foreign) in [
        (false, 2, false),
        (true, 2, false),
        (true, 3, true),
        (true, 99, false),
    ] {
        let mut records = vec![
            message(1, 0, "user", json!("Inspect")),
            block(2, 1, "abandoned", json!([call("reused")])),
            block(3, 1, "selected", json!([call("reused")])),
        ];
        let good = linked_result(4, 3, 3, "reused", "retry output");
        let mut stale = linked_result(5, 3, stale_source, "reused", "abandoned output");
        if foreign {
            stale["agentId"] = json!("worker");
        }
        stale["message"]["content"]
            .as_array_mut()
            .unwrap()
            .push(json!({"type":"text","text":"keep this note"}));
        records.extend(if stale_last {
            [good, stale]
        } else {
            [stale, good]
        });
        records.push(message(6, 5, "assistant", json!("Finished")));
        let snapshot = read(&records);
        assert_eq!(tool_outcomes(&snapshot), [("retry output".into(), false)]);
        let mut recovery = Vec::new();
        omnis_core::write_recovery_handoff(&snapshot, &mut recovery).unwrap();
        let recovery = String::from_utf8(recovery).unwrap();
        assert!(recovery.contains("keep this note"));
        assert!(!recovery.contains("abandoned output"));
    }
}

#[test]
fn linear_retries_keep_distinct_native_pairs_and_original_provider_ids() {
    let mut first = block(2, 1, "first", json!([call("same")]));
    first["message"]["content"][0]["input"] = json!({"file":"old"});
    let mut latest = block(3, 2, "second", json!([call("same")]));
    latest["message"]["content"][0]["input"] = json!({"file":"new"});
    let snapshot = read(&[
        message(1, 0, "user", json!("Inspect")),
        first,
        latest,
        linked_result(4, 3, 2, "same", "wrong attempt"),
        linked_result(5, 4, 3, "same", "correct"),
    ]);
    let pairs: Vec<_> = native_trajectory_items(&import_trajectory(&snapshot))
        .into_iter()
        .filter_map(|item| {
            if let NativeTrajectoryItem::Tool { input, output, .. } = item {
                Some((input, output))
            } else {
                None
            }
        })
        .collect();
    assert_eq!(pairs, [(json!({"file":"new"}), "correct".into())]);
    let provider_ids: Vec<_> = snapshot
        .events
        .iter()
        .filter_map(|event| {
            event.payload["id"]
                .as_str()
                .or(event.payload["tool_use_id"].as_str())
        })
        .collect();
    assert_eq!(provider_ids, ["same", "same", "same"]);
    assert!(
        !snapshot
            .events
            .iter()
            .any(|e| e.payload["content"] == "wrong attempt")
    );
}

#[test]
fn selected_results_with_equal_provenance_and_conflicting_outputs_are_omitted() {
    for linked in [true, false] {
        let mut first = linked_result(3, 2, 2, "id", "first outcome");
        let mut second = linked_result(4, 3, 2, "id", "other outcome");
        if !linked {
            for row in [&mut first, &mut second] {
                row.as_object_mut()
                    .unwrap()
                    .remove("sourceToolAssistantUUID");
            }
        }
        let snapshot = read(&[
            message(1, 0, "user", json!("Inspect")),
            block(2, 1, "r", json!([call("id")])),
            first,
            second,
        ]);
        assert!(tool_outcomes(&snapshot).is_empty());
        assert!(
            !snapshot
                .events
                .iter()
                .any(|e| matches!(e.kind, EventKind::ToolCompleted | EventKind::ToolFailed))
        );
    }
}

#[test]
fn selected_result_cannot_answer_a_call_in_another_compaction_epoch() {
    let mut records = vec![
        message(1, 0, "user", json!("Inspect")),
        block(2, 1, "old", json!([call("same")])),
    ];
    records.extend(compaction(3, 2, &[]));
    records.extend([
        block(5, 4, "new", json!([call("same")])),
        linked_result(6, 5, 5, "same", "right epoch"),
        linked_result(7, 6, 2, "same", "wrong epoch"),
    ]);
    let snapshot = read(&records);
    assert_eq!(tool_outcomes(&snapshot), [("right epoch".into(), false)]);
    assert!(
        !snapshot
            .events
            .iter()
            .any(|e| e.payload["content"] == "wrong epoch")
    );
}

#[test]
fn implicit_retained_parent_disambiguates_a_result_after_a_retry() {
    let mut rows = vec![
        message(1, 0, "user", json!("Inspect")),
        block(2, 1, "retained", json!([call("same")])),
        json!({"type":"progress","uuid":Uuid::from_u128(3),"parentUuid":Uuid::from_u128(2)}),
    ];
    rows.extend(compaction(4, 3, &[2, 3]));
    rows.extend([
        block(6, 5, "discarded", json!([call("same")])),
        message(
            7,
            3,
            "user",
            json!([result("same", "retained output", false)]),
        ),
    ]);
    assert_eq!(
        tool_outcomes(&read(&rows)),
        [("retained output".into(), false)]
    );
}

#[test]
fn a_retained_result_fills_only_a_call_that_the_new_branch_left_unanswered() {
    for named in [false, true] {
        for replace in [false, true] {
            let calls = json!([call("a"), call("b")]);
            let mut rows = vec![
                message(1, 0, "user", json!("Inspect both")),
                if named {
                    block(2, 1, "request", calls)
                } else {
                    message(2, 1, "assistant", calls)
                },
                linked_result(3, 2, 2, "a", "original"),
                message(
                    4,
                    3,
                    "user",
                    json!([
                        result("b", "sibling", false), result("unknown", "Withdrawn result", true),
                        {"type":"text", "text":"Withdrawn request"}
                    ]),
                ),
                message(5, 4, "user", json!("Withdrawn follow-up")),
            ];
            rows.extend(compaction(6, 5, &[2, 3, 4, 5]));
            let (parent, expected) = if replace {
                rows.push(linked_result(8, 2, 2, "a", "replacement"));
                (8, "replacement")
            } else {
                (3, "original")
            };
            rows.push(message(9, parent, "assistant", json!("Finished")));
            let snapshot = read(&rows);
            assert_eq!(
                tool_outcomes(&snapshot),
                [(expected.into(), false), ("sibling".into(), false)],
                "named={named}, replace={replace}"
            );
            assert!(
                !omnis_core::trajectory_search_document(&snapshot)
                    .text
                    .contains("Withdrawn")
            );
        }
    }
}

#[test]
fn a_mixed_result_keeps_its_verified_pair_when_another_call_is_unanswered() {
    for reverse in [false, true] {
        let first = block(
            2,
            1,
            "request",
            json!([call(if reverse { "b" } else { "a" })]),
        );
        let second = block(
            3,
            2,
            "request",
            json!([call(if reverse { "a" } else { "b" })]),
        );
        let mut mixed = linked_result(4, 3, if reverse { 3 } else { 2 }, "a", "verified");
        mixed["message"]["content"].as_array_mut().unwrap().extend([
            result("b", "unverified", true),
            json!({"type":"text","text":"keep this note"}),
        ]);
        let snapshot = read(&[
            message(1, 0, "user", json!("Inspect")),
            first,
            second,
            mixed,
        ]);
        assert_eq!(tool_outcomes(&snapshot), [("verified".into(), false)]);
        assert!(
            snapshot
                .events
                .iter()
                .any(|e| e.payload["text"] == "keep this note")
        );
        assert!(
            !snapshot
                .events
                .iter()
                .any(|e| e.payload["content"] == "unverified")
        );
    }
}
