use std::{
    collections::BTreeMap,
    env, fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use chrono::Utc;
use directories::BaseDirs;
use md5::{Digest as Md5Digest, Md5};
use omnis_core::{HandoffMessage, HandoffRole, TrajectoryItemKind, import_trajectory};
use omnis_ir::{CanonicalSnapshot, Provider, SessionRef};
use prost::Message;
use rusqlite::{Connection, OpenFlags, params};
use serde_json::{Value, json};
use sha2::{Digest as Sha2Digest, Sha256};
use tempfile::NamedTempFile;
use uuid::Uuid;
use wait_timeout::ChildExt;

const SUPPORTED_CURSOR_VERSION: &str = "2026.07.23-e383d2b";
const SUPPORTED_INDEX_SHA256: &str =
    "99280b06fa6ab9f726e012f66f2d1ce349de7b37348f88913688cc6b69c57a35";
const SUPPORTED_STORE_CHUNK_SHA256: &str =
    "759cbc5c949092a65a203a10c0132c130d631705db4297a1dbe671c7f0459027";
const SUPPORTED_SESSION_CHUNK_SHA256: &str =
    "87309f6596d73c1c5ae1bae025a11538cf9dc4e4201fa1973c0dcaec4ea83fde";
const CURSOR_SCHEMA_VERSION: i64 = 1;
const CURSOR_IMPORT_TURN_LIMIT: usize = 1_024;

pub struct CursorImport {
    pub target: SessionRef,
    pub expected_messages: Vec<HandoffMessage>,
    pub history_items: usize,
    pub tool_events: usize,
    pub truncated: bool,
    blobs: BTreeMap<String, Vec<u8>>,
    database_metadata: String,
    sidecar: Vec<u8>,
    chats_root: PathBuf,
    workspace_dir: PathBuf,
    target_dir: PathBuf,
    cwd: PathBuf,
}

struct CursorGraph {
    blobs: BTreeMap<String, Vec<u8>>,
    latest_root: Vec<u8>,
}

pub fn build(snapshot: &CanonicalSnapshot, cwd: &Path) -> Result<CursorImport> {
    build_with_root(snapshot, cwd, cursor_chats_root()?)
}

pub(crate) fn build_with_root(
    snapshot: &CanonicalSnapshot,
    cwd: &Path,
    chats_root: PathBuf,
) -> Result<CursorImport> {
    if !cwd.is_absolute() {
        bail!("Cursor native import requires an absolute workspace path");
    }
    let cwd_text = cwd
        .to_str()
        .context("Cursor native import requires a UTF-8 workspace path")?;
    let trajectory = import_trajectory(snapshot);
    if trajectory.items.is_empty() {
        bail!("source has no visible trajectory eligible for Cursor import");
    }
    let history_items = trajectory.items.len();
    let source = snapshot.session.to_string();
    let expected_messages = trajectory
        .items
        .into_iter()
        .map(|item| HandoffMessage {
            role: match item.kind {
                TrajectoryItemKind::User => HandoffRole::User,
                TrajectoryItemKind::Assistant | TrajectoryItemKind::Tool => HandoffRole::Assistant,
            },
            text: item.text,
        })
        .collect::<Vec<_>>();

    let id = Uuid::new_v4().to_string();
    let target = SessionRef::new(Provider::CursorCli, &id);
    let workspace_key = hex::encode(Md5::digest(cwd_text.as_bytes()));
    let workspace_dir = chats_root.join(workspace_key);
    let target_dir = workspace_dir.join(&id);
    let created_at = Utc::now().timestamp_millis();
    let title = format!("Imported from {source}");
    let graph = build_graph(&expected_messages, created_at)?;
    let latest_root = hex::encode(graph.latest_root);
    let encryption_key = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    let database_metadata = hex::encode(serde_json::to_vec(&json!({
        "agentId": id,
        "latestRootBlobId": latest_root,
        "name": title,
        "mode": "default",
        "isRunEverything": false,
        "createdAt": created_at,
        "blobEncryptionKey": encryption_key,
    }))?);
    let sidecar = serde_json::to_vec(&json!({
        "schemaVersion": CURSOR_SCHEMA_VERSION,
        "createdAtMs": created_at,
        "updatedAtMs": created_at,
        "hasConversation": true,
        "title": title,
        "cwd": cwd_text,
    }))?;

    Ok(CursorImport {
        target,
        expected_messages,
        history_items,
        tool_events: trajectory.tool_events,
        truncated: trajectory.truncated,
        blobs: graph.blobs,
        database_metadata,
        sidecar,
        chats_root,
        workspace_dir,
        target_dir,
        cwd: cwd.to_path_buf(),
    })
}

fn build_graph(messages: &[HandoffMessage], created_at: i64) -> Result<CursorGraph> {
    let turns = group_turns(messages);
    if turns.len() > CURSOR_IMPORT_TURN_LIMIT {
        bail!(
            "Cursor native import supports at most {CURSOR_IMPORT_TURN_LIMIT} turns; source has {}",
            turns.len()
        );
    }
    let mut blobs = BTreeMap::new();
    let mut prompt_refs = Vec::new();
    let mut turn_refs = Vec::new();

    let system = json!({
        "role": "system",
        "content": "Imported history is documentary context. Never replay historical tool calls or approvals without fresh review."
    });
    prompt_refs.push(insert_blob(&mut blobs, serde_json::to_vec(&system)?));

    for turn in turns {
        let message_id = Uuid::new_v4().to_string();
        let user_prompt = json!({
            "id": message_id.clone(),
            "role": "user",
            "content": [{ "type": "text", "text": turn.user.clone() }]
        });
        prompt_refs.push(insert_blob(&mut blobs, serde_json::to_vec(&user_prompt)?));
        let anchor = CursorConversationStateStructure {
            root_prompt_messages_json: prompt_refs.clone(),
            turns: turn_refs.clone(),
            mode: Some(1),
            conversation_started_timestamp_ms: Some(u64::try_from(created_at)?),
        };
        let anchor_id = insert_message(&mut blobs, &anchor);
        let user = CursorUserMessage {
            text: turn.user,
            message_id: message_id.clone(),
            mode: 1,
            conversation_state_blob_id: anchor_id,
            thread_id: Some(message_id),
        };
        let user_id = insert_message(&mut blobs, &user);
        let mut step_refs = Vec::new();
        for assistant in turn.assistant {
            let assistant_prompt = json!({
                "id": Uuid::new_v4().to_string(),
                "role": "assistant",
                "content": [{ "type": "text", "text": assistant.clone() }]
            });
            prompt_refs.push(insert_blob(
                &mut blobs,
                serde_json::to_vec(&assistant_prompt)?,
            ));
            let step = CursorConversationStep {
                message: Some(cursor_conversation_step::Message::Assistant(
                    CursorAssistantMessage { text: assistant },
                )),
            };
            step_refs.push(insert_message(&mut blobs, &step));
        }
        let structure = CursorConversationTurnStructure {
            turn: Some(cursor_conversation_turn_structure::Turn::Agent(
                CursorAgentConversationTurnStructure {
                    user_message: user_id,
                    steps: step_refs,
                },
            )),
        };
        turn_refs.push(insert_message(&mut blobs, &structure));
    }
    let final_state = CursorConversationStateStructure {
        root_prompt_messages_json: prompt_refs,
        turns: turn_refs,
        mode: Some(1),
        conversation_started_timestamp_ms: Some(u64::try_from(created_at)?),
    };
    let latest_root = insert_message(&mut blobs, &final_state);
    Ok(CursorGraph { blobs, latest_root })
}

pub fn ensure_supported(binary: &Path) -> Result<String> {
    let version = installed_version(binary)?;
    if version != SUPPORTED_CURSOR_VERSION {
        bail!(
            "Cursor Agent {version} is not verified for native trajectory import; supported version: {SUPPORTED_CURSOR_VERSION}"
        );
    }
    let package = binary
        .parent()
        .context("Cursor Agent binary has no package directory")?;
    for (name, expected) in [
        ("index.js", SUPPORTED_INDEX_SHA256),
        ("8176.index.js", SUPPORTED_STORE_CHUNK_SHA256),
        ("1931.index.js", SUPPORTED_SESSION_CHUNK_SHA256),
    ] {
        let path = package.join(name);
        let bytes = fs::read(&path)
            .with_context(|| format!("reading Cursor Agent bundle `{}`", path.display()))?;
        let actual = hex::encode(Sha256::digest(bytes));
        if actual != expected {
            bail!("Cursor Agent {version} bundle fingerprint is not verified");
        }
    }
    Ok(version)
}

pub fn materialize(import: &CursorImport, binary: &Path) -> Result<()> {
    ensure_supported(binary)?;
    materialize_store(import)
}

fn materialize_store(import: &CursorImport) -> Result<()> {
    ensure_directory(&import.chats_root)?;
    ensure_directory(&import.workspace_dir)?;
    validate_directory_chain(&import.workspace_dir, &import.chats_root, "writing")?;
    verify_workspace_identity(&import.workspace_dir, &import.cwd)?;
    if import.target_dir.exists() {
        bail!("generated Cursor target session already exists");
    }

    let staging = tempfile::Builder::new()
        .prefix(".omnisession-cursor-")
        .tempdir_in(&import.workspace_dir)
        .context("creating Cursor import staging directory")?;
    secure_directory(staging.path())?;
    let database = staging.path().join("store.db");
    write_database(import, &database)?;
    let sidecar = staging.path().join("meta.json");
    write_private_file(&sidecar, &import.sidecar)?;
    sync_directory(staging.path()).context("syncing Cursor import staging directory")?;

    create_private_directory(&import.target_dir)?;
    let publish = (|| -> Result<()> {
        fs::hard_link(&database, import.target_dir.join("store.db"))
            .context("publishing Cursor session database")?;
        fs::hard_link(&sidecar, import.target_dir.join("meta.json"))
            .context("publishing Cursor session metadata")?;
        sync_directory(&import.target_dir).context("syncing Cursor target session directory")?;
        sync_directory(&import.workspace_dir).context("syncing Cursor workspace session directory")
    })();
    if let Err(error) = publish {
        return Err(combine_rollback_error(
            error,
            rollback_partial(import),
            "publishing Cursor import",
        ));
    }
    Ok(())
}

pub fn rollback(import: &CursorImport) -> Result<()> {
    validate_generated(import, true)?;
    remove_generated_directory(import)
}

pub fn readback_matches(snapshot: &CanonicalSnapshot, expected: &[HandoffMessage]) -> bool {
    let trajectory = import_trajectory(snapshot);
    let actual = trajectory
        .items
        .into_iter()
        .map(|item| HandoffMessage {
            role: match item.kind {
                TrajectoryItemKind::User => HandoffRole::User,
                TrajectoryItemKind::Assistant | TrajectoryItemKind::Tool => HandoffRole::Assistant,
            },
            text: item.text,
        })
        .collect::<Vec<_>>();
    !trajectory.truncated && actual == expected
}

fn write_database(import: &CursorImport, path: &Path) -> Result<()> {
    let mut connection = Connection::open(path).context("creating Cursor session database")?;
    connection.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = FULL;
         PRAGMA busy_timeout = 5000;
         PRAGMA user_version = 1;
         CREATE TABLE blobs (id TEXT PRIMARY KEY, data BLOB);
         CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT);",
    )?;
    let transaction = connection.transaction()?;
    {
        let mut insert = transaction.prepare("INSERT INTO blobs (id, data) VALUES (?1, ?2)")?;
        for (id, data) in &import.blobs {
            insert.execute(params![id, data])?;
        }
    }
    transaction.execute(
        "INSERT INTO meta (key, value) VALUES ('0', ?1)",
        [&import.database_metadata],
    )?;
    transaction.commit()?;
    connection.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
    connection
        .close()
        .map_err(|(_, error)| error)
        .context("closing Cursor session database")?;
    set_private_file_permissions(path)?;
    fs::OpenOptions::new()
        .write(true)
        .open(path)
        .and_then(|file| file.sync_all())
        .context("syncing Cursor session database")
}

