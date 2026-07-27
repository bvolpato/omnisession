use std::{fs, path::Path};

use omnis_adapters::{
    AdapterRegistry, ClaudeAdapter, CodexAdapter, CursorCliAdapter, CursorIdeAdapter, GrokAdapter,
    LaunchTarget, OpenCodeAdapter, ProviderAdapter,
};
use omnis_ir::{EventKind, Provider, SessionRef};
use tempfile::TempDir;

const CLAUDE_ID: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
const CODEX_ID: &str = "44444444-4444-4444-8444-444444444444";
const CODEX_SUBAGENT_ID: &str = "55555555-5555-4555-8555-555555555555";

#[test]
fn claude_fixture_discovery_is_metadata_only_and_read_is_non_mutating() {
    let fixture = include_bytes!("fixtures/claude-session.jsonl");
    let temporary = TempDir::new().expect("temporary directory");
    let projects_root = temporary.path().join("projects");
    let project_directory = projects_root.join("encoded-project");
    fs::create_dir_all(&project_directory).expect("project fixture directory");
    fs::write(
        temporary.path().join("history.jsonl"),
        format!(
            "{{\"display\":\"not metadata\",\"project\":\"/workspace/demo\",\"sessionId\":\"{CLAUDE_ID}\",\"timestamp\":1767225600000}}\n"
        ),
    )
    .expect("Claude history index");
    let source = project_directory.join(format!("{CLAUDE_ID}.jsonl"));
    fs::write(&source, fixture).expect("Claude fixture");
    fs::write(project_directory.join("not-a-uuid.jsonl"), fixture).expect("invalid fixture");
    fs::write(
        project_directory.join("bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb.jsonl"),
        b"malformed only\n\n",
    )
    .expect("empty fixture");

    let adapter = ClaudeAdapter::with_root(&projects_root);
    let sessions = adapter
        .list_sessions(Some(Path::new("/workspace/demo")))
        .expect("Claude discovery");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].session.id, CLAUDE_ID);
    assert!(sessions[0].title.is_none());
    assert_eq!(sessions[0].event_count, 0);

    let before = fs::read(&source).expect("fixture before read");
    let snapshot = adapter
        .read_session(&SessionRef::new(Provider::Claude, CLAUDE_ID))
        .expect("Claude read");
    let after = fs::read(&source).expect("fixture after read");
    assert_eq!(before, after);
    assert_eq!(snapshot.events.len(), 4);
    let reread = adapter
        .read_session(&SessionRef::new(Provider::Claude, CLAUDE_ID))
        .expect("Claude reread");
    assert_eq!(
        snapshot
            .events
            .iter()
            .map(|event| event.event_id)
            .collect::<Vec<_>>(),
        reread
            .events
            .iter()
            .map(|event| event.event_id)
            .collect::<Vec<_>>()
    );
    assert!(
        snapshot
            .events
            .iter()
            .all(|event| !event.payload.to_string().contains("not visible"))
    );
}

#[test]
fn codex_fixture_uses_source_metadata_and_newest_index_title() {
    let temporary = TempDir::new().expect("temporary directory");
    let sessions = temporary.path().join("sessions/2026/01/02");
    fs::create_dir_all(&sessions).expect("Codex session fixture directory");
    fs::write(
        sessions.join(format!("rollout-2026-01-02T00-00-00-{CODEX_ID}.jsonl")),
        include_bytes!("fixtures/codex-session.jsonl"),
    )
    .expect("Codex fixture");
    fs::write(
        sessions.join(format!(
            "rollout-2026-01-02T00-00-01-{CODEX_SUBAGENT_ID}.jsonl"
        )),
        format!(
            "{{\"timestamp\":\"2026-01-02T00:00:01Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"{CODEX_SUBAGENT_ID}\",\"cwd\":\"/workspace/new\",\"agent_role\":\"fast_scan\"}}}}\n"
        ),
    )
    .expect("Codex subagent fixture");
    fs::write(
        temporary.path().join("session_index.jsonl"),
        format!(
            "{{\"id\":\"{CODEX_ID}\",\"thread_name\":\"Old title\",\"updated_at\":\"2026-01-01T00:00:00Z\"}}\n\
             {{\"id\":\"{CODEX_ID}\",\"thread_name\":\"New title\",\"updated_at\":\"2026-01-03T00:00:00Z\"}}\n"
        ),
    )
    .expect("Codex index fixture");

    let adapter = CodexAdapter::with_root(temporary.path());
    let discovered = adapter
        .list_sessions(Some(Path::new("/workspace/old")))
        .expect("Codex discovery");
    assert_eq!(discovered.len(), 1);
    assert_eq!(discovered[0].title.as_deref(), Some("New title"));
    assert_eq!(discovered[0].event_count, 0);

    let snapshot = adapter
        .read_session(&SessionRef::new(Provider::Codex, CODEX_ID))
        .expect("Codex read");
    assert_eq!(snapshot.workspace.current_dir, Path::new("/workspace/new"));
    assert_eq!(snapshot.events.len(), 3);
    assert_eq!(snapshot.events[0].kind, EventKind::MessageUser);
    assert_eq!(snapshot.events[1].kind, EventKind::MessageAssistant);
    assert_eq!(snapshot.events[2].kind, EventKind::ToolCompleted);
    let rendered = serde_json::to_string(&snapshot).expect("serialize snapshot");
    assert!(!rendered.contains("must be omitted"));
    assert!(!rendered.contains("sensitive synthetic output"));
}

