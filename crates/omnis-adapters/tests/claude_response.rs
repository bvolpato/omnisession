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

fn opaque(index: u128, parent: u128, kind: &str) -> Value {
    json!({"type":kind,"uuid":Uuid::from_u128(index),"parentUuid":Uuid::from_u128(parent)})
}

#[test]
fn late_results_via_transparent_records_keep_the_selected_compaction() {
    for (retained, result_parent) in [
        (&[2, 3][..], 2),
        (&[2, 3][..], 5),
        (&[2, 3][..], 8),
        (&[2, 3, 4][..], 5),
        (&[4][..], 5),
    ] {
        let mut records = vec![
            message(1, 0, "user", json!("Read both")),
            block(2, 1, "request", json!([call("a")])),
            block(3, 2, "request", json!([call("b")])),
            opaque(4, 2, "progress"),
            opaque(5, 4, "attachment"),
        ];
        records.extend(compaction(6, 3, retained));
        records.extend([
            opaque(8, 5, "progress"),
            linked_result(9, 3, 3, "b", "second"),
            linked_result(10, result_parent, 2, "a", "first"),
            message(11, 10, "assistant", json!("Finished")),
        ]);
        // A listed progress record invalidates the native relink. A stream-only one is harmless.
        let list_is_valid = !retained.contains(&4);
        records[5]["compactMetadata"]["preservedMessages"]["allUuids"] =
            json!([Uuid::from_u128(2), Uuid::from_u128(3), Uuid::from_u128(4)]);
        let snapshot = read(&records);
        assert_eq!(
            snapshot
                .events
                .iter()
                .any(|event| event.kind == EventKind::CompactionCreated),
            list_is_valid
        );
        assert_eq!(
            snapshot
                .events
                .iter()
                .any(|event| event.payload["native_history_unverified"] == true),
            !list_is_valid
        );
        assert_eq!(
            tool_outcomes(&snapshot),
            [("first".into(), false), ("second".into(), false)]
        );
    }
}

#[test]
fn a_failed_retention_list_marks_only_the_affected_branch() {
    let mut rows = vec![
        message(1, 0, "user", json!("Separate root")),
        message(2, 0, "user", json!("Compacted task")),
        message(3, 2, "assistant", json!("A decision")),
        opaque(4, 3, "progress"),
    ];
    rows.extend(compaction(5, 3, &[3, 4]));
    rows.push(message(7, 4, "user", json!("Affected continuation")));
    let unverified = |records: &[Value]| {
        read(records)
            .events
            .iter()
            .any(|event| event.payload["native_history_unverified"] == true)
    };
    assert!(unverified(&rows));
    for parent in [0, 1, 2] {
        let mut independent = rows.clone();
        independent.push(message(
            8,
            parent,
            "user",
            json!("Independent continuation"),
        ));
        assert!(!unverified(&independent));
        independent.push(message(9, 7, "user", json!("Return to affected branch")));
        assert!(unverified(&independent));
    }
}

#[test]
fn returning_to_a_valid_summary_abandons_the_other_branches_rejected_list() {
    for invalid in [&[2, 99][..], &[2, 2][..]] {
        let mut rows = vec![
            message(1, 0, "user", json!("Task")),
            block(2, 1, "answer", json!("Decision")),
            message(30, 1, "user", json!("Retained note")),
        ];
        rows.extend(compaction(3, 2, &[30, 2]));
        rows.extend([
            message(5, 2, "user", json!("Continue")),
            message(6, 5, "assistant", json!("Ready")),
        ]);
        rows.extend(compaction(7, 6, invalid));
        rows.push(message(9, 2, "user", json!("Physical fallback")));
        assert!(import_trajectory(&read(&rows)).truncated);
        rows.push(message(10, 6, "user", json!("Return to valid branch")));
        rows.push(opaque(11, 2, "progress"));
        rows.push(message(12, 11, "user", json!("Resume from decision")));
        let snapshot = read(&rows);
        assert_eq!(
            snapshot
                .events
                .iter()
                .filter_map(|e| e.payload["text"].as_str())
                .collect::<Vec<_>>(),
            [
                "Task",
                "Summary 3",
                "Retained note",
                "Decision",
                "Resume from decision"
            ]
        );
        assert!(!import_trajectory(&snapshot).truncated);
    }
}

#[test]
fn a_late_result_on_an_independent_turn_abandons_failed_retention() {
    let id = Uuid::from_u128;
    let mut rows = vec![
        message(20, 0, "user", json!("Independent task")),
        block(21, 20, "other", json!([call("other")])),
        message(1, 0, "user", json!("Compacted task")),
        block(2, 1, "decision", json!("Decision")),
    ];
    let [header, summary] = compaction(3, 2, &[2, 99]);
    for segment in [false, true] {
        let mut header = header.clone();
        if segment {
            header["compactMetadata"] = json!({"preservedSegment":{
                "anchorUuid":id(4),"headUuid":id(99),"tailUuid":id(2)
            }});
        }
        rows.extend([
            header,
            summary.clone(),
            linked_result(5, 21, 21, "other", "Unrelated result"),
            message(6, 2, "user", json!("Resume decision")),
        ]);
        assert!(
            !import_trajectory(&read(&rows)).truncated,
            "segment {segment}"
        );
        rows.truncate(4);
    }
}

