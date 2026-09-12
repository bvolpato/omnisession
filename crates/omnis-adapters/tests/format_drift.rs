//! Version-tagged synthetic fixtures that mirror provider record layouts.
//!
//! Each fixture directory is a provider home. Content is invented; only record shapes are real.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use omnis_adapters::{ClaudeAdapter, CodexAdapter, NativeSession, ProviderAdapter};
use omnis_ir::{CanonicalSnapshot, EventKind, Provider, SessionRef};

const CODEX_CLI_ID: &str = "11111111-1111-4111-8111-111111111111";
const CODEX_EXEC_ID: &str = "22222222-2222-4222-8222-222222222222";
const CLAUDE_LEGACY_ID: &str = "10000000-0000-4000-8000-000000000001";
const CLAUDE_DRIFT_ID: &str = "21000000-0000-4000-8000-000000000001";
const CLAUDE_FOLLOWUP_ID: &str = "21000000-0000-4000-8000-000000000002";

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn listed(adapter: &dyn ProviderAdapter) -> HashMap<String, NativeSession> {
    let sessions = adapter.list_sessions(None).expect("fixture listing");
    assert!(
        adapter.discovery_notes().is_empty(),
        "{:?}",
        adapter.discovery_notes()
    );
    sessions
        .into_iter()
        .map(|session| (session.session.id.clone(), session))
        .collect()
}

fn sorted_ids(sessions: &HashMap<String, NativeSession>) -> Vec<&str> {
    let mut ids = sessions.keys().map(String::as_str).collect::<Vec<_>>();
    ids.sort_unstable();
    ids
}

fn kinds(snapshot: &CanonicalSnapshot) -> Vec<&EventKind> {
    snapshot.events.iter().map(|event| &event.kind).collect()
}

fn visible_text(snapshot: &CanonicalSnapshot) -> Vec<&str> {
    snapshot
        .events
        .iter()
        .filter(|event| {
            matches!(
                event.kind,
                EventKind::MessageUser | EventKind::MessageAssistant
            )
        })
        .filter_map(|event| event.payload["text"].as_str())
        .collect()
}

fn assert_absent(snapshot: &CanonicalSnapshot, fragments: &[&str]) {
    let rendered = serde_json::to_string(snapshot).expect("serialize snapshot");
    for fragment in fragments {
        assert!(!rendered.contains(fragment), "snapshot kept `{fragment}`");
    }
}

#[test]
fn codex_0_153_listing_hides_subagents_and_uses_the_newest_index_title() {
    let adapter = CodexAdapter::with_root(fixture("codex-0.153"));
    let sessions = listed(&adapter);

    // Guardian (`source.subagent.other`) and spawned workers (`source.subagent.thread_spawn`) stay hidden.
    assert_eq!(sorted_ids(&sessions), [CODEX_CLI_ID, CODEX_EXEC_ID]);
    assert_eq!(
        sessions[CODEX_CLI_ID].title.as_deref(),
        Some("Synthetic drift rollback renamed")
    );
    assert_eq!(
        sessions[CODEX_EXEC_ID].title.as_deref(),
        Some("Synthetic exec run")
    );
    for session in sessions.values() {
        assert_eq!(
            session.project_path.as_deref(),
            Some(Path::new("/workspace/drift"))
        );
        assert_eq!(session.git_branch.as_deref(), Some("drift-fixture"));
    }
}

#[test]
fn codex_0_153_rollback_after_compaction_keeps_only_surviving_turns() {
    let adapter = CodexAdapter::with_root(fixture("codex-0.153"));
    let session = SessionRef::new(Provider::Codex, CODEX_CLI_ID);

    for snapshot in [
        adapter.read_session(&session).expect("Codex read"),
        adapter.preview_session(&session).expect("Codex preview"),
    ] {
        assert_eq!(
            snapshot.title.as_deref(),
            Some("Synthetic drift rollback renamed")
        );
        assert_eq!(
            visible_text(&snapshot),
            [
                "<environment_context>synthetic environment</environment_context>",
                "First synthetic request",
                "First synthetic answer",
                "Replacement synthetic request",
                "Replacement synthetic answer",
            ]
        );
        assert_eq!(
            snapshot.workspace.current_dir,
            Path::new("/workspace/drift")
        );
        assert_eq!(
            snapshot
                .events
                .iter()
                .filter(|event| matches!(
                    event.kind,
                    EventKind::ToolCalled | EventKind::ToolCompleted
                ))
                .count(),
            2
        );
        assert_absent(
            &snapshot,
            &[
                "Discarded synthetic",
                "drift-discarded",
                "Compacted synthetic context",
                "synthetic-opaque",
            ],
        );
    }
}