fn validate_generated(import: &CursorImport, require_complete: bool) -> Result<()> {
    validate_directory_chain(&import.target_dir, &import.chats_root, "rolling back")?;
    if import.target.provider != Provider::CursorCli
        || Uuid::parse_str(&import.target.id).is_err()
        || import.target_dir.file_name().and_then(|name| name.to_str())
            != Some(import.target.id.as_str())
        || import.target_dir.parent() != Some(import.workspace_dir.as_path())
        || !import.target_dir.starts_with(&import.chats_root)
    {
        bail!("refusing to remove unverified Cursor target path");
    }
    let mut files = fs::read_dir(&import.target_dir)
        .context("reading generated Cursor target directory")?
        .map(|entry| {
            entry
                .map(|entry| entry.file_name())
                .context("reading generated Cursor target entry")
        })
        .collect::<Result<Vec<_>>>()?;
    files.sort();
    let expected = if require_complete {
        vec!["meta.json".into(), "store.db".into()]
    } else {
        files.clone()
    };
    if files != expected
        || files
            .iter()
            .any(|name| name != "meta.json" && name != "store.db")
    {
        bail!("generated Cursor target contains foreign files");
    }
    let sidecar = import.target_dir.join("meta.json");
    if sidecar.exists() && fs::read(&sidecar)? != import.sidecar {
        bail!("generated Cursor target metadata changed after materialization");
    }
    let database = import.target_dir.join("store.db");
    if database.exists() {
        validate_database(import, &database)?;
    } else if require_complete {
        bail!("generated Cursor target database is missing");
    }
    Ok(())
}