#[test]
fn cursor_ide_reads_only_root_composer_headers_from_read_only_database() {
    let temporary = TempDir::new().expect("temporary directory");
    let workspace = temporary.path().join("workspace");
    let global_storage = temporary.path().join("globalStorage");
    let workspace_storage = temporary.path().join("workspaceStorage/ws-fixture");
    fs::create_dir_all(&workspace).expect("workspace fixture");
    fs::create_dir_all(&global_storage).expect("global storage fixture");
    fs::create_dir_all(&workspace_storage).expect("workspace storage fixture");
    fs::write(
        workspace_storage.join("workspace.json"),
        serde_json::json!({"folder": format!("file://{}", workspace.display())}).to_string(),
    )
    .expect("workspace mapping");
    let database = global_storage.join("state.vscdb");
    let connection = rusqlite::Connection::open(&database).expect("fixture database");
    connection
        .execute_batch(
            "CREATE TABLE composerHeaders (
                composerId TEXT PRIMARY KEY,
                workspaceId TEXT,
                createdAt INTEGER,
                lastUpdatedAt INTEGER,
                isArchived INTEGER,
                isSubagent INTEGER,
                value TEXT
             ); \
             CREATE TABLE ItemTable (key TEXT PRIMARY KEY, value BLOB); \
             CREATE TABLE cursorDiskKV (key TEXT PRIMARY KEY, value BLOB);",
        )
        .expect("fixture schema");
    let headers = serde_json::json!({
        "composerHeaders": [
            {
                "composerId": "subagent-composer",
                "parentComposerId": "root-composer",
                "workspacePath": workspace
            },
            {
                "composerId": "unscoped-composer",
                "name": "No workspace"
            }
        ]
    });
    connection
        .execute(
            "INSERT INTO composerHeaders(
                composerId, workspaceId, createdAt, lastUpdatedAt,
                isArchived, isSubagent, value
             ) VALUES (?1, ?2, ?3, ?4, 0, 0, ?5)",
            rusqlite::params![
                "root-composer",
                "ws-fixture",
                1_767_225_600_i64,
                1_767_225_601_i64,
                serde_json::json!({"name": "Root composer"}).to_string()
            ],
        )
        .expect("native composer header");
    connection
        .execute(
            "INSERT INTO ItemTable(key, value) VALUES (?1, ?2)",
            rusqlite::params!["composer.composerData", headers.to_string()],
        )
        .expect("composer headers");
    connection
        .execute(
            "INSERT INTO cursorDiskKV(key, value) VALUES (?1, ?2)",
            rusqlite::params!["composerData:root-composer", vec![0xff_u8; 64]],
        )
        .expect("opaque blob fixture");
    drop(connection);

    let adapter = CursorIdeAdapter::with_root(temporary.path());
    let sessions = adapter
        .list_sessions(Some(&workspace))
        .expect("Cursor IDE discovery");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].session.id, "root-composer");
    assert_eq!(sessions[0].title.as_deref(), Some("Root composer"));

    let snapshot = adapter
        .read_session(&SessionRef::new(Provider::CursorIde, "root-composer"))
        .expect("Cursor IDE metadata read");
    assert_eq!(snapshot.events.len(), 1);
    assert_eq!(snapshot.events[0].kind, EventKind::ProviderEvent);
    assert_eq!(snapshot.events[0].payload["storage"], "opaque");
}

