use std::{
    fs::File,
    io::{BufWriter, Write},
};

use omnis_adapters::{ClaudeAdapter, ProviderAdapter};
use omnis_ir::{Provider, SessionRef};
use serde_json::json;
use uuid::Uuid;

#[test]
fn preview_preserves_the_kind_of_a_retained_gap_link() {
    let root = tempfile::tempdir().unwrap();
    let session = SessionRef::new(Provider::Claude, Uuid::from_u128(99).to_string());
    let source = root.path().join(format!("{}.jsonl", session.id));
    let adapter = ClaudeAdapter::with_root(root.path());
    let id = Uuid::from_u128;
    let message = |n, parent: Option<u128>, role, text| json!({"type":role,"uuid":id(n),"parentUuid":parent.map(id),"message":{"content":text}});
    let padding = json!({"type":"progress","padding":"x".repeat(64 * 1024)}).to_string();
    let retained = &["Task", "Summary", "Decision", "Selected note", "Continue"][..];
    let physical = &["Task", "Decision", "Continue"][..];
    for (kind, expected) in [
        ("attachment", retained),
        ("system", retained),
        ("progress", physical),
        ("unknown", physical),
    ] {
        let mut file = BufWriter::new(File::create(&source).unwrap());
        for record in [
            message(1, None, "user", "Task"),
            message(2, Some(1), "assistant", "Decision"),
            message(3, Some(2), "user", "Selected note"),
            message(4, Some(3), "assistant", "Withdrawn reply"),
        ] {
            writeln!(file, "{record}").unwrap();
        }
        // Put the link between the 4 MiB preview windows.
        for row in [
            json!({"type":kind,"uuid":id(5),"parentUuid":id(2)}),
            json!({
                "type":"system","subtype":"compact_boundary","uuid":id(6),"logicalParentUuid":id(4),
                "compactMetadata":{"preservedMessages":{"anchorUuid":id(7),"uuids":[id(2),id(3),id(5),id(4)]}}
            }),
        ] {
            for _ in 0..68 {
                writeln!(file, "{padding}").unwrap();
            }
            writeln!(file, "{row}").unwrap();
        }
        let mut summary = message(7, Some(6), "user", "Summary");
        summary["isCompactSummary"] = json!(true);
        writeln!(file, "{summary}").unwrap();
        writeln!(file, "{}", message(8, Some(5), "user", "Continue")).unwrap();
        file.flush().unwrap();

        for snapshot in [
            adapter.read_session(&session),
            adapter.preview_session(&session),
        ] {
            let snapshot = snapshot.unwrap();
            let texts: Vec<_> = snapshot
                .events
                .iter()
                .filter_map(|event| event.payload["text"].as_str())
                .collect();
            assert_eq!(texts, expected, "{kind}");
        }
    }
}

#[test]
fn preview_preserves_retained_links_and_explicit_roots_in_the_gap() {
    let root = tempfile::tempdir().unwrap();
    let id = Uuid::from_u128;
    let session = SessionRef::new(Provider::Claude, id(99).to_string());
    let source = root.path().join(format!("{}.jsonl", session.id));
    let adapter = ClaudeAdapter::with_root(root.path());
    let task = json!({"type":"user","uuid":id(1),"parentUuid":null,"message":{"content":"Task"}});
    let reply = |n, text| json!({"type":"assistant","uuid":id(n),"parentUuid":id(1),"message":{"content":text}});
    let withdrawn = reply(2, "Withdrawn answer");
    let kept = reply(3, "Retained answer");
    let duration = json!({"type":"system","subtype":"turn_duration","uuid":id(4),"parentUuid":id(3),"durationMs":200});
    let boundary = json!({"type":"system","subtype":"compact_boundary","uuid":id(5),"logicalParentUuid":id(3),
        "compactMetadata":{"preservedMessages":{"anchorUuid":id(6),"uuids":[id(3),id(4)]}}});
    let summary = json!({"type":"user","isCompactSummary":true,"uuid":id(6),"parentUuid":id(5),"message":{"content":"Summary"}});
    let padding = json!({"type":"progress","padding":"x".repeat(64 * 1024)}).to_string();
    let mut root_duration = duration.clone();
    root_duration["parentUuid"] = serde_json::Value::Null;
    let directory = json!({"type":"attachment","uuid":id(4),"parentUuid":null,
        "attachment":{"type":"directory","path":"fixture","content":""}});
    let progress = json!({"type":"progress","uuid":id(7),"parentUuid":null});
    for (case, gap, tail) in [
        (
            "copied duration",
            vec![duration.clone()],
            vec![duration.clone()],
        ),
        ("root duration", vec![root_duration], vec![]),
        ("root directory", vec![directory], vec![]),
        (
            "root progress before duration",
            vec![progress, duration],
            vec![],
        ),
    ] {
        let mut file = BufWriter::new(File::create(&source).unwrap());
        for row in [&task, &withdrawn, &kept] {
            writeln!(file, "{row}").unwrap();
        }
        for rows in [gap, tail] {
            for _ in 0..68 {
                writeln!(file, "{padding}").unwrap();
            }
            for row in rows {
                writeln!(file, "{row}").unwrap();
            }
        }
        for row in [&boundary, &summary, &withdrawn] {
            writeln!(file, "{row}").unwrap();
        }
        file.flush().unwrap();

        for (snapshot, sampled) in [
            (adapter.read_session(&session).unwrap(), false),
            (adapter.preview_session(&session).unwrap(), true),
        ] {
            let document = omnis_core::trajectory_search_document(&snapshot);
            assert_eq!(
                document.text, "Task\n\nSummary\n\nRetained answer",
                "{case}, sampled={sampled}"
            );
            assert_eq!(document.source_complete, !sampled);
        }
    }
}