fn validate_database(import: &CursorImport, path: &Path) -> Result<()> {
    let snapshot_directory = tempfile::tempdir().context("creating Cursor validation snapshot")?;
    let snapshot = snapshot_directory.path().join("store.db");
    fs::copy(path, &snapshot).context("copying Cursor database for validation")?;
    let connection = Connection::open_with_flags(
        snapshot,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    connection.pragma_update(None, "query_only", "ON")?;
    let integrity =
        connection.query_row("PRAGMA quick_check(1)", [], |row| row.get::<_, String>(0))?;
    if integrity != "ok" {
        bail!("generated Cursor target failed SQLite integrity check");
    }
    let version = connection.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))?;
    if version != CURSOR_SCHEMA_VERSION {
        bail!("generated Cursor target schema version changed");
    }
    let metadata: String =
        connection.query_row("SELECT value FROM meta WHERE key = '0'", [], |row| {
            row.get(0)
        })?;
    if metadata != import.database_metadata {
        bail!("generated Cursor target root metadata changed");
    }
    let meta_rows =
        connection.query_row("SELECT COUNT(*) FROM meta", [], |row| row.get::<_, i64>(0))?;
    if meta_rows != 1 {
        bail!("generated Cursor target contains foreign metadata");
    }
    let mut statement = connection.prepare("SELECT id, data FROM blobs ORDER BY id")?;
    let actual = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
        })?
        .collect::<rusqlite::Result<BTreeMap<_, _>>>()?;
    if actual != import.blobs {
        bail!("generated Cursor target graph changed after materialization");
    }
    Ok(())
}