#[test]
fn an_invalid_retention_descriptor_cannot_redirect_a_rewind() {
    let id = Uuid::from_u128;
    for fault in [
        "missing",
        "late",
        "after_boundary",
        "repeat",
        "ambiguous",
        "anchor",
        "invalid",
        "boundary",
        "older",
    ] {
        let mut rows = vec![
            message(1, 0, "user", json!("Original task")),
            message(2, 1, "assistant", json!("Shared decision")),
            message(3, 2, "user", json!("Withdrawn request")),
            message(4, 3, "assistant", json!("Withdrawn reply")),
        ];
        if fault == "ambiguous" {
            rows.push(message(
                4,
                3,
                "assistant",
                json!("Different reply with the same UUID"),
            ));
        }
        if fault == "older" {
            rows.extend(compaction(10, 4, &[2]));
        }
        let [mut boundary, summary] =
            compaction(12, if fault == "older" { 11 } else { 4 }, &[2, 99]);
        let list = &mut boundary["compactMetadata"]["preservedMessages"];
        match fault {
            "repeat" => list["uuids"] = json!([id(2), id(2)]),
            "ambiguous" => list["uuids"] = json!([id(2), id(4)]),
            "anchor" => {
                list["uuids"] = json!([id(2)]);
                list["anchorUuid"] = json!(id(4));
            }
            "invalid" => list["uuids"] = json!([id(2), "malformed"]),
            "boundary" => list["uuids"] = json!([id(2), id(12)]),
            _ => {}
        }
        rows.push(boundary);
        if fault == "after_boundary" {
            rows.push(opaque(99, 1, "attachment"));
        }
        rows.push(summary);
        if fault == "late" {
            rows.push(message(99, 13, "assistant", json!("Future member")));
        }
        rows.push(message(14, 2, "user", json!("Selected continuation")));
        let snapshot = read(&rows);
        assert_eq!(
            snapshot
                .events
                .iter()
                .filter_map(|e| e.payload["text"].as_str())
                .collect::<Vec<_>>(),
            ["Original task", "Shared decision", "Selected continuation"],
            "{fault}"
        );
        assert!(
            snapshot
                .events
                .iter()
                .any(|e| e.payload["native_history_unverified"] == true),
            "{fault}"
        );
        assert!(import_trajectory(&snapshot).truncated, "{fault}");
        rows.push(message(15, 1, "user", json!("Separate branch")));
        assert!(!import_trajectory(&read(&rows)).truncated, "{fault}");
    }
}

