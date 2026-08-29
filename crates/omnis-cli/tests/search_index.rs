use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use chrono::{TimeZone, Utc};
use omnis_core::{SearchTruncationStrategy, capture_workspace, trajectory_search_document};
use omnis_ir::{
    BundleManifest, CanonicalSnapshot, EventKind, EventSource, GitState, OmniEvent, PortableBundle,
    Provider, ReplayPolicy, SCHEMA_VERSION, Sensitivity, SessionRef, WorkspaceSnapshot,
};
use omnis_store::{SessionTrajectoryOrigin, Store};
use rusqlite::{Connection, params};
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
    let temporary_directory = tempdir().expect("temporary directory");
    let workspace = temporary_directory.path().join("workspace");
    fs::create_dir_all(&workspace).expect("create synthetic workspace");
    let mut snapshot = synthetic_snapshot();
    snapshot.workspace.root.clone_from(&workspace);
    snapshot.workspace.current_dir.clone_from(&workspace);
    let document = trajectory_search_document(&snapshot);
    assert_search_document(&document);

    let environment = TestEnvironment::new(temporary_directory.path(), workspace);
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
    assert_import_succeeds(&environment, &bundle_path, "initial import");
    assert_import_succeeds(&environment, &bundle_path, "identical repeated import");
    let imported = SessionRef::new(Provider::Imported, bundle.manifest.bundle_id.to_string());
    let store =
        Store::open(environment.state_root.join("store.sqlite3")).expect("reopen CLI store");
    assert_imported_search_index(&store, &imported, &document);
    assert_read_commands(
        &environment,
        &imported,
        &snapshot,
        temporary_directory.path(),
    );
    assert_task_and_resume(&environment, &imported);
    assert_missing_uuid(&environment);
    assert_uuid_collision_fails(&environment, bundle, temporary_directory.path());
}

#[test]
fn process_task_binding_maps_relocated_import_by_repository_fingerprint() {
    let temporary_directory = tempdir().expect("temporary directory");
    let workspace = temporary_directory.path().join("current-workspace");
    let other_workspace = temporary_directory.path().join("other-workspace");
    initialize_git_workspace(&workspace, "https://example.invalid/acme/project.git");
    initialize_git_workspace(
        &other_workspace,
        "https://example.invalid/acme/different.git",
    );
    let environment = TestEnvironment::new(temporary_directory.path(), workspace.clone());
    let mut snapshot = synthetic_snapshot();
    snapshot.workspace = capture_workspace(&workspace).expect("capture source repository");
    let missing_workspace = temporary_directory.path().join("relocated-source");
    snapshot.workspace.root.clone_from(&missing_workspace);
    snapshot
        .workspace
        .current_dir
        .clone_from(&missing_workspace);
    snapshot.workspace.git.worktree = Some(missing_workspace);
    let bundle = PortableBundle {
        manifest: BundleManifest {
            schema_version: SCHEMA_VERSION.to_owned(),
            bundle_id: Uuid::from_u128(12),
            created_at: snapshot.captured_at,
            source: snapshot.session.clone(),
            event_count: snapshot.events.len(),
            redactions: Vec::new(),
        },
        snapshot,
        fidelity: None,
    };
    let bundle_path = temporary_directory.path().join("relocated-bundle.json");
    fs::write(
        &bundle_path,
        serde_json::to_vec(&bundle).expect("encode relocated bundle"),
    )
    .expect("write relocated bundle");
    assert_import_succeeds(&environment, &bundle_path, "relocated import");
    let imported = format!("imported:{}", bundle.manifest.bundle_id);

    let matching = environment
        .command()
        .current_dir(&workspace)
        .args(["task", "start", "relocated", "--from", &imported])
        .output()
        .expect("bind relocated import to matching repository");
    assert!(
        matching.status.success(),
        "relocated binding failed: {}",
        String::from_utf8_lossy(&matching.stderr)
    );

    let mismatched = environment
        .command()
        .current_dir(other_workspace)
        .args(["task", "start", "wrong-repository", "--from", &imported])
        .output()
        .expect("reject relocated import in different repository");
    assert!(!mismatched.status.success());
    assert!(String::from_utf8_lossy(&mismatched.stderr).contains("belongs to"));
}

fn initialize_git_workspace(path: &Path, remote: &str) {
    fs::create_dir_all(path).expect("create Git workspace");
    let initialized = Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(path)
        .output()
        .expect("initialize Git workspace");
    assert!(initialized.status.success());
    let remote = Command::new("git")
        .args(["remote", "add", "origin", remote])
        .current_dir(path)
        .output()
        .expect("configure Git remote");
    assert!(remote.status.success());
}