fn rollback_partial(import: &CursorImport) -> Result<()> {
    validate_generated(import, false)?;
    remove_generated_directory(import)
}

fn remove_generated_directory(import: &CursorImport) -> Result<()> {
    for name in ["meta.json", "store.db"] {
        let path = import.target_dir.join(name);
        if path.exists() {
            fs::remove_file(&path)
                .with_context(|| format!("removing generated Cursor file `{name}`"))?;
        }
    }
    fs::remove_dir(&import.target_dir).context("removing generated Cursor target directory")?;
    sync_directory(&import.workspace_dir)
        .context("syncing Cursor workspace directory after rollback")
}

fn verify_workspace_identity(workspace_dir: &Path, cwd: &Path) -> Result<()> {
    for entry in fs::read_dir(workspace_dir).context("reading Cursor workspace directory")? {
        let entry = entry.context("reading Cursor workspace entry")?;
        let file_type = entry.file_type().context("reading Cursor entry type")?;
        if file_type.is_symlink() || !file_type.is_dir() {
            continue;
        }
        let metadata = entry.path().join("meta.json");
        if !metadata.is_file() {
            continue;
        }
        let value: Value = serde_json::from_reader(
            fs::File::open(&metadata).context("reading Cursor workspace identity")?,
        )
        .context("parsing Cursor workspace identity")?;
        let Some(recorded) = value.get("cwd").and_then(Value::as_str) else {
            continue;
        };
        if Path::new(recorded) != cwd {
            bail!(
                "Cursor workspace key collision: existing session records `{recorded}`, target is `{}`",
                cwd.display()
            );
        }
    }
    Ok(())
}