#[test]
fn a_disconnected_preview_keeps_the_known_assistant_record_repeated_in_its_tail() {
    let root = tempfile::tempdir().unwrap();
    let id = Uuid::from_u128;
    let session = SessionRef::new(Provider::Claude, id(99).to_string());
    let source = root.path().join(format!("{}.jsonl", session.id));
    let mut file = BufWriter::new(File::create(source).unwrap());
    let assistant = json!({"type":"assistant", "uuid":id(2), "parentUuid":id(1), "message":{"content":"Repeated in the tail"}});
    let request =
        json!({"type":"user", "uuid":id(1), "parentUuid":null, "message":{"content":"Inspect"}});
    writeln!(file, "{request}").unwrap();
    writeln!(file, "{assistant}").unwrap();
    let padding = json!({"type":"progress", "padding":"x".repeat(64 * 1024)}).to_string();
    for _ in 0..208 {
        writeln!(file, "{padding}").unwrap();
    }
    writeln!(file, "{request}").unwrap();
    writeln!(file, "{assistant}").unwrap();
    file.flush().unwrap();

    let snapshot = ClaudeAdapter::with_root(root.path())
        .preview_session(&session)
        .unwrap();
    let text: Vec<_> = snapshot
        .events
        .iter()
        .filter_map(|e| e.payload["text"].as_str())
        .collect();
    assert_eq!(text, ["Inspect", "Repeated in the tail"]);
    assert!(omnis_core::import_trajectory(&snapshot).truncated);
}

#[test]
fn repeated_native_bytes_keep_physical_limits_and_omissions() {
    let root = tempfile::tempdir().unwrap();
    let session = SessionRef::new(Provider::Claude, Uuid::from_u128(99).to_string());
    let source = root.path().join(format!("{}.jsonl", session.id));
    let adapter = ClaudeAdapter::with_root(root.path());
    let prompt = json!({"type":"user","uuid":Uuid::from_u128(1),"parentUuid":null,"message":{"content":"First request"}});
    let padding = json!({"type":"progress","uuid":Uuid::from_u128(2),"parentUuid":Uuid::from_u128(1),"padding":"x".repeat(2 * 1024 * 1024)});
    let half = format!("{prompt}\n{padding}\n");
    let write = |tail: &str| std::fs::write(&source, format!("{half}{tail}").repeat(2)).unwrap();

    write("{\n");
    let snapshot = adapter.read_session(&session).unwrap();
    let notice = snapshot
        .events
        .iter()
        .find(|event| {
            event.source.raw_record_type.as_deref() == Some("omnisession.malformed_jsonl")
        })
        .unwrap();
    assert_eq!(notice.payload["omitted_events"], 2);

    let implicit = json!({"type":"user","message":{"content":"Legacy turn"}});
    write(&format!("{implicit}\n"));
    let snapshot = adapter.read_session(&session).unwrap();
    assert_eq!(
        snapshot
            .events
            .iter()
            .filter(|event| event.payload["text"] == "Legacy turn")
            .count(),
        2
    );

    write(&"{}\n".repeat(50_000));
    assert!(
        adapter
            .read_session(&session)
            .unwrap_err()
            .to_string()
            .contains("exceeds safe record limit")
    );
}

