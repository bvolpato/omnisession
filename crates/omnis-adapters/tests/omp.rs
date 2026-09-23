use std::{fs, path::Path};

use omnis_adapters::{LaunchTarget, PiAdapter, ProviderAdapter};
use omnis_ir::{EventKind, Provider, ReplayPolicy, SessionRef};
use serde_json::json;

#[test]
fn omp_title_slot_and_native_history_are_read_only_and_distinct_from_pi() {
    let temp = tempfile::tempdir().unwrap();
    let directory = temp.path().join("project");
    fs::create_dir(&directory).unwrap();
    let file = directory.join("synthetic_omp-session.jsonl");
    let records = [
        json!({"type":"title", "v":1, "title":"OMP fixture", "updatedAt":"2026-01-01T00:00:00Z", "pad":""}),
        json!({"type":"session", "version":3, "id":"omp-session", "cwd":"/workspace/demo", "timestamp":"2026-01-01T00:00:00Z"}),
        json!({"type":"message", "id":"a", "parentId":null, "message":{"role":"user", "content":"question 🦀"}}),
        json!({"type":"message", "id":"b", "parentId":"a", "message":{"role":"assistant", "content":[{"type":"thinking", "thinking":"private reasoning"},{"type":"text", "text":"answer"},{"type":"toolCall", "id":"call", "name":"bash", "arguments":{"command":"never execute"}}]}}),
        json!({"type":"message", "id":"c", "parentId":"b", "message":{"role":"toolResult", "toolCallId":"call", "toolName":"bash", "content":[{"type":"text", "text":"recorded output"}], "isError":false}}),
        json!({"type":"future_metadata", "id":"d", "parentId":"c", "instruction":"never execute"}),
    ];
    let document = records
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    fs::write(&file, &document).unwrap();
    let adapter = PiAdapter::oh_my_pi_with_root(temp.path());
    let session = SessionRef::new(Provider::OhMyPi, "omp-session");
    let discovered = adapter
        .list_sessions(Some(Path::new("/workspace/demo")))
        .unwrap();
    assert_eq!(discovered.len(), 1);
    assert_eq!(discovered[0].session, session);
    assert_eq!(discovered[0].title.as_deref(), Some("OMP fixture"));
    assert!(
        PiAdapter::with_root(temp.path())
            .list_sessions(None)
            .unwrap()
            .is_empty()
    );
    assert!(
        adapter
            .read_session(&SessionRef::new(Provider::Pi, "omp-session"))
            .is_err()
    );
    let snapshot = adapter.read_session(&session).unwrap();
    assert_eq!(snapshot.session.provider, Provider::OhMyPi);
    assert_eq!(snapshot.title.as_deref(), Some("OMP fixture"));
    assert_eq!(
        adapter.preview_session(&session).unwrap().events,
        snapshot.events
    );
    assert!(
        !serde_json::to_string(&snapshot)
            .unwrap()
            .contains("private reasoning")
    );
    let call = snapshot
        .events
        .iter()
        .find(|event| event.kind == EventKind::ToolCalled)
        .unwrap();
    assert_eq!(call.replay_policy, ReplayPolicy::HistoricalOnly);
    assert_eq!(call.source.provider, Provider::OhMyPi);
    for fork in [false, true] {
        let plan = adapter
            .launch_plan(
                &session,
                &LaunchTarget {
                    cwd: None,
                    fork,
                    prompt: None,
                },
            )
            .unwrap();
        assert_eq!(plan.program, "omp");
        assert_eq!(
            plan.args,
            [
                if fork { "--fork" } else { "--session" },
                file.to_str().unwrap()
            ]
        );
    }
    assert_eq!(fs::read_to_string(&file).unwrap(), document);
    fs::write(&file, document.replace("\"version\":3", "\"version\":4")).unwrap();
    assert!(adapter.list_sessions(None).unwrap().is_empty());
    assert!(adapter.read_session(&session).is_err());
}