#[test]
fn process_startup_migrates_legacy_bundle_without_exposing_mismatched_identity() {
    let temporary_directory = tempdir().expect("temporary directory");
    let workspace = temporary_directory.path().join("workspace");
    fs::create_dir_all(&workspace).expect("create synthetic workspace");
    let environment = TestEnvironment::new(temporary_directory.path(), workspace.clone());
    let mut snapshot = synthetic_snapshot();
    snapshot.workspace.root.clone_from(&workspace);
    snapshot.workspace.current_dir = workspace;
    snapshot.workspace.git.branch = Some(format!("feature/api_key={SECRET_VALUE}"));
    let (bundle, mismatched_id) = seed_legacy_store(&environment, &snapshot);

    let listed = environment
        .command()
        .args(["--json", "list", "--provider", "imported", "--all-projects"])
        .output()
        .expect("migrate legacy bundle during startup");
    assert!(
        listed.status.success(),
        "legacy migration failed: {}",
        String::from_utf8_lossy(&listed.stderr)
    );
    assert!(
        String::from_utf8_lossy(&listed.stderr)
            .contains("ignored malformed stored bundle `00000000-0000-0000-0000-00000000000b`")
    );
    let listed: serde_json::Value = serde_json::from_slice(&listed.stdout).expect("list JSON");
    let imported = SessionRef::new(Provider::Imported, bundle.manifest.bundle_id.to_string());
    assert_eq!(listed["sessions"][0]["session"], json!(imported));
    assert_eq!(listed["sessions"].as_array().map(Vec::len), Some(1));

    let store_path = environment.state_root.join("store.sqlite3");
    let reopened = Store::open(&store_path).expect("reopen migrated store");
    let page = reopened
        .search_session_trajectory_page_for_sessions(
            VISIBLE_MARKER,
            10,
            std::slice::from_ref(&imported),
        )
        .expect("search migrated imported source");
    assert_eq!(page.matches.len(), 1);
    assert_eq!(page.matches[0].session, imported);
    let imported_metadata = reopened
        .indexed_sessions_for_provider(Provider::Imported)
        .expect("read migrated metadata");
    assert!(
        !imported_metadata[0]
            .git_branch
            .as_deref()
            .unwrap_or_default()
            .contains(SECRET_VALUE)
    );
    drop(reopened);

    let mismatched = environment
        .command()
        .args(["show", &format!("imported:{mismatched_id}")])
        .output()
        .expect("reject mismatched stored identity");
    assert!(!mismatched.status.success());
    assert!(String::from_utf8_lossy(&mismatched.stderr).contains("mismatched identity"));
}

fn seed_legacy_store(
    environment: &TestEnvironment,
    snapshot: &CanonicalSnapshot,
) -> (PortableBundle, Uuid) {
    let bundle = PortableBundle {
        manifest: BundleManifest {
            schema_version: SCHEMA_VERSION.to_owned(),
            bundle_id: Uuid::from_u128(10),
            created_at: snapshot.captured_at,
            source: snapshot.session.clone(),
            event_count: snapshot.events.len(),
            redactions: Vec::new(),
        },
        snapshot: snapshot.clone(),
        fidelity: None,
    };
    let document = trajectory_search_document(snapshot);
    let store_path = environment.state_root.join("store.sqlite3");
    fs::create_dir_all(&environment.state_root).expect("create legacy state root");
    let store = Store::open(&store_path).expect("create legacy store");
    store
        .save_new_bundle(&bundle)
        .expect("store legacy bundle without durable locator");
    store
        .upsert_session_trajectory_document(
            &snapshot.session,
            &document.text,
            snapshot.captured_at,
            document.source_byte_count,
            document.indexed_byte_count,
            document.truncation_strategy.as_str(),
            document.source_complete,
            SessionTrajectoryOrigin::ImportedBundle,
        )
        .expect("store legacy source-routed trajectory");
    drop(store);

    let mismatched_id = Uuid::from_u128(11);
    let connection = Connection::open(&store_path).expect("open legacy database");
    connection
        .execute(
            "INSERT INTO bundles (bundle_id, bundle_json, saved_at) VALUES (?1, ?2, ?3)",
            params![
                mismatched_id.to_string(),
                serde_json::to_string(&bundle).expect("encode mismatched bundle"),
                snapshot.captured_at.timestamp_millis(),
            ],
        )
        .expect("insert mismatched stored identity");
    connection
        .execute(
            "INSERT INTO bundles (bundle_id, bundle_json, saved_at) VALUES (?1, ?2, ?3)",
            params!["not-a-uuid", "{}", snapshot.captured_at.timestamp_millis(),],
        )
        .expect("insert malformed bundle ID");
    (bundle, mismatched_id)
}