#[test]
fn claude_1_0_legacy_summaries_title_sessions_and_sidechains_stay_hidden() {
    let adapter = ClaudeAdapter::with_root(fixture("claude-1.0/projects"));
    let sessions = listed(&adapter);

    assert_eq!(sorted_ids(&sessions), [CLAUDE_LEGACY_ID]);
    let legacy = &sessions[CLAUDE_LEGACY_ID];
    assert_eq!(
        legacy.title.as_deref(),
        Some("Synthetic legacy summary revised")
    );
    // Legacy history rows carry no session ID, so the workspace comes from the transcript head.
    assert_eq!(
        legacy.project_path.as_deref(),
        Some(Path::new("/workspace/legacy"))
    );
    assert_eq!(legacy.git_branch.as_deref(), Some("legacy-fixture"));

    let snapshot = adapter
        .read_session(&legacy.session)
        .expect("legacy Claude read");
    assert_eq!(
        snapshot.title.as_deref(),
        Some("Synthetic legacy summary revised")
    );
    assert_eq!(
        kinds(&snapshot),
        [
            &EventKind::MessageUser,
            &EventKind::ProviderEvent,
            &EventKind::ToolCalled,
            &EventKind::ToolCompleted,
            &EventKind::ProviderEvent,
            &EventKind::MessageAssistant,
        ]
    );
    assert_eq!(
        visible_text(&snapshot),
        ["Synthetic legacy request", "Synthetic legacy answer"]
    );
    // The Task tool input repeats the delegated prompt, so only the sidechain answer must vanish.
    assert_absent(&snapshot, &["sidechain-only"]);
}

#[test]
fn claude_2_1_ai_titles_history_prompts_and_subagent_logs() {
    let adapter = ClaudeAdapter::with_root(fixture("claude-2.1/projects"));
    let sessions = listed(&adapter);

    // Subagent logs under `<session>/subagents/` are not sessions.
    assert_eq!(sorted_ids(&sessions), [CLAUDE_DRIFT_ID, CLAUDE_FOLLOWUP_ID]);
    let drift = &sessions[CLAUDE_DRIFT_ID];
    assert_eq!(
        drift.title.as_deref(),
        Some("Synthetic drift investigation")
    );
    assert_eq!(
        drift.project_path.as_deref(),
        Some(Path::new("/workspace/drift"))
    );
    // Without a title record, discovery falls back to the first history prompt.
    let followup = &sessions[CLAUDE_FOLLOWUP_ID];
    assert_eq!(
        followup.title.as_deref(),
        Some("Synthetic follow-up prompt")
    );
    assert_eq!(
        followup.project_path.as_deref(),
        Some(Path::new("/workspace/drift-followup"))
    );

    let snapshot = adapter
        .read_session(&drift.session)
        .expect("current Claude read");
    assert_eq!(
        snapshot.title.as_deref(),
        Some("Synthetic drift investigation")
    );
    assert_eq!(
        kinds(&snapshot),
        [
            &EventKind::MessageUser,
            &EventKind::ProviderEvent,
            &EventKind::MessageAssistant,
            &EventKind::ToolCalled,
            &EventKind::ToolCompleted,
            &EventKind::ProviderEvent,
            &EventKind::MessageAssistant,
        ]
    );
    assert_eq!(
        visible_text(&snapshot),
        [
            "Synthetic drift request",
            "Synthetic drift answer",
            "Synthetic drift summary answer"
        ]
    );
    assert_absent(
        &snapshot,
        &["hidden synthetic reasoning", "subagent-only answer"],
    );
}
