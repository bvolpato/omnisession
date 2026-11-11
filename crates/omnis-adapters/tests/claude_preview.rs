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
