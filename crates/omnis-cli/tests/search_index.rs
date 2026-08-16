use std::{fs, process::Command};

use chrono::{TimeZone, Utc};
use omnis_core::{SearchTruncationStrategy, trajectory_search_document};
use omnis_ir::{
    BundleManifest, CanonicalSnapshot, EventKind, EventSource, GitState, OmniEvent, PortableBundle,
    Provider, ReplayPolicy, SCHEMA_VERSION, Sensitivity, SessionRef, WorkspaceSnapshot,
};
use omnis_store::Store;
use serde_json::json;
use tempfile::tempdir;
use uuid::Uuid;

const VISIBLE_MARKER: &str = "visible trajectory marker";
const SECRET_VALUE: &str = "sk-proj-SYNTHETICSECRET0123456789";
const HIDDEN_REASONING_MARKER: &str = "hidden reasoning marker";
const HIDDEN_APPROVAL_MARKER: &str = "hidden approval marker";
const HIDDEN_PROVIDER_MARKER: &str = "hidden provider metadata marker";

#[test]
fn process_import_indexes_redacted_snapshot_then_store_reopens_for_search() {
    let snapshot = synthetic_snapshot();
    let document = trajectory_search_document(&snapshot);

    assert!(document.text.contains(VISIBLE_MARKER));
    assert!(document.text.contains("[REDACTED: API_KEY]"));
    assert!(!document.text.contains(SECRET_VALUE));
    assert!(!document.text.contains(HIDDEN_REASONING_MARKER));
    assert!(!document.text.contains(HIDDEN_APPROVAL_MARKER));
    assert!(!document.text.contains(HIDDEN_PROVIDER_MARKER));
    assert!(!document.truncated);
    assert_eq!(document.truncation_strategy, SearchTruncationStrategy::None);
    assert_eq!(document.indexed_byte_count, document.text.len());
    assert_eq!(document.source_byte_count, document.indexed_byte_count);

    let temporary_directory = tempdir().expect("temporary directory");
    let state_root = temporary_directory.path().join("state");
    let bundle_path = temporary_directory.path().join("synthetic-bundle.json");
    let bundle = PortableBundle {
        manifest: BundleManifest {
            schema_version: SCHEMA_VERSION.to_owned(),
            bundle_id: Uuid::from_u128(3),
            created_at: snapshot.captured_at,
            source: snapshot.session.clone(),
            event_count: snapshot.events.len(),
            redactions: Vec::new(),
        },
        snapshot: snapshot.clone(),
        fidelity: None,
    };
    fs::write(
        &bundle_path,
        serde_json::to_vec(&bundle).expect("encode synthetic bundle"),
    )
    .expect("write synthetic bundle");
    let output = Command::new(env!("CARGO_BIN_EXE_omni"))
        .args(["import", bundle_path.to_str().expect("bundle path")])
        .env("OMNISESSION_HOME", &state_root)
        .env("HOME", temporary_directory.path().join("home"))
        .env("OMNI_NO_UPDATE_CHECK", "1")
        .output()
        .expect("run CLI import");
    assert!(
        output.status.success(),
        "CLI import failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let store = Store::open(state_root.join("store.sqlite3")).expect("reopen CLI store");
    let matches = store
        .search_session_trajectory_matches(VISIBLE_MARKER, 10)
        .expect("search visible trajectory");
    assert_eq!(matches.len(), 1);
    let trajectory_match = &matches[0];
    assert_eq!(trajectory_match.session, snapshot.session);
    assert!(trajectory_match.snippet.contains(VISIBLE_MARKER));
    assert!(!trajectory_match.snippet.contains(SECRET_VALUE));
    assert!(!trajectory_match.snippet.contains(HIDDEN_REASONING_MARKER));
    assert!(!trajectory_match.snippet.contains(HIDDEN_APPROVAL_MARKER));
    assert!(!trajectory_match.snippet.contains(HIDDEN_PROVIDER_MARKER));
    assert!(trajectory_match.complete);
    assert!(trajectory_match.source_complete);
    assert_eq!(
        trajectory_match.indexed_byte_count,
        document.indexed_byte_count
    );
    assert_eq!(
        trajectory_match.source_byte_count,
        document.source_byte_count
    );
    assert_eq!(
        trajectory_match.truncation_strategy,
        document.truncation_strategy.as_str()
    );

    for excluded_marker in [
        SECRET_VALUE,
        HIDDEN_REASONING_MARKER,
        HIDDEN_APPROVAL_MARKER,
        HIDDEN_PROVIDER_MARKER,
    ] {
        assert!(
            store
                .search_session_trajectories(excluded_marker, 10)
                .expect("search excluded content")
                .is_empty(),
            "excluded marker was indexed: {excluded_marker}"
        );
    }
}

fn synthetic_snapshot() -> CanonicalSnapshot {
    let captured_at = Utc
        .with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
        .single()
        .expect("synthetic timestamp");
    CanonicalSnapshot {
        schema_version: SCHEMA_VERSION.to_owned(),
        session: SessionRef::new(Provider::Codex, "synthetic-search-session"),
        thread_id: Uuid::from_u128(1),
        branch_id: Uuid::from_u128(2),
        title: Some("Synthetic search fixture".to_owned()),
        captured_at,
        workspace: WorkspaceSnapshot {
            schema_version: SCHEMA_VERSION.to_owned(),
            captured_at,
            root: "synthetic/workspace".into(),
            current_dir: "synthetic/workspace".into(),
            git: GitState::default(),
            instruction_files: Vec::new(),
            environment_names: Vec::new(),
            available_tools: Vec::new(),
        },
        events: vec![
            event(
                0,
                EventKind::MessageUser,
                json!({
                    "text": format!(
                        "{VISIBLE_MARKER}; api_key={SECRET_VALUE}"
                    )
                }),
                Sensitivity::Normal,
                ReplayPolicy::Contextual,
            ),
            event(
                1,
                EventKind::MessageAssistant,
                json!({"text": "visible assistant response"}),
                Sensitivity::Normal,
                ReplayPolicy::Contextual,
            ),
            event(
                2,
                EventKind::ReasoningSummary,
                json!({"text": HIDDEN_REASONING_MARKER}),
                Sensitivity::Normal,
                ReplayPolicy::HistoricalOnly,
            ),
            event(
                3,
                EventKind::ApprovalRequested,
                json!({"command": HIDDEN_APPROVAL_MARKER}),
                Sensitivity::Normal,
                ReplayPolicy::HistoricalOnly,
            ),
            event(
                4,
                EventKind::ProviderEvent,
                json!({
                    "provider_metadata": HIDDEN_PROVIDER_MARKER,
                    "provider_secret": SECRET_VALUE,
                }),
                Sensitivity::Normal,
                ReplayPolicy::HistoricalOnly,
            ),
        ],
    }
}

fn event(
    sequence: u64,
    kind: EventKind,
    payload: serde_json::Value,
    sensitivity: Sensitivity,
    replay_policy: ReplayPolicy,
) -> OmniEvent {
    OmniEvent {
        schema_version: SCHEMA_VERSION.to_owned(),
        event_id: Uuid::from_u128(u128::from(sequence) + 10),
        thread_id: Uuid::from_u128(1),
        branch_id: Uuid::from_u128(2),
        sequence,
        timestamp: None,
        source: EventSource {
            provider: Provider::Codex,
            native_session_id: "synthetic-search-session".to_owned(),
            provider_version: Some("synthetic-provider-1.0".to_owned()),
            raw_record_type: Some("synthetic.provider.record".to_owned()),
        },
        kind,
        payload,
        raw_blob_hash: None,
        sensitivity,
        replay_policy,
    }
}