#[test]
fn reordered_copies_cannot_break_an_observed_chain() {
    let root = tempfile::tempdir().unwrap();
    let id = Uuid::from_u128;
    let session = SessionRef::new(Provider::Claude, id(99).to_string());
    let file = root.path().join(format!("{}.jsonl", session.id));
    let adapter = ClaudeAdapter::with_root(root.path());
    let chain = [
        json!({"type":"user","uuid":id(1),"parentUuid":null,"message":{"content":"A"}}),
        json!({"type":"assistant","uuid":id(2),"parentUuid":id(1),"message":{"content":"B"}}),
        json!({"type":"user","uuid":id(3),"parentUuid":id(2),"message":{"content":"C"}}),
    ];
    let complete = [0, 1, 2];
    let padding = json!({"type":"progress","padding":"x".repeat(64 * 1024)}).to_string();
    for (head, tail, padded) in [
        (&complete[..], &[0, 2, 1][..], false),
        (&complete, &[0, 2, 1], true),
        (&complete, &[2, 0, 1], false),
        (&complete, &[1], false),
        (&[0, 2], &complete, false),
        (&[2], &complete, false),
    ] {
        let mut writer = BufWriter::new(File::create(&file).unwrap());
        for (rows, pad_after) in [(head, padded), (tail, false)] {
            for &row in rows {
                writeln!(writer, "{}", chain[row]).unwrap();
            }
            if pad_after {
                for _ in 0..136 {
                    writeln!(writer, "{padding}").unwrap();
                }
            }
        }
        writer.flush().unwrap();
        for (snapshot, partial) in [
            (adapter.read_session(&session), false),
            (adapter.preview_session(&session), padded),
        ] {
            let snapshot = snapshot.unwrap();
            let text: Vec<_> = snapshot
                .events
                .iter()
                .filter_map(|e| e.payload["text"].as_str())
                .collect();
            assert_eq!(text, ["A", "B", "C"], "{head:?} + {tail:?}");
            assert_eq!(
                omnis_core::trajectory_search_document(&snapshot).source_complete,
                !partial
            );
        }
    }
}

#[test]
fn a_restored_summary_keeps_the_verified_issuer_when_tool_ids_repeat() {
    let root = tempfile::tempdir().unwrap();
    let id = Uuid::from_u128;
    let session = SessionRef::new(Provider::Claude, id(99).to_string());
    let source = root.path().join(format!("{}.jsonl", session.id));
    let message = |n, parent: Option<u128>, role: &str, content| {
        json!({
            "type":role,"uuid":id(n),"parentUuid":parent.map(id),"message":{"role":role,"content":content}
        })
    };
    let call =
        |key, input| json!({"type":"tool_use","id":key,"name":"Read","input":{"value":input}});
    let result = |key, text| json!([{"type":"tool_result","tool_use_id":key,"content":text}]);
    let mut rows = [
        message(1, None, "user", json!("First turn")),
        message(2, Some(1), "assistant", json!([call("x", "first")])),
        message(3, Some(2), "user", result("x", "First output")),
        message(4, Some(3), "user", json!("Second turn")),
        message(
            5,
            Some(4),
            "assistant",
            json!([call("x", "second"), call("y", "parallel")]),
        ),
        message(6, Some(5), "user", result("y", "Parallel output")),
        message(7, Some(6), "user", result("x", "Second output")),
        json!({"type":"system","subtype":"compact_boundary","uuid":id(8),"logicalParentUuid":id(7),
            "compactMetadata":{"preservedMessages":{"anchorUuid":id(9),"uuids":[id(4),id(5),id(6),id(7)]}}}),
        message(9, Some(8), "user", json!("Second turn summary")),
        message(10, Some(9), "assistant", json!("Done")),
    ];
    rows[8]["isCompactSummary"] = json!(true);
    for prefix in [&[][..], &rows[7..]] {
        let mut file = BufWriter::new(File::create(&source).unwrap());
        for row in prefix.iter().chain(&rows) {
            writeln!(file, "{row}").unwrap();
        }
        file.flush().unwrap();
        let snapshot = ClaudeAdapter::with_root(root.path())
            .read_session(&session)
            .unwrap();
        let imported = omnis_core::import_trajectory(&snapshot);
        assert!(!imported.truncated);
        let pairs: Vec<_> = omnis_core::native_trajectory_items(&imported)
            .into_iter()
            .filter_map(|item| {
                if let omnis_core::NativeTrajectoryItem::Tool { input, output, .. } = item {
                    Some((input["value"].clone(), output))
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(
            pairs,
            [
                (json!("second"), "Second output".to_owned()),
                (json!("parallel"), "Parallel output".to_owned())
            ]
        );
    }
}
