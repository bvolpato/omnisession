use std::io::Write;

use omnis_adapters::{ClaudeAdapter, ProviderAdapter};
use omnis_core::{
    ImportTrajectory, NativeTrajectoryItem, import_trajectory, native_trajectory_items,
};
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
    read_session(rows, false)
}

fn read_session(rows: &[Value], preview: bool) -> CanonicalSnapshot {
    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let id = Uuid::from_u128(99);
    let mut file = std::fs::File::create(project.join(format!("{id}.jsonl"))).unwrap();
    for row in rows {
        writeln!(file, "{row}").unwrap();
    }
    let adapter = ClaudeAdapter::with_root(root.path());
    let session = SessionRef::new(Provider::Claude, id.to_string());
    if preview {
        adapter.preview_session(&session)
    } else {
        adapter.read_session(&session)
    }
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

fn handoff_messages(handoff: &ImportTrajectory) -> Vec<&str> {
    handoff
        .items
        .iter()
        .filter(|item| !item.text.starts_with("[OmniSession retained "))
        .map(|item| item.text.lines().last().unwrap())
        .collect()
}

fn recovery_text(snapshot: &CanonicalSnapshot) -> String {
    let mut bytes = Vec::new();
    omnis_core::write_recovery_handoff(snapshot, &mut bytes).unwrap();
    String::from_utf8(bytes).unwrap()
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

const CLAUDE_SUMMARY_ENVELOPE: &str = "This session is being continued from a previous conversation that ran out of context. The summary below covers the earlier portion of the conversation.\n\nSummary:\n";

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

fn directory_attachment(index: u128, parent: u128) -> Value {
    let mut row = opaque(index, parent, "attachment");
    row["attachment"] = json!({"type":"directory", "path":"fixture", "content":""});
    row
}

#[test]
fn a_retained_attachment_can_repeat_only_with_the_same_definition_and_parent() {
    let mut attachment = opaque(3, 2, "attachment");
    attachment["attachment"] = json!({"type":"directory","path":"fixture","content":"original"});
    let decision = message(2, 1, "assistant", json!("Retained decision"));
    for (mode, complete) in [
        ("single", true),
        ("copy", true),
        ("changed content", false),
        ("changed parent", false),
        ("changed parent definition", false),
    ] {
        let mut rows = vec![
            message(1, 0, "user", json!("Earlier request")),
            decision.clone(),
        ];
        let mut repeated = attachment.clone();
        if mode == "changed parent definition" {
            rows.push(message(7, 2, "assistant", json!("First attachment parent")));
            repeated["parentUuid"] = json!(Uuid::from_u128(7));
        }
        rows.push(repeated.clone());
        match mode {
            "changed content" => repeated["attachment"]["content"] = json!("different"),
            "changed parent" => repeated["parentUuid"] = json!(Uuid::from_u128(1)),
            "changed parent definition" => rows.push(message(
                7,
                2,
                "assistant",
                json!("Different attachment parent"),
            )),
            _ => {}
        }
        if mode != "single" {
            rows.push(repeated);
        }
        rows.extend(compaction(4, 3, &[2, 3]));
        rows.push(message(6, 5, "user", json!("Continue")));
        for preview in [false, true] {
            let snapshot = read_session(&rows, preview);
            let handoff = import_trajectory(&snapshot);
            assert_eq!(!handoff.truncated, complete, "{mode}");
            assert_eq!(
                handoff.items.iter().any(|i| i.text == "Retained decision"),
                complete,
                "{mode}"
            );
            assert_eq!(handoff.items.last().unwrap().text, "Continue");
        }
    }
}

#[test]
fn a_sampled_attachment_link_compares_the_complete_definitions() {
    for conflict in [false, true] {
        let mut rows = vec![
            message(1, 0, "user", json!("Start")),
            message(2, 1, "assistant", json!("Retained decision")),
        ];
        // Put both attachments between the four-MiB head and tail samples.
        let padding = json!({"type":"progress","data":"p".repeat(64*1024)});
        rows.extend(std::iter::repeat_n(padding.clone(), 65));
        let mut attachment = opaque(3, 2, "attachment");
        attachment["attachment"] = json!({"type":"directory", "content":"first"});
        rows.push(attachment.clone());
        if conflict {
            attachment["attachment"]["content"] = json!("second");
        }
        rows.push(attachment);
        rows.extend(std::iter::repeat_n(padding, 65));
        rows.extend(compaction(4, 3, &[2, 3]));
        rows.push(message(6, 5, "user", json!("Continue")));
        let handoff = import_trajectory(&read_session(&rows, true));
        assert!(handoff.truncated);
        assert_eq!(
            handoff
                .items
                .iter()
                .any(|item| item.text == "Retained decision"),
            !conflict
        );
        assert_eq!(handoff.items.last().unwrap().text, "Continue");
    }
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
fn a_rejected_replacement_keeps_its_predecessor_in_recovery_after_a_branch_return() {
    let left = message(2, 1, "assistant", json!("Discarded left"));
    let mut base = vec![
        message(1, 0, "user", json!("Task")),
        left.clone(),
        message(3, 1, "assistant", json!("Verified right")),
    ];
    base.extend(compaction(4, 3, &[3]));
    base.extend([
        message(6, 3, "user", json!("First continuation")),
        message(7, 6, "assistant", json!("Known reply")),
    ]);
    let back = message(11, 7, "user", json!("On verified branch"));
    let last = message(12, 2, "user", json!("New child of omitted left"));
    for (rejected, copied, continued) in [
        (false, false, true),
        (true, false, true),
        (true, true, true),
        (true, false, false),
        (true, true, false),
    ] {
        let mut rows = base.clone();
        if rejected {
            rows.extend(compaction(8, 7, &[3, 99]));
        }
        if copied {
            rows.push(left.clone());
        }
        rows.push(back.clone());
        if continued {
            rows.push(last.clone());
        }
        let expected: &[&str] = if !rejected {
            &["Summary 4", "Verified right", "New child of omitted left"]
        } else if continued {
            &["Task", "Discarded left", "New child of omitted left"]
        } else {
            &[
                "Task",
                "Verified right",
                "First continuation",
                "Known reply",
                "On verified branch",
            ]
        };
        for preview in [false, true] {
            let snapshot = read_session(&rows, preview);
            let handoff = import_trajectory(&snapshot);
            assert_eq!(
                handoff_messages(&handoff),
                expected,
                "rejected={rejected} copied={copied} continued={continued} preview={preview}"
            );
            assert_eq!(handoff.truncated, rejected);
            assert_eq!(
                omnis_core::trajectory_search_document(&snapshot).source_complete,
                !rejected
            );
            let mut recovery = Vec::new();
            omnis_core::write_recovery_handoff(&snapshot, &mut recovery).unwrap();
            let recovery = String::from_utf8(recovery).unwrap();
            assert!(recovery.contains("Summary 4") && recovery.contains("Verified right"));
        }
    }
}

#[test]
fn an_independent_root_cannot_restore_a_compacted_root_attachment() {
    for nested in [false, true] {
        let mut outer = directory_attachment(30, 0);
        outer["parentUuid"] = Value::Null;
        let mut rows = vec![outer];
        if nested {
            rows.push(directory_attachment(32, 30));
        }
        rows.extend([
            message(1, if nested { 32 } else { 30 }, "user", json!("Task")),
            message(2, 1, "assistant", json!("Verified answer")),
        ]);
        rows.extend(compaction(3, 2, &[2]));
        rows.push(message(10, 0, "user", json!("Independent root")));
        rows.push(directory_attachment(31, 30));
        rows.push(message(11, 31, "user", json!("New request")));
        for preview in [false, true] {
            let snapshot = read_session(&rows, preview);
            let active = import_trajectory(&snapshot);
            assert_eq!(handoff_messages(&active), ["New request"]);
            assert!(active.truncated);
            let search = omnis_core::trajectory_search_document(&snapshot);
            assert!(!search.source_complete);
            let recovery = recovery_text(&snapshot);
            assert!(recovery.contains("Verified answer") && recovery.contains("Summary 3"));
            assert!(!recovery.contains("Independent root"));
        }
    }
}

#[test]
fn the_latest_checkpoint_selects_the_continuation_of_an_older_root_attachment() {
    let kept = &["Summary 7", "Second reply", "New request"][..];
    let recent = &["Summary 7", "New request"][..];
    let detached = &["New request"][..];
    let epoch = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z").unwrap();
    for (nested, explicitly_kept, intervening, links, delay, expected) in [
        (false, true, false, 0, None, kept),
        (false, true, false, 1, None, kept),
        (false, true, false, 2, None, kept),
        (false, true, true, 0, None, kept),
        (false, true, true, 1, None, kept),
        (true, true, true, 2, Some(6), kept),
        (true, false, false, 1, Some(1), recent),
        (true, false, false, 1, Some(6), detached),
        (true, false, false, 1, None, detached),
    ] {
        let mut root = directory_attachment(30, 0);
        root["parentUuid"] = Value::Null;
        let mut rows = vec![root];
        if nested {
            rows.push(directory_attachment(33, 30));
        }
        rows.extend([
            message(1, if nested { 33 } else { 30 }, "user", json!("First task")),
            message(2, 1, "assistant", json!("First reply")),
        ]);
        rows.extend(compaction(3, 2, &[2]));
        rows.extend([
            message(5, 0, "user", json!("Second task")),
            message(6, 5, "assistant", json!("Second reply")),
        ]);
        rows.extend(compaction(
            7,
            6,
            if explicitly_kept { &[6, 30] } else { &[6] },
        ));
        let summary_time =
            epoch + chrono::Duration::seconds(i64::try_from(rows.len() - 1).unwrap());
        for (seconds, row) in (0..).zip(&mut rows) {
            row["timestamp"] = json!(epoch + chrono::Duration::seconds(seconds));
            row["isSidechain"] = json!(false);
        }
        let date = |row: &mut Value, milliseconds| {
            row["timestamp"] = json!(summary_time + chrono::Duration::milliseconds(milliseconds));
            row["isSidechain"] = json!(false);
        };
        if intervening {
            let mut unrelated = message(12, 0, "user", json!("Independent root"));
            date(&mut unrelated, 100);
            rows.push(unrelated);
        }
        let mut parent = 30;
        for index in 0..links {
            let id = 40 + index;
            let mut link = directory_attachment(id, parent);
            if let Some(seconds) = delay {
                date(&mut link, seconds * 1000 + i64::try_from(index).unwrap());
            }
            rows.push(link);
            parent = id;
        }
        let mut request = message(50, parent, "user", json!("New request"));
        if let Some(seconds) = delay {
            date(&mut request, seconds * 1000 + i64::try_from(links).unwrap());
        }
        rows.push(request);
        for preview in [false, true] {
            let case = (nested, explicitly_kept, intervening, links, delay, preview);
            let snapshot = read_session(&rows, preview);
            let active = import_trajectory(&snapshot);
            assert_eq!(handoff_messages(&active), expected, "{case:?}");
            assert_eq!(active.truncated, !explicitly_kept, "{case:?}");
            let search = omnis_core::trajectory_search_document(&snapshot);
            assert_eq!(search.source_complete, explicitly_kept, "{case:?}");
            let recovery = recovery_text(&snapshot);
            if expected == detached {
                assert!(
                    recovery.contains("First reply") && recovery.contains("Summary 3"),
                    "{case:?}"
                );
            } else {
                assert!(
                    recovery.contains("Second reply") && recovery.contains("Summary 7"),
                    "{case:?}"
                );
                assert!(
                    !recovery.contains("First reply") && !search.text.contains("First reply"),
                    "{case:?}"
                );
            }
            assert!(
                !recovery.contains("Independent root") && !search.text.contains("Independent root"),
                "{case:?}"
            );
        }
    }
}

#[test]
fn a_retained_directory_with_a_user_parent_keeps_its_checkpoint_position() {
    let mut base = vec![
        message(1, 0, "user", json!("Original task")),
        directory_attachment(30, 1),
        message(2, 1, "assistant", json!("Retained reply")),
    ];
    base.extend(compaction(3, 2, &[2, 30]));
    for intervening_root in [false, true] {
        for via_link in [false, true] {
            let mut rows = base.clone();
            if intervening_root {
                rows.push(message(10, 0, "user", json!("Independent root")));
            }
            if via_link {
                rows.push(directory_attachment(31, 30));
            }
            rows.push(message(
                11,
                if via_link { 31 } else { 30 },
                "user",
                json!("New request"),
            ));
            for preview in [false, true] {
                let case = (intervening_root, via_link, preview);
                let snapshot = read_session(&rows, preview);
                let active = import_trajectory(&snapshot);
                assert_eq!(
                    handoff_messages(&active),
                    ["Summary 3", "Retained reply", "New request"],
                    "{case:?}"
                );
                assert!(!active.truncated, "{case:?}");
                let search = omnis_core::trajectory_search_document(&snapshot);
                assert!(search.source_complete, "{case:?}");
                for text in [&search.text, &recovery_text(&snapshot)] {
                    assert!(
                        text.contains("Retained reply") && !text.contains("Independent root"),
                        "{case:?}"
                    );
                }
            }
        }
    }
}

#[test]
fn a_retained_root_directory_only_supplies_its_position_to_children_written_after_compaction() {
    let mut root = directory_attachment(30, 0);
    root["parentUuid"] = Value::Null;
    let full = &["Summary 3", "Retained reply", "New request"][..];
    let summary = &["Summary 3", "New request"][..];
    let epoch = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z").unwrap();
    for (before, new_link_delay, expected) in [
        (false, None, summary),
        (true, None, full),
        (true, Some(13), &["New request"][..]),
        (true, Some(1), summary),
    ] {
        let child = directory_attachment(31, 30);
        let mut rows = vec![root.clone()];
        if before {
            rows.push(child.clone());
        }
        rows.extend([
            message(1, 30, "user", json!("Original task")),
            message(2, 1, "assistant", json!("Retained reply")),
        ]);
        rows.extend(compaction(3, 2, &[30, 2]));
        let boundary = rows.len();
        if !before {
            rows.push(child);
        }
        let parent = if new_link_delay.is_some() {
            rows.push(directory_attachment(32, 31));
            32
        } else {
            31
        };
        rows.push(message(11, parent, "user", json!("New request")));
        if let Some(delay) = new_link_delay {
            for (i, row) in rows.iter_mut().enumerate() {
                let seconds = i64::try_from(i).unwrap() + if i >= boundary { delay - 1 } else { 0 };
                row["timestamp"] = json!(epoch + chrono::Duration::seconds(seconds));
                row["isSidechain"] = json!(false);
            }
        }
        for preview in [false, true] {
            let case = (before, new_link_delay, preview);
            let snapshot = read_session(&rows, preview);
            let active = import_trajectory(&snapshot);
            assert_eq!(handoff_messages(&active), expected, "{case:?}");
            assert_eq!(active.truncated, new_link_delay.is_some(), "{case:?}");
            let search = omnis_core::trajectory_search_document(&snapshot);
            assert_eq!(search.source_complete, new_link_delay.is_none(), "{case:?}");
            for text in [&search.text, &recovery_text(&snapshot)] {
                assert_eq!(text.contains("Retained reply"), before, "{case:?}");
            }
        }
    }
}

#[test]
fn a_request_returning_to_an_omitted_directory_keeps_the_whole_retained_list() {
    for (kept_directory, through_progress) in [(true, false), (false, false), (false, true)] {
        let mut rows = vec![
            message(1, 0, "user", json!("Original task")),
            directory_attachment(30, 1),
        ];
        if through_progress {
            rows.push(opaque(31, 30, "progress"));
        }
        rows.push(message(2, 1, "assistant", json!("Retained reply")));
        rows.extend(compaction(
            3,
            2,
            if kept_directory { &[1, 30, 2] } else { &[1, 2] },
        ));
        rows.push(message(
            11,
            if through_progress { 31 } else { 30 },
            "user",
            json!("New request"),
        ));
        let expected: &[&str] = if kept_directory {
            &["Summary 3", "Original task", "New request"]
        } else {
            &[
                "Summary 3",
                "Original task",
                "Retained reply",
                "New request",
            ]
        };
        for preview in [false, true] {
            let snapshot = read_session(&rows, preview);
            let active = import_trajectory(&snapshot);
            assert_eq!(
                handoff_messages(&active),
                expected,
                "kept={kept_directory}, progress={through_progress}, preview={preview}"
            );
            assert!(!active.truncated);
            let search = omnis_core::trajectory_search_document(&snapshot);
            assert!(search.source_complete);
            for text in [&search.text, &recovery_text(&snapshot)] {
                assert_eq!(text.contains("Retained reply"), !kept_directory);
            }
        }
    }
}

#[test]
fn an_omitted_directory_keeps_the_checkpoint_tail_after_an_independent_root() {
    for (root_directory, intervening_root) in
        [(false, false), (false, true), (true, false), (true, true)]
    {
        let mut rows = if root_directory {
            let mut directory = directory_attachment(30, 0);
            directory["parentUuid"] = Value::Null;
            vec![directory, message(1, 30, "user", json!("Original task"))]
        } else {
            vec![
                message(1, 0, "user", json!("Original task")),
                directory_attachment(30, 1),
            ]
        };
        rows.push(message(2, 1, "assistant", json!("Retained reply")));
        rows.extend(compaction(3, 2, &[2]));
        if intervening_root {
            rows.push(message(10, 0, "user", json!("Independent root")));
        }
        rows.push(message(11, 30, "user", json!("New request")));
        for preview in [false, true] {
            let snapshot = read_session(&rows, preview);
            let active = import_trajectory(&snapshot);
            assert_eq!(
                handoff_messages(&active),
                ["Summary 3", "Retained reply", "New request"],
                "root directory={root_directory}, independent={intervening_root}, preview={preview}"
            );
            assert!(!active.truncated);
            let search = omnis_core::trajectory_search_document(&snapshot);
            assert!(search.source_complete);
            for text in [&search.text, &recovery_text(&snapshot)] {
                assert!(text.contains("Retained reply") && !text.contains("Independent root"));
            }
        }
    }
}

#[test]
fn progress_written_between_checkpoints_uses_the_latest_retained_tail() {
    let mut base = vec![
        message(1, 0, "user", json!("Original task")),
        directory_attachment(30, 1),
        message(2, 1, "assistant", json!("First reply")),
    ];
    base.extend(compaction(3, 2, &[1, 2]));
    base.push(opaque(50, 30, "progress"));
    base.push(message(6, 2, "assistant", json!("Second reply")));
    base.extend(compaction(7, 6, &[6]));
    base.push(message(10, 0, "user", json!("Independent root")));
    for parent in [30, 50] {
        let mut rows = base.clone();
        rows.push(message(11, parent, "user", json!("New request")));
        for preview in [false, true] {
            let snapshot = read_session(&rows, preview);
            let active = import_trajectory(&snapshot);
            assert_eq!(
                handoff_messages(&active),
                ["Summary 7", "Second reply", "New request"],
                "parent={parent}, preview={preview}"
            );
            assert!(!active.truncated);
            let search = omnis_core::trajectory_search_document(&snapshot);
            assert!(search.source_complete);
            for text in [&search.text, &recovery_text(&snapshot)] {
                assert!(text.contains("Second reply") && !text.contains("Independent root"));
            }
        }
    }
}

fn meta_user(id: u128, parent: u128) -> Value {
    let mut row = message(id, parent, "user", json!("Internal meta text"));
    row["isMeta"] = json!(true);
    row
}

fn turn_duration(id: u128, parent: u128) -> Value {
    let mut row = opaque(id, parent, "system");
    row["subtype"] = json!("turn_duration");
    row["durationMs"] = json!(300);
    row["level"] = json!("info");
    row["content"] = json!("Turn duration");
    row
}

fn put_between_preview_windows(rows: &mut Vec<Value>, index: usize) {
    let padding = json!({"type":"progress", "padding":"x".repeat(64 * 1024)});
    for position in [index + 1, index] {
        rows.splice(position..position, std::iter::repeat_n(padding.clone(), 68));
    }
}

#[test]
fn an_empty_checkpoint_matches_the_recent_independent_turn_for_a_removed_directory() {
    let mut base = vec![
        message(1, 0, "user", json!("Original task")),
        message(2, 1, "assistant", json!("Removed reply")),
        directory_attachment(30, 2),
    ];
    base.extend(compaction(3, 2, &[]));
    base.push(message(10, 0, "user", json!("Independent turn")));
    for (parent, delay) in [(10, 1), (30, 1), (45, 1), (45, 9)] {
        let mut rows = base.clone();
        if parent == 45 {
            rows.push(meta_user(45, 30));
        }
        rows.push(message(11, parent, "user", json!("New request")));
        for (i, row) in rows.iter_mut().enumerate() {
            let second = i + 1 + if i >= base.len() { delay - 1 } else { 0 };
            row["timestamp"] = json!(format!("2026-01-01T00:00:{second:02}Z"));
            row["isSidechain"] = json!(false);
        }
        for preview in [false, true] {
            let snapshot = read_session(&rows, preview);
            let active = import_trajectory(&snapshot);
            let expected: &[&str] = if delay == 1 {
                &["Independent turn", "New request"]
            } else {
                &["New request"]
            };
            assert_eq!(
                handoff_messages(&active),
                expected,
                "parent={parent}, preview={preview}"
            );
            let search = omnis_core::trajectory_search_document(&snapshot);
            for text in [&search.text, &recovery_text(&snapshot)] {
                if delay == 1 {
                    assert!(text.contains("Independent turn"));
                }
                assert!(text.contains("New request"));
            }
        }
    }
}

#[test]
fn progress_through_a_compacted_duration_selects_the_later_checkpoint() {
    for (link, carrier, retain_first) in [
        (
            directory_attachment(30, 2),
            opaque(50, 30, "progress"),
            true,
        ),
        (turn_duration(30, 2), opaque(50, 30, "progress"), true),
        (directory_attachment(30, 2), meta_user(50, 30), true),
        (directory_attachment(30, 2), meta_user(50, 30), false),
    ] {
        let mut rows = vec![
            message(1, 0, "user", json!("First task")),
            message(2, 1, "assistant", json!("First reply")),
            link,
        ];
        rows.extend(compaction(3, 2, if retain_first { &[2] } else { &[] }));
        rows.extend([
            carrier,
            message(5, 2, "user", json!("Second task")),
            message(6, 5, "assistant", json!("Second reply")),
        ]);
        rows.extend(compaction(7, 6, &[6]));
        rows.push(message(10, 0, "user", json!("Independent root")));
        rows.push(message(11, 50, "user", json!("New request")));
        for (second, row) in (1..).zip(&mut rows) {
            row["timestamp"] = json!(format!("2026-01-01T00:00:{second:02}Z"));
            row["isSidechain"] = json!(false);
        }
        for preview in [false, true] {
            let snapshot = read_session(&rows, preview);
            let active = import_trajectory(&snapshot);
            assert_eq!(
                handoff_messages(&active),
                ["Summary 7", "Second reply", "New request"]
            );
            let search = omnis_core::trajectory_search_document(&snapshot);
            for text in [&search.text, &recovery_text(&snapshot)] {
                assert!(text.contains("Second reply") && !text.contains("Independent root"));
            }
        }
    }
}

#[test]
fn meta_user_records_carry_the_removed_directory_link_to_the_visible_request() {
    let mut root = directory_attachment(30, 0);
    root["parentUuid"] = Value::Null;
    let mut base = vec![
        root,
        message(1, 30, "user", json!("Original task")),
        message(2, 1, "assistant", json!("Retained reply")),
    ];
    base.extend(compaction(3, 2, &[2]));
    for (via_meta, padded) in [(false, false), (true, false), (true, true)] {
        let mut rows = base.clone();
        if via_meta {
            rows.push(meta_user(45, 30));
        }
        rows.push(message(
            11,
            if via_meta { 45 } else { 30 },
            "user",
            json!("New request"),
        ));
        if padded {
            put_between_preview_windows(&mut rows, base.len());
        }
        for preview in [false, true] {
            let snapshot = read_session(&rows, preview);
            let active = import_trajectory(&snapshot);
            assert_eq!(
                handoff_messages(&active),
                ["Summary 3", "Retained reply", "New request"],
                "via_meta={via_meta} padded={padded} preview={preview}"
            );
            if !padded || !preview {
                assert!(!active.truncated);
            }
            let search = omnis_core::trajectory_search_document(&snapshot);
            if !padded || !preview {
                assert!(search.source_complete);
            }
            for text in [&search.text, &recovery_text(&snapshot)] {
                assert!(text.contains("Retained reply") && !text.contains("Internal meta text"));
            }
        }
    }
}

#[test]
fn the_latest_duration_identifies_its_parent_after_empty_compaction() {
    let mut base = vec![
        message(1, 0, "user", json!("Original task")),
        message(2, 1, "assistant", json!("Removed reply")),
        directory_attachment(30, 2),
    ];
    base.extend(compaction(3, 2, &[]));
    base.push(message(10, 0, "user", json!("Independent A")));
    base.push(message(11, 0, "user", json!("Independent B")));
    for (duration_parent, via_meta) in [
        (None, false),
        (Some(10), false),
        (Some(2), false),
        (Some(10), true),
    ] {
        let mut rows = base.clone();
        if let Some(parent) = duration_parent {
            rows.push(if via_meta {
                meta_user(50, parent)
            } else {
                turn_duration(50, parent)
            });
        }
        rows.push(message(12, 30, "user", json!("New request")));
        for (second, row) in (1..).zip(&mut rows) {
            row["timestamp"] = json!(format!("2026-01-01T00:00:{second:02}Z"));
            row["isSidechain"] = json!(false);
        }
        let (selected, other) = if duration_parent == Some(10) {
            ("Independent A", "Independent B")
        } else {
            ("Independent B", "Independent A")
        };
        for preview in [false, true] {
            let snapshot = read_session(&rows, preview);
            let active = import_trajectory(&snapshot);
            assert_eq!(handoff_messages(&active), [selected, "New request"]);
            for text in [
                omnis_core::trajectory_search_document(&snapshot).text,
                recovery_text(&snapshot),
            ] {
                assert!(text.contains(selected) && !text.contains(other));
            }
        }
    }
}

#[test]
fn hidden_users_preserve_the_link_between_a_call_and_its_result() {
    for via_meta in [false, true] {
        let mut rows = vec![
            message(1, 0, "user", json!("Read the file")),
            message(2, 1, "assistant", json!([call("lookup")])),
        ];
        if via_meta {
            rows.push(meta_user(50, 2));
        }
        rows.push(message(
            3,
            if via_meta { 50 } else { 2 },
            "user",
            json!([result("lookup", "Known output", false)]),
        ));
        for preview in [false, true] {
            let snapshot = read_session(&rows, preview);
            assert_eq!(
                tool_outcomes(&snapshot),
                [("Known output".to_owned(), false)]
            );
            for text in [
                omnis_core::trajectory_search_document(&snapshot).text,
                recovery_text(&snapshot),
            ] {
                assert!(text.contains("Known output") && !text.contains("Internal meta text"));
            }
        }
    }
}

#[test]
fn a_new_checkpoint_supersedes_the_cutoff_of_its_retained_hidden_record() {
    for keep_hidden in [false, true] {
        let mut rows = vec![
            message(1, 0, "user", json!("Original task")),
            message(2, 1, "assistant", json!("Removed reply")),
            directory_attachment(30, 2),
        ];
        rows.extend(compaction(3, 2, &[]));
        rows.extend([
            message(10, 0, "user", json!("Independent B")),
            meta_user(50, 30),
            message(12, 50, "user", json!("New request")),
        ]);
        rows.extend(compaction(
            20,
            12,
            if keep_hidden { &[50, 12] } else { &[12] },
        ));
        rows.push(message(22, 12, "assistant", json!("Second reply")));
        for (i, row) in rows.iter_mut().enumerate() {
            let second = i + 1 + if i >= 6 { 9 } else { 0 };
            row["timestamp"] = json!(format!("2026-01-01T00:00:{second:02}Z"));
            row["isSidechain"] = json!(false);
        }
        for preview in [false, true] {
            let snapshot = read_session(&rows, preview);
            assert_eq!(
                handoff_messages(&import_trajectory(&snapshot)),
                ["Summary 20", "New request", "Second reply"]
            );
        }
    }
}

#[test]
fn a_final_hidden_root_ends_the_visible_branch() {
    let base = [
        message(1, 0, "user", json!("Independent A")),
        message(2, 0, "user", json!("Independent B")),
    ];
    for hidden_root in [false, true] {
        let mut rows = base.to_vec();
        if hidden_root {
            rows.push(meta_user(50, 0));
        }
        for preview in [false, true] {
            let snapshot = read_session(&rows, preview);
            let expected: &[&str] = if hidden_root { &[] } else { &["Independent B"] };
            assert_eq!(handoff_messages(&import_trajectory(&snapshot)), expected);
            for text in [
                omnis_core::trajectory_search_document(&snapshot).text,
                recovery_text(&snapshot),
            ] {
                assert!(
                    !text.contains("Independent A")
                        && text.contains("Independent B") != hidden_root
                );
            }
        }
    }
}

#[test]
fn a_final_hidden_user_preserves_a_linked_exchange_with_a_missing_ancestor() {
    for parent in [0, 9999] {
        let rows = [
            message(1, parent, "user", json!("Independent A")),
            message(2, 1, "assistant", json!("Known reply")),
            message(3, 0, "user", json!("Independent B")),
            meta_user(50, 2),
        ];
        for preview in [false, true] {
            let snapshot = read_session(&rows, preview);
            let active = import_trajectory(&snapshot);
            assert_eq!(handoff_messages(&active), ["Independent A", "Known reply"]);
            assert_eq!(active.truncated, parent != 0);
            for text in [
                omnis_core::trajectory_search_document(&snapshot).text,
                recovery_text(&snapshot),
            ] {
                assert!(text.contains("Known reply") && !text.contains("Independent B"));
            }
        }
    }
}

#[test]
fn a_copy_of_a_hidden_user_displaced_by_compaction_preserves_the_new_request() {
    let hidden = meta_user(4, 1);
    let mut rows = vec![
        message(1, 0, "user", json!("Original task")),
        hidden.clone(),
        message(3, 1, "assistant", json!("Retained reply")),
    ];
    rows.extend(compaction(6, 3, &[3]));
    rows.push(message(8, 7, "user", json!("New request")));
    for copied in [false, true] {
        if copied {
            rows.push(hidden.clone());
        }
        for preview in [false, true] {
            let snapshot = read_session(&rows, preview);
            assert_eq!(
                handoff_messages(&import_trajectory(&snapshot)),
                ["Summary 6", "Retained reply", "New request"]
            );
            for text in [
                omnis_core::trajectory_search_document(&snapshot).text,
                recovery_text(&snapshot),
            ] {
                assert!(text.contains("New request") && !text.contains("Internal meta text"));
            }
        }
    }
}

#[test]
fn a_copy_of_a_retained_hidden_record_reselects_its_checkpoint() {
    let hidden = meta_user(4, 2);
    let mut rows = vec![
        message(1, 0, "user", json!("Original task")),
        message(2, 1, "assistant", json!("Retained reply")),
        hidden.clone(),
    ];
    rows.extend(compaction(6, 2, &[2, 4]));
    rows.push(message(8, 0, "user", json!("Independent task")));
    for copied in [false, true] {
        if copied {
            rows.push(hidden.clone());
        }
        let expected: &[&str] = if copied {
            &["Summary 6", "Retained reply"]
        } else {
            &["Independent task"]
        };
        for preview in [false, true] {
            let snapshot = read_session(&rows, preview);
            assert_eq!(handoff_messages(&import_trajectory(&snapshot)), expected);
            for text in [
                omnis_core::trajectory_search_document(&snapshot).text,
                recovery_text(&snapshot),
            ] {
                assert_eq!(text.contains("Retained reply"), copied);
                assert_eq!(text.contains("Independent task"), !copied);
            }
        }
    }
}

#[test]
fn a_copy_of_a_hidden_record_uses_the_current_definition_of_its_parent() {
    let hidden = meta_user(4, 1);
    for replaced in [false, true] {
        let mut rows = vec![message(1, 0, "user", json!("Old request")), hidden.clone()];
        if replaced {
            rows.push(message(1, 0, "user", json!("New request")));
        }
        rows.extend([
            hidden.clone(),
            message(5, 4, "assistant", json!("Known reply")),
        ]);
        let (selected, old) = if replaced {
            ("New request", "Old request")
        } else {
            ("Old request", "New request")
        };
        for preview in [false, true] {
            let snapshot = read_session(&rows, preview);
            assert_eq!(
                handoff_messages(&import_trajectory(&snapshot)),
                [selected, "Known reply"]
            );
            for text in [
                omnis_core::trajectory_search_document(&snapshot).text,
                recovery_text(&snapshot),
            ] {
                assert!(text.contains(selected) && !text.contains(old));
            }
        }
    }
}

fn file_calls(id: u128, parent: u128, file: &str, ids: &[&str]) -> Value {
    let calls = ids
        .iter()
        .map(|id| {
            let mut item = call(id);
            item["input"] = json!({"file_path":file});
            item
        })
        .collect::<Vec<_>>();
    let mut row = block(id, parent, file, json!(calls));
    row["message"]["id"] = json!(format!("response-{id}"));
    row
}

#[test]
fn a_copy_of_a_retained_hidden_row_preserves_its_later_retained_request_and_reply() {
    let hidden = meta_user(3, 2);
    let mut rows = vec![
        message(1, 0, "user", json!("Initial request")),
        message(2, 1, "assistant", json!("Retained reply")),
        hidden.clone(),
        message(4, 3, "user", json!("Follow-up request")),
    ];
    rows.extend(compaction(6, 4, &[2, 3, 4]));
    rows.push(message(9, 4, "assistant", json!("Latest answer")));
    for copied in [false, true] {
        if copied {
            rows.push(hidden.clone());
        }
        for preview in [false, true] {
            let snapshot = read_session(&rows, preview);
            assert_eq!(
                handoff_messages(&import_trajectory(&snapshot)),
                [
                    "Summary 6",
                    "Retained reply",
                    "Follow-up request",
                    "Latest answer"
                ]
            );
            for text in [
                omnis_core::trajectory_search_document(&snapshot).text,
                recovery_text(&snapshot),
            ] {
                assert!(text.contains("Follow-up request") && text.contains("Latest answer"));
            }
        }
    }
}

#[test]
fn a_copy_of_a_hidden_child_keeps_its_reply_when_both_copies_follow_the_replacement() {
    let hidden = meta_user(3, 1);
    let mut rows = vec![
        message(1, 0, "user", json!("Old request")),
        message(1, 0, "user", json!("Replacement request")),
        hidden.clone(),
        message(4, 3, "assistant", json!("Verified reply")),
    ];
    for copied in [false, true] {
        if copied {
            rows.push(hidden.clone());
        }
        for preview in [false, true] {
            let snapshot = read_session(&rows, preview);
            assert_eq!(
                handoff_messages(&import_trajectory(&snapshot)),
                ["Replacement request", "Verified reply"]
            );
            for text in [
                omnis_core::trajectory_search_document(&snapshot).text,
                recovery_text(&snapshot),
            ] {
                assert!(text.contains("Verified reply") && !text.contains("Old request"));
            }
        }
    }
}

#[test]
fn a_copy_of_a_hidden_row_follows_the_current_definition_of_its_grandparent() {
    let hidden = meta_user(3, 2);
    for replaced in [false, true] {
        let mut rows = vec![
            message(1, 0, "user", json!("Old request")),
            message(2, 1, "assistant", json!("Reply")),
            hidden.clone(),
        ];
        if replaced {
            rows.push(message(1, 0, "user", json!("Replacement request")));
        }
        rows.extend([
            hidden.clone(),
            message(4, 3, "assistant", json!("Final reply")),
        ]);
        let (selected, other) = if replaced {
            ("Replacement request", "Old request")
        } else {
            ("Old request", "Replacement request")
        };
        for preview in [false, true] {
            let snapshot = read_session(&rows, preview);
            assert_eq!(
                handoff_messages(&import_trajectory(&snapshot)),
                [selected, "Reply", "Final reply"]
            );
            for text in [
                omnis_core::trajectory_search_document(&snapshot).text,
                recovery_text(&snapshot),
            ] {
                assert!(text.contains(selected) && !text.contains(other));
            }
        }
    }
}

#[test]
fn a_late_result_remains_in_recovery_when_only_the_hidden_child_of_its_call_survives() {
    for keep_call in [true, false] {
        let mut rows = vec![
            message(1, 0, "user", json!("Read file")),
            file_calls(2, 1, "file.txt", &["left"]),
            meta_user(3, 2),
        ];
        rows.extend(compaction(6, 3, if keep_call { &[2, 3] } else { &[3] }));
        rows.extend([
            linked_result(8, 3, 2, "left", "Late output"),
            message(9, 8, "assistant", json!("Done")),
        ]);
        for preview in [false, true] {
            let snapshot = read_session(&rows, preview);
            if !keep_call {
                assert!(tool_outcomes(&snapshot).is_empty());
            }
            for text in [
                omnis_core::trajectory_search_document(&snapshot).text,
                recovery_text(&snapshot),
            ] {
                assert!(text.contains("Late output"));
            }
        }
    }
}

#[test]
fn a_hidden_row_after_a_tool_result_cannot_disambiguate_a_reused_call_id() {
    for via_meta in [false, true] {
        let mut rows = vec![
            message(1, 0, "user", json!("Read files")),
            file_calls(2, 1, "first.txt", &["x"]),
            file_calls(3, 2, "second.txt", &["x", "bridge"]),
            linked_result(4, 3, 3, "bridge", "Bridge output"),
        ];
        if via_meta {
            rows.push(meta_user(5, 4));
        }
        rows.push(message(
            6,
            if via_meta { 5 } else { 4 },
            "user",
            json!([result("x", "Disputed output", false)]),
        ));
        rows.push(message(7, 6, "assistant", json!("Done")));
        for preview in [false, true] {
            let snapshot = read_session(&rows, preview);
            assert!(
                tool_outcomes(&snapshot)
                    .iter()
                    .all(|(output, _)| output != "Disputed output")
            );
            assert!(import_trajectory(&snapshot).truncated);
            if via_meta {
                for text in [
                    omnis_core::trajectory_search_document(&snapshot).text,
                    recovery_text(&snapshot),
                ] {
                    assert!(text.contains("Disputed output"));
                }
            }
        }
    }
}

#[test]
fn an_observed_result_with_a_conflicting_assistant_stays_documentary() {
    for (issuer, bridge) in [(3, false), (2, false), (3, true), (2, true)] {
        let mut rows = vec![
            message(1, 0, "user", json!("Read files")),
            file_calls(2, 1, "left.txt", &["left"]),
            file_calls(3, 2, "right.txt", &["right", "bridge"]),
        ];
        if bridge {
            rows.push(linked_result(8, 3, 3, "bridge", "Bridge output"));
        }
        rows.extend([
            meta_user(4, if bridge { 8 } else { 3 }),
            linked_result(5, 4, issuer, "right", "Observed right output"),
            message(6, 5, "assistant", json!("Done")),
        ]);
        for preview in [false, true] {
            let snapshot = read_session(&rows, preview);
            let active = import_trajectory(&snapshot);
            assert!(
                active
                    .items
                    .iter()
                    .any(|item| item.text.contains("Observed right output"))
            );
            if issuer == 2 {
                assert!(active.truncated);
                assert!(
                    tool_outcomes(&snapshot)
                        .iter()
                        .all(|(output, _)| output != "Observed right output")
                );
            }
            for text in [
                omnis_core::trajectory_search_document(&snapshot).text,
                recovery_text(&snapshot),
            ] {
                assert!(text.contains("Observed right output"));
            }
        }
    }
}

#[test]
fn an_unidentified_hidden_user_in_a_preview_gap_preserves_the_linked_exchange() {
    for present in [false, true] {
        let mut rows = vec![
            message(1, 0, "user", json!("Known request")),
            message(2, 1, "assistant", json!("Known reply")),
        ];
        if present {
            let mut hidden = meta_user(3, 2);
            let fields = hidden.as_object_mut().unwrap();
            fields.remove("parentUuid");
            fields.remove("uuid");
            rows.push(hidden);
        } else {
            rows.push(json!({"type":"progress"}));
        }
        put_between_preview_windows(&mut rows, 2);
        for preview in [false, true] {
            let snapshot = read_session(&rows, preview);
            assert_eq!(
                handoff_messages(&import_trajectory(&snapshot)),
                ["Known request", "Known reply"]
            );
            for text in [
                omnis_core::trajectory_search_document(&snapshot).text,
                recovery_text(&snapshot),
            ] {
                assert!(
                    text.contains("Known request")
                        && text.contains("Known reply")
                        && !text.contains("Internal meta text")
                );
            }
        }
    }
}

#[test]
fn a_hidden_tool_parent_cannot_verify_a_conflicting_declared_issuer() {
    for conflict in [false, true] {
        let mut first = call("lookup");
        first["input"] = json!({"file_path":"first.txt"});
        let mut second = call("lookup");
        second["input"] = json!({"file_path":"second.txt"});
        let mut retry = block(3, 1, "retry", json!([second]));
        retry["message"]["id"] = json!("second-response");
        let rows = [
            message(1, 0, "user", json!("Read the selected file")),
            block(2, 1, "first", json!([first])),
            retry,
            meta_user(4, if conflict { 2 } else { 3 }),
            linked_result(5, 4, 3, "lookup", "Documented output"),
            message(6, 3, "assistant", json!("Finished")),
        ];
        for preview in [false, true] {
            let snapshot = read_session(&rows, preview);
            if conflict {
                assert!(tool_outcomes(&snapshot).is_empty());
            } else {
                assert_eq!(
                    tool_outcomes(&snapshot),
                    [("Documented output".to_owned(), false)]
                );
            }
        }
    }
}

#[test]
fn a_result_with_a_nonassistant_declared_issuer_remains_available_as_evidence() {
    for source in [2, 1] {
        let rows = [
            message(1, 0, "user", json!("Read file")),
            block(2, 1, "first", json!([call("lookup")])),
            linked_result(3, 2, source, "lookup", "Observed output"),
            message(4, 3, "assistant", json!("Finished")),
        ];
        for preview in [false, true] {
            let snapshot = read_session(&rows, preview);
            let active = import_trajectory(&snapshot);
            assert_eq!(active.truncated, source != 2);
            if source != 2 {
                assert!(tool_outcomes(&snapshot).is_empty());
            }
            assert!(
                active
                    .items
                    .iter()
                    .any(|item| item.text.contains("Observed output"))
            );
            for text in [
                omnis_core::trajectory_search_document(&snapshot).text,
                recovery_text(&snapshot),
            ] {
                assert!(text.contains("Observed output"));
            }
        }
    }
}

#[test]
fn a_repeated_duration_preserves_the_visible_child_of_its_original_occurrence() {
    let duration = turn_duration(50, 10);
    let mut rows = vec![
        message(10, 0, "user", json!("Independent A")),
        duration.clone(),
        message(12, 50, "assistant", json!("First reply")),
        message(11, 0, "user", json!("Independent B")),
        message(13, 11, "assistant", json!("Second reply")),
    ];
    for copied in [false, true] {
        if copied {
            rows.push(duration.clone());
        }
        let (task, reply, other) = if copied {
            ("Independent A", "First reply", "Second reply")
        } else {
            ("Independent B", "Second reply", "First reply")
        };
        for preview in [false, true] {
            let snapshot = read_session(&rows, preview);
            assert_eq!(
                handoff_messages(&import_trajectory(&snapshot)),
                [task, reply]
            );
            for text in [
                omnis_core::trajectory_search_document(&snapshot).text,
                recovery_text(&snapshot),
            ] {
                assert!(text.contains(reply) && !text.contains(other));
            }
        }
    }
}

#[test]
fn a_repeated_duration_reselects_its_parent_after_an_independent_reply() {
    let duration = turn_duration(50, 10);
    let mut rows = vec![
        message(10, 0, "user", json!("Independent A")),
        message(11, 0, "user", json!("Independent B")),
        duration.clone(),
        message(13, 11, "assistant", json!("Known reply")),
    ];
    for copied in [false, true] {
        if copied {
            rows.push(duration.clone());
        }
        let expected: &[&str] = if copied {
            &["Independent A"]
        } else {
            &["Independent B", "Known reply"]
        };
        for preview in [false, true] {
            let snapshot = read_session(&rows, preview);
            assert_eq!(handoff_messages(&import_trajectory(&snapshot)), expected);
            for text in [
                omnis_core::trajectory_search_document(&snapshot).text,
                recovery_text(&snapshot),
            ] {
                assert_eq!(text.contains("Independent A"), copied);
                assert_eq!(text.contains("Known reply"), !copied);
            }
        }
    }
}

fn check_terminal_claude_selection(retained: &[u128], terminal: Value) {
    let mut rows = vec![
        message(1, 0, "user", json!("Original task")),
        message(2, 1, "assistant", json!("Removed reply")),
        directory_attachment(30, 2),
    ];
    rows.extend(compaction(3, 2, retained));
    rows.push(message(10, 0, "user", json!("Independent A")));
    rows.push(message(11, 0, "user", json!("Independent B")));
    rows.push(terminal);
    for (second, row) in (1..).zip(&mut rows) {
        row["timestamp"] = json!(format!("2026-01-01T00:00:{second:02}Z"));
        row["isSidechain"] = json!(false);
    }
    for preview in [false, true] {
        let snapshot = read_session(&rows, preview);
        assert_eq!(
            handoff_messages(&import_trajectory(&snapshot)),
            ["Independent A"]
        );
        for text in [
            omnis_core::trajectory_search_document(&snapshot).text,
            recovery_text(&snapshot),
        ] {
            assert!(text.contains("Independent A") && !text.contains("Independent B"));
        }
    }
}

#[test]
fn a_final_duration_selects_its_surviving_parent() {
    check_terminal_claude_selection(&[], turn_duration(50, 10));
}

#[test]
fn a_final_meta_user_selects_its_surviving_parent() {
    check_terminal_claude_selection(&[2], meta_user(50, 10));
}

#[test]
fn a_duration_without_a_uuid_in_a_preview_gap_preserves_the_linked_exchange() {
    for (progress, missing_uuid, missing_parent) in [
        (false, false, false),
        (false, true, false),
        (false, true, true),
        (false, false, true),
        (true, false, false),
        (true, false, true),
    ] {
        let mut duration = if progress {
            opaque(30, 2, "progress")
        } else {
            turn_duration(30, 2)
        };
        if missing_uuid {
            duration.as_object_mut().unwrap().remove("uuid");
        }
        if missing_parent {
            duration.as_object_mut().unwrap().remove("parentUuid");
        }
        let mut rows = vec![
            message(1, 0, "user", json!("Known request")),
            message(2, 1, "assistant", json!("Known reply")),
            duration,
        ];
        put_between_preview_windows(&mut rows, 2);
        for preview in [false, true] {
            let snapshot = read_session(&rows, preview);
            assert_eq!(
                handoff_messages(&import_trajectory(&snapshot)),
                ["Known request", "Known reply"],
                "missing UUID={missing_uuid}, missing parent={missing_parent}, preview={preview}"
            );
            for text in [
                omnis_core::trajectory_search_document(&snapshot).text,
                recovery_text(&snapshot),
            ] {
                assert!(text.contains("Known request") && text.contains("Known reply"));
            }
        }
    }
}

#[test]
fn descendants_of_an_omitted_directory_keep_its_cutoff_when_the_request_was_retained() {
    let mut base = vec![
        message(1, 0, "user", json!("Original task")),
        directory_attachment(30, 1),
        directory_attachment(31, 30),
        message(2, 1, "assistant", json!("Retained reply")),
    ];
    base.extend(compaction(3, 2, &[1, 30, 2]));
    let epoch = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z").unwrap();
    for (delay, links, progress_rows) in [
        (13, 1, 0),
        (13, 2, 0),
        (1, 1, 0),
        (1, 2, 0),
        (13, 1, 1),
        (13, 1, 2),
    ] {
        let mut rows = base.clone();
        let mut parent = 31;
        if progress_rows > 0 {
            rows.push(opaque(50, parent, "progress"));
            parent = 50;
        }
        for id in 32..32 + links {
            rows.push(directory_attachment(id, parent));
            parent = id;
        }
        rows.push(message(11, parent, "user", json!("New request")));
        for (i, row) in rows.iter_mut().enumerate() {
            let seconds = i64::try_from(i).unwrap() + if i >= base.len() { delay - 1 } else { 0 };
            row["timestamp"] = json!(epoch + chrono::Duration::seconds(seconds));
            row["isSidechain"] = json!(false);
        }
        if progress_rows == 2 {
            let index = base.len();
            rows.insert(index + 1, rows[index].clone());
        }
        let expected: &[&str] = if delay == 1 {
            &["Summary 3", "New request"]
        } else {
            &["New request"]
        };
        for preview in [false, true] {
            let case = (delay, links, progress_rows, preview);
            let snapshot = read_session(&rows, preview);
            let active = import_trajectory(&snapshot);
            assert_eq!(handoff_messages(&active), expected, "{case:?}");
            assert!(active.truncated, "{case:?}");
            let search = omnis_core::trajectory_search_document(&snapshot);
            assert!(!search.source_complete, "{case:?}");
            let recovery = recovery_text(&snapshot);
            assert!(
                search.text.contains("Retained reply") && recovery.contains("Retained reply"),
                "{case:?}"
            );
        }
    }
}

#[test]
fn a_retained_directory_with_a_missing_parent_still_identifies_its_verified_checkpoint() {
    let mut rows = vec![
        message(1, 0, "user", json!("Original task")),
        directory_attachment(30, 90),
        directory_attachment(31, 30),
        message(2, 1, "assistant", json!("Retained reply")),
    ];
    rows.extend(compaction(3, 2, &[2, 30]));
    rows.push(message(10, 0, "user", json!("Independent root")));
    for parent in [30, 31] {
        let mut rows = rows.clone();
        rows.push(message(11, parent, "user", json!("New request")));
        for preview in [false, true] {
            let snapshot = read_session(&rows, preview);
            let active = import_trajectory(&snapshot);
            assert_eq!(
                handoff_messages(&active),
                ["Summary 3", "Retained reply", "New request"]
            );
            assert!(active.truncated);
            let search = omnis_core::trajectory_search_document(&snapshot);
            assert!(!search.source_complete);
            let recovery = recovery_text(&snapshot);
            assert!(recovery.contains("Summary 3"));
            for text in [&search.text, &recovery] {
                assert!(text.contains("Retained reply") && !text.contains("Independent root"));
            }
        }
    }
}

#[test]
fn a_new_directory_after_a_checkpoint_can_supersede_an_inherited_cutoff() {
    let mut root = directory_attachment(30, 0);
    root["parentUuid"] = Value::Null;
    let mut rows = vec![
        root,
        message(1, 30, "user", json!("First task")),
        message(2, 1, "assistant", json!("First reply")),
    ];
    rows.extend(compaction(3, 2, &[2]));
    rows.extend([
        directory_attachment(31, 30),
        message(5, 0, "user", json!("Second task")),
        message(6, 5, "assistant", json!("Second reply")),
    ]);
    rows.extend(compaction(7, 6, &[6]));
    rows.extend([
        directory_attachment(32, 31),
        message(9, 32, "user", json!("New request")),
    ]);
    let epoch = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z").unwrap();
    for (seconds, row) in (0..).zip(&mut rows) {
        row["timestamp"] = json!(epoch + chrono::Duration::seconds(seconds));
        row["isSidechain"] = json!(false);
    }
    for padded in [false, true] {
        let mut rows = rows.clone();
        if padded {
            let boundary = rows.len() - 2;
            put_between_preview_windows(&mut rows, boundary);
        }
        for preview in [false, true] {
            let snapshot = read_session(&rows, preview);
            let active = import_trajectory(&snapshot);
            assert_eq!(handoff_messages(&active), ["Summary 7", "New request"]);
            assert!(active.truncated);
            let search = omnis_core::trajectory_search_document(&snapshot);
            assert!(!search.source_complete);
            let recovery = recovery_text(&snapshot);
            assert!(recovery.contains("Summary 7"));
            for text in [&search.text, &recovery] {
                assert!(text.contains("Second reply") && !text.contains("First reply"));
            }
        }
    }
}

#[test]
fn rejected_replacement_archives_the_verified_pair_in_source_order() {
    let mut lookup = call("lookup");
    lookup["input"] = json!({"password": "synthetic-private-input"});
    let mut rows = vec![
        message(1, 0, "user", json!("Original task")),
        message(2, 1, "assistant", json!("Old left")),
        block(
            3,
            1,
            "right",
            json!([{"type":"text","text":"Verified right"},lookup]),
        ),
        message(
            30,
            3,
            "user",
            json!([result("lookup", "Recorded outcome", false)]),
        ),
    ];
    rows.extend(compaction(4, 30, &[3, 30]));
    rows.extend([
        message(6, 30, "user", json!("First continuation")),
        message(7, 6, "assistant", json!("Known reply")),
    ]);
    rows.extend(compaction(8, 7, &[3, 99]));
    rows.push(message(11, 7, "user", json!("On verified branch")));
    rows.push(message(12, 2, "user", json!("New child of omitted left")));
    for preview in [false, true] {
        let snapshot = read_session(&rows, preview);
        let active = import_trajectory(&snapshot);
        assert!(active.truncated);
        assert_eq!(
            handoff_messages(&active),
            ["Original task", "Old left", "New child of omitted left"]
        );
        assert!(tool_outcomes(&snapshot).is_empty());
        for text in [
            recovery_text(&snapshot),
            omnis_core::trajectory_search_document(&snapshot).text,
        ] {
            assert!(!text.contains("synthetic-private-input"));
            let positions = [
                "Original task",
                "Verified right",
                "lookup",
                "Recorded outcome",
                "New child of omitted left",
            ]
            .map(|marker| {
                text.find(marker)
                    .unwrap_or_else(|| panic!("missing {marker}: {text}"))
            });
            assert!(positions.windows(2).all(|p| p[0] < p[1]), "{text}");
        }
    }
}

#[test]
fn a_rejected_checkpoint_marks_a_copied_reply_only_on_an_affected_turn() {
    let other = message(21, 20, "assistant", json!("Independent reply"));
    let separate_turn = message(31, 30, "assistant", json!("Separate turn reply"));
    let left = message(2, 1, "assistant", json!("Earlier alternative"));
    let attachment = directory_attachment(10, 3);
    let mut changed = attachment.clone();
    changed["attachment"]["path"] = json!("another directory");
    let mut prefix = vec![
        message(20, 0, "user", json!("Independent task")),
        other.clone(),
        message(1, 0, "user", json!("Main task")),
        left.clone(),
        message(30, 1, "user", json!("Separate turn")),
        separate_turn.clone(),
        message(3, 1, "assistant", json!("Retained reply")),
        attachment,
        changed,
    ];
    let fresh = message(22, 0, "user", json!("New task"));
    let mut ordinary = prefix.clone();
    ordinary.extend([fresh.clone(), left.clone()]);
    for preview in [false, true] {
        let handoff = import_trajectory(&read_session(&ordinary, preview));
        assert!(!handoff.truncated);
        assert_eq!(handoff.items.last().unwrap().text, "Earlier alternative");
    }
    prefix.extend(compaction(4, 10, &[3, 10]));
    let [valid, summary] = compaction(8, 2, &[2]);
    let continuation = message(11, 2, "user", json!("Follow up"));
    for (suffix, incomplete, selected) in [
        (vec![left.clone()], true, "Earlier alternative"),
        (vec![left.clone(), continuation.clone()], true, "Follow up"),
        (vec![other.clone()], false, "Independent reply"),
        (vec![separate_turn], false, "Separate turn reply"),
        (
            vec![left.clone(), other.clone(), left.clone()],
            true,
            "Earlier alternative",
        ),
        (vec![other.clone(), continuation.clone()], true, "Follow up"),
        (
            vec![
                left.clone(),
                message(23, 30, "user", json!("New separate turn")),
            ],
            false,
            "New separate turn",
        ),
        (
            vec![fresh.clone(), other.clone()],
            false,
            "Independent reply",
        ),
        (vec![left.clone(), other], false, "Independent reply"),
        (
            vec![valid.clone(), summary.clone(), continuation.clone()],
            false,
            "Follow up",
        ),
        (
            vec![left.clone(), valid, summary, continuation],
            false,
            "Follow up",
        ),
        (vec![left.clone(), fresh.clone()], false, "New task"),
        (vec![fresh.clone(), left.clone()], true, "New task"),
        (
            vec![
                fresh,
                left,
                message(24, 22, "user", json!("Continue new task")),
            ],
            false,
            "Continue new task",
        ),
    ] {
        let mut rows = prefix.clone();
        rows.extend(suffix);
        for preview in [false, true] {
            let snapshot = read_session(&rows, preview);
            let handoff = import_trajectory(&snapshot);
            assert_eq!(handoff.items.last().unwrap().text, selected);
            assert_eq!(handoff.truncated, incomplete);
            assert_eq!(
                omnis_core::trajectory_search_document(&snapshot).source_complete,
                !incomplete
            );
            let mut recovery = Vec::new();
            omnis_core::write_recovery_handoff(&snapshot, &mut recovery).unwrap();
            let recovery = String::from_utf8(recovery).unwrap();
            assert!(recovery.contains(selected));
            assert_eq!(
                recovery.contains("missing or unverified history"),
                incomplete
            );
        }
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
fn a_later_summary_can_retain_a_previously_unretained_attachment() {
    let mut rows = vec![
        message(1, 0, "user", json!("Task")),
        message(2, 1, "assistant", json!("Decision")),
        message(3, 2, "user", json!("Retained note")),
        opaque(4, 2, "attachment"),
        opaque(5, 4, "progress"),
        opaque(10, 2, "attachment"),
        message(6, 3, "assistant", json!("Discarded tail")),
    ];
    rows.extend(compaction(7, 6, &[2, 10]));
    rows.push(opaque(9, 5, "progress"));
    rows.extend(compaction(11, 6, &[2, 3, 4, 6]));
    rows.push(message(13, 5, "user", json!("Second continuation")));
    let snapshot = read(&rows);
    let messages: Vec<_> = snapshot
        .events
        .iter()
        .filter_map(|e| e.payload["text"].as_str())
        .collect();
    let start = messages
        .iter()
        .position(|text| *text == "Summary 11")
        .unwrap();
    assert_eq!(
        &messages[start..],
        [
            "Summary 11",
            "Decision",
            "Retained note",
            "Second continuation"
        ]
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
        let missing = stale_source == 99;
        assert_eq!(recovery.contains("abandoned output"), missing);
        assert_eq!(import_trajectory(&snapshot).truncated, missing);
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
fn a_parallel_result_does_not_restore_a_withdrawn_prompt_in_its_parent() {
    for (cutoff, retained) in [
        (2, vec![2]),
        (2, vec![2, 4]),
        (3, vec![2, 3]),
        (6, vec![2, 3]),
    ] {
        let mut mixed = linked_result(3, 2, 2, "right", "right output");
        mixed["message"]["content"]
            .as_array_mut()
            .unwrap()
            .push(json!({"type":"text","text":"followupnote"}));
        let mut rows = vec![
            message(1, 0, "user", json!("Inspect both")),
            block(2, 1, "parallel", json!([call("left"), call("right")])),
            mixed,
            linked_result(4, 3, 2, "left", "left output"),
        ];
        rows.extend(compaction(5, 4, &retained));
        rows.push(message(7, cutoff, "user", json!("Replacement")));
        for relisted in [false, true] {
            if relisted {
                rows.extend(compaction(8, 7, &[3, 7]));
            }
            let snapshot = read(&rows);
            let mut recovery = Vec::new();
            omnis_core::write_recovery_handoff(&snapshot, &mut recovery).unwrap();
            for text in [
                omnis_core::trajectory_search_document(&snapshot).text,
                String::from_utf8(recovery).unwrap(),
            ] {
                assert_eq!(
                    text.contains("followupnote"),
                    cutoff != 2,
                    "cutoff={cutoff} retained={retained:?} relisted={relisted}: {text}"
                );
                assert!(text.contains("Replacement"));
                assert_eq!(text.matches("right output").count(), 1);
                assert_eq!(text.matches("left output").count(), 1);
            }
            let imported = import_trajectory(&snapshot);
            let native = imported
                .items
                .iter()
                .map(|item| item.text.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            assert_eq!(imported.truncated, relisted && cutoff == 2);
            if relisted {
                assert!(native.contains("right output"));
                assert_eq!(native.contains("followupnote"), cutoff != 2);
            }
        }
    }
}

#[test]
fn a_return_to_the_original_mixed_branch_keeps_its_request_and_reply() {
    let mut mixed = linked_result(3, 2, 2, "a", "verified output");
    mixed["message"]["content"].as_array_mut().unwrap().extend([
        json!({"type":"text","text":"Original question"}),
        result("orphan", "unverified output", false),
    ]);
    let mut rows = vec![
        message(1, 0, "user", json!("Inspect")),
        block(2, 1, "parallel", json!([call("a"), call("b")])),
        mixed,
        block(4, 3, "answer", json!("Original reply")),
        linked_result(5, 4, 2, "b", "other outcome"),
    ];
    rows.extend(compaction(6, 5, &[2]));
    rows.push(message(8, 2, "user", json!("Replacement")));
    for restore in [false, true] {
        let mut branch = rows.clone();
        let continuation = if restore {
            branch.push(message(9, 4, "user", json!("Continue the original")));
            9
        } else {
            8
        };
        branch.extend(compaction(10, continuation, &[3, 4, continuation]));
        let snapshot = read(&branch);
        let text = omnis_core::trajectory_search_document(&snapshot).text;
        assert_eq!(text.contains("Original question"), restore);
        assert_eq!(text.contains("Original reply"), restore);
        assert_eq!(text.contains("unverified output"), restore);
        assert!(text.contains("verified output"));
        assert!(import_trajectory(&snapshot).truncated);
        assert_eq!(
            snapshot
                .events
                .iter()
                .any(|event| event.source.raw_record_type.as_deref()
                    == Some("omnisession.missing_tool_source")),
            restore
        );
    }
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
fn repeated_explicit_messages_keep_the_completed_exchange() {
    let request = message(1, 0, "user", json!("Inspect"));
    let issue = block(2, 1, "request", json!([call("raw")]));
    let result = linked_result(3, 2, 2, "raw", "Completed output");
    let reply = message(4, 3, "assistant", json!("Done"));
    let original = [request, issue, result, reply];
    let snapshot = read(&original);
    for first in [0, 2] {
        let mut restored = original.to_vec();
        restored.extend_from_slice(&original[first..]);
        let repeated = read(&restored);
        assert_eq!(repeated.events, snapshot.events);
        assert_eq!(
            tool_outcomes(&repeated),
            [("Completed output".into(), false)]
        );
        assert!(!import_trajectory(&repeated).truncated);
    }
}

#[test]
fn a_complete_copy_reconnects_its_orphaned_suffix() {
    let complete = [
        message(1, 0, "user", json!("Earlier request")),
        block(2, 1, "first", json!("Earlier reply")),
        message(3, 2, "user", json!("Current request")),
        block(4, 3, "second", json!("Current reply")),
    ];
    for start in [2, 3] {
        let mut rows = complete[start..].to_vec();
        rows.extend_from_slice(&complete);
        let restored = read(&rows);
        assert_eq!(restored.events, read(&complete).events);
        assert!(!import_trajectory(&restored).truncated);
    }

    let tool_exchange = [
        complete[0].clone(),
        block(2, 1, "tools", json!([call("raw")])),
        linked_result(3, 2, 2, "raw", "Verified output"),
        complete[3].clone(),
    ];
    let mut restored = tool_exchange[2..].to_vec();
    restored.extend_from_slice(&tool_exchange);
    let mut original = tool_exchange.to_vec();
    for rows in [&mut restored, &mut original] {
        rows.extend(compaction(5, 4, &[2, 3, 4]));
    }
    let retained = read(&restored);
    assert_eq!(retained.events, read(&original).events);
    assert_eq!(
        tool_outcomes(&retained),
        [("Verified output".into(), false)]
    );

    let mut partial = vec![complete[3].clone()];
    partial.extend_from_slice(&complete[2..]);
    let restored = read(&partial);
    assert_eq!(restored.events, read(&complete[2..]).events);
    assert!(import_trajectory(&restored).truncated);
}

#[test]
fn an_empty_compaction_keeps_earlier_requests_in_recovery() {
    let [header, mut summary] = compaction(3, 2, &[2]);
    for empty in [
        json!([]),
        json!([{"type":"text", "text":"   "}]),
        json!(CLAUDE_SUMMARY_ENVELOPE),
    ] {
        summary["message"]["content"] = empty;
        let rows = [
            message(1, 0, "user", json!("Earlier request")),
            block(2, 1, "initial", json!("Retained decision")),
            header.clone(),
            summary.clone(),
            message(5, 2, "user", json!("Current request")),
        ];
        let snapshot = read(&rows);
        let handoff = import_trajectory(&snapshot);
        assert!(handoff.truncated);
        assert!(
            handoff
                .items
                .iter()
                .any(|item| item.text == "Retained decision")
        );
        assert!(
            handoff
                .items
                .iter()
                .any(|item| item.text == "Current request")
        );
        assert!(
            !handoff
                .items
                .iter()
                .any(|item| item.text == "Earlier request")
        );
        assert!(
            snapshot
                .events
                .iter()
                .any(|event| event.payload["text"] == "Earlier request")
        );
    }
}

#[test]
fn a_missing_declared_anchor_keeps_retained_history_on_its_own_branch() {
    let [header, summary] = compaction(3, 2, &[2]);
    for descriptor in [
        header["compactMetadata"].clone(),
        json!({"preservedSegment":{"anchorUuid":Uuid::from_u128(4),
            "headUuid":Uuid::from_u128(2),"tailUuid":Uuid::from_u128(2)}}),
    ] {
        let mut header = header.clone();
        header["compactMetadata"] = descriptor;
        let before = [
            message(1, 0, "user", json!("Earlier request")),
            message(2, 1, "assistant", json!("Retained decision")),
            header,
        ];
        for parent in [2, 3, 4, 6] {
            let mut rows = before.to_vec();
            if parent == 6 {
                rows.push(opaque(6, 4, "progress"));
            }
            rows.push(message(7, parent, "user", json!("Current request")));
            let snapshot = read(&rows);
            assert!(import_trajectory(&snapshot).truncated);
            let text: Vec<_> = import_trajectory(&snapshot)
                .items
                .into_iter()
                .map(|i| i.text)
                .collect();
            assert_eq!(text.last().unwrap(), "Current request");
            assert_eq!(text.iter().any(|s| s == "Retained decision"), parent != 3);
            assert!(!text.iter().any(|item| item.contains("Earlier request")));
            let mut recovery = Vec::new();
            omnis_core::write_recovery_handoff(&snapshot, &mut recovery).unwrap();
            assert!(
                String::from_utf8(recovery)
                    .unwrap()
                    .contains("Earlier request")
            );

            rows.push(message(8, 0, "user", json!("Independent task")));
            let independent = import_trajectory(&read(&rows));
            assert!(!independent.truncated);
            assert_eq!(independent.items.len(), 1);
            rows.push(message(9, 4, "user", json!("Back to the boundary")));
            let resumed = import_trajectory(&read(&rows));
            assert!(resumed.truncated);
            assert!(resumed.items.iter().any(|i| i.text == "Retained decision"));

            rows.extend([
                summary.clone(),
                message(10, 4, "user", json!("Verified continuation")),
            ]);
            let verified = import_trajectory(&read(&rows));
            assert!(!verified.truncated);
            assert!(
                verified
                    .items
                    .iter()
                    .any(|i| i.text.ends_with("\nSummary 3"))
            );
            assert!(verified.items.iter().any(|i| i.text == "Retained decision"));

            if parent == 6 {
                continue;
            }
            let mut restored = before.to_vec();
            let continuation = message(12, parent, "user", json!("Restored continuation"));
            restored.extend([continuation.clone(), summary.clone(), continuation]);
            let restored = import_trajectory(&read(&restored));
            assert!(!restored.truncated);
            assert_eq!(
                restored.items.last().unwrap().text,
                "Restored continuation",
                "parent {parent}"
            );
        }
        let mut unrelated = before.to_vec();
        unrelated.push(message(11, 1, "user", json!("Rewind before compression")));
        let rewind = import_trajectory(&read(&unrelated));
        assert!(rewind.truncated);
        assert!(
            rewind
                .items
                .iter()
                .any(|item| item.text == "Retained decision")
        );
        assert!(
            !rewind
                .items
                .iter()
                .any(|item| item.text == "Earlier request")
        );
        assert_eq!(
            rewind.items.last().unwrap().text,
            "Rewind before compression"
        );
    }
}

#[test]
fn a_terminal_boundary_uses_only_a_verified_retention_descriptor() {
    for (retained, earlier_active, decision_active) in [
        (Some(vec![2]), false, true),
        (Some(Vec::new()), false, false),
        (Some(vec![99]), true, true),
        (None, true, true),
    ] {
        let [mut header, _] = compaction(3, 2, retained.as_deref().unwrap_or_default());
        if retained.is_none() {
            header.as_object_mut().unwrap().remove("compactMetadata");
        }
        let snapshot = read(&[
            message(1, 0, "user", json!("Earlier request")),
            message(2, 1, "assistant", json!("Retained decision")),
            header,
        ]);
        let handoff = import_trajectory(&snapshot);
        assert!(handoff.truncated);
        assert_eq!(
            handoff.items.iter().any(|i| i.text == "Earlier request"),
            earlier_active
        );
        assert_eq!(
            handoff.items.iter().any(|i| i.text == "Retained decision"),
            decision_active
        );
        let mut recovery = Vec::new();
        omnis_core::write_recovery_handoff(&snapshot, &mut recovery).unwrap();
        let recovery = String::from_utf8(recovery).unwrap();
        assert!(recovery.contains("Earlier request") && recovery.contains("Retained decision"));
    }
}

#[test]
fn an_unlinked_assistant_after_empty_retention_uses_the_dated_user_turn() {
    let with_user = &["Summary 7", "Intervening request", "Final reply"][..];
    let reply_only = &["Final reply"][..];
    let with_decision = &["Summary 7", "Selected decision", "Final reply"][..];
    let start = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z").unwrap();
    for (gap_ms, all_tied, retained, expected) in [
        (Some(0), false, &[][..], Some(with_user)),
        (Some(4_999), false, &[], Some(with_user)),
        (Some(5_000), false, &[], Some(with_user)),
        (Some(5_001), false, &[], Some(reply_only)),
        (Some(0), true, &[], Some(reply_only)),
        (Some(5_001), false, &[2], Some(with_decision)),
        (None, false, &[], None),
    ] {
        let mut rows = vec![
            message(1, 0, "user", json!("Initial task")),
            block(2, 1, "r", json!("Selected decision")),
            message(30, 1, "user", json!("Earlier note")),
        ];
        rows.extend(compaction(3, 2, &[30, 2]));
        rows.extend([
            message(5, 2, "user", json!("Previous request")),
            message(6, 5, "assistant", json!("Ready")),
        ]);
        rows.extend(compaction(7, 6, retained));
        rows.push(message(9, 6, "user", json!("Intervening request")));
        let mut time = start;
        for (seconds, row) in (0..).zip(&mut rows) {
            if !all_tied {
                time = start + chrono::Duration::seconds(seconds);
            }
            row["timestamp"] = json!(time);
        }
        let mut reply = message(10, 2, "assistant", json!("Final reply"));
        if let Some(ms) = gap_ms {
            reply["timestamp"] = json!(time + chrono::Duration::milliseconds(ms));
        }
        rows.push(reply);

        for preview in [false, true] {
            let snapshot = read_session(&rows, preview);
            let handoff = import_trajectory(&snapshot);
            if let Some(expected) = expected {
                let text: Vec<_> = handoff
                    .items
                    .iter()
                    .map(|item| item.text.lines().last().unwrap())
                    .collect();
                assert_eq!(
                    text, expected,
                    "{gap_ms:?}, all_tied={all_tied}, retained={retained:?}"
                );
            } else {
                assert!(handoff.truncated);
                assert!(!omnis_core::trajectory_search_document(&snapshot).source_complete);
                let mut recovery = Vec::new();
                omnis_core::write_recovery_handoff(&snapshot, &mut recovery).unwrap();
                let recovery = String::from_utf8(recovery).unwrap();
                assert!(recovery.contains("missing or unverified history"));
                assert!(recovery.contains("Final reply"));
            }
            let preview = omnis_core::session_preview(&snapshot);
            assert_eq!(preview.latest.unwrap().text, "Final reply");
        }
    }
}

#[test]
fn transparent_links_apply_the_compaction_cutoff_to_their_conversation_parent() {
    for (kind, before, has_summary, parent) in [
        ("progress", true, true, 2),
        ("progress", false, true, 2),
        ("progress", false, false, 2),
        ("attachment", true, true, 2),
        ("attachment", false, true, 7),
    ] {
        let mut rows = vec![
            message(1, 0, "user", json!("Start")),
            message(2, 1, "assistant", json!("Withdrawn left reply")),
            message(3, 1, "assistant", json!("Retained right reply")),
        ];
        let bridge = if kind == "attachment" {
            directory_attachment(8, parent)
        } else {
            opaque(8, parent, kind)
        };
        if before {
            rows.push(bridge.clone());
        }
        let [header, summary] = compaction(5, 3, &[3]);
        rows.push(header);
        if has_summary {
            rows.push(summary);
        }
        if parent == 7 {
            rows.push(message(7, 6, "user", json!("Later request")));
        }
        let mut direct = rows.clone();
        direct.push(message(9, parent, "user", json!("Continue")));
        if !before {
            rows.push(bridge);
        }
        rows.push(message(9, 8, "user", json!("Continue")));

        let snapshot = read(&rows);
        let handoff = import_trajectory(&snapshot);
        assert_eq!(
            handoff,
            import_trajectory(&read(&direct)),
            "{kind}, before={before}, parent={parent}"
        );
        assert_eq!(handoff.truncated, !has_summary);
        assert!(
            handoff
                .items
                .iter()
                .any(|i| i.text == "Retained right reply")
        );
        assert_eq!(handoff.items.last().unwrap().text, "Continue");
        assert_eq!(
            handoff.items.iter().any(|i| i.text == "Later request"),
            parent == 7
        );
        let search = omnis_core::trajectory_search_document(&snapshot);
        assert_eq!(search.source_complete, has_summary);
        assert!(search.text.contains("Retained right reply"));
        assert!(!search.text.contains("Withdrawn left reply"));
    }
}

#[test]
fn a_progress_link_preserves_an_explicit_root_or_missing_conversation_parent() {
    for parent in [None, Some(Value::Null), Some(json!(Uuid::from_u128(90)))] {
        let mut progress = opaque(7, 0, "progress");
        let tip = message(8, 7, "user", json!("Independent request"));
        let mut direct = tip.clone();
        for row in [&mut direct, &mut progress] {
            if let Some(id) = &parent {
                row["parentUuid"] = id.clone();
            } else {
                row.as_object_mut().unwrap().remove("parentUuid");
            }
        }
        for before in [false, true] {
            let mut rows = vec![
                message(1, 0, "user", json!("Previous request")),
                message(2, 1, "assistant", json!("Previously retained answer")),
            ];
            if before {
                rows.push(progress.clone());
            }
            rows.extend(compaction(4, 2, &[2]));
            let mut control = rows.clone();
            control.push(direct.clone());
            if !before {
                rows.push(progress.clone());
            }
            rows.push(tip.clone());
            for preview in [false, true] {
                let expected = read_session(&control, preview);
                let actual = read_session(&rows, preview);
                assert_eq!(import_trajectory(&actual), import_trajectory(&expected));
                assert_eq!(
                    omnis_core::trajectory_search_document(&actual),
                    omnis_core::trajectory_search_document(&expected)
                );
                let recovery = recovery_text(&actual);
                assert!(recovery.contains("Independent request"));
                assert!(
                    !recovery.contains("Summary 4")
                        && !recovery.contains("Previously retained answer")
                );
                let omissions: Vec<_> = actual
                    .events
                    .iter()
                    .filter(|event| omnis_core::reports_omitted_events(event))
                    .filter_map(|event| event.source.raw_record_type.as_deref())
                    .collect();
                let missing = parent.as_ref().is_some_and(|v| !v.is_null());
                assert_eq!(
                    omissions,
                    if missing {
                        &["omnisession.missing_parent"][..]
                    } else {
                        &[][..]
                    }
                );
            }
        }
    }
}

#[test]
fn a_root_attachment_keeps_its_compaction_position_through_progress() {
    let mut attachment = directory_attachment(10, 0);
    attachment["parentUuid"] = Value::Null;
    for (retained, expect_right) in [(&[3][..], true), (&[10, 3], false), (&[3, 10], true)] {
        for target in [10, 11, 12] {
            let mut rows = vec![
                attachment.clone(),
                opaque(11, 10, "progress"),
                message(1, 10, "user", json!("Start")),
                message(2, 1, "assistant", json!("Withdrawn left")),
                message(3, 1, "assistant", json!("Retained right")),
            ];
            rows.extend(compaction(4, 3, retained));
            rows.extend([
                opaque(12, 11, "progress"),
                message(6, target, "user", json!("Continue")),
            ]);
            for preview in [false, true] {
                let snapshot = read_session(&rows, preview);
                let handoff = import_trajectory(&snapshot);
                assert!(handoff.items[0].text.ends_with("\nSummary 4"));
                let suffix: Vec<_> = handoff.items[1..]
                    .iter()
                    .map(|item| item.text.as_str())
                    .collect();
                let expected = if expect_right {
                    &["Retained right", "Continue"][..]
                } else {
                    &["Continue"][..]
                };
                assert_eq!(suffix, expected, "retained={retained:?}, target={target}");
                assert!(!handoff.truncated);
                let search = omnis_core::trajectory_search_document(&snapshot);
                assert!(search.source_complete && !search.text.contains("Withdrawn left"));
                assert!(search.text.contains("Continue"));
            }
        }
    }
}

#[test]
fn a_directory_attachment_with_a_compacted_parent_marks_its_descendants_partial() {
    let mut rows = vec![
        message(1, 0, "user", json!("Start")),
        message(2, 1, "assistant", json!("Withdrawn left reply")),
        message(3, 1, "assistant", json!("Retained right reply")),
    ];
    rows.extend(compaction(4, 3, &[3]));
    rows.push(directory_attachment(6, 2));
    for parent in [6, 7] {
        if parent == 7 {
            rows.push(opaque(7, 6, "progress"));
        }
        rows.push(message(8, parent, "user", json!("Continue")));
        let snapshot = read(&rows);
        let handoff = import_trajectory(&snapshot);
        assert!(handoff.truncated);
        assert_eq!(handoff.items.last().unwrap().text, "Continue");
        assert!(
            !handoff
                .items
                .iter()
                .any(|i| i.text == "Withdrawn left reply")
        );
        let search = omnis_core::trajectory_search_document(&snapshot);
        assert!(!search.source_complete);
        assert!(search.text.contains("Continue"));
        assert!(!search.text.contains("Withdrawn left reply"));
        let recovery = recovery_text(&snapshot);
        assert!(recovery.contains("missing or unverified history"));
        assert!(recovery.contains("Continue"));
        assert!(!recovery.contains("Withdrawn left reply"));
        rows.pop();
    }
}

#[test]
fn a_dated_directory_attachment_recovers_the_selected_compaction_at_its_own_time() {
    let epoch = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z").unwrap();
    let dated = |ms, mut row: Value| {
        row["timestamp"] = json!(epoch + chrono::Duration::milliseconds(ms));
        row["isSidechain"] = json!(false);
        row
    };
    let [first, earlier_summary] = compaction(4, 3, &[3, 70]);
    let [second, current_summary] = compaction(8, 7, &[7]);
    let mut root = directory_attachment(69, 0);
    root["parentUuid"] = Value::Null;
    let base = [
        dated(500, root),
        dated(1_000, message(1, 69, "user", json!("Start"))),
        dated(
            2_000,
            message(2, 1, "assistant", json!("Withdrawn left reply")),
        ),
        dated(3_000, message(3, 1, "assistant", json!("Earlier decision"))),
        dated(3_500, directory_attachment(70, 3)),
        dated(4_000, first),
        dated(5_000, earlier_summary),
        dated(7_000, message(6, 70, "user", json!("Earlier request"))),
        dated(7_600, message(7, 6, "assistant", json!("Current decision"))),
        dated(8_000, second),
    ];
    let selected = [dated(9_000, current_summary.clone())];
    let mut blank = current_summary;
    blank["message"]["content"] = json!(CLAUDE_SUMMARY_ENVELOPE);
    let blank = [dated(9_000, blank)];
    let mut flag = message(33, 31, "user", json!("Disconnected summary"));
    flag["isCompactSummary"] = json!(true);
    let disconnected = [
        selected[0].clone(),
        dated(9_200, message(30, 0, "user", json!("Separate task"))),
        dated(
            9_300,
            message(31, 30, "assistant", json!("Separate answer")),
        ),
        dated(9_400, flag),
    ];
    let with_summary = &["Summary 8", "New request"][..];
    let only_request = &["New request"][..];
    let retained = &["Summary 8", "Current decision", "New request"][..];
    for (anchors, parent, at, via_progress, expected, uncertain) in [
        (&selected[..], 2, 14_000, false, with_summary, false),
        (&selected, 2, 14_000, true, with_summary, false),
        (&selected, 2, 14_001, false, only_request, false),
        (&selected, 69, 14_000, true, with_summary, true),
        (&selected, 69, 14_001, false, only_request, true),
        (&selected, 5, 14_000, true, with_summary, false),
        (&selected, 7, 9_500, false, retained, false),
        (&[][..], 2, 9_500, false, only_request, true),
        (&blank, 2, 9_500, false, only_request, true),
        (&disconnected, 2, 9_500, false, only_request, true),
    ] {
        let mut rows = base.to_vec();
        rows.extend_from_slice(anchors);
        rows.push(dated(at, directory_attachment(71, parent)));
        let user_parent = if via_progress {
            rows.push(dated(15_000, opaque(75, 71, "progress")));
            75
        } else {
            71
        };
        rows.push(dated(
            16_000,
            message(72, user_parent, "user", json!("New request")),
        ));
        for preview in [false, true] {
            let snapshot = read_session(&rows, preview);
            let handoff = import_trajectory(&snapshot);
            assert_eq!(
                handoff_messages(&handoff),
                expected,
                "{at} ms, parent {parent}, preview={preview}"
            );
            let search = omnis_core::trajectory_search_document(&snapshot);
            let recovery = recovery_text(&snapshot);
            for text in [&search.text, &recovery] {
                assert!(text.contains("New request"));
                assert!(!text.contains("Withdrawn left reply"));
                assert!(!text.contains("Disconnected summary"));
            }
            for marker in expected {
                assert!(
                    recovery.contains(*marker),
                    "{marker} at {at} ms, parent {parent}"
                );
            }
            if parent == 7 {
                assert!(search.text.contains("Current decision"));
            }
            if uncertain {
                assert!(handoff.truncated);
                assert!(!search.source_complete);
                assert!(recovery.contains("missing or unverified history"));
            }
        }
    }
}

fn history_with_turn_duration() -> Vec<Value> {
    let mut duration = opaque(10, 2, "system");
    duration["subtype"] = json!("turn_duration");
    duration["durationMs"] = json!(300);
    duration["level"] = json!("info");
    duration["content"] = json!("Turn duration");
    let [header, summary] = compaction(4, 3, &[3]);
    let mut prefix = vec![
        message(1, 0, "user", json!("Start")),
        message(2, 1, "assistant", json!("Withdrawn reply")),
        duration,
        message(3, 1, "assistant", json!("Retained result")),
        header,
        summary,
    ];
    for (second, row) in (1..).zip(&mut prefix) {
        row["timestamp"] = json!(format!("2026-01-01T00:00:{second:02}Z"));
        row["isSidechain"] = json!(false);
    }
    prefix
}

#[test]
fn updating_elapsed_time_on_a_retained_turn_duration_preserves_the_checkpoint() {
    let left = message(2, 1, "assistant", json!("Withdrawn reply"));
    let mut duration = opaque(10, 3, "system");
    duration["subtype"] = json!("turn_duration");
    duration["durationMs"] = json!(200);
    duration["timestamp"] = json!("2026-01-01T00:00:03.500Z");
    duration["isSidechain"] = json!(false);
    let first = [
        message(1, 0, "user", json!("Start")),
        left.clone(),
        message(3, 1, "assistant", json!("Retained reply")),
        duration.clone(),
    ];
    for parent in [3, 2] {
        let mut update = duration.clone();
        update["durationMs"] = json!(201);
        update["parentUuid"] = json!(Uuid::from_u128(parent));
        let mut rows = first.to_vec();
        rows.push(update);
        rows.extend(compaction(4, 10, &[3, 10]));
        rows.push(left.clone());
        for preview in [false, true] {
            let snapshot = read_session(&rows, preview);
            let handoff = import_trajectory(&snapshot);
            let search = omnis_core::trajectory_search_document(&snapshot);
            assert_eq!(handoff.truncated, parent != 3);
            assert_eq!(search.source_complete, parent == 3);
            if parent == 3 {
                assert_eq!(handoff.items.len(), 2);
                assert!(handoff.items[0].text.ends_with("\nSummary 4"));
                assert_eq!(handoff.items[1].text, "Retained reply");
                assert!(search.text.contains("Retained reply"));
                assert!(!search.text.contains("Withdrawn reply"));
            }
        }
    }
}

#[test]
fn a_repeated_turn_duration_cannot_change_the_next_attachments_retained_parent() {
    let prefix = history_with_turn_duration();
    let repeated = &prefix[2];
    let mut changed = repeated.clone();
    changed["durationMs"] = json!(301);
    let mut next = directory_attachment(11, 3);
    next["timestamp"] = json!("2026-01-01T00:00:08Z");
    let mut child = message(12, 11, "user", json!("New request"));
    child["timestamp"] = json!("2026-01-01T00:00:09Z");
    let mut control = prefix.clone();
    control.extend([next.clone(), child.clone()]);
    for preview in [false, true] {
        let expected = read_session(&control, preview);
        for old in [repeated, &changed] {
            let mut rows = prefix.clone();
            rows.extend([old.clone(), next.clone(), child.clone()]);
            let actual = read_session(&rows, preview);
            let handoff = import_trajectory(&actual);
            assert_eq!(handoff, import_trajectory(&expected));
            assert!(!handoff.truncated);
            let markdown = omnis_core::render_markdown_export(&actual);
            for marker in ["Summary 4", "Retained result", "New request"] {
                assert!(markdown.contains(marker));
            }
            assert!(!markdown.contains("Withdrawn reply"));
        }
    }
}

#[test]
fn a_turn_duration_copy_and_a_new_duration_on_either_side_of_a_checkpoint() {
    let mut base = history_with_turn_duration();
    let timed = |mut row: Value, time: &str| {
        row["timestamp"] = json!(format!("2026-01-01T00:00:{time}Z"));
        row["isSidechain"] = json!(false);
        row
    };
    base.push(timed(
        message(6, 3, "assistant", json!("New decision")),
        "07",
    ));
    let [boundary, summary] = compaction(7, 6, &[6]);
    let checkpoint = [timed(boundary, "08"), timed(summary, "09")];
    let old = &base[2];
    let mut distinct = old.clone();
    distinct["uuid"] = json!(Uuid::from_u128(19));
    let retained = &["Summary 7", "New decision", "New request"][..];
    for (before, duration, expected) in [
        (true, old.clone(), retained),
        (false, old.clone(), retained),
        (true, timed(distinct.clone(), "07.500"), retained),
        (
            false,
            timed(distinct.clone(), "09.500"),
            &["Summary 7", "New request"][..],
        ),
        (false, distinct, &["New request"][..]),
    ] {
        let mut rows = base.clone();
        let mut progress = opaque(11, 10, "progress");
        progress["parentUuid"] = duration["uuid"].clone();
        if before {
            rows.push(duration.clone());
        }
        rows.extend(checkpoint.clone());
        if !before {
            rows.push(duration);
        }
        rows.push(timed(progress, "10"));
        rows.push(timed(message(12, 11, "user", json!("New request")), "11"));
        for preview in [false, true] {
            let handoff = import_trajectory(&read_session(&rows, preview));
            assert_eq!(
                handoff_messages(&handoff),
                expected,
                "before={before}, preview={preview}"
            );
        }
    }
}

#[test]
fn old_leaf_copies_respect_a_verified_compaction_cutoff() {
    let before = [
        message(1, 0, "user", json!("Start")),
        message(2, 1, "assistant", json!("Left reply")),
        message(3, 1, "assistant", json!("Right reply")),
    ];
    let [header, summary] = compaction(4, 3, &[3]);
    let mut empty = summary.clone();
    empty["message"]["content"] = json!(CLAUDE_SUMMARY_ENVELOPE);
    for (anchor, complete) in [(Some(summary), true), (None, false), (Some(empty), false)] {
        let mut rows = before.to_vec();
        rows.push(header.clone());
        rows.extend(anchor);
        let expected = read(&rows);
        let search = omnis_core::trajectory_search_document(&expected);
        assert_eq!(search.source_complete, complete);
        assert!(!search.text.contains("Left reply"));
        assert!(search.text.contains("Right reply"));
        assert!(
            import_trajectory(&expected)
                .items
                .iter()
                .any(|item| item.text == "Right reply")
        );
        for copy in &before {
            rows.push(copy.clone());
            let snapshot = read(&rows);
            assert_eq!(import_trajectory(&snapshot), import_trajectory(&expected));
            assert_eq!(omnis_core::trajectory_search_document(&snapshot), search);
            if !complete {
                let mut recovery = Vec::new();
                omnis_core::write_recovery_handoff(&snapshot, &mut recovery).unwrap();
                let recovery = String::from_utf8(recovery).unwrap();
                assert!(recovery.contains("missing or unverified history"));
                assert!(recovery.contains("Right reply"));
                assert!(!recovery.contains("Left reply"));
            }
            rows.pop();
        }
    }
}

#[test]
fn an_unflagged_declared_anchor_scopes_incomplete_history_to_its_branch() {
    let [header, mut anchor] = compaction(3, 2, &[2]);
    anchor.as_object_mut().unwrap().remove("isCompactSummary");
    anchor["message"]["content"] = json!([
        {"type":"text","text":format!("{CLAUDE_SUMMARY_ENVELOPE}Known summary")},
        {"type":"text","text":"Unverified note"}
    ]);
    for members in [
        header["compactMetadata"].clone(),
        json!({"preservedSegment":{"anchorUuid":Uuid::from_u128(4),
            "headUuid":Uuid::from_u128(2), "tailUuid":Uuid::from_u128(2)}}),
    ] {
        let mut header = header.clone();
        header["compactMetadata"] = members;
        let mut rows = vec![
            message(1, 0, "user", json!("Earlier request")),
            message(2, 1, "assistant", json!("Retained decision")),
            header,
            anchor.clone(),
            message(5, 4, "user", json!("Current request")),
        ];
        let snapshot = read(&rows);
        let handoff = import_trajectory(&snapshot);
        assert!(handoff.truncated);
        let items: Vec<_> = handoff
            .items
            .iter()
            .map(|item| item.text.as_str())
            .collect();
        assert!(items.contains(&"Retained decision") && items.contains(&"Current request"));
        assert!(!items.contains(&"Earlier request") && !items.contains(&"Unverified note"));
        assert!(handoff.items.iter().any(|item| item.kind
            == omnis_core::TrajectoryItemKind::Assistant
            && item.text.ends_with("\nKnown summary")));
        let mut recovery = Vec::new();
        omnis_core::write_recovery_handoff(&snapshot, &mut recovery).unwrap();
        let recovery = String::from_utf8(recovery).unwrap();
        assert!(recovery.contains("Earlier request"));
        assert!(recovery.contains("### Unverified Claude compaction anchor\n\n> Unverified note"));

        rows.push(message(6, 1, "user", json!("Summary 3")));
        let independent = import_trajectory(&read(&rows));
        assert!(!independent.truncated);
        assert_eq!(independent.items.last().unwrap().text, "Summary 3");
    }
}

#[test]
fn copied_compaction_records_keep_the_reply_and_verify_the_boundary() {
    let [header, summary] = compaction(3, 2, &[2]);
    let reply = block(5, 4, "continued", json!("Continued answer"));
    let original = [
        message(1, 0, "user", json!("Before compression")),
        block(2, 1, "initial", json!("Original answer")),
        header.clone(),
        summary.clone(),
        reply.clone(),
    ];
    let before = read(&original);
    let mut restored = original.to_vec();
    restored.extend([header.clone(), summary.clone(), reply]);
    let after = read(&restored);
    assert_eq!(after.events, before.events);
    assert!(!import_trajectory(&after).truncated);

    let mut changed = header;
    changed["compactMetadata"]["preservedMessages"]["uuids"] = json!([Uuid::from_u128(100)]);
    restored.extend([changed, summary]);
    assert!(import_trajectory(&read(&restored)).truncated);
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
fn a_repeated_source_distinguishes_ambiguous_calls_from_missing_selected_calls() {
    for (second_parent, result_parent, source, tool, warn) in [
        (1, 2, 2, "raw", true),
        (2, 2, 2, "raw", true),
        (1, 1, 2, "raw", false),
        (2, 2, 2, "other", true),
        (1, 1, 2, "other", false),
        (2, 2, 99, "raw", true),
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
        assert_eq!(
            search.text.contains("unverifiedoutput"),
            source == 99 || (tool == "other" && warn)
        );
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

#[test]
fn a_retained_parallel_result_keeps_its_summarized_issuer_as_documentary_history() {
    let id = Uuid::from_u128;
    for segment in [false, true] {
        for same_response in [true, false] {
            let mut rows = vec![
                message(1, 0, "user", json!("Inspect both")),
                block(
                    2,
                    1,
                    if same_response { "r" } else { "old" },
                    json!([call("left")]),
                ),
                block(3, 1, "r", json!([call("right")])),
                linked_result(4, 3, 3, "right", "verifiedright"),
                linked_result(5, 4, 2, "left", "documentaryleft"),
                message(6, 5, "assistant", json!("Done")),
            ];
            let [mut header, summary] = compaction(7, 6, &[3, 4, 5, 6]);
            if segment {
                header["compactMetadata"] = json!({"preservedSegment": {
                    "anchorUuid":id(8),"headUuid":id(3),"tailUuid":id(6)
                }});
            }
            rows.extend([header, summary]);
            let snapshot = read(&rows);
            let search = omnis_core::trajectory_search_document(&snapshot);
            let mut recovery = Vec::new();
            omnis_core::write_recovery_handoff(&snapshot, &mut recovery).unwrap();
            for text in [&search.text, &String::from_utf8(recovery).unwrap()] {
                assert_eq!(
                    text.matches("documentaryleft").count(),
                    usize::from(same_response)
                );
            }
            assert_eq!(tool_outcomes(&snapshot), [("verifiedright".into(), false)]);
            assert!(search.source_complete);
        }
    }
}

#[test]
fn a_selected_result_with_an_absent_source_preserves_the_output_and_reports_the_gap() {
    for (chosen, claimed_id) in [(true, "left"), (true, "right"), (false, "left")] {
        let snapshot = read(&[
            message(1, 0, "user", json!("Inspect both")),
            block(3, 1, "r", json!([call("right")])),
            linked_result(4, 3, 3, "right", "verifiedright"),
            linked_result(5, 4, 2, claimed_id, "unpairedleft"),
            message(6, if chosen { 5 } else { 4 }, "assistant", json!("Done")),
        ]);
        let search = omnis_core::trajectory_search_document(&snapshot);
        assert_eq!(search.source_complete, !chosen);
        assert_eq!(import_trajectory(&snapshot).truncated, chosen);
        let mut recovery = Vec::new();
        omnis_core::write_recovery_handoff(&snapshot, &mut recovery).unwrap();
        for text in [&search.text, &String::from_utf8(recovery).unwrap()] {
            assert_eq!(text.matches("unpairedleft").count(), usize::from(chosen));
        }
        assert_eq!(tool_outcomes(&snapshot), [("verifiedright".into(), false)]);
    }
}

#[test]
fn a_selected_result_survives_when_its_assistant_lost_the_call_block() {
    for (same_row, chosen) in [(false, true), (true, true), (false, false)] {
        let mut verified = linked_result(3, 2, 2, "right", "verifiedright");
        let mut rows = vec![
            message(1, 0, "user", json!("Inspect both")),
            block(2, 1, "parallel", json!([call("right")])),
        ];
        let missing = linked_result(4, 3, 2, "left", "unpairedleft");
        if same_row {
            verified["message"]["content"]
                .as_array_mut()
                .unwrap()
                .push(missing["message"]["content"][0].clone());
            rows.push(verified);
        } else {
            rows.extend([verified, missing]);
        }
        rows.push(message(
            5,
            if same_row || !chosen { 3 } else { 4 },
            "assistant",
            json!("Done"),
        ));
        let snapshot = read(&rows);
        let search = omnis_core::trajectory_search_document(&snapshot);
        assert_eq!(search.source_complete, !chosen);
        assert_eq!(import_trajectory(&snapshot).truncated, chosen);
        let mut recovery = Vec::new();
        omnis_core::write_recovery_handoff(&snapshot, &mut recovery).unwrap();
        for text in [&search.text, &String::from_utf8(recovery).unwrap()] {
            assert_eq!(text.matches("unpairedleft").count(), usize::from(chosen));
        }
        assert_eq!(tool_outcomes(&snapshot), [("verifiedright".into(), false)]);
    }
}

#[test]
fn another_verified_issuer_prevents_a_selected_result_from_claiming_a_missing_call() {
    for request in ["parallel", "retry"] {
        let snapshot = read(&[
            message(1, 0, "user", json!("Inspect both")),
            block(2, 1, "parallel", json!([call("right")])),
            linked_result(3, 2, 2, "right", "verifiedright"),
            block(4, 3, request, json!([call("left")])),
            linked_result(5, 4, 4, "left", "verifiedleft"),
            linked_result(6, 5, 2, "left", "contradicted"),
        ]);
        let search = omnis_core::trajectory_search_document(&snapshot);
        assert!(search.source_complete);
        assert!(!search.text.contains("contradicted"));
        assert_eq!(
            tool_outcomes(&snapshot),
            [
                ("verifiedright".into(), false),
                ("verifiedleft".into(), false)
            ]
        );
    }
}

#[test]
fn a_copied_branch_returns_to_its_most_recently_created_leaf() {
    let rows = [
        message(1, 0, "user", json!("Root")),
        message(2, 1, "assistant", json!("Earlier branch")),
        message(3, 2, "user", json!("First child")),
        message(4, 2, "user", json!("Second child")),
        message(5, 3, "assistant", json!("Newest descendant")),
        message(6, 1, "assistant", json!("Other branch")),
    ];
    let mut restored = rows.to_vec();
    for (id, answer) in [
        (4, "Second child"),
        (2, "Newest descendant"),
        (6, "Other branch"),
    ] {
        restored.push(rows[id - 1].clone());
        assert_eq!(
            import_trajectory(&read(&restored))
                .items
                .last()
                .unwrap()
                .text,
            answer
        );
    }
}

#[test]
fn a_complete_copy_after_an_ordered_fragment_restores_the_same_handoff() {
    let mut complete = vec![
        message(1, 0, "user", json!("First request")),
        message(2, 1, "assistant", json!("First reply")),
        message(3, 2, "user", json!("Withdrawn")),
        message(4, 2, "user", json!("Replacement")),
        block(5, 4, "parallel", json!([call("left")])),
        block(6, 4, "parallel", json!([call("right")])),
        linked_result(7, 6, 6, "right", "right output"),
        linked_result(8, 7, 5, "left", "left output"),
        message(9, 8, "assistant", json!("Decision")),
    ];
    complete.extend(compaction(10, 9, &[4, 5, 6, 7, 8, 9]));
    complete.extend([
        opaque(12, 11, "progress"),
        opaque(13, 12, "attachment"),
        message(14, 13, "assistant", json!("Final response")),
    ]);

    let history = |snapshot: &CanonicalSnapshot| {
        let mut recovery = Vec::new();
        omnis_core::write_recovery_handoff(snapshot, &mut recovery).unwrap();
        (
            import_trajectory(snapshot),
            omnis_core::trajectory_search_document(snapshot),
            recovery,
        )
    };
    let original = read(&complete);
    let (native, searchable, recovery) = history(&original);
    assert!(searchable.source_complete && !native.truncated);
    assert!(searchable.text.contains("Final response"));
    assert!(!searchable.text.contains("Withdrawn"));
    assert_eq!(
        tool_outcomes(&original),
        [
            ("left output".into(), false),
            ("right output".into(), false)
        ]
    );

    let ids = 1..=complete.len();
    for (name, members) in [
        (
            "withdrawn request omitted",
            ids.clone().filter(|&id| id != 3).collect(),
        ),
        (
            "parallel issuer omitted",
            ids.filter(|&id| id != 5).collect(),
        ),
        ("progress and attachment", (12..=14).collect()),
        ("checkpoint and tail", (10..=14).collect()),
        ("nonadjacent records", vec![2, 6, 7, 9, 11, 14]),
        (
            "overlapping rootless checkpoint fragments",
            vec![10, 11, 14, 9, 10, 12, 14],
        ),
        (
            "successively fuller copies",
            vec![1, 4, 14, 1, 4, 5, 7, 11, 14],
        ),
    ] {
        let mut restored: Vec<_> = members
            .into_iter()
            .map(|id| complete[id - 1].clone())
            .collect();
        restored.extend_from_slice(&complete);
        for preview in [false, true] {
            let (imported, indexed, handoff) = history(&read_session(&restored, preview));
            assert_eq!(imported, native, "{name}, preview={preview}");
            assert_eq!(indexed, searchable, "{name}, preview={preview}");
            assert_eq!(handoff, recovery, "{name}, preview={preview}");
        }
    }
}

fn check_selected_messages(rows: &[Value], expected: &[&str], discarded: &[&str]) {
    for preview in [false, true] {
        let snapshot = read_session(rows, preview);
        assert_eq!(handoff_messages(&import_trajectory(&snapshot)), expected);
        for text in [
            omnis_core::trajectory_search_document(&snapshot).text,
            recovery_text(&snapshot),
        ] {
            for message in expected {
                assert!(text.contains(message), "{message}: {text}");
            }
            for message in discarded {
                assert!(!text.contains(message), "{message}");
            }
        }
    }
}

#[test]
fn retained_hidden_copy_rewinds_a_sibling_in_the_same_checkpoint() {
    let hidden = meta_user(3, 2);
    let mut rows = vec![
        message(1, 0, "user", json!("Initial task")),
        message(2, 1, "assistant", json!("Retained answer")),
        hidden.clone(),
    ];
    rows.extend(compaction(6, 3, &[2, 3]));
    rows.extend([
        message(8, 2, "user", json!("Sibling request")),
        message(9, 8, "assistant", json!("Sibling answer")),
        hidden,
    ]);
    for preview in [false, true] {
        let snapshot = read_session(&rows, preview);
        assert_eq!(
            handoff_messages(&import_trajectory(&snapshot)),
            ["Summary 6", "Retained answer"]
        );
        for text in [
            omnis_core::trajectory_search_document(&snapshot).text,
            recovery_text(&snapshot),
        ] {
            assert!(text.contains("Retained answer"));
            assert!(!text.contains("Sibling request"));
            assert!(!text.contains("Sibling answer"));
        }
    }
}

#[test]
fn hidden_copy_uses_the_latest_definition_of_an_intermediate_link() {
    for directory in [false, true] {
        let link = |parent| {
            if directory {
                directory_attachment(3, parent)
            } else {
                opaque(3, parent, "progress")
            }
        };
        let hidden = meta_user(4, 3);
        let rows = [
            message(1, 0, "user", json!("First task")),
            link(1),
            hidden.clone(),
            message(2, 0, "user", json!("Second task")),
            link(2),
            hidden,
            message(5, 4, "assistant", json!("Final answer")),
        ];
        check_selected_messages(&rows, &["Second task", "Final answer"], &["First task"]);
    }
}

#[test]
fn rebuilding_a_hidden_row_preserves_its_existing_descendants() {
    let hidden = meta_user(3, 2);
    let rows = [
        message(1, 0, "user", json!("First task")),
        message(2, 1, "assistant", json!("Initial answer")),
        hidden.clone(),
        message(4, 3, "user", json!("Follow-up request")),
        message(5, 4, "assistant", json!("Final answer")),
        message(1, 0, "user", json!("Replacement task")),
        hidden,
    ];
    check_selected_messages(
        &rows,
        &[
            "Replacement task",
            "Initial answer",
            "Follow-up request",
            "Final answer",
        ],
        &["First task"],
    );
}

#[test]
fn a_continuation_resolves_the_rebuilt_assistant_by_uuid() {
    let hidden = meta_user(3, 2);
    for tool in [true, false] {
        let assistant = if tool {
            file_calls(2, 1, "file.txt", &["x"])
        } else {
            message(2, 1, "assistant", json!("Initial answer"))
        };
        let mut rows = vec![
            message(1, 0, "user", json!("First task")),
            assistant,
            hidden.clone(),
            message(1, 0, "user", json!("Replacement task")),
            hidden.clone(),
        ];
        if tool {
            rows.extend([
                linked_result(4, 3, 2, "x", "Observed output"),
                message(5, 4, "assistant", json!("Done")),
            ]);
            for preview in [false, true] {
                let snapshot = read_session(&rows, preview);
                assert_eq!(
                    tool_outcomes(&snapshot),
                    [("Observed output".to_string(), false)]
                );
                for text in [
                    omnis_core::trajectory_search_document(&snapshot).text,
                    recovery_text(&snapshot),
                ] {
                    assert!(text.contains("Observed output"));
                }
            }
        } else {
            rows.push(message(4, 2, "user", json!("Follow-up request")));
            check_selected_messages(
                &rows,
                &["Replacement task", "Initial answer", "Follow-up request"],
                &["First task"],
            );
        }
    }
}

#[test]
fn rebuilding_a_streamed_response_preserves_its_other_block() {
    let hidden = meta_user(4, 3);
    let rows = [
        message(1, 0, "user", json!("First task")),
        block(2, 1, "shared", json!("First block")),
        block(3, 1, "shared", json!("Second block")),
        hidden.clone(),
        message(1, 0, "user", json!("Replacement task")),
        hidden,
        message(5, 4, "user", json!("Follow-up request")),
    ];
    for preview in [false, true] {
        let snapshot = read_session(&rows, preview);
        let handoff = import_trajectory(&snapshot)
            .items
            .into_iter()
            .map(|item| item.text)
            .collect::<Vec<_>>()
            .join("\n");
        for text in [
            handoff,
            omnis_core::trajectory_search_document(&snapshot).text,
            recovery_text(&snapshot),
        ] {
            for expected in [
                "Replacement task",
                "First block",
                "Second block",
                "Follow-up request",
            ] {
                assert!(text.contains(expected), "{expected}");
            }
            assert!(!text.contains("First task"));
        }
    }
}

#[test]
fn duplicate_duration_under_a_replacement_request_keeps_the_reply() {
    let duration = turn_duration(2, 1);
    let rows = [
        message(1, 0, "user", json!("Original task")),
        message(1, 0, "user", json!("Replacement task")),
        duration.clone(),
        message(3, 2, "assistant", json!("Verified reply")),
        duration,
    ];
    check_selected_messages(
        &rows,
        &["Replacement task", "Verified reply"],
        &["Original task"],
    );
}

#[test]
fn replies_to_a_rebuilt_hidden_row_inherit_its_current_parent_coverage() {
    for (before, after) in [(0, 100), (100, 0)] {
        let hidden = meta_user(3, 2);
        let mut rows = vec![
            message(1, before, "user", json!("Old task")),
            message(2, 1, "assistant", json!("Initial reply")),
            hidden.clone(),
            message(1, after, "user", json!("Replacement task")),
            hidden,
        ];
        for reply in [false, true] {
            if reply {
                rows.push(message(4, 3, "assistant", json!("Final answer")));
            }
            for preview in [false, true] {
                let snapshot = read_session(&rows, preview);
                assert_eq!(
                    import_trajectory(&snapshot).truncated,
                    after != 0,
                    "reply={reply}"
                );
                assert_eq!(
                    !omnis_core::trajectory_search_document(&snapshot).source_complete,
                    after != 0,
                    "reply={reply}"
                );
            }
        }
    }
}

#[test]
fn a_late_undeclared_result_remains_documentary_after_its_call_is_compacted() {
    let mut rows = vec![
        message(1, 0, "user", json!("Read file")),
        file_calls(2, 1, "file.txt", &["x"]),
        meta_user(3, 2),
    ];
    rows.extend(compaction(6, 3, &[3]));
    rows.extend([
        message(8, 3, "user", json!([result("x", "Late output", false)])),
        message(9, 8, "assistant", json!("Done")),
    ]);
    for preview in [false, true] {
        let snapshot = read_session(&rows, preview);
        assert!(tool_outcomes(&snapshot).is_empty());
        for text in [
            omnis_core::trajectory_search_document(&snapshot).text,
            recovery_text(&snapshot),
        ] {
            assert!(text.contains("Late output"));
        }
        assert!(import_trajectory(&snapshot).truncated);
    }
}

#[test]
fn an_answered_reused_tool_id_keeps_a_second_distinct_documentary_output() {
    let rows = [
        message(1, 0, "user", json!("Read files")),
        file_calls(2, 1, "first.txt", &["x"]),
        file_calls(3, 2, "second.txt", &["x", "bridge"]),
        linked_result(4, 3, 3, "x", "Verified output"),
        linked_result(5, 4, 3, "bridge", "Bridge output"),
        meta_user(6, 5),
        message(7, 6, "user", json!([result("x", "Disputed output", false)])),
        message(8, 7, "assistant", json!("Done")),
    ];
    for preview in [false, true] {
        let snapshot = read_session(&rows, preview);
        assert!(import_trajectory(&snapshot).truncated);
        assert!(
            tool_outcomes(&snapshot)
                .iter()
                .all(|(output, _)| output != "Disputed output")
        );
        for text in [
            omnis_core::trajectory_search_document(&snapshot).text,
            recovery_text(&snapshot),
        ] {
            assert!(text.contains("Verified output"));
            assert!(text.contains("Disputed output"));
        }
    }
}

#[test]
fn copied_hidden_child_of_progress_resolves_the_updated_request() {
    let hidden = meta_user(3, 2);
    let rows = [
        message(1, 0, "user", json!("Original request")),
        opaque(2, 1, "progress"),
        hidden.clone(),
        message(1, 0, "user", json!("Replacement request")),
        hidden,
        message(4, 3, "assistant", json!("Final reply")),
    ];
    check_selected_messages(
        &rows,
        &["Replacement request", "Final reply"],
        &["Original request"],
    );
}

#[test]
fn hidden_copy_resolves_replaced_request_and_reply_in_dependency_order() {
    let hidden = meta_user(3, 2);
    let rows = [
        message(1, 0, "user", json!("Original request")),
        message(2, 1, "assistant", json!("Original reply")),
        hidden.clone(),
        message(2, 1, "assistant", json!("Replacement reply")),
        message(1, 0, "user", json!("Replacement request")),
        hidden,
        message(4, 3, "user", json!("Continue task")),
    ];
    check_selected_messages(
        &rows,
        &["Replacement request", "Replacement reply", "Continue task"],
        &["Original request", "Original reply"],
    );
}

#[test]
fn copying_a_retained_hidden_parent_rewinds_its_alternative_child() {
    let hidden = meta_user(3, 2);
    let mut rows = vec![
        message(1, 0, "user", json!("Original task")),
        message(2, 1, "assistant", json!("Retained answer")),
        hidden.clone(),
        message(4, 3, "user", json!("Retained follow-up")),
    ];
    rows.extend(compaction(6, 4, &[2, 3, 4]));
    rows.extend([
        message(8, 3, "user", json!("Alternative request")),
        message(9, 8, "assistant", json!("Alternative answer")),
        hidden,
    ]);
    for preview in [false, true] {
        let snapshot = read_session(&rows, preview);
        assert_eq!(
            handoff_messages(&import_trajectory(&snapshot)),
            ["Summary 6", "Retained answer"]
        );
        for text in [
            omnis_core::trajectory_search_document(&snapshot).text,
            recovery_text(&snapshot),
        ] {
            assert!(text.contains("Retained answer"));
            for discarded in ["Alternative request", "Alternative answer"] {
                assert!(!text.contains(discarded));
            }
        }
    }
}

#[test]
fn copied_request_selects_the_newest_source_answer_after_rebuilding() {
    let hidden = meta_user(3, 2);
    let replacement = message(1, 0, "user", json!("Replacement request"));
    let rows = [
        message(1, 0, "user", json!("Original request")),
        message(2, 1, "assistant", json!("Initial reply")),
        hidden.clone(),
        message(4, 3, "assistant", json!("Older answer")),
        replacement.clone(),
        message(5, 1, "assistant", json!("Newer answer")),
        hidden,
        replacement,
    ];
    check_selected_messages(
        &rows,
        &["Replacement request", "Newer answer"],
        &["Original request", "Initial reply", "Older answer"],
    );
}

#[test]
fn copied_duration_finds_its_parent_when_the_parent_arrived_late() {
    let duration = turn_duration(2, 1);
    let mut parent = message(1, 0, "user", json!("Known request"));
    parent["timestamp"] = json!("2026-01-01T00:00:01Z");
    let mut later = parent.clone();
    later["timestamp"] = json!("2026-01-01T00:00:02Z");
    let rows = [
        duration.clone(),
        parent,
        later,
        duration,
        message(3, 2, "assistant", json!("Verified reply")),
    ];
    check_selected_messages(&rows, &["Known request", "Verified reply"], &[]);
}

#[test]
fn late_output_below_nested_hidden_rows_survives_in_documentary_history() {
    for declared in [false, true] {
        let mut rows = vec![
            message(1, 0, "user", json!("Read file")),
            file_calls(2, 1, "file.txt", &["x"]),
            meta_user(3, 2),
        ];
        rows.extend(compaction(6, 3, &[3]));
        let late = if declared {
            linked_result(9, 8, 2, "x", "Late output")
        } else {
            message(9, 8, "user", json!([result("x", "Late output", false)]))
        };
        rows.extend([
            meta_user(8, 3),
            late,
            message(10, 9, "assistant", json!("Final answer")),
        ]);
        for preview in [false, true] {
            let snapshot = read_session(&rows, preview);
            assert!(tool_outcomes(&snapshot).is_empty());
            assert!(handoff_messages(&import_trajectory(&snapshot)).contains(&"Final answer"));
            for text in [
                omnis_core::trajectory_search_document(&snapshot).text,
                recovery_text(&snapshot),
            ] {
                assert!(text.contains("Final answer"));
                assert!(text.contains("Late output"));
            }
            assert!(import_trajectory(&snapshot).truncated);
        }
    }
}

#[test]
fn a_distinct_result_for_a_reused_tool_id_survives_an_intervening_call() {
    let rows = [
        message(1, 0, "user", json!("Read files")),
        file_calls(2, 1, "one.txt", &["x"]),
        linked_result(3, 2, 2, "x", "First output"),
        file_calls(4, 3, "two.txt", &["x"]),
        linked_result(5, 4, 4, "x", "Second output"),
        file_calls(6, 5, "three.txt", &["z"]),
        linked_result(7, 6, 6, "z", "Third output"),
        message(8, 7, "user", json!([result("x", "Distinct output", false)])),
        message(9, 8, "assistant", json!("Done")),
    ];
    for preview in [false, true] {
        let snapshot = read_session(&rows, preview);
        let outputs = tool_outcomes(&snapshot);
        for verified in ["First output", "Second output", "Third output"] {
            assert!(
                outputs.iter().any(|(text, _)| text == verified),
                "{verified}"
            );
        }
        assert!(!outputs.iter().any(|(text, _)| text == "Distinct output"));
        assert!(import_trajectory(&snapshot).truncated);
        for text in [
            omnis_core::trajectory_search_document(&snapshot).text,
            recovery_text(&snapshot),
        ] {
            for output in [
                "First output",
                "Second output",
                "Third output",
                "Distinct output",
            ] {
                assert!(text.contains(output), "{output}");
            }
        }
    }
}

#[test]
fn copying_the_last_retained_hidden_row_keeps_its_new_exchange() {
    for retained_tail in [false, true] {
        let hidden = meta_user(3, 2);
        let mut rows = vec![
            message(1, 0, "user", json!("Original task")),
            message(2, 1, "assistant", json!("Retained answer")),
            hidden.clone(),
        ];
        if !retained_tail {
            rows.push(message(4, 3, "user", json!("Retained follow-up")));
        }
        rows.extend(if retained_tail {
            compaction(6, 3, &[2, 3])
        } else {
            compaction(6, 4, &[2, 3, 4])
        });
        rows.extend([
            message(8, 3, "user", json!("New request")),
            message(9, 8, "assistant", json!("New answer")),
            hidden,
        ]);
        for preview in [false, true] {
            let snapshot = read_session(&rows, preview);
            let expected: &[&str] = if retained_tail {
                &["Summary 6", "Retained answer", "New request", "New answer"]
            } else {
                &["Summary 6", "Retained answer"]
            };
            assert_eq!(handoff_messages(&import_trajectory(&snapshot)), expected);
            for text in [
                omnis_core::trajectory_search_document(&snapshot).text,
                recovery_text(&snapshot),
            ] {
                assert!(text.contains("Retained answer"));
                for marker in ["New request", "New answer"] {
                    assert_eq!(text.contains(marker), retained_tail);
                }
            }
        }
    }
}

#[test]
fn progress_after_a_compaction_uses_the_replacement_request() {
    let hidden = meta_user(12, 11);
    let mut rows = vec![
        message(1, 0, "user", json!("Initial task")),
        message(2, 1, "assistant", json!("Retained reply")),
    ];
    rows.extend(compaction(6, 2, &[2]));
    rows.extend([
        message(10, 2, "user", json!("Superseded request")),
        opaque(11, 10, "progress"),
        hidden.clone(),
        message(10, 2, "user", json!("Replacement request")),
        hidden,
        message(13, 12, "assistant", json!("Latest answer")),
    ]);
    for preview in [false, true] {
        let snapshot = read_session(&rows, preview);
        assert_eq!(
            handoff_messages(&import_trajectory(&snapshot)),
            [
                "Summary 6",
                "Retained reply",
                "Replacement request",
                "Latest answer"
            ]
        );
        for text in [
            omnis_core::trajectory_search_document(&snapshot).text,
            recovery_text(&snapshot),
        ] {
            for marker in ["Retained reply", "Replacement request", "Latest answer"] {
                assert!(text.contains(marker));
            }
            assert!(!text.contains("Superseded request"));
        }
    }
}

#[test]
fn reconnecting_a_duration_keeps_its_existing_reply() {
    let duration = turn_duration(2, 1);
    let mut parent = message(1, 0, "user", json!("Known request"));
    parent["timestamp"] = json!("2026-01-01T00:00:01Z");
    let mut later = parent.clone();
    later["timestamp"] = json!("2026-01-01T00:00:02Z");
    let rows = [
        duration.clone(),
        message(3, 2, "assistant", json!("Known reply")),
        parent,
        later,
        duration,
    ];
    check_selected_messages(&rows, &["Known request", "Known reply"], &[]);
}

#[test]
fn a_progress_parent_that_arrives_late_reconnects_its_hidden_child() {
    let hidden = meta_user(3, 2);
    let rows = [
        opaque(2, 1, "progress"),
        hidden.clone(),
        message(1, 0, "user", json!("Known request")),
        hidden,
        message(4, 3, "assistant", json!("Known reply")),
    ];
    check_selected_messages(&rows, &["Known request", "Known reply"], &[]);
    for preview in [false, true] {
        let snapshot = read_session(&rows, preview);
        assert!(!import_trajectory(&snapshot).truncated);
        assert!(omnis_core::trajectory_search_document(&snapshot).source_complete);
    }
}

#[test]
fn both_late_outputs_survive_when_their_compacted_results_are_split() {
    for split in [false, true] {
        let mut rows = vec![
            message(1, 0, "user", json!("Read files")),
            file_calls(2, 1, "file.txt", &["x", "y"]),
            meta_user(3, 2),
        ];
        rows.extend(compaction(6, 3, &[3]));
        rows.push(meta_user(8, 3));
        let mut first = linked_result(9, 8, 2, "x", "First late output");
        if split {
            rows.extend([first, linked_result(10, 9, 2, "y", "Second late output")]);
        } else {
            first["message"]["content"]
                .as_array_mut()
                .unwrap()
                .push(result("y", "Second late output", false));
            rows.push(first);
        }
        rows.push(message(
            11,
            if split { 10 } else { 9 },
            "assistant",
            json!("Done"),
        ));
        for preview in [false, true] {
            let snapshot = read_session(&rows, preview);
            assert!(tool_outcomes(&snapshot).is_empty());
            for text in [
                omnis_core::trajectory_search_document(&snapshot).text,
                recovery_text(&snapshot),
            ] {
                for marker in ["First late output", "Second late output", "Done"] {
                    assert!(text.contains(marker), "{marker}, split={split}");
                }
            }
        }
    }
}

fn check_distinct_output_with_intervening_call(completed: bool, mixed: bool) {
    let mut rows = vec![
        message(1, 0, "user", json!("Read files")),
        file_calls(2, 1, "one.txt", &["x"]),
        linked_result(3, 2, 2, "x", "First output"),
        file_calls(4, 3, "two.txt", &["x"]),
    ];
    if completed {
        rows.push(linked_result(5, 4, 4, "x", "Second output"));
    }
    rows.extend([
        file_calls(6, if completed { 5 } else { 4 }, "three.txt", &["z"]),
        linked_result(7, 6, 6, "z", "Third output"),
    ]);
    let mut content = vec![result("x", "Distinct output", false)];
    if mixed {
        content.push(json!({"type":"text","text":"New request"}));
    }
    rows.extend([
        message(8, 7, "user", json!(content)),
        message(9, 8, "assistant", json!("Done")),
    ]);
    for preview in [false, true] {
        let snapshot = read_session(&rows, preview);
        let outputs = tool_outcomes(&snapshot);
        for marker in ["First output", "Third output"] {
            assert!(outputs.iter().any(|(text, _)| text == marker));
        }
        assert_eq!(
            outputs.iter().any(|(text, _)| text == "Second output"),
            completed
        );
        assert!(!outputs.iter().any(|(text, _)| text == "Distinct output"));
        for text in [
            omnis_core::trajectory_search_document(&snapshot).text,
            recovery_text(&snapshot),
        ] {
            assert_eq!(text.contains("New request"), mixed);
            assert_eq!(text.contains("Second output"), completed);
            for marker in ["First output", "Third output", "Distinct output"] {
                assert!(text.contains(marker), "{marker}");
            }
        }
    }
}

#[test]
fn a_mixed_user_result_keeps_its_distinct_documentary_output() {
    for mixed in [false, true] {
        check_distinct_output_with_intervening_call(true, mixed);
    }
}

#[test]
fn an_unanswered_reused_call_keeps_its_distinct_documentary_output() {
    for completed in [true, false] {
        check_distinct_output_with_intervening_call(completed, false);
    }
}

#[test]
fn copying_a_retained_tail_with_competing_children_rewinds_both() {
    for competing in [false, true] {
        let hidden = meta_user(3, 2);
        let mut rows = vec![
            message(1, 0, "user", json!("Initial task")),
            message(2, 1, "assistant", json!("Retained reply")),
            hidden.clone(),
        ];
        rows.extend(compaction(6, 3, &[2, 3]));
        rows.extend([
            message(8, 3, "user", json!("First request")),
            message(9, 8, "assistant", json!("First answer")),
        ]);
        if competing {
            rows.extend([
                message(10, 3, "user", json!("Second request")),
                message(11, 10, "assistant", json!("Second answer")),
            ]);
        }
        rows.push(hidden);
        for preview in [false, true] {
            let snapshot = read_session(&rows, preview);
            let expected: &[&str] = if competing {
                &["Summary 6", "Retained reply"]
            } else {
                &[
                    "Summary 6",
                    "Retained reply",
                    "First request",
                    "First answer",
                ]
            };
            assert_eq!(handoff_messages(&import_trajectory(&snapshot)), expected);
            for text in [
                omnis_core::trajectory_search_document(&snapshot).text,
                recovery_text(&snapshot),
            ] {
                assert!(text.contains("Retained reply"));
                for marker in ["First request", "First answer"] {
                    assert_eq!(text.contains(marker), !competing);
                }
                for marker in ["Second request", "Second answer"] {
                    assert!(!text.contains(marker));
                }
            }
        }
    }
}

#[test]
fn repairing_an_old_hidden_row_preserves_the_current_compacted_exchange() {
    let hidden = meta_user(2, 1);
    let mut rows = vec![
        hidden.clone(),
        message(3, 2, "assistant", json!("Early reply")),
        message(1, 0, "user", json!("Original task")),
        message(4, 1, "assistant", json!("Retained reply")),
    ];
    rows.extend(compaction(6, 4, &[4]));
    rows.extend([
        message(8, 4, "user", json!("Current request")),
        message(9, 8, "assistant", json!("Current answer")),
        hidden,
    ]);
    for preview in [false, true] {
        let snapshot = read_session(&rows, preview);
        assert_eq!(
            handoff_messages(&import_trajectory(&snapshot)),
            [
                "Summary 6",
                "Retained reply",
                "Current request",
                "Current answer"
            ]
        );
        for text in [
            omnis_core::trajectory_search_document(&snapshot).text,
            recovery_text(&snapshot),
        ] {
            for marker in ["Retained reply", "Current request", "Current answer"] {
                assert!(text.contains(marker));
            }
            assert!(!text.contains("Early reply"));
        }
    }
}

fn check_compacted_reused_id_outputs(keep_first: bool) {
    let mut rows = vec![
        message(1, 0, "user", json!("Read files")),
        file_calls(2, 1, "one.txt", &["x"]),
        linked_result(3, 2, 2, "x", "First output"),
        file_calls(4, 3, "two.txt", &["x"]),
        linked_result(5, 4, 4, "x", "Second output"),
        file_calls(6, 5, "three.txt", &["z"]),
        linked_result(7, 6, 6, "z", "Third output"),
    ];
    rows.extend(compaction(
        10,
        7,
        if keep_first {
            &[2, 3, 4, 5, 6, 7]
        } else {
            &[4, 5, 6, 7]
        },
    ));
    rows.extend([
        message(
            12,
            7,
            "user",
            json!([result("x", "Distinct output", false)]),
        ),
        message(13, 12, "assistant", json!("Done")),
    ]);
    for preview in [false, true] {
        let snapshot = read_session(&rows, preview);
        let outputs = tool_outcomes(&snapshot);
        for marker in ["Second output", "Third output"] {
            assert!(outputs.iter().any(|(s, _)| s == marker), "{marker}");
        }
        assert_eq!(outputs.iter().any(|(s, _)| s == "First output"), keep_first);
        assert!(!outputs.iter().any(|(s, _)| s == "Distinct output"));
        for text in [
            omnis_core::trajectory_search_document(&snapshot).text,
            recovery_text(&snapshot),
        ] {
            for marker in ["Second output", "Third output", "Distinct output"] {
                assert!(text.contains(marker), "{marker}");
            }
            if keep_first {
                assert!(text.contains("First output"));
            }
        }
    }
}

#[test]
fn a_compacted_verified_result_wins_over_a_late_inferred_result() {
    check_compacted_reused_id_outputs(false);
}

#[test]
fn a_compaction_retaining_all_verified_results_keeps_the_distinct_observation() {
    check_compacted_reused_id_outputs(true);
}

#[test]
fn duration_repair_keeps_a_reply_connected_through_progress() {
    let duration = turn_duration(2, 1);
    let mut parent = message(1, 0, "user", json!("Known request"));
    parent["timestamp"] = json!("2026-01-01T00:00:01Z");
    let mut later = parent.clone();
    later["timestamp"] = json!("2026-01-01T00:00:02Z");
    let rows = [
        duration.clone(),
        opaque(3, 2, "progress"),
        message(4, 3, "assistant", json!("Known reply")),
        parent,
        later,
        duration,
    ];
    check_selected_messages(&rows, &["Known request", "Known reply"], &[]);
}

#[test]
fn repairing_progress_updates_every_link_to_the_missing_parent() {
    for link in [3, 2] {
        let hidden = meta_user(4, 3);
        let rows = [
            opaque(2, 1, "progress"),
            opaque(3, 2, "progress"),
            hidden.clone(),
            message(1, 0, "user", json!("Known request")),
            hidden,
            message(5, link, "assistant", json!("Known reply")),
        ];
        check_selected_messages(&rows, &["Known request", "Known reply"], &[]);
        for preview in [false, true] {
            let snapshot = read_session(&rows, preview);
            assert!(!import_trajectory(&snapshot).truncated);
            assert!(omnis_core::trajectory_search_document(&snapshot).source_complete);
        }
    }
}

#[test]
fn split_late_results_without_an_issuer_keep_both_compacted_outputs() {
    for split in [false, true] {
        let mut rows = vec![
            message(1, 0, "user", json!("Read files")),
            file_calls(2, 1, "file.txt", &["x", "y"]),
            meta_user(3, 2),
        ];
        rows.extend(compaction(6, 3, &[3]));
        rows.push(meta_user(8, 3));
        let first = result("x", "First late output", false);
        let second = result("y", "Second late output", false);
        if split {
            rows.extend([
                message(9, 8, "user", json!([first])),
                message(10, 9, "user", json!([second])),
            ]);
        } else {
            rows.push(message(9, 8, "user", json!([first, second])));
        }
        rows.push(message(
            11,
            if split { 10 } else { 9 },
            "assistant",
            json!("Done"),
        ));
        for preview in [false, true] {
            let snapshot = read_session(&rows, preview);
            assert!(tool_outcomes(&snapshot).is_empty());
            for text in [
                omnis_core::trajectory_search_document(&snapshot).text,
                recovery_text(&snapshot),
            ] {
                for marker in ["First late output", "Second late output", "Done"] {
                    assert!(text.contains(marker), "{marker}, split={split}");
                }
            }
        }
    }
}

#[test]
fn duration_repair_uses_source_timestamps_to_select_the_existing_reply() {
    for tied in [false, true] {
        let mut duration = turn_duration(2, 1);
        duration["timestamp"] = json!("2026-01-01T00:00:01Z");
        let mut reply = message(3, 2, "assistant", json!("Known reply"));
        let mut parent = message(1, 0, "user", json!("Known request"));
        reply["timestamp"] = if tied {
            duration["timestamp"].clone()
        } else {
            json!("2026-01-01T00:00:02Z")
        };
        parent["timestamp"] = if tied {
            duration["timestamp"].clone()
        } else {
            json!("2026-01-01T00:00:03Z")
        };
        let rows = [duration.clone(), reply, parent, duration];
        for preview in [false, true] {
            let snapshot = read_session(&rows, preview);
            let expected: &[&str] = if tied {
                &["Known request"]
            } else {
                &["Known request", "Known reply"]
            };
            assert_eq!(handoff_messages(&import_trajectory(&snapshot)), expected);
        }
    }
}