fn cursor_chats_root() -> Result<PathBuf> {
    if let Some(root) = env::var_os("CURSOR_AGENT_HOME").filter(|value| !value.is_empty()) {
        let root = PathBuf::from(root);
        if !root.is_absolute() {
            bail!("CURSOR_AGENT_HOME must be an absolute chats path");
        }
        return Ok(root);
    }
    if let Some(root) = env::var_os("CURSOR_CONFIG_DIR").filter(|value| !value.is_empty()) {
        let root = PathBuf::from(root);
        if !root.is_absolute() {
            bail!("CURSOR_CONFIG_DIR must be an absolute path");
        }
        return Ok(root.join("chats"));
    }
    if let Some(root) = env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty()) {
        let root = PathBuf::from(root);
        if !root.is_absolute() {
            bail!("XDG_CONFIG_HOME must be an absolute path");
        }
        return Ok(root.join("cursor").join("chats"));
    }
    BaseDirs::new()
        .map(|directories| directories.home_dir().join(".cursor").join("chats"))
        .context("home directory is unavailable")
}

fn group_turns(messages: &[HandoffMessage]) -> Vec<CursorTurn> {
    let mut turns = Vec::new();
    let mut current: Option<CursorTurn> = None;
    for message in messages {
        match message.role {
            HandoffRole::User => {
                if let Some(turn) = current.take() {
                    turns.push(turn);
                }
                current = Some(CursorTurn {
                    user: message.text.clone(),
                    assistant: Vec::new(),
                });
            }
            HandoffRole::Assistant => {
                if let Some(turn) = &mut current {
                    turn.assistant.push(message.text.clone());
                }
            }
        }
    }
    if let Some(turn) = current {
        turns.push(turn);
    }
    turns
}

struct CursorTurn {
    user: String,
    assistant: Vec<String>,
}

fn insert_message(
    message_store: &mut BTreeMap<String, Vec<u8>>,
    message: &impl Message,
) -> Vec<u8> {
    insert_blob(message_store, message.encode_to_vec())
}

fn insert_blob(message_store: &mut BTreeMap<String, Vec<u8>>, data: Vec<u8>) -> Vec<u8> {
    let id = Sha256::digest(&data).to_vec();
    message_store.insert(hex::encode(&id), data);
    id
}

