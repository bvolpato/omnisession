use std::path::Path;

use chrono::Utc;
use omnis_adapters::{
    AntigravityAdapter, ClaudeAdapter, CursorCliAdapter, CursorIdeAdapter, HermesAdapter,
    PiAdapter, ProviderAdapter,
};
use omnis_core::{HandoffMessage, HandoffRole};
use omnis_ir::{
    CanonicalSnapshot, EventKind, EventSource, GitState, OmniEvent, Provider, ReplayPolicy,
    SCHEMA_VERSION, Sensitivity, SessionRef, WorkspaceSnapshot,
};
use serde_json::json;
use uuid::Uuid;

use crate::{
    antigravity_import, claude_import, codex_import, cursor_ide_import, cursor_import, grok_import,
    hermes_import, opencode_import, pi_import,
};

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
#[allow(clippy::too_many_lines)]
fn every_provider_pair_builder_matches_synthetic_oracle() {
    let temporary = tempfile::tempdir().expect("matrix root");
    let root = temporary
        .path()
        .canonicalize()
        .expect("canonical matrix root");
    let workspace = root.join("workspace");
    std::fs::create_dir(&workspace).expect("matrix workspace");
    let cursor_ide_root = root.join("cursor-ide/User");
    cursor_ide_import::create_fixture_store(&cursor_ide_root, &workspace)
        .expect("Cursor IDE matrix store");
    let antigravity_root = root.join("antigravity");
    antigravity_import::create_fixture_store(&antigravity_root).expect("Antigravity matrix store");
    let hermes_root = root.join("hermes");
    hermes_import::create_fixture_store(&hermes_root).expect("Hermes matrix store");
    let oracle = oracle();
    let providers = [
        Provider::Claude,
        Provider::Codex,
        Provider::OpenCode,
        Provider::Grok,
        Provider::Hermes,
        Provider::Antigravity,
        Provider::Pi,
        Provider::CursorCli,
        Provider::CursorIde,
    ];

    for source in providers {
        let snapshot = synthetic_snapshot(source, &workspace);
        if source == Provider::Hermes {
            rusqlite::Connection::open(hermes_root.join("state.db"))
                .expect("Hermes matrix database")
                .execute(
                    "INSERT INTO sessions (id, source, started_at, cwd) VALUES (?1, 'cli', 1, ?2)",
                    rusqlite::params![snapshot.session.id, workspace.to_string_lossy()],
                )
                .expect("Hermes matrix parent");
        }
        let claude = claude_import::build_with_lock_root(
            &snapshot,
            &workspace,
            root.join("claude"),
            root.join("omnisession/locks/claude"),
        )
        .expect("Claude matrix build");
        claude_import::materialize_records(&claude).expect("Claude matrix materialization");
        let claude_readback = ClaudeAdapter::with_root(root.join("claude"))
            .read_session(&claude.target)
            .expect("Claude matrix readback");
        assert!(
            claude_import::readback_matches(&claude_readback, &claude.expected_messages),
            "{source} -> claude native readback"
        );
        claude_import::rollback_records(&claude).expect("Claude matrix rollback");
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
        let hermes = hermes_import::build_with_root(&snapshot, &workspace, hermes_root.clone())
            .expect("Hermes matrix build");
        hermes_import::materialize_store(&hermes).expect("Hermes matrix materialization");
        let hermes_readback = HermesAdapter::with_root(&hermes_root)
            .read_session(&hermes.target)
            .expect("Hermes matrix readback");
        assert!(
            hermes_import::readback_matches(&hermes_readback, &hermes.expected_messages),
            "{source} -> hermes native readback"
        );
        hermes_import::rollback_store(&hermes).expect("Hermes matrix rollback");
        if source == Provider::Hermes {
            rusqlite::Connection::open(hermes_root.join("state.db"))
                .expect("Hermes matrix database")
                .execute("DELETE FROM sessions WHERE id = ?1", [&snapshot.session.id])
                .expect("Hermes matrix parent cleanup");
        }
        let cursor = cursor_import::build_with_root(&snapshot, &workspace, root.join("cursor"))
            .expect("Cursor matrix build");
        cursor_import::materialize_store(&cursor).expect("Cursor matrix materialization");
        let cursor_readback = CursorCliAdapter::with_root(root.join("cursor"))
            .read_session(&cursor.target)
            .expect("Cursor matrix readback");
        assert!(
            cursor_import::readback_matches(&cursor_readback, &cursor.expected_messages),
            "{source} -> cursor native readback"
        );
        cursor_import::rollback(&cursor).expect("Cursor matrix rollback");
        let pi = pi_import::build_with_root(&snapshot, &workspace, root.join("pi"))
            .expect("Pi matrix build");
        pi_import::materialize_records(&pi).expect("Pi matrix materialization");
        let pi_readback = PiAdapter::with_root(root.join("pi"))
            .read_session(&pi.target)
            .expect("Pi matrix readback");
        assert!(
            pi_import::readback_matches(&pi_readback, &pi.expected_messages),
            "{source} -> pi native readback"
        );
        pi_import::rollback(&pi).expect("Pi matrix rollback");
        let antigravity =
            antigravity_import::build_with_root(&snapshot, &workspace, antigravity_root.clone())
                .expect("Antigravity matrix build");
        antigravity_import::materialize_store(&antigravity)
            .expect("Antigravity matrix materialization");
        let antigravity_readback = AntigravityAdapter::with_root(&antigravity_root)
            .read_session(&antigravity.target)
            .expect("Antigravity matrix readback");
        assert!(
            antigravity_import::readback_matches(
                &antigravity_readback,
                &antigravity.expected_messages
            ),
            "{source} -> antigravity native readback"
        );
        antigravity_import::rollback_store(&antigravity).expect("Antigravity matrix rollback");
        let cursor_ide =
            cursor_ide_import::build_with_root(&snapshot, &workspace, cursor_ide_root.clone())
                .expect("Cursor IDE matrix build");
        cursor_ide_import::materialize_store(&cursor_ide)
            .expect("Cursor IDE matrix materialization");
        let cursor_ide_readback = CursorIdeAdapter::with_root(&cursor_ide_root)
            .read_session(&cursor_ide.target)
            .expect("Cursor IDE matrix readback");
        assert!(
            cursor_ide_import::readback_matches(
                &cursor_ide_readback,
                &cursor_ide.expected_messages
            ),
            "{source} -> cursor-ide native readback"
        );
        cursor_ide_import::rollback_store(&cursor_ide).expect("Cursor IDE matrix rollback");
        let targets = [
            (Provider::Claude, claude.expected_messages),
            (Provider::Codex, codex.expected_messages),
            (Provider::OpenCode, opencode.expected_messages),
            (Provider::Grok, grok.expected_messages),
            (Provider::Hermes, hermes.expected_messages),
            (Provider::Antigravity, antigravity.expected_messages),
            (Provider::CursorCli, cursor.expected_messages),
            (Provider::Pi, pi.expected_messages),
            (Provider::CursorIde, cursor_ide.expected_messages),
        ];

        for (target, messages) in targets {
            assert_eq!(messages, oracle, "{source} -> {target}");
        }
    }
}