#[test]
fn native_launch_arguments_match_provider_contracts() {
    let target = LaunchTarget::default();
    let claude = ClaudeAdapter::with_root("missing");
    let claude_plan = claude
        .launch_plan(&SessionRef::new(Provider::Claude, CLAUDE_ID), &target)
        .expect("Claude launch plan");
    assert_eq!(claude_plan.args, ["--resume", CLAUDE_ID, "--fork-session"]);

    let codex = CodexAdapter::with_root("missing");
    let codex_plan = codex
        .launch_plan(&SessionRef::new(Provider::Codex, CODEX_ID), &target)
        .expect("Codex launch plan");
    assert_eq!(codex_plan.args, ["fork", CODEX_ID]);

    let grok = GrokAdapter::with_root("missing");
    let grok_plan = grok
        .launch_plan(&SessionRef::new(Provider::Grok, "grok-id"), &target)
        .expect("Grok launch plan");
    assert_eq!(grok_plan.args, ["--resume", "grok-id", "--fork-session"]);

    let cursor = CursorCliAdapter::with_root("missing");
    assert!(
        cursor
            .launch_plan(&SessionRef::new(Provider::CursorCli, "cursor-id"), &target)
            .is_err()
    );
    let in_place_target = LaunchTarget {
        fork: false,
        ..LaunchTarget::default()
    };
    let cursor_plan = cursor
        .launch_plan(
            &SessionRef::new(Provider::CursorCli, "cursor-id"),
            &in_place_target,
        )
        .expect("Cursor launch plan");
    assert_eq!(cursor_plan.args, ["--resume", "cursor-id"]);

    let opencode_plan = OpenCodeAdapter
        .launch_plan(&SessionRef::new(Provider::OpenCode, "ses_fixture"), &target)
        .expect("OpenCode launch plan");
    assert_eq!(opencode_plan.args, ["--session", "ses_fixture", "--fork"]);
}

#[test]
fn registry_selects_all_local_provider_adapters() {
    let registry = AdapterRegistry::with_local_adapters();
    for provider in [
        Provider::Claude,
        Provider::Codex,
        Provider::OpenCode,
        Provider::Grok,
        Provider::CursorCli,
        Provider::CursorIde,
    ] {
        assert_eq!(
            registry
                .adapter(provider)
                .expect("registered adapter")
                .provider(),
            provider
        );
    }
}

#[test]
fn grok_discovery_uses_read_only_search_catalog() {
    let temporary = TempDir::new().expect("temporary directory");
    let database = temporary.path().join("session_search.sqlite");
    let connection = rusqlite::Connection::open(&database).expect("Grok search database");
    let _: String = connection
        .query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))
        .expect("WAL mode");
    connection
        .execute_batch(
            "CREATE TABLE session_docs (
                session_id TEXT PRIMARY KEY,
                cwd TEXT NOT NULL,
                updated_at INTEGER NOT NULL,
                title TEXT
            );",
        )
        .expect("Grok search schema");
    connection
        .execute(
            "INSERT INTO session_docs (session_id, cwd, updated_at, title)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![
                "77777777-7777-4777-8777-777777777777",
                "/workspace/demo",
                1_785_000_000_i64,
                "Catalog fixture"
            ],
        )
        .expect("Grok search fixture");
    let before = directory_contents(temporary.path());

    let adapter = GrokAdapter::with_root(temporary.path());
    let sessions = adapter
        .list_sessions(Some(Path::new("/workspace/demo")))
        .expect("Grok catalog discovery");

    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].title.as_deref(), Some("Catalog fixture"));
    assert_eq!(sessions[0].event_count, 0);
    assert_eq!(directory_contents(temporary.path()), before);
    drop(connection);
}

#[test]
fn grok_catalog_session_reopens_by_exact_id() {
    let temporary = TempDir::new().expect("temporary directory");
    let id = "77777777-7777-4777-8777-777777777777";
    let session = temporary.path().join("workspace-hash").join(id);
    fs::create_dir_all(&session).expect("Grok session directory");
    fs::write(
        session.join("summary.json"),
        serde_json::json!({"id": id, "cwd": "/workspace/demo"}).to_string(),
    )
    .expect("Grok summary");
    fs::write(
        session.join("updates.jsonl"),
        serde_json::json!({
            "params": {"update": {"sessionUpdate": "user_message", "content": "hello"}}
        })
        .to_string(),
    )
    .expect("Grok updates");

    let adapter = GrokAdapter::with_root(temporary.path());
    let snapshot = adapter
        .read_session(&SessionRef::new(Provider::Grok, id))
        .expect("Grok exact read");

    assert_eq!(snapshot.events.len(), 1);
    assert_eq!(snapshot.events[0].kind, EventKind::MessageUser);
}

fn directory_contents(root: &Path) -> Vec<(String, Vec<u8>)> {
    let mut files = fs::read_dir(root)
        .expect("fixture directory")
        .filter_map(std::result::Result::ok)
        .filter_map(|entry| {
            if !entry.file_type().ok()?.is_file() {
                return None;
            }
            Some((
                entry.file_name().to_string_lossy().into_owned(),
                fs::read(entry.path()).ok()?,
            ))
        })
        .collect::<Vec<_>>();
    files.sort_by(|left, right| left.0.cmp(&right.0));
    files
}
