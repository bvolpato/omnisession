//! Synthetic path edge cases: non-ASCII names, spaces, and long nested workspaces.

use std::{
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
};

use omnis_adapters::{ClaudeAdapter, CodexAdapter, ProviderAdapter};
use omnis_ir::{CanonicalSnapshot, EventKind, Provider, SessionRef};
use serde_json::{Value, json};
use tempfile::TempDir;

struct Workspace {
    path: PathBuf,
    session: &'static str,
    question: &'static str,
}

/// Workspaces under a mixed-script parent with spaces. `проект` and `项目 раб` have the same
/// UTF-16 length, so Claude encodes both into one project directory.
fn workspaces(root: &Path) -> [Workspace; 3] {
    let parent = root.join("рабочие места 工作区");
    let long = (0..20).fold(parent.join("long nested"), |path, depth| {
        path.join(format!("深层 каталог {depth}"))
    });
    [
        Workspace {
            path: parent.join("проект"),
            session: "aaaaaaaa-0000-4000-8000-000000000001",
            question: "вопрос о проекте",
        },
        Workspace {
            path: parent.join("项目 раб"),
            session: "aaaaaaaa-0000-4000-8000-000000000002",
            question: "关于项目的问题",
        },
        Workspace {
            path: long,
            session: "aaaaaaaa-0000-4000-8000-000000000003",
            question: "深层 вопрос",
        },
    ]
}

/// Mirrors Claude Code's project directory name: each UTF-16 unit that is not ASCII
/// alphanumeric becomes `-`, and keys over 200 units keep a prefix plus a base36 hash.
/// Fixture names avoid decomposable characters, so NFC normalization is a no-op here.
fn claude_project_key(workspace: &Path) -> String {
    let units = workspace
        .to_str()
        .expect("UTF-8 workspace")
        .encode_utf16()
        .collect::<Vec<_>>();
    let mut key = units
        .iter()
        .map(|unit| match u8::try_from(*unit) {
            Ok(byte) if byte.is_ascii_alphanumeric() => char::from(byte),
            _ => '-',
        })
        .collect::<String>();
    if key.len() > 200 {
        let hash = units.iter().fold(0_i32, |hash, unit| {
            hash.wrapping_mul(31).wrapping_add(i32::from(*unit))
        });
        key.truncate(200);
        key.push('-');
        key.push_str(&base36(i64::from(hash).unsigned_abs()));
    }
    key
}

fn base36(mut value: u64) -> String {
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut encoded = Vec::new();
    loop {
        encoded.push(DIGITS[usize::try_from(value % 36).expect("base36 digit")]);
        value /= 36;
        if value == 0 {
            break;
        }
    }
    encoded.reverse();
    String::from_utf8(encoded).expect("ASCII base36")
}

fn jsonl(records: &[Value]) -> String {
    records.iter().fold(String::new(), |mut document, record| {
        document.push_str(&record.to_string());
        document.push('\n');
        document
    })
}

fn user_texts(snapshot: &CanonicalSnapshot) -> Vec<&str> {
    snapshot
        .events
        .iter()
        .filter(|event| event.kind == EventKind::MessageUser)
        .filter_map(|event| event.payload["text"].as_str())
        .collect()
}