fn write_private_file(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = path.parent().context("private file has no parent")?;
    let mut temporary = NamedTempFile::new_in(parent).context("creating private temporary file")?;
    set_private_file_permissions(temporary.path())?;
    temporary.write_all(contents)?;
    temporary.flush()?;
    temporary.as_file().sync_all()?;
    temporary
        .persist_noclobber(path)
        .map_err(|error| error.error)
        .context("publishing private file")?;
    Ok(())
}

fn ensure_directory(path: &Path) -> Result<()> {
    if path.exists() {
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            bail!("`{}` is not a safe directory", path.display());
        }
        return Ok(());
    }
    let parent = path
        .parent()
        .context("Cursor target directory has no parent")?;
    ensure_directory(parent)?;
    create_private_directory(path)
}

fn create_private_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = fs::DirBuilder::new();
        builder.mode(0o700);
        builder
            .create(path)
            .with_context(|| format!("creating `{}`", path.display()))
    }
    #[cfg(not(unix))]
    fs::create_dir(path).with_context(|| format!("creating `{}`", path.display()))
}

#[cfg(unix)]
fn secure_directory(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn secure_directory(path: &Path) -> Result<()> {
    if !path.is_dir() {
        bail!("`{}` is not a directory", path.display());
    }
    Ok(())
}

#[cfg(unix)]
fn set_private_file_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private_file_permissions(path: &Path) -> Result<()> {
    if !path.is_file() {
        bail!("`{}` is not a file", path.display());
    }
    Ok(())
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<()> {
    fs::File::open(path)?.sync_all().map_err(Into::into)
}

#[cfg(not(unix))]
fn sync_directory(path: &Path) -> Result<()> {
    if !path.is_dir() {
        bail!("`{}` is not a directory", path.display());
    }
    Ok(())
}

fn validate_directory_chain(path: &Path, root: &Path, operation: &str) -> Result<()> {
    if !path.starts_with(root) {
        bail!("refusing {operation} outside Cursor chats directory");
    }
    let mut directory = path;
    loop {
        let metadata = fs::symlink_metadata(directory)
            .with_context(|| format!("reading `{}`", directory.display()))?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            bail!(
                "refusing {operation} through unsafe directory `{}`",
                directory.display()
            );
        }
        if directory == root {
            break;
        }
        directory = directory
            .parent()
            .context("Cursor target path escaped chats directory")?;
    }
    Ok(())
}

fn combine_rollback_error(
    error: anyhow::Error,
    rollback: Result<()>,
    action: &str,
) -> anyhow::Error {
    match rollback {
        Ok(()) => error,
        Err(rollback_error) => error.context(format!(
            "{action} failed and rollback also failed: {rollback_error}"
        )),
    }
}

fn installed_version(binary: &Path) -> Result<String> {
    let mut child = Command::new(binary)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("executing `{}`", binary.display()))?;
    let status = child
        .wait_timeout(Duration::from_secs(5))
        .context("waiting for Cursor Agent version")?;
    let Some(status) = status else {
        child
            .kill()
            .context("stopping Cursor Agent version probe")?;
        let _ = child.wait();
        bail!("Cursor Agent version probe timed out");
    };
    let output = child
        .wait_with_output()
        .context("reading Cursor Agent version")?;
    if !status.success() {
        bail!("Cursor Agent version probe exited with status {status}");
    }
    let stdout = String::from_utf8(output.stdout).context("Cursor Agent version was not UTF-8")?;
    parse_version(&stdout).context("Cursor Agent returned an unrecognized version")
}