#[test]
fn an_opaque_bridge_uses_the_nearest_retained_marker_in_the_newest_summary() {
    let mut rows = vec![
        message(1, 0, "user", json!("Task")),
        message(2, 1, "assistant", json!("Decision")),
        message(3, 2, "user", json!("Retained note")),
        opaque(4, 2, "attachment"),
        opaque(5, 4, "progress"),
        message(6, 3, "assistant", json!("Discarded tail")),
    ];
    rows.extend(compaction(7, 6, &[2, 3, 4, 6]));
    rows.push(opaque(9, 5, "progress"));
    let mut first = rows.clone();
    first.push(message(10, 9, "user", json!("Continue")));
    let messages = |records: &[Value]| {
        read(records)
            .events
            .into_iter()
            .filter_map(|event| event.payload["text"].as_str().map(str::to_owned))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        messages(&first),
        ["Task", "Summary 7", "Decision", "Retained note", "Continue"]
    );
    rows.extend(compaction(11, 8, &[2, 3]));
    rows.push(message(13, 9, "user", json!("Continue")));
    let latest = messages(&rows);
    let summary = latest.iter().position(|text| text == "Summary 11").unwrap();
    assert_eq!(latest[summary..], ["Summary 11", "Decision", "Continue"]);
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
fn a_rewind_after_the_last_listed_message_excludes_only_the_withdrawn_path() {
    for keep_attachment in [false, true] {
        let mut rows = vec![
            message(1, 0, "user", json!("Original task")),
            message(2, 1, "assistant", json!("Decision")),
            message(3, 2, "user", json!("Withdrawn request")),
            message(4, 3, "assistant", json!("Withdrawn reply")),
            opaque(5, 2, "attachment"),
        ];
        let retained: &[u128] = if keep_attachment { &[2, 5] } else { &[2] };
        rows.extend(compaction(6, 4, retained));
        for parent in [2, 7] {
            let mut branch = rows.clone();
            branch.push(message(8, parent, "user", json!("Continuation")));
            let snapshot = read(&branch);
            let mut recovery = Vec::new();
            omnis_core::write_recovery_handoff(&snapshot, &mut recovery).unwrap();
            for text in [
                omnis_core::trajectory_search_document(&snapshot).text,
                String::from_utf8(recovery).unwrap(),
            ] {
                assert!(
                    text.contains("Original task")
                        && text.contains("Decision")
                        && text.contains("Continuation")
                );
                assert_eq!(text.contains("Withdrawn request"), parent == 7);
                assert_eq!(text.contains("Withdrawn reply"), parent == 7);
            }
            assert!(!import_trajectory(&snapshot).truncated);
        }
    }
}

#[test]
fn a_rewind_at_either_chunk_prunes_the_unlisted_tail_of_its_response() {
    for (chunk_parent, tail_parent) in [(1, 3), (2, 3), (1, 2)] {
        for cutoff in [2, 3] {
            for nested in [false, true] {
                let mut rows = vec![
                    message(1, 0, "user", json!("Original task")),
                    block(2, 1, "selected", json!("First selected chunk")),
                ];
                let sibling = block(3, chunk_parent, "selected", json!("Second selected chunk"));
                let withdrawn = [
                    message(4, tail_parent, "user", json!("Withdrawn request")),
                    block(5, 4, "other", json!("Withdrawn answer")),
                ];
                if tail_parent == 2 {
                    rows.extend(withdrawn);
                    rows.push(sibling);
                } else {
                    rows.push(sibling);
                    rows.extend(withdrawn);
                }
                rows.push(message(6, 0, "user", json!("Independent conversation")));
                rows.push(message(7, 0, "user", json!("Separately discarded")));
                rows.extend(compaction(10, 5, &[2, 3]));
                let mut later = vec![4, 5, 6, 30];
                if nested {
                    rows.push(message(12, 11, "user", json!("Intervening request")));
                    rows.extend(compaction(20, 12, &[2, 3, 7]));
                    later.insert(2, 12);
                }
                rows.push(message(30, cutoff, "user", json!("Replacement")));
                for relisted in [false, true] {
                    if relisted {
                        rows.extend(compaction(40, 30, &later));
                    }
                    let snapshot = read(&rows);
                    assert_eq!(import_trajectory(&snapshot).truncated, relisted);
                    let mut recovery = Vec::new();
                    omnis_core::write_recovery_handoff(&snapshot, &mut recovery).unwrap();
                    for text in [
                        omnis_core::trajectory_search_document(&snapshot).text,
                        String::from_utf8(recovery).unwrap(),
                    ] {
                        assert!(
                            text.contains("First selected chunk")
                                && text.contains("Second selected chunk")
                                && text.contains("Replacement")
                        );
                        assert!(
                            !text.contains("Withdrawn") && !text.contains("Intervening"),
                            "chunk_parent={chunk_parent} tail_parent={tail_parent} cutoff={cutoff} nested={nested} relisted={relisted}: {text}"
                        );
                        assert_eq!(text.contains("Independent conversation"), relisted);
                    }
                    assert!(native_trajectory_items(&import_trajectory(&snapshot)).iter().all(|item| {
                        !matches!(item, NativeTrajectoryItem::Message { text, .. } if text.contains("Withdrawn") || text.contains("Intervening"))
                    }));
                }
            }
        }
    }
}

#[test]
fn a_retained_result_can_prove_the_end_of_a_parallel_response() {
    let mut rows = vec![
        message(1, 0, "user", json!("Question")),
        block(2, 1, "selected", json!([call("a")])),
        block(3, 1, "selected", json!("Parallel block")),
        linked_result(4, 2, 2, "a", "Verified result"),
        message(5, 3, "user", json!("Withdrawn request")),
    ];
    rows.extend(compaction(6, 5, &[2, 4]));
    rows.push(message(8, 2, "user", json!("Replacement")));
    let snapshot = read(&rows);
    assert_eq!(
        tool_outcomes(&snapshot),
        [("Verified result".into(), false)]
    );
    assert!(
        !omnis_core::trajectory_search_document(&snapshot)
            .text
            .contains("Withdrawn")
    );
}

#[test]
fn a_parallel_cutoff_uses_the_assistant_version_known_at_its_summary() {
    for duplicate_before_summary in [false, true] {
        let mut rows = vec![
            message(1, 0, "user", json!("Question")),
            block(2, 1, "proof", json!("Retained block")),
            block(3, 1, "proof", json!("Unlisted block")),
            message(4, 3, "user", json!("Old logical history")),
        ];
        let duplicate = block(3, 1, "proof", json!("Different unlisted version"));
        if duplicate_before_summary {
            rows.push(duplicate.clone());
        }
        rows.extend(compaction(5, 4, &[2]));
        rows.push(message(7, 2, "user", json!("Replacement")));
        if !duplicate_before_summary {
            rows.push(duplicate);
        }
        rows.push(message(8, 7, "assistant", json!("Final answer")));
        let snapshot = read(&rows);
        let text = omnis_core::trajectory_search_document(&snapshot).text;
        assert_eq!(
            text.contains("Old logical history"),
            duplicate_before_summary
        );
        assert_eq!(
            import_trajectory(&snapshot).truncated,
            duplicate_before_summary
        );
        assert!(text.contains("Retained block") && text.contains("Replacement"));
    }
}

#[test]
fn an_incompatible_version_cannot_supply_a_retained_results_parallel_cutoff() {
    let mut rows = vec![
        message(1, 0, "user", json!("Question")),
        block(2, 1, "proof", json!([call("a")])),
        linked_result(3, 2, 2, "a", "Selected proof"),
        block(2, 1, "proof", json!("Different version")),
        message(4, 2, "user", json!("Independent logic")),
    ];
    rows.extend(compaction(5, 4, &[3]));
    rows.push(message(7, 3, "user", json!("Replacement")));
    let snapshot = read(&rows);
    let text = omnis_core::trajectory_search_document(&snapshot).text;
    assert!(text.contains("Independent logic") && text.contains("Replacement"));
}

#[test]
fn a_sparse_cutoff_keeps_the_logical_alternative_to_an_explicitly_discarded_branch() {
    let mut rows = vec![
        message(1, 0, "user", json!("Question")),
        block(2, 1, "selected", json!("First block")),
        block(3, 1, "selected", json!("Second block")),
        message(4, 2, "user", json!("Shared later request")),
        block(5, 4, "discard", json!("Withdrawn reply")),
        block(6, 4, "alternative", json!("Independent reply")),
    ];
    rows.extend(compaction(7, 6, &[2, 3, 5]));
    rows.push(message(9, 2, "user", json!("Replacement")));
    let snapshot = read(&rows);
    let text = omnis_core::trajectory_search_document(&snapshot).text;
    assert!(text.contains("First block") && text.contains("Second block"));
    assert!(text.contains("Shared later request") && text.contains("Independent reply"));
    assert!(!text.contains("Withdrawn reply"));
}

#[test]
fn a_trailing_rewind_distinguishes_another_response_in_the_same_turn() {
    for (request, keep, uncertain) in [
        (Some("first"), true, false),
        (Some("later"), false, false),
        (None, false, true),
    ] {
        let mut continuation = block(
            4,
            3,
            request.unwrap_or_default(),
            json!("Response continuation"),
        );
        if request.is_none() {
            continuation["message"]
                .as_object_mut()
                .unwrap()
                .remove("id");
        }
        let prefix = [
            message(1, 0, "user", json!("Original task")),
            block(2, 1, "first", json!([call("shared")])),
            linked_result(3, 2, 2, "shared", "Verified output"),
            continuation,
        ];
        for retained in [&[2][..], &[2, 3][..]] {
            let mut rows = prefix.to_vec();
            rows.extend(compaction(5, 4, retained));
            rows.push(message(
                7,
                *retained.last().unwrap(),
                "user",
                json!("Replacement"),
            ));
            let snapshot = read(&rows);
            let mut recovery = Vec::new();
            omnis_core::write_recovery_handoff(&snapshot, &mut recovery).unwrap();
            for text in [
                omnis_core::trajectory_search_document(&snapshot).text,
                String::from_utf8(recovery).unwrap(),
            ] {
                assert_eq!(text.contains("Response continuation"), keep, "{request:?}");
                assert!(text.contains("Replacement"), "{request:?}");
            }
            assert_eq!(
                import_trajectory(&snapshot).truncated,
                uncertain,
                "{request:?}"
            );
            if retained.contains(&3) {
                assert_eq!(
                    tool_outcomes(&snapshot),
                    [("Verified output".to_owned(), false)]
                );
            }
        }
    }
}

#[test]
fn a_trailing_rewind_follows_only_verified_earlier_compactions() {
    for (retained, root, uncertain) in [(true, 2, false), (false, 2, true), (false, 10, false)] {
        let mut rows = vec![
            message(1, 0, "user", json!("Original task")),
            message(2, 1, "assistant", json!("Shared decision")),
            message(10, 0, "user", json!("Independent task")),
        ];
        rows.extend(compaction(20, root, if retained { &[2] } else { &[] }));
        let mut parent = 21;
        for i in 0..4 {
            let n = 30 + 4 * i;
            rows.push(message(n, parent, "user", json!("Intervening request")));
            rows.push(message(n + 1, n, "assistant", json!("Intervening reply")));
            rows.extend(compaction(n + 2, n + 1, &[2]));
            parent = n + 3;
        }
        rows.push(message(99, 2, "user", json!("Replacement")));
        let snapshot = read(&rows);
        let mut recovery = Vec::new();
        omnis_core::write_recovery_handoff(&snapshot, &mut recovery).unwrap();
        for text in [
            omnis_core::trajectory_search_document(&snapshot).text,
            String::from_utf8(recovery).unwrap(),
        ] {
            assert!(text.contains("Replacement") && text.contains("Shared decision"));
            assert_eq!(text.contains("Intervening request"), root == 10);
            assert_eq!(text.contains("Intervening reply"), root == 10);
            assert_eq!(text.contains("Independent task"), root == 10);
        }
        assert_eq!(import_trajectory(&snapshot).truncated, uncertain);
    }
}

#[test]
fn a_nested_rewind_can_reuse_an_explicit_retained_attachment() {
    let mut rows = vec![
        message(1, 0, "user", json!("Task")),
        message(2, 1, "assistant", json!("Decision")),
        opaque(3, 2, "attachment"),
    ];
    rows.extend(compaction(4, 2, &[3]));
    rows.push(message(6, 5, "user", json!("Later task")));
    rows.extend(compaction(7, 6, &[3]));
    rows.push(message(9, 3, "user", json!("Replacement")));
    let snapshot = read(&rows);
    assert!(
        !omnis_core::trajectory_search_document(&snapshot)
            .text
            .contains("Later task")
    );
    assert!(!import_trajectory(&snapshot).truncated);
}

#[test]
fn a_sparse_retained_list_only_withdraws_the_branch_after_the_cutoff() {
    for (request_parent, keep_sibling, logical_parent) in
        [(2, false, 4), (2, false, 5), (1, false, 4), (2, true, 4)]
    {
        let mut records = vec![
            message(1, 0, "user", json!("Initial task")),
            message(2, 1, "assistant", json!("Cutoff decision")),
            message(3, request_parent, "user", json!("Intermediate request")),
            message(4, 3, "assistant", json!("Withdrawn reply")),
            message(5, 3, "assistant", json!("Independent history")),
        ];
        let retained: &[u128] = if keep_sibling { &[5, 2, 4] } else { &[2, 4] };
        records.extend(compaction(6, logical_parent, retained));
        records.push(message(8, 2, "user", json!("Replacement request")));

        for _ in 0..2 {
            let snapshot = read(&records);
            let mut recovery = Vec::new();
            omnis_core::write_recovery_handoff(&snapshot, &mut recovery).unwrap();
            for text in [
                omnis_core::trajectory_search_document(&snapshot).text,
                String::from_utf8(recovery).unwrap(),
            ] {
                assert!(text.contains("Cutoff decision"));
                assert!(text.contains("Replacement request"));
                assert!(!text.contains("Withdrawn reply"));
                let shared = logical_parent == 5 || keep_sibling;
                assert_eq!(
                    text.contains("Intermediate request"),
                    shared || request_parent != 2
                );
                assert_eq!(text.contains("Independent history"), shared);
            }
            records.extend(compaction(9, 8, &[3, 8]));
        }
    }
}

#[test]
fn a_future_tool_call_cannot_change_the_response_kept_at_a_cutoff() {
    for (explicit_source, duplicate_uuid) in [(false, false), (true, false), (true, true)] {
        let mut records = vec![
            message(1, 0, "user", json!("Task")),
            block(2, 1, "issuer", json!([call("same")])),
            block(3, 1, "different", json!("Other response")),
        ];
        let result = if explicit_source {
            linked_result(4, 3, 2, "same", "done")
        } else {
            message(4, 3, "user", json!([result("same", "done", false)]))
        };
        let kept_request = if explicit_source {
            "issuer"
        } else {
            "different"
        };
        records.extend([
            result,
            block(5, 2, kept_request, json!("Response continuation")),
            block(6, 3, "different", json!([call("same")])),
        ]);
        if duplicate_uuid {
            records.push(block(2, 6, "withdrawn", json!([call("same")])));
        }
        records.extend(compaction(7, 6, &[4, 5]));
        records.push(message(9, 4, "user", json!("Replacement")));
        let snapshot = read(&records);
        assert_eq!(
            omnis_core::trajectory_search_document(&snapshot)
                .text
                .contains("Response continuation"),
            explicit_source,
        );
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
        assert!(!omnis_core::trajectory_search_document(&snapshot).source_complete);
        assert!(import_trajectory(&snapshot).truncated);
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

#[test]
fn a_rewind_does_not_restore_a_response_from_another_user_turn() {
    for independent_root in [false, true] {
        for compacted in [false, true] {
            let mut issued = block(2, 1, "reused", json!([call("a")]));
            issued["message"]["usage"] = json!({"output_tokens":10});
            let mut chunk = block(5, 4, "reused", json!("Selected trailing block"));
            chunk["message"]["usage"] = json!({"output_tokens":12});
            let mut discarded = block(
                7,
                6,
                "reused",
                json!([
                    {"type":"text","text":"withdrawn answer"},
                    {"type":"tool_use","id":"b","name":"Read","input":{"file":"withdrawn argument"}}
                ]),
            );
            discarded["message"]["usage"] = json!({"output_tokens":90});
            discarded["message"]["model"] = json!("withdrawn-model");
            let mut rows = vec![
                message(1, 0, "user", json!("Original request")),
                issued,
                linked_result(3, 2, 2, "a", "kept output"),
                opaque(4, 3, "progress"),
                chunk,
                message(
                    6,
                    if independent_root { 0 } else { 5 },
                    "user",
                    json!("withdrawn request"),
                ),
                discarded,
            ];
            if compacted {
                rows.extend(compaction(8, 7, &[2, 3, 5, 6, 7]));
            }
            rows.push(message(
                10,
                if compacted { 5 } else { 3 },
                "user",
                json!("Replacement"),
            ));
            let snapshot = read(&rows);
            let mut recovery = Vec::new();
            omnis_core::write_recovery_handoff(&snapshot, &mut recovery).unwrap();
            let mut emitted = serde_json::to_string(&snapshot.events).unwrap();
            emitted.push_str(&String::from_utf8(recovery).unwrap());
            assert!(!emitted.contains("withdrawn"), "{emitted}");
            assert!(emitted.contains("Selected trailing block"));
            assert_eq!(tool_outcomes(&snapshot), [("kept output".into(), false)]);
            assert_eq!(
                omnis_core::session_preview(&snapshot).total_tokens,
                Some(12)
            );
        }
    }
}

#[test]
fn a_result_from_another_user_turn_cannot_fill_an_unlinked_call_by_id() {
    for new_prompt in [true, false] {
        let middle = if new_prompt {
            message(3, 2, "user", json!("Separate question"))
        } else {
            opaque(3, 2, "progress")
        };
        let snapshot = read(&[
            message(1, 0, "user", json!("Original request")),
            block(2, 1, "original", json!([call("pending")])),
            middle,
            block(4, 3, "different", json!("Another response")),
            message(
                5,
                4,
                "user",
                json!([result("pending", "unlinked output", false)]),
            ),
            message(6, 2, "user", json!("Replacement")),
        ]);
        let expected = if new_prompt {
            vec![]
        } else {
            vec![("unlinked output".into(), false)]
        };
        assert_eq!(tool_outcomes(&snapshot), expected);
    }
}

#[test]
fn a_mixed_result_starts_a_new_turn_for_later_chunks_and_usage() {
    let mut original = block(2, 1, "reused", json!([call("a")]));
    original["message"]["usage"] = json!({"output_tokens":6});
    let mut next = block(4, 3, "reused", json!("New turn answer"));
    next["message"]["usage"] = json!({"output_tokens":11});
    let rows = [
        message(1, 0, "user", json!("Check")),
        original,
        message(
            3,
            2,
            "user",
            json!([result("a", "original result", false), {"type":"text","text":"New question"}]),
        ),
        next,
    ];
    let snapshot = read(&rows);
    assert_eq!(
        tool_outcomes(&snapshot),
        [("original result".into(), false)]
    );
    assert_eq!(
        omnis_core::session_preview(&snapshot).total_tokens,
        Some(17)
    );
    let mut rewound = rows.to_vec();
    rewound.push(message(5, 2, "user", json!("Replacement")));
    let snapshot = read(&rewound);
    assert_eq!(
        tool_outcomes(&snapshot),
        [("original result".into(), false)]
    );
    assert_eq!(omnis_core::session_preview(&snapshot).total_tokens, Some(6));
    assert!(
        snapshot
            .events
            .iter()
            .all(|event| event.payload["text"] != "New turn answer")
    );
}

#[test]
fn retained_chunks_can_receive_late_blocks_after_a_compaction() {
    let mut call_chunk = block(2, 1, "stable", json!([call("a")]));
    call_chunk["message"]["usage"] = json!({"output_tokens":7});
    let mut omitted = block(20, 1, "stable", json!("Discarded block"));
    omitted["message"]["usage"] = json!({"output_tokens":90});
    let mut rows = vec![
        message(1, 0, "user", json!("Question")),
        omitted,
        call_chunk,
        linked_result(3, 2, 2, "a", "answer"),
    ];
    rows.extend(compaction(4, 3, &[2, 3]));
    let mut late = block(7, 6, "stable", json!("Late response block"));
    late["message"]["usage"] = json!({"output_tokens":12});
    rows.extend([
        opaque(6, 3, "progress"),
        late,
        message(8, 3, "user", json!("Next")),
    ]);
    let snapshot = read(&rows);
    assert!(
        snapshot
            .events
            .iter()
            .all(|e| e.payload["text"] != "Discarded block")
    );
    assert_eq!(tool_outcomes(&snapshot), [("answer".into(), false)]);
    assert!(
        snapshot
            .events
            .iter()
            .any(|event| event.payload["text"] == "Late response block")
    );
    assert_eq!(
        omnis_core::session_preview(&snapshot).total_tokens,
        Some(12)
    );
}

#[test]
fn a_rewind_keeps_completed_calls_before_later_blocks_of_the_same_response() {
    for parallel in [false, true] {
        for trailing_selected in [false, true] {
            let mut rows = vec![
                message(1, 0, "user", json!("Inspect")),
                block(2, 1, "r", json!([call("a")])),
                linked_result(3, 2, 2, "a", "first"),
            ];
            if parallel {
                rows.extend([
                    block(4, 3, "r", json!([call("b")])),
                    linked_result(5, 4, 4, "b", "second"),
                ]);
            }
            let parent = if parallel { 5 } else { 3 };
            rows.push(block(6, parent, "r", json!("After the results")));
            let parent = if trailing_selected { 6 } else { parent };
            rows.extend([
                message(7, parent, "user", json!("Discarded")),
                message(8, parent, "user", json!("Replacement")),
            ]);
            let snapshot = read(&rows);
            let mut expected = vec![(String::from("first"), false)];
            if parallel {
                expected.push((String::from("second"), false));
            }
            assert_eq!(tool_outcomes(&snapshot), expected);
            let timeline: Vec<_> = snapshot
                .events
                .iter()
                .filter_map(|event| {
                    event.payload["text"]
                        .as_str()
                        .or(event.payload["id"].as_str())
                        .or(event.payload["content"].as_str())
                })
                .collect();
            let expected = if parallel {
                vec![
                    "Inspect",
                    "a",
                    "first",
                    "b",
                    "second",
                    "After the results",
                    "Replacement",
                ]
            } else {
                vec!["Inspect", "a", "first", "After the results", "Replacement"]
            };
            assert_eq!(timeline, expected);
        }
    }
}

#[test]
fn a_retained_outcome_without_its_call_remains_documentary() {
    for linked in [true, false] {
        let mut outcome = linked_result(3, 2, 2, "a", "retainedonlyoutput");
        if !linked {
            outcome
                .as_object_mut()
                .unwrap()
                .remove("sourceToolAssistantUUID");
        }
        let mut rows = vec![
            message(1, 0, "user", json!("Inspect")),
            block(2, 1, "first", json!([call("a")])),
            outcome,
            message(4, 3, "assistant", json!("Finished")),
        ];
        rows.extend(compaction(5, 4, &[3, 4]));
        let snapshot = read(&rows);
        assert!(tool_outcomes(&snapshot).is_empty());
        let mut recovery = Vec::new();
        omnis_core::write_recovery_handoff(&snapshot, &mut recovery).unwrap();
        for text in [
            omnis_core::trajectory_search_document(&snapshot).text,
            String::from_utf8(recovery).unwrap(),
            import_trajectory(&snapshot)
                .items
                .into_iter()
                .map(|item| item.text)
                .collect::<Vec<_>>()
                .join("\n"),
        ] {
            assert_eq!(text.matches("retainedonlyoutput").count(), 1);
        }
    }
}

#[test]
fn a_retained_result_cannot_revive_a_discarded_retry_outcome() {
    let mut rows = vec![
        message(1, 0, "user", json!("Inspect")),
        block(2, 1, "discarded", json!([call("x")])),
        block(3, 1, "current", json!([call("x"), call("y")])),
        linked_result(4, 3, 3, "y", "current output"),
        linked_result(5, 4, 2, "x", "discardedoutput"),
    ];
    rows.extend(compaction(6, 5, &[5]));
    assert!(
        read(&rows)
            .events
            .iter()
            .all(|e| e.payload["content"] != "discardedoutput")
    );
}

#[test]
fn a_future_reuse_cannot_invalidate_a_completed_result() {
    for (later_request, linked) in [
        ("retry", false),
        ("retry", true),
        ("first", false),
        ("first", true),
    ] {
        let mut original = linked_result(4, 3, 2, "b", "earlieroutput");
        if !linked {
            original
                .as_object_mut()
                .unwrap()
                .remove("sourceToolAssistantUUID");
        }
        let snapshot = read(&[
            message(1, 0, "user", json!("Inspect")),
            block(2, 1, "first", json!([call("a"), call("b")])),
            message(3, 2, "user", json!([result("a", "firstoutput", false)])),
            original,
            message(5, 4, "assistant", json!("Finished")),
            message(6, 5, "user", json!("Another question")),
            block(7, 6, later_request, json!([call("b")])),
            message(8, 5, "user", json!("Replacement")),
        ]);
        assert_eq!(
            tool_outcomes(&snapshot),
            [
                ("firstoutput".into(), false),
                ("earlieroutput".into(), false)
            ],
            "{later_request} {linked}"
        );
        assert_eq!(
            snapshot
                .events
                .iter()
                .filter(|e| e.payload["content"] == "earlieroutput")
                .count(),
            1
        );
    }
}

#[test]
fn conflicting_parent_response_blocks_cannot_suppress_a_verified_retry() {
    for (parent, thinking_only) in [(2, false), (3, false), (3, true)] {
        let mut prior = block(2, 1, "abandoned", json!([call("same")]));
        // This creates the metadata entry that the next thinking block updates in place.
        prior["message"]["usage"] = json!({"output_tokens": 1});
        let sibling = if thinking_only {
            json!([{"type":"thinking","thinking":"private reasoning"}])
        } else {
            json!("A sibling text block")
        };
        let snapshot = read(&[
            message(1, 0, "user", json!("Inspect")),
            prior,
            block(3, 2, "abandoned", sibling),
            block(4, 1, "selected", json!([call("same")])),
            linked_result(5, parent, 4, "same", "rejectedoutput"),
            linked_result(6, 4, 4, "same", "verifiedoutput"),
            message(7, 4, "assistant", json!("Finished")),
        ]);
        assert_eq!(tool_outcomes(&snapshot), [("verifiedoutput".into(), false)]);
        assert!(
            snapshot
                .events
                .iter()
                .all(|e| e.payload["content"] != "rejectedoutput")
        );
    }
}

#[test]
fn reusing_the_complete_response_identity_keeps_the_earlier_pair() {
    for (abandoned, textual_parent) in [(true, false), (false, false), (false, true)] {
        let mut rows = vec![
            message(1, 0, "user", json!("Inspect")),
            block(2, 1, "same", json!([call("raw")])),
            linked_result(3, 2, 2, "raw", "establishedoutput"),
            message(4, 3, "user", json!("Next request")),
            block(5, 4, "same", json!([call("raw")])),
        ];
        if textual_parent {
            rows.push(block(
                8,
                5,
                "same",
                json!("Continuation of the later response"),
            ));
            rows.push(message(
                6,
                8,
                "user",
                json!([result("raw", "laterclaim", false)]),
            ));
        } else {
            rows.push(linked_result(6, 5, 5, "raw", "laterclaim"));
        }
        rows.push(message(
            7,
            if abandoned { 3 } else { 6 },
            "assistant",
            json!("Finished"),
        ));
        let snapshot = read(&rows);
        let mut expected = vec![("establishedoutput".into(), false)];
        if !abandoned && !textual_parent {
            expected.push(("laterclaim".into(), false));
        }
        assert_eq!(tool_outcomes(&snapshot), expected);
        assert_eq!(
            snapshot
                .events
                .iter()
                .any(|e| e.payload["content"] == "laterclaim"),
            !abandoned
        );
    }
}

#[test]
fn concurrent_responses_with_the_same_call_id_keep_their_verified_outputs() {
    let mut first = call("same");
    first["input"] = json!({"file": "first"});
    let mut second = call("same");
    second["input"] = json!({"file": "second"});
    let snapshot = read(&[
        message(1, 0, "user", json!("Inspect")),
        block(2, 1, "one", json!([first])),
        block(3, 2, "two", json!([second])),
        linked_result(4, 3, 3, "same", "second output"),
        linked_result(5, 4, 2, "same", "first output"),
        message(6, 5, "assistant", json!("Done")),
    ]);
    let pairs: Vec<_> = native_trajectory_items(&import_trajectory(&snapshot))
        .into_iter()
        .filter_map(|item| match item {
            NativeTrajectoryItem::Tool { input, output, .. } => Some((input, output)),
            NativeTrajectoryItem::Message { .. } => None,
        })
        .collect();
    assert_eq!(
        pairs,
        [
            (json!({"file":"first"}), "first output".to_owned()),
            (json!({"file":"second"}), "second output".to_owned()),
        ]
    );
    assert!(
        snapshot
            .events
            .iter()
            .filter(|event| event.kind == EventKind::ToolCalled)
            .all(|event| event.payload["id"] == "same")
    );
}

#[test]
fn identical_assistant_records_do_not_create_another_turn_or_call() {
    let issuing = block(
        2,
        1,
        "request",
        json!([{"type":"text","text":"Checking status"}, call("raw")]),
    );
    let prefix = [
        message(1, 0, "user", json!("Inspect")),
        issuing.clone(),
        linked_result(3, 2, 2, "raw", "verified output"),
        message(4, 3, "assistant", json!("Done")),
    ];
    for rewind in [false, true] {
        let mut rows = prefix.to_vec();
        if rewind {
            rows.push(message(5, 4, "user", json!("Discard this")));
        }
        let mut copied = rows.clone();
        copied.extend([issuing.clone(), issuing.clone()]);
        if rewind {
            let chosen = message(7, 4, "user", json!("Continue"));
            rows.push(chosen.clone());
            copied.push(chosen);
        }
        let original = read(&rows);
        let repeated = read(&copied);
        assert_eq!(repeated.events, original.events);
        assert_eq!(
            tool_outcomes(&repeated),
            [("verified output".into(), false)]
        );
    }

    let mut rows = prefix[..2].to_vec();
    rows.push(issuing);
    rows.extend_from_slice(&prefix[2..]);
    assert_eq!(
        tool_outcomes(&read(&rows)),
        [("verified output".into(), false)]
    );
}

#[test]
fn conflicting_versions_of_an_assistant_uuid_cannot_become_sibling_blocks() {
    let initial = message(1, 0, "user", json!("Inspect"));
    let original = block(
        2,
        1,
        "request",
        json!([{"type":"text","text":"Original"}, call("a")]),
    );
    let mut changed = original.clone();
    changed["message"]["content"][0]["text"] = json!("Conflicting");
    changed["message"]["content"][1]["input"] = json!({"file":"other arguments"});
    for (parent, selected, absent, partial) in [
        (3, "Original", "Conflicting", false),
        (2, "Conflicting", "Original", false),
        (4, "Sibling", "Conflicting", true),
    ] {
        let rows = [
            initial.clone(),
            original.clone(),
            linked_result(3, 2, 2, "a", "verified"),
            changed.clone(),
            block(4, 1, "request", json!("Sibling")),
            message(5, parent, "user", json!("Continue")),
        ];
        let snapshot = read(&rows);
        let search = omnis_core::trajectory_search_document(&snapshot);
        assert!(search.text.contains(selected));
        assert!(!search.text.contains(absent));
        assert_eq!(search.source_complete, !partial);
        if parent == 3 {
            assert!(!search.text.contains("other arguments"));
            assert_eq!(tool_outcomes(&snapshot), [("verified".into(), false)]);
        }
        if partial {
            assert!(!search.text.contains("Original"));
        }
    }
}

#[test]
fn a_later_duplicate_source_uuid_preserves_the_earlier_verified_output() {
    let issuing = block(2, 1, "first", json!([call("raw")]));
    for duplicate in [issuing.clone(), block(2, 5, "retry", json!([call("raw")]))] {
        let rows = [
            message(1, 0, "user", json!("Inspect")),
            issuing.clone(),
            linked_result(3, 2, 2, "raw", "verifiedoutput"),
            message(4, 3, "assistant", json!("Done")),
            message(5, 4, "user", json!("withdrawn")),
            duplicate,
            message(7, 4, "user", json!("Continue here")),
        ];
        let snapshot = read(&rows);
        assert_eq!(tool_outcomes(&snapshot), [("verifiedoutput".into(), false)]);
        assert!(omnis_core::trajectory_search_document(&snapshot).source_complete);
    }
}

#[test]
fn an_outcome_can_outlive_its_previously_retained_call() {
    for linked in [true, false] {
        for keep_call in [true, false] {
            let mut rows = vec![
                message(1, 0, "user", json!("Inspect")),
                block(2, 1, "current", json!([call("a")])),
            ];
            rows.extend(compaction(3, 2, &[2]));
            let mut outcome = linked_result(5, 2, 2, "a", "lateoutput");
            if !linked {
                outcome
                    .as_object_mut()
                    .unwrap()
                    .remove("sourceToolAssistantUUID");
            }
            rows.push(outcome);
            rows.push(message(6, 5, "assistant", json!("Done")));
            rows.extend(compaction(
                7,
                6,
                if keep_call { &[2, 5, 6] } else { &[5, 6] },
            ));
            rows.extend(compaction(9, 8, if keep_call { &[2, 5] } else { &[5] }));
            let snapshot = read(&rows);
            let search = omnis_core::trajectory_search_document(&snapshot);
            assert_eq!(search.text.matches("lateoutput").count(), 1);
            assert!(search.source_complete);
            assert_eq!(tool_outcomes(&snapshot).len(), usize::from(keep_call));
        }
    }
}

#[test]
fn an_unselected_conflict_does_not_mark_a_verified_answer_incomplete() {
    let rows = [
        message(1, 0, "user", json!("Inspect")),
        block(2, 1, "current", json!([call("a")])),
        linked_result(3, 2, 2, "a", "alternative one"),
        linked_result(4, 2, 2, "a", "alternative two"),
        linked_result(5, 2, 2, "a", "selected"),
    ];
    let snapshot = read(&rows);
    assert_eq!(tool_outcomes(&snapshot), [("selected".into(), false)]);
    assert!(omnis_core::trajectory_search_document(&snapshot).source_complete);
    assert!(!import_trajectory(&snapshot).truncated);
}

#[test]
fn keeping_a_result_cannot_restore_a_call_from_an_unrelated_summary() {
    let mut rows = vec![
        message(1, 0, "user", json!("Inspect")),
        block(2, 1, "old", json!([call("raw")])),
    ];
    rows.extend(compaction(3, 2, &[2]));
    rows.push(message(5, 1, "user", json!("Replace this turn")));
    rows.push(block(6, 5, "current", json!([call("raw")])));
    rows.extend(compaction(7, 6, &[6]));
    rows.push(linked_result(9, 6, 6, "raw", "verifiedoutput"));
    rows.push(linked_result(10, 9, 2, "raw", "withdrawnoutput"));
    rows.extend(compaction(11, 10, &[9, 10]));
    let snapshot = read(&rows);
    let search = omnis_core::trajectory_search_document(&snapshot);
    assert!(search.text.contains("verifiedoutput"));
    assert!(!search.text.contains("withdrawnoutput"));
    assert!(search.source_complete);
}

#[test]
fn a_call_and_its_earlier_result_can_move_to_different_summaries() {
    for new_request in [true, false] {
        let mut rows = vec![
            message(1, 0, "user", json!("Inspect both")),
            block(2, 1, "r", json!([call("a"), call("b")])),
        ];
        rows.extend(compaction(3, 2, &[2]));
        rows.push(linked_result(5, 2, 2, "a", "firstoutput"));
        if new_request {
            rows.push(message(
                12,
                5,
                "user",
                json!("Start an independent request"),
            ));
        }
        rows.push(message(
            6,
            if new_request { 12 } else { 5 },
            "user",
            json!([result("b", "lateoutput", false)]),
        ));
        rows.push(message(7, 6, "assistant", json!("Done")));
        rows.extend(compaction(8, 7, &[6, 7]));
        rows.extend(compaction(10, 9, &[2]));
        let snapshot = read(&rows);
        let mut recovery = Vec::new();
        omnis_core::write_recovery_handoff(&snapshot, &mut recovery).unwrap();
        assert_eq!(
            String::from_utf8(recovery)
                .unwrap()
                .matches("lateoutput")
                .count(),
            usize::from(!new_request)
        );
        assert!(tool_outcomes(&snapshot).is_empty());
    }
}

#[test]
fn an_ambiguous_result_only_warns_if_the_selected_history_contains_a_candidate_call() {
    for (anchor, incomplete) in [(1, false), (2, true), (3, true)] {
        let snapshot = read(&[
            message(1, 0, "user", json!("Inspect both")),
            block(2, 1, "first", json!([call("raw")])),
            block(3, 2, "second", json!([call("raw")])),
            block(4, anchor, "carrier", json!([call("bridge")])),
            linked_result(5, 4, 4, "bridge", "verifiedoutput"),
            message(
                6,
                5,
                "user",
                json!([result("raw", "ambiguousoutput", false)]),
            ),
        ]);
        let search = omnis_core::trajectory_search_document(&snapshot);
        assert_eq!(search.source_complete, !incomplete);
        assert_eq!(import_trajectory(&snapshot).truncated, incomplete);
        assert_eq!(search.text.contains("ambiguousoutput"), !incomplete);
        assert_eq!(tool_outcomes(&snapshot), [("verifiedoutput".into(), false)]);
    }
}

#[test]
fn a_discarded_call_cannot_remove_a_selected_documentary_result() {
    for compacted in [false, true] {
        for discarded in [false, true] {
            let mut rows = vec![message(1, 0, "user", json!("Task"))];
            if discarded {
                rows.extend([
                    message(80, 1, "user", json!("Withdrawn")),
                    block(81, 80, "old", json!([call("x")])),
                ]);
            }
            let parent = if compacted {
                rows.extend(compaction(2, 1, &[]));
                3
            } else {
                rows.push(message(8, 0, "user", json!("Independent conversation")));
                8
            };
            rows.push(message(
                4,
                parent,
                "user",
                json!([result("x", "documentaryoutput", false)]),
            ));
            let snapshot = read(&rows);
            let search = omnis_core::trajectory_search_document(&snapshot);
            assert!(search.source_complete);
            assert_eq!(search.text.matches("documentaryoutput").count(), 1);
            assert!(tool_outcomes(&snapshot).is_empty());
            assert!(!import_trajectory(&snapshot).truncated);
        }
    }
}

#[test]
fn duplicate_source_uuids_warn_only_for_a_possible_selected_call() {
    for (second_parent, result_parent, source, tool, warn) in [
        (1, 2, 2, "raw", true),
        (2, 2, 2, "raw", true),
        (1, 1, 2, "raw", false),
        (2, 2, 2, "other", false),
        (2, 2, 99, "raw", false),
    ] {
        let rows = [
            message(1, 0, "user", json!("Inspect")),
            block(2, 1, "first", json!([call("raw")])),
            block(2, second_parent, "second", json!([call("raw")])),
            linked_result(4, result_parent, source, tool, "unverifiedoutput"),
        ];
        let snapshot = read(&rows);
        let search = omnis_core::trajectory_search_document(&snapshot);
        assert_eq!(search.source_complete, !warn);
        assert_eq!(import_trajectory(&snapshot).truncated, warn);
        assert!(!search.text.contains("unverifiedoutput"));
        assert!(tool_outcomes(&snapshot).is_empty());
    }
}

#[test]
fn a_repeated_source_uuid_with_one_matching_call_preserves_the_verified_pair() {
    for (parent, prior, latest, sought, different_writer, expected) in [
        (1, "x", "y", "y", false, true),
        (1, "y", "x", "x", false, true),
        (2, "x", "y", "x", false, true),
        (1, "x", "y", "x", false, false),
        (2, "x", "x", "x", true, true),
    ] {
        let mut rows = [
            message(1, 0, "user", json!("Inspect")),
            block(2, 1, "first", json!([call(prior)])),
            block(2, parent, "retry", json!([call(latest)])),
            linked_result(3, 2, 2, sought, "selectedoutput"),
        ];
        if different_writer {
            rows[2]["agentId"] = json!("another-writer");
        }
        let snapshot = read(&rows);
        let search = omnis_core::trajectory_search_document(&snapshot);
        assert!(search.source_complete);
        assert_eq!(search.text.contains("selectedoutput"), expected);
        assert_eq!(
            tool_outcomes(&snapshot),
            if expected {
                vec![("selectedoutput".into(), false)]
            } else {
                vec![]
            }
        );
    }
}
