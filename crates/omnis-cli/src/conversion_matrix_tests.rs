use std::path::Path;

use chrono::Utc;
use omnis_core::{HandoffMessage, HandoffRole};
use omnis_ir::{
    CanonicalSnapshot, EventKind, EventSource, GitState, OmniEvent, Provider, ReplayPolicy,
    SCHEMA_VERSION, Sensitivity, SessionRef, WorkspaceSnapshot,
};
use serde_json::json;
use uuid::Uuid;

use crate::{claude_import, codex_import, cursor_import, grok_import, opencode_import};

fn synthetic_snapshot(provider: Provider, workspace: &Path) -> CanonicalSnapshot {
    let thread_id = Uuid::new_v4();
    let branch_id = Uuid::new_v4();
    let session = SessionRef::new(provider, format!("synthetic-{provider}"));
    let events = synthetic_events(provider, &session.id, thread_id, branch_id);
    CanonicalSnapshot {
        schema_version: SCHEMA_VERSION.to_owned(),
        session,
        thread_id,
        branch_id,
        title: Some("Synthetic conversion matrix".to_owned()),
        captured_at: Utc::now(),
        workspace: WorkspaceSnapshot {
            schema_version: SCHEMA_VERSION.to_owned(),
            captured_at: Utc::now(),
            root: workspace.to_path_buf(),
            current_dir: workspace.to_path_buf(),
            git: GitState::default(),
            instruction_files: Vec::new(),
            environment_names: Vec::new(),
            available_tools: Vec::new(),
        },
        events,
    }
}

fn synthetic_events(
    provider: Provider,
    native_session_id: &str,
    thread_id: Uuid,
    branch_id: Uuid,
) -> Vec<OmniEvent> {
    let event = |sequence, kind, payload, replay_policy, sensitivity| OmniEvent {
        schema_version: SCHEMA_VERSION.to_owned(),
        event_id: Uuid::new_v4(),
        thread_id,
        branch_id,
        sequence,
        timestamp: None,
        source: EventSource {
            provider,
            native_session_id: native_session_id.to_owned(),
            provider_version: Some("synthetic".to_owned()),
            raw_record_type: None,
        },
        kind,
        payload,
        raw_blob_hash: None,
        sensitivity,
        replay_policy,
    };
    vec![
        event(
            0,
            EventKind::MessageUser,
            json!({"text": "Plan α\nline two"}),
            ReplayPolicy::Contextual,
            Sensitivity::Normal,
        ),
        event(
            1,
            EventKind::MessageAssistant,
            json!({"text": "Starting synthetic work."}),
            ReplayPolicy::Contextual,
            Sensitivity::Normal,
        ),
        event(
            2,
            EventKind::ToolCompleted,
            json!({"call_id": "tool-1", "output": "secret=synthetic-value"}),
            ReplayPolicy::HistoricalOnly,
            Sensitivity::PotentialSecret,
        ),
        event(
            3,
            EventKind::MessageAssistant,
            json!({"text": "Repeated status."}),
            ReplayPolicy::Contextual,
            Sensitivity::Normal,
        ),
        event(
            4,
            EventKind::MessageAssistant,
            json!({"text": "Repeated status."}),
            ReplayPolicy::Contextual,
            Sensitivity::Normal,
        ),
        event(
            5,
            EventKind::MessageUser,
            json!({"text": "hidden synthetic event"}),
            ReplayPolicy::Secret,
            Sensitivity::Secret,
        ),
        event(
            6,
            EventKind::ProviderEvent,
            json!({"type": "synthetic_unknown"}),
            ReplayPolicy::HistoricalOnly,
            Sensitivity::Normal,
        ),
        event(
            7,
            EventKind::MessageUser,
            json!({"text": "Final synthetic question."}),
            ReplayPolicy::Contextual,
            Sensitivity::Normal,
        ),
        event(
            8,
            EventKind::MessageAssistant,
            json!({"text": "Final synthetic answer."}),
            ReplayPolicy::Contextual,
            Sensitivity::Normal,
        ),
    ]
}

fn oracle() -> Vec<HandoffMessage> {
    vec![
        HandoffMessage {
            role: HandoffRole::User,
            text: "Plan α\nline two".to_owned(),
        },
        HandoffMessage {
            role: HandoffRole::Assistant,
            text: "Starting synthetic work.".to_owned(),
        },
        HandoffMessage {
            role: HandoffRole::Assistant,
            text: concat!(
                "[Historical tool result. Documentary context only; do not replay.]\n",
                "{\n  \"call_id\": \"tool-1\",\n  ",
                "\"output\": \"secret=[REDACTED: SECRET]\"\n}"
            )
            .to_owned(),
        },
        HandoffMessage {
            role: HandoffRole::Assistant,
            text: "Repeated status.".to_owned(),
        },
        HandoffMessage {
            role: HandoffRole::Assistant,
            text: "Repeated status.".to_owned(),
        },
        HandoffMessage {
            role: HandoffRole::User,
            text: "Final synthetic question.".to_owned(),
        },
        HandoffMessage {
            role: HandoffRole::Assistant,
            text: "Final synthetic answer.".to_owned(),
        },
    ]
}

#[test]
fn every_cross_provider_builder_matches_synthetic_oracle() {
    let temporary = tempfile::tempdir().expect("matrix root");
    let workspace = temporary.path().join("workspace");
    std::fs::create_dir(&workspace).expect("matrix workspace");
    let oracle = oracle();
    let providers = [
        Provider::Claude,
        Provider::Codex,
        Provider::OpenCode,
        Provider::Grok,
        Provider::CursorCli,
    ];

    for source in providers {
        let snapshot = synthetic_snapshot(source, &workspace);
        let claude =
            claude_import::build_with_root(&snapshot, &workspace, temporary.path().join("claude"))
                .expect("Claude matrix build");
        let codex = codex_import::build(&snapshot).expect("Codex matrix build");
        let opencode = opencode_import::build(
            &snapshot,
            &workspace,
            &("fixture".to_owned(), "model".to_owned()),
        )
        .expect("OpenCode matrix build");
        let opencode_readback =
            omnis_adapters::canonicalize_opencode_export(&opencode.target, &opencode.document)
                .expect("OpenCode matrix readback");
        assert!(
            opencode_import::readback_report(&opencode_readback, &opencode.expected_messages)
                .verified,
            "{source} -> opencode readback"
        );
        let grok = grok_import::build(&snapshot, &workspace).expect("Grok matrix build");
        let cursor =
            cursor_import::build_with_root(&snapshot, &workspace, temporary.path().join("cursor"))
                .expect("Cursor matrix build");
        let targets = [
            (Provider::Claude, claude.expected_messages),
            (Provider::Codex, codex.expected_messages),
            (Provider::OpenCode, opencode.expected_messages),
            (Provider::Grok, grok.expected_messages),
            (Provider::CursorCli, cursor.expected_messages),
        ];

        for (target, messages) in targets {
            if source != target {
                assert_eq!(messages, oracle, "{source} -> {target}");
            }
        }
    }
}