fn parse_version(output: &str) -> Option<String> {
    output
        .split_whitespace()
        .find(|part| {
            let Some((date, build)) = part.split_once('-') else {
                return false;
            };
            date.split('.').count() == 3
                && date
                    .split('.')
                    .all(|component| component.parse::<u64>().is_ok())
                && build.len() >= 7
                && build.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
        .map(str::to_owned)
}

#[derive(Clone, PartialEq, Message)]
struct CursorConversationStateStructure {
    #[prost(bytes = "vec", repeated, tag = "1")]
    root_prompt_messages_json: Vec<Vec<u8>>,
    #[prost(bytes = "vec", repeated, tag = "8")]
    turns: Vec<Vec<u8>>,
    #[prost(int32, optional, tag = "10")]
    mode: Option<i32>,
    #[prost(uint64, optional, tag = "26")]
    conversation_started_timestamp_ms: Option<u64>,
}

#[derive(Clone, PartialEq, Message)]
struct CursorConversationTurnStructure {
    #[prost(oneof = "cursor_conversation_turn_structure::Turn", tags = "1")]
    turn: Option<cursor_conversation_turn_structure::Turn>,
}

mod cursor_conversation_turn_structure {
    use prost::Oneof;

    #[derive(Clone, PartialEq, Oneof)]
    pub(super) enum Turn {
        #[prost(message, tag = "1")]
        Agent(super::CursorAgentConversationTurnStructure),
    }
}

#[derive(Clone, PartialEq, Message)]
struct CursorAgentConversationTurnStructure {
    #[prost(bytes = "vec", tag = "1")]
    user_message: Vec<u8>,
    #[prost(bytes = "vec", repeated, tag = "2")]
    steps: Vec<Vec<u8>>,
}

#[derive(Clone, PartialEq, Message)]
struct CursorUserMessage {
    #[prost(string, tag = "1")]
    text: String,
    #[prost(string, tag = "2")]
    message_id: String,
    #[prost(int32, tag = "4")]
    mode: i32,
    #[prost(bytes = "vec", tag = "10")]
    conversation_state_blob_id: Vec<u8>,
    #[prost(string, optional, tag = "17")]
    thread_id: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
struct CursorConversationStep {
    #[prost(oneof = "cursor_conversation_step::Message", tags = "1")]
    message: Option<cursor_conversation_step::Message>,
}

mod cursor_conversation_step {
    use prost::Oneof;

    #[derive(Clone, PartialEq, Oneof)]
    pub(super) enum Message {
        #[prost(message, tag = "1")]
        Assistant(super::CursorAssistantMessage),
    }
}

#[derive(Clone, PartialEq, Message)]
struct CursorAssistantMessage {
    #[prost(string, tag = "1")]
    text: String,
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use chrono::Utc;
    use omnis_adapters::{CursorCliAdapter, ProviderAdapter};
    use omnis_ir::{
        EventKind, EventSource, GitState, OmniEvent, ReplayPolicy, SCHEMA_VERSION, Sensitivity,
        WorkspaceSnapshot,
    };

    use super::*;

    #[test]
    fn native_store_round_trip_and_exact_rollback() {
        let temporary = tempfile::tempdir().expect("temporary Cursor root");
        let workspace = temporary.path().join("workspace");
        fs::create_dir(&workspace).expect("workspace");
        let snapshot = fixture_snapshot(&workspace);
        let chats = temporary.path().join("cursor/chats");
        let import = build_with_root(&snapshot, &workspace, chats.clone()).expect("build import");

        materialize_store(&import).expect("materialize Cursor graph");
        let adapter = CursorCliAdapter::with_root(chats);
        let readback = adapter
            .read_session(&import.target)
            .expect("independent Cursor readback");
        assert!(readback_matches(&readback, &import.expected_messages));

        rollback(&import).expect("exact rollback");
        assert!(!import.target_dir.exists());
    }

    #[test]
    fn rollback_refuses_foreign_file() {
        let temporary = tempfile::tempdir().expect("temporary Cursor root");
        let workspace = temporary.path().join("workspace");
        fs::create_dir(&workspace).expect("workspace");
        let snapshot = fixture_snapshot(&workspace);
        let chats = temporary.path().join("cursor/chats");
        let import = build_with_root(&snapshot, &workspace, chats).expect("build import");
        materialize_store(&import).expect("materialize Cursor graph");
        fs::write(import.target_dir.join("foreign"), b"owned elsewhere").expect("foreign file");

        assert!(rollback(&import).is_err());
        assert!(import.target_dir.exists());
    }

    #[test]
    fn materialize_refuses_existing_target_without_changing_it() {
        let temporary = tempfile::tempdir().expect("temporary Cursor root");
        let workspace = temporary.path().join("workspace");
        fs::create_dir(&workspace).expect("workspace");
        let snapshot = fixture_snapshot(&workspace);
        let chats = temporary.path().join("cursor/chats");
        let import = build_with_root(&snapshot, &workspace, chats).expect("build import");
        fs::create_dir_all(&import.target_dir).expect("existing target");
        let marker = import.target_dir.join("foreign");
        fs::write(&marker, b"owned elsewhere").expect("foreign marker");

        assert!(materialize_store(&import).is_err());
        assert_eq!(
            fs::read(marker).expect("preserved marker"),
            b"owned elsewhere"
        );
    }

    #[cfg(unix)]
    #[test]
    fn path_validation_ignores_symlinks_above_managed_root() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().expect("temporary Cursor root");
        let actual = temporary.path().join("actual/cursor/chats/workspace");
        fs::create_dir_all(&actual).expect("Cursor workspace");
        let alias = temporary.path().join("alias");
        symlink(temporary.path().join("actual"), &alias).expect("parent alias");
        let chats = alias.join("cursor/chats");

        validate_directory_chain(&chats.join("workspace"), &chats, "writing")
            .expect("system path aliases above managed root are safe");
    }

    #[test]
    fn version_parser_requires_full_cursor_build() {
        assert_eq!(
            parse_version("2026.07.23-e383d2b\n").as_deref(),
            Some(SUPPORTED_CURSOR_VERSION)
        );
        assert_eq!(parse_version("2026.07.23"), None);
    }

    fn fixture_snapshot(workspace: &Path) -> CanonicalSnapshot {
        let thread_id = Uuid::new_v4();
        let branch_id = Uuid::new_v4();
        let event = |sequence, kind, text: &str, replay_policy| OmniEvent {
            schema_version: SCHEMA_VERSION.to_owned(),
            event_id: Uuid::new_v4(),
            thread_id,
            branch_id,
            sequence,
            timestamp: None,
            source: EventSource {
                provider: Provider::Claude,
                native_session_id: "source".to_owned(),
                provider_version: None,
                raw_record_type: None,
            },
            kind,
            payload: json!({ "text": text }),
            raw_blob_hash: None,
            sensitivity: Sensitivity::Normal,
            replay_policy,
        };
        CanonicalSnapshot {
            schema_version: SCHEMA_VERSION.to_owned(),
            session: SessionRef::new(Provider::Claude, "source"),
            thread_id,
            branch_id,
            title: None,
            captured_at: Utc::now(),
            workspace: WorkspaceSnapshot {
                schema_version: SCHEMA_VERSION.to_owned(),
                captured_at: Utc::now(),
                root: PathBuf::from(workspace),
                current_dir: PathBuf::from(workspace),
                git: GitState::default(),
                instruction_files: Vec::new(),
                environment_names: Vec::new(),
                available_tools: Vec::new(),
            },
            events: vec![
                event(
                    1,
                    EventKind::MessageUser,
                    "question",
                    ReplayPolicy::Contextual,
                ),
                event(
                    2,
                    EventKind::MessageAssistant,
                    "answer",
                    ReplayPolicy::Contextual,
                ),
                event(
                    3,
                    EventKind::ToolCompleted,
                    "tool output",
                    ReplayPolicy::HistoricalOnly,
                ),
            ],
        }
    }
}