fn assert_search_document(document: &omnis_core::SearchDocument) {
    assert!(document.text.contains(VISIBLE_MARKER));
    assert!(document.text.contains("[REDACTED: API_KEY]"));
    for excluded in [
        SECRET_VALUE,
        HIDDEN_REASONING_MARKER,
        HIDDEN_APPROVAL_MARKER,
        HIDDEN_PROVIDER_MARKER,
    ] {
        assert!(!document.text.contains(excluded));
    }
    assert!(!document.truncated);
    assert_eq!(document.truncation_strategy, SearchTruncationStrategy::None);
    assert_eq!(document.indexed_byte_count, document.text.len());
    assert_eq!(document.source_byte_count, document.indexed_byte_count);
}

fn assert_import_succeeds(environment: &TestEnvironment, path: &Path, operation: &str) {
    let output = environment
        .command()
        .args(["import", path.to_str().expect("bundle path")])
        .output()
        .expect("run CLI import");
    assert!(
        output.status.success(),
        "{operation} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_imported_search_index(
    store: &Store,
    imported: &SessionRef,
    document: &omnis_core::SearchDocument,
) {
    let matches = store
        .search_session_trajectory_matches(VISIBLE_MARKER, 10)
        .expect("search visible trajectory");
    assert_eq!(matches.len(), 1);
    let trajectory_match = &matches[0];
    assert_eq!(&trajectory_match.session, imported);
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
    let indexed_imports = store
        .indexed_sessions_for_provider(Provider::Imported)
        .expect("read imported picker metadata");
    assert_eq!(&indexed_imports[0].session, imported);
    assert!(
        !indexed_imports[0]
            .title
            .as_deref()
            .unwrap_or_default()
            .contains(SECRET_VALUE)
    );
    assert!(
        !indexed_imports[0]
            .git_branch
            .as_deref()
            .unwrap_or_default()
            .contains(SECRET_VALUE)
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

fn assert_read_commands(
    environment: &TestEnvironment,
    imported: &SessionRef,
    snapshot: &CanonicalSnapshot,
    output_root: &Path,
) {
    let source = imported.to_string();
    let show = environment
        .command()
        .args(["--json", "show", &source])
        .output()
        .expect("show imported source in reopened process");
    assert!(
        show.status.success(),
        "show imported source failed: {}",
        String::from_utf8_lossy(&show.stderr)
    );
    let shown: serde_json::Value = serde_json::from_slice(&show.stdout).expect("show JSON");
    assert_eq!(shown["session"], json!(snapshot.session));
    assert!(!String::from_utf8_lossy(&show.stdout).contains(SECRET_VALUE));

    let verify = environment
        .command()
        .args(["--json", "verify", &source])
        .output()
        .expect("verify imported source in reopened process");
    assert!(
        verify.status.success(),
        "verify imported source failed: {}",
        String::from_utf8_lossy(&verify.stderr)
    );
    let verified: serde_json::Value = serde_json::from_slice(&verify.stdout).expect("verify JSON");
    assert_eq!(verified["session"], json!(imported));
    assert_eq!(verified["readable"], true);

    let listed = environment
        .command()
        .args(["--json", "list", "--provider", "imported", "--all-projects"])
        .output()
        .expect("list imported source in reopened process");
    assert!(
        listed.status.success(),
        "list imported source failed: {}",
        String::from_utf8_lossy(&listed.stderr)
    );
    let listed: serde_json::Value = serde_json::from_slice(&listed.stdout).expect("list JSON");
    assert_eq!(listed["sessions"][0]["session"], json!(imported));

    let inspected = environment
        .command()
        .args(["--json", "inspect", &source])
        .output()
        .expect("inspect imported source in reopened process");
    assert!(
        inspected.status.success(),
        "inspect imported source failed: {}",
        String::from_utf8_lossy(&inspected.stderr)
    );
    let inspected: serde_json::Value =
        serde_json::from_slice(&inspected.stdout).expect("inspect JSON");
    assert_eq!(inspected["source"], "codex");
    assert_eq!(inspected["target"], "codex");

    let exported_path = output_root.join("re-exported.json");
    let exported = environment
        .command()
        .args([
            "--json",
            "export",
            &source,
            "--output",
            exported_path.to_str().expect("exported path"),
        ])
        .output()
        .expect("export imported source in reopened process");
    assert!(
        exported.status.success(),
        "export imported source failed: {}",
        String::from_utf8_lossy(&exported.stderr)
    );
    let exported_bundle: PortableBundle =
        serde_json::from_slice(&fs::read(&exported_path).expect("read re-exported bundle"))
            .expect("parse re-exported bundle");
    assert_eq!(exported_bundle.manifest.source, snapshot.session);
    assert_eq!(exported_bundle.snapshot.session, snapshot.session);
}

fn assert_task_and_resume(environment: &TestEnvironment, imported: &SessionRef) {
    let source = imported.to_string();
    let task = environment
        .command()
        .current_dir(&environment.workspace)
        .args(["task", "start", "imported-source", "--from", &source])
        .output()
        .expect("bind imported source to task");
    assert!(
        task.status.success(),
        "bind imported source failed: {}",
        String::from_utf8_lossy(&task.stderr)
    );
    let task_status = environment
        .command()
        .current_dir(&environment.workspace)
        .args(["--json", "task", "status"])
        .output()
        .expect("read imported task binding");
    assert!(task_status.status.success());
    let task_status: serde_json::Value =
        serde_json::from_slice(&task_status.stdout).expect("task status JSON");
    assert_eq!(task_status["session"], json!(imported));

    assert!(!environment.home.join(".codex").exists());
    let resume = environment
        .command()
        .current_dir(&environment.workspace)
        .args(["--json", "resume", &source, "--dry-run"])
        .output()
        .expect("dry-run imported continuation in reopened process");
    assert!(
        resume.status.success(),
        "resume imported source failed: {}",
        String::from_utf8_lossy(&resume.stderr)
    );
    let resumed: serde_json::Value = serde_json::from_slice(&resume.stdout).expect("resume JSON");
    assert_eq!(resumed["source"], json!(imported));
    assert_eq!(resumed["target"], "codex");
    assert_eq!(resumed["dry_run"], true);
    let resumed_text = String::from_utf8_lossy(&resume.stdout);
    assert!(!resumed_text.contains(SECRET_VALUE));
    assert!(!resumed_text.contains(HIDDEN_REASONING_MARKER));
    assert!(!resumed_text.contains(HIDDEN_APPROVAL_MARKER));
}

fn assert_missing_uuid(environment: &TestEnvironment) {
    let missing_source = format!("imported:{}", Uuid::from_u128(4));
    let missing = environment
        .command()
        .args(["show", &missing_source])
        .output()
        .expect("read missing imported UUID");
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("was not found"));
}

fn assert_uuid_collision_fails(
    environment: &TestEnvironment,
    mut conflicting: PortableBundle,
    output_root: &Path,
) {
    conflicting.snapshot.title = Some("conflicting bundle".to_owned());
    let conflict_path = output_root.join("conflicting-bundle.json");
    fs::write(
        &conflict_path,
        serde_json::to_vec(&conflicting).expect("encode conflicting bundle"),
    )
    .expect("write conflicting bundle");
    let conflict = environment
        .command()
        .args(["import", conflict_path.to_str().expect("conflict path")])
        .output()
        .expect("run conflicting import");
    assert!(!conflict.status.success());
    assert!(
        String::from_utf8_lossy(&conflict.stderr).contains("already exists with different content")
    );
}

struct TestEnvironment {
    state_root: PathBuf,
    home: PathBuf,
    missing_codex: PathBuf,
    workspace: PathBuf,
}

impl TestEnvironment {
    fn new(root: &Path, workspace: PathBuf) -> Self {
        let home = root.join("home");
        let empty_path = root.join("empty-bin");
        fs::create_dir_all(&home).expect("create isolated home");
        fs::create_dir_all(&empty_path).expect("create empty provider directory");
        Self {
            state_root: root.join("state"),
            home,
            missing_codex: empty_path.join("missing-codex"),
            workspace,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_omni"));
        command
            .env("OMNISESSION_HOME", &self.state_root)
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("OMNI_CODEX_BIN", &self.missing_codex)
            .env("OMNI_NO_UPDATE_CHECK", "1");
        command
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
        title: Some(format!("Synthetic search fixture; api_key={SECRET_VALUE}")),
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