#[test]
fn claude_transcripts_in_encoded_project_directories_list_and_read() {
    let temporary = TempDir::new().expect("temporary directory");
    let projects = temporary
        .path()
        .join("Пользователь 用户")
        .join(".claude")
        .join("projects");
    let workspaces = workspaces(temporary.path());
    assert_eq!(
        claude_project_key(&workspaces[0].path),
        claude_project_key(&workspaces[1].path),
        "fixture must place two workspaces in one encoded project directory"
    );
    assert!(claude_project_key(&workspaces[2].path).len() > 201);
    for (index, workspace) in workspaces.iter().enumerate() {
        fs::create_dir_all(&workspace.path).expect("workspace directory");
        let project = projects.join(claude_project_key(&workspace.path));
        fs::create_dir_all(&project).expect("encoded project directory");
        let transcript = jsonl(&[
            json!({
                "type": "user",
                "uuid": format!("11111111-1111-4111-8111-00000000000{index}"),
                "sessionId": workspace.session,
                "timestamp": "2026-01-01T00:00:00Z",
                "cwd": workspace.path,
                "message": {"role": "user", "content": workspace.question}
            }),
            json!({
                "type": "assistant",
                "uuid": format!("22222222-2222-4222-8222-00000000000{index}"),
                "sessionId": workspace.session,
                "timestamp": "2026-01-01T00:00:01Z",
                "cwd": workspace.path,
                "message": {"role": "assistant", "content": [{"type": "text", "text": "ответ 回答"}]}
            }),
        ]);
        fs::write(
            project.join(format!("{}.jsonl", workspace.session)),
            transcript,
        )
        .expect("synthetic transcript");
    }

    let adapter = ClaudeAdapter::with_root(&projects);
    let listed = adapter.list_sessions(None).expect("list Claude sessions");
    assert_eq!(listed.len(), workspaces.len());
    for workspace in &workspaces {
        let session = SessionRef::new(Provider::Claude, workspace.session);
        let native = listed
            .iter()
            .find(|native| native.session == session)
            .expect("listed Claude session");
        assert_eq!(
            native.project_path.as_deref(),
            Some(workspace.path.as_path())
        );
        let key = claude_project_key(&workspace.path);
        assert_eq!(
            native
                .source_path
                .as_deref()
                .and_then(Path::parent)
                .and_then(Path::file_name),
            Some(OsStr::new(&key))
        );

        let scoped = adapter
            .list_sessions(Some(&workspace.path))
            .expect("list Claude sessions for workspace");
        assert_eq!(
            scoped
                .iter()
                .map(|native| &native.session)
                .collect::<Vec<_>>(),
            [&session],
            "encoded project directory must not merge workspaces"
        );

        let snapshot = adapter
            .read_session(&session)
            .expect("read Claude transcript");
        assert_eq!(snapshot.workspace.current_dir, workspace.path);
        assert_eq!(user_texts(&snapshot), [workspace.question]);
    }
}

#[test]
fn codex_rollouts_under_non_ascii_home_list_and_read() {
    let temporary = TempDir::new().expect("temporary directory");
    let codex_home = temporary.path().join("Пользователь 用户").join(".codex");
    let day = codex_home
        .join("sessions")
        .join("2026")
        .join("01")
        .join("02");
    fs::create_dir_all(&day).expect("Codex session day");
    let workspaces = workspaces(temporary.path());
    for workspace in &workspaces {
        fs::create_dir_all(&workspace.path).expect("workspace directory");
        let rollout = jsonl(&[
            json!({
                "timestamp": "2026-01-02T00:00:00Z",
                "type": "session_meta",
                "payload": {"id": workspace.session, "cwd": workspace.path}
            }),
            json!({
                "timestamp": "2026-01-02T00:00:01Z",
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": workspace.question}]
                }
            }),
            json!({
                "timestamp": "2026-01-02T00:00:02Z",
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "ответ 回答"}]
                }
            }),
        ]);
        fs::write(
            day.join(format!(
                "rollout-2026-01-02T00-00-00-{}.jsonl",
                workspace.session
            )),
            rollout,
        )
        .expect("synthetic rollout");
    }

    let adapter = CodexAdapter::with_root(&codex_home);
    let listed = adapter.list_sessions(None).expect("list Codex sessions");
    assert_eq!(listed.len(), workspaces.len());
    for workspace in &workspaces {
        let session = SessionRef::new(Provider::Codex, workspace.session);
        let native = listed
            .iter()
            .find(|native| native.session == session)
            .expect("listed Codex session");
        assert_eq!(
            native.project_path.as_deref(),
            Some(workspace.path.as_path())
        );

        let scoped = adapter
            .list_sessions(Some(&workspace.path))
            .expect("list Codex sessions for workspace");
        assert_eq!(
            scoped
                .iter()
                .map(|native| &native.session)
                .collect::<Vec<_>>(),
            [&session]
        );

        let snapshot = adapter.read_session(&session).expect("read Codex rollout");
        assert_eq!(snapshot.workspace.current_dir, workspace.path);
        assert_eq!(user_texts(&snapshot), [workspace.question]);
    }
}
