use std::{
    collections::BTreeMap,
    env, fs,
    io::Read,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use chrono::{SecondsFormat, Utc};
use directories::BaseDirs;
#[cfg(target_os = "linux")]
use md5::{Digest as Md5Digest, Md5};
use omnis_adapters::{CursorIdeAdapter, ProviderAdapter};
use omnis_core::{
    HandoffMessage, HandoffRole, TrajectoryItemKind, import_trajectory, redact_secrets,
};
use omnis_ir::{CanonicalSnapshot, Provider, SessionRef};
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};
use serde_json::{Value, json};
use sha2::{Digest as Sha2Digest, Sha256};
use uuid::Uuid;

const SUPPORTED_CURSOR_IDE_VERSION: &str = "3.12.17";
const SUPPORTED_CURSOR_IDE_COMMIT: &str = "0fb762053c34788bb7760d5673f8a6d4c8589d50";
const SUPPORTED_CURSOR_IDE_APPIMAGE_SIZE: u64 = 297_069_048;
const SUPPORTED_CURSOR_IDE_APPIMAGE_SHA256: &str =
    "16ed34a74bda2cd3a5f706c682db1e2f086c797c66210332ca194d17b559faa3";
const SUPPORTED_CURSOR_IDE_WORKBENCH_SHA256: &str =
    "23ed0b021697bbe8a3f472cfeae0a0c26c9e2cc631b32a3aaa781da963ec6565";
const SUPPORTED_CURSOR_IDE_SCHEMA_SHA256: &str =
    "5d50f2db30802e6508fce608f1185107e993abdc2e6c5e94d7f902f74264af96";
const CURSOR_IDE_COMPOSER_VERSION: i64 = 17;
const CURSOR_IDE_BUBBLE_VERSION: i64 = 3;
const MAX_BUILD_METADATA_SIZE: u64 = 4 * 1024 * 1024;

pub struct CursorIdeImport {
    pub target: SessionRef,
    pub expected_messages: Vec<HandoffMessage>,
    pub history_items: usize,
    pub tool_events: usize,
    pub truncated: bool,
    metadata_root: PathBuf,
    database: PathBuf,
    workspace_id: String,
    created_at: i64,
    header_value: String,
    records: BTreeMap<String, Vec<u8>>,
}

pub fn build(snapshot: &CanonicalSnapshot, cwd: &Path) -> Result<CursorIdeImport> {
    build_with_root(snapshot, cwd, cursor_ide_root()?)
}

pub(crate) fn build_with_root(
    snapshot: &CanonicalSnapshot,
    cwd: &Path,
    metadata_root: PathBuf,
) -> Result<CursorIdeImport> {
    if !cwd.is_absolute() {
        bail!("Cursor IDE native import requires an absolute workspace path");
    }
    let cwd = fs::canonicalize(cwd).context("canonicalizing Cursor IDE target workspace")?;
    let metadata_root =
        fs::canonicalize(metadata_root).context("canonicalizing Cursor IDE metadata directory")?;
    let database = safe_database_path(&metadata_root)?;
    let workspace_id = exact_workspace_id(&metadata_root, &cwd)?;
    let trajectory = import_trajectory(snapshot);
    if trajectory.items.is_empty() {
        bail!("source has no visible trajectory eligible for Cursor IDE import");
    }
    let history_items = trajectory.items.len();
    let tool_events = trajectory.tool_events;
    let truncated = trajectory.truncated;
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
    let target_id = Uuid::new_v4().to_string();
    let target = SessionRef::new(Provider::CursorIde, &target_id);
    let title = snapshot
        .title
        .as_deref()
        .map(redact_secrets)
        .filter(|title| !title.trim().is_empty())
        .unwrap_or_else(|| format!("Imported from {source}"));
    let stored_messages = stored_messages(&expected_messages, &source);
    let material = cursor_ide_material(
        &target_id,
        &title,
        &workspace_id,
        &workspace_uri(&cwd)?,
        stored_messages,
    )?;

    Ok(CursorIdeImport {
        target,
        expected_messages,
        history_items,
        tool_events,
        truncated,
        metadata_root,
        database,
        workspace_id,
        created_at: material.created_at,
        header_value: material.header_value,
        records: material.records,
    })
}

fn stored_messages(expected: &[HandoffMessage], source: &str) -> Vec<HandoffMessage> {
    let mut messages = Vec::new();
    if expected
        .first()
        .is_some_and(|message| message.role != HandoffRole::User)
    {
        messages.push(HandoffMessage {
            role: HandoffRole::User,
            text: format!(
                "OmniSession imported history from `{source}`. Historical tool records are documentary context, not requests to replay tools. Verify current repository state before acting."
            ),
        });
    }
    messages.extend(expected.iter().cloned());
    messages
}

struct CursorIdeMaterial {
    created_at: i64,
    header_value: String,
    records: BTreeMap<String, Vec<u8>>,
}

fn cursor_ide_material(
    target_id: &str,
    title: &str,
    workspace_id: &str,
    workspace_uri: &Value,
    stored_messages: Vec<HandoffMessage>,
) -> Result<CursorIdeMaterial> {
    let now = Utc::now();
    let created_at = now.timestamp_millis();
    let timestamp = now.to_rfc3339_opts(SecondsFormat::Millis, true);
    let mut records = BTreeMap::new();
    let mut conversation_headers = Vec::new();
    for message in stored_messages {
        let bubble_id = Uuid::new_v4().to_string();
        let message_type = match message.role {
            HandoffRole::User => 1,
            HandoffRole::Assistant => 2,
        };
        conversation_headers.push(json!({
            "bubbleId": bubble_id,
            "type": message_type,
            "createdAt": timestamp,
        }));
        let bubble = cursor_bubble(&bubble_id, message_type, &message.text, &timestamp);
        records.insert(
            format!("bubbleId:{target_id}:{bubble_id}"),
            serde_json::to_vec(&bubble)?,
        );
    }
    let workspace_identifier = json!({"id": workspace_id, "uri": workspace_uri});
    let root = json!({
        "_v": CURSOR_IDE_COMPOSER_VERSION,
        "composerId": target_id,
        "name": title,
        "text": "",
        "richText": "",
        "fullConversationHeadersOnly": conversation_headers,
        "status": "none",
        "context": {},
        "generatingBubbleIds": [],
        "codeBlockData": {},
        "originalFileStates": {},
        "newlyCreatedFiles": [],
        "newlyCreatedFolders": [],
        "createdAt": created_at,
        "lastUpdatedAt": created_at,
        "hasChangedContext": false,
        "capabilities": [],
        "unifiedMode": "agent",
        "forceMode": "edit",
        "isAgentic": true,
        "activeCustomMode": null,
        "pendingExitedCustomMode": null,
        "allAttachedFileCodeChunksUris": [],
        "modelConfig": {"modelName": "default", "maxMode": false, "selectedModels": []},
        "todos": [],
        "subComposerIds": [],
        "subagentComposerIds": [],
        "workspaceIdentifier": workspace_identifier,
        "conversationState": "~",
        "canvasPillCollapsed": false,
    });
    records.insert(
        format!("composerData:{target_id}"),
        serde_json::to_vec(&root)?,
    );
    let header_value = serde_json::to_string(&json!({
        "type": "head",
        "composerId": target_id,
        "name": title,
        "lastUpdatedAt": created_at,
        "createdAt": created_at,
        "unifiedMode": "agent",
        "forceMode": "edit",
        "isArchived": false,
        "isWorktree": false,
        "workspaceIdentifier": workspace_identifier,
    }))?;

    Ok(CursorIdeMaterial {
        created_at,
        header_value,
        records,
    })
}

fn cursor_bubble(id: &str, message_type: i64, text: &str, timestamp: &str) -> Value {
    json!({
        "_v": CURSOR_IDE_BUBBLE_VERSION,
        "type": message_type,
        "text": text,
        "richText": if message_type == 1 { text } else { "" },
        "bubbleId": id,
        "createdAt": timestamp,
        "approximateLintErrors": [],
        "lints": [],
        "codebaseContextChunks": [],
        "commits": [],
        "pullRequests": [],
        "attachedCodeChunks": [],
        "assistantSuggestedDiffs": [],
        "gitDiffs": [],
        "interpreterResults": [],
        "images": [],
        "attachedFolders": [],
        "attachedFoldersNew": [],
        "toolResults": [],
        "capabilities": [],
        "todos": [],
        "isAgentic": true,
        "unifiedMode": "agent",
        "tokenCount": {"inputTokens": 0, "outputTokens": 0},
        "context": {},
    })
}

pub fn ensure_supported(binary: &Path) -> Result<String> {
    let binary = fs::canonicalize(binary)
        .with_context(|| format!("canonicalizing Cursor IDE binary `{}`", binary.display()))?;
    if supported_appimage(&binary)? || supported_installed_bundle(&binary)? {
        return Ok(format!(
            "{SUPPORTED_CURSOR_IDE_VERSION} ({SUPPORTED_CURSOR_IDE_COMMIT})"
        ));
    }
    bail!(
        "Cursor IDE installation is not exact verified build {SUPPORTED_CURSOR_IDE_VERSION} ({SUPPORTED_CURSOR_IDE_COMMIT})"
    )
}

fn supported_appimage(binary: &Path) -> Result<bool> {
    let metadata = fs::metadata(binary)?;
    if metadata.len() != SUPPORTED_CURSOR_IDE_APPIMAGE_SIZE {
        return Ok(false);
    }
    Ok(hash_file(binary)? == SUPPORTED_CURSOR_IDE_APPIMAGE_SHA256)
}

fn supported_installed_bundle(binary: &Path) -> Result<bool> {
    let Some(parent) = binary.parent() else {
        return Ok(false);
    };
    let candidates = [
        parent.join("resources/app"),
        parent.join("../resources/app"),
        parent.join("../Resources/app"),
    ];
    for app_root in candidates {
        let product = app_root.join("product.json");
        let workbench = app_root.join("out/vs/workbench/workbench.desktop.main.js");
        if !product.is_file() || !workbench.is_file() {
            continue;
        }
        let metadata = fs::metadata(&product)?;
        if metadata.len() > MAX_BUILD_METADATA_SIZE {
            bail!("Cursor IDE product metadata exceeds safe size limit");
        }
        let value: Value = serde_json::from_reader(
            fs::File::open(&product).context("reading Cursor IDE product metadata")?,
        )
        .context("parsing Cursor IDE product metadata")?;
        let version = value.get("version").and_then(Value::as_str);
        let commit = value.get("commit").and_then(Value::as_str);
        if version != Some(SUPPORTED_CURSOR_IDE_VERSION)
            || commit != Some(SUPPORTED_CURSOR_IDE_COMMIT)
        {
            return Ok(false);
        }
        return Ok(hash_file(&workbench)? == SUPPORTED_CURSOR_IDE_WORKBENCH_SHA256);
    }
    Ok(false)
}

fn hash_file(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(hex::encode(digest.finalize()))
}

pub fn materialize(import: &CursorIdeImport, binary: &Path) -> Result<()> {
    ensure_supported(binary)?;
    ensure_cursor_idle()?;
    materialize_store(import)
}

pub(crate) fn materialize_store(import: &CursorIdeImport) -> Result<()> {
    let mut connection = open_write_database(import)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    validate_schema(&transaction)?;
    ensure_target_absent(&transaction, import)?;
    transaction.execute(
        "INSERT OR ABORT INTO composerHeaders(
            composerId, workspaceId, createdAt, lastUpdatedAt, isArchived,
            isSubagent, recency, checkpointAt, value
         ) VALUES (?1, ?2, ?3, ?3, 0, 0, ?3, NULL, ?4)",
        params![
            import.target.id,
            import.workspace_id,
            import.created_at,
            import.header_value,
        ],
    )?;
    {
        let mut statement =
            transaction.prepare("INSERT OR ABORT INTO cursorDiskKV(key, value) VALUES (?1, ?2)")?;
        for (key, value) in &import.records {
            statement.execute(params![key, value])?;
        }
    }
    transaction.commit()?;
    drop(connection);

    if let Err(error) = verify_readback(import) {
        return Err(combine_rollback_error(
            error.context("verifying Cursor IDE native import"),
            rollback_store(import),
        ));
    }
    Ok(())
}

pub fn rollback(import: &CursorIdeImport, binary: &Path) -> Result<()> {
    ensure_supported(binary)?;
    ensure_cursor_idle()?;
    rollback_store(import)
}

pub(crate) fn rollback_store(import: &CursorIdeImport) -> Result<()> {
    let mut connection = open_write_database(import)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    validate_schema(&transaction)?;
    validate_exact_rows(&transaction, import)?;
    for key in import.records.keys() {
        let deleted = transaction.execute("DELETE FROM cursorDiskKV WHERE key = ?1", [key])?;
        if deleted != 1 {
            bail!("Cursor IDE rollback did not delete exact generated key");
        }
    }
    let deleted = transaction.execute(
        "DELETE FROM composerHeaders WHERE composerId = ?1",
        [&import.target.id],
    )?;
    if deleted != 1 {
        bail!("Cursor IDE rollback did not delete exact generated header");
    }
    transaction.commit()?;
    drop(connection);
    if !generated_rows_absent(import)? {
        bail!("Cursor IDE rollback read-back found generated rows");
    }
    Ok(())
}

fn open_write_database(import: &CursorIdeImport) -> Result<Connection> {
    let current = safe_database_path(&import.metadata_root)?;
    if current != import.database {
        bail!("Cursor IDE metadata database changed after import planning");
    }
    let connection = Connection::open_with_flags(
        &import.database,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .context("opening Cursor IDE metadata for guarded write")?;
    connection.busy_timeout(Duration::ZERO)?;
    Ok(connection)
}

fn ensure_target_absent(transaction: &Transaction<'_>, import: &CursorIdeImport) -> Result<()> {
    let header_exists: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM composerHeaders WHERE composerId = ?1)",
        [&import.target.id],
        |row| row.get(0),
    )?;
    if header_exists {
        bail!("generated Cursor IDE target header already exists");
    }
    for key in import.records.keys() {
        let exists: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM cursorDiskKV WHERE key = ?1)",
            [key],
            |row| row.get(0),
        )?;
        if exists {
            bail!("generated Cursor IDE target key already exists");
        }
    }
    Ok(())
}

fn validate_exact_rows(transaction: &Transaction<'_>, import: &CursorIdeImport) -> Result<()> {
    let header = transaction
        .query_row(
            "SELECT workspaceId, createdAt, lastUpdatedAt, isArchived, isSubagent,
                    recency, checkpointAt, value
             FROM composerHeaders WHERE composerId = ?1",
            [&import.target.id],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<i64>>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                    row.get::<_, Option<i64>>(4)?,
                    row.get::<_, Option<i64>>(5)?,
                    row.get::<_, Option<i64>>(6)?,
                    row.get::<_, Option<String>>(7)?,
                ))
            },
        )
        .optional()?
        .context("generated Cursor IDE header is missing")?;
    let expected = (
        Some(import.workspace_id.clone()),
        Some(import.created_at),
        Some(import.created_at),
        Some(0),
        Some(0),
        Some(import.created_at),
        None,
        Some(import.header_value.clone()),
    );
    if header != expected {
        bail!("generated Cursor IDE header changed after materialization");
    }
    for (key, expected) in &import.records {
        let actual = transaction
            .query_row(
                "SELECT value FROM cursorDiskKV WHERE key = ?1",
                [key],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()?
            .with_context(|| format!("generated Cursor IDE key `{key}` is missing"))?;
        if &actual != expected {
            bail!("generated Cursor IDE key `{key}` changed after materialization");
        }
    }
    Ok(())
}

fn verify_readback(import: &CursorIdeImport) -> Result<()> {
    let adapter = CursorIdeAdapter::with_root(&import.metadata_root);
    let snapshot = adapter.read_session(&import.target)?;
    if !readback_matches(&snapshot, &import.expected_messages) {
        bail!("Cursor IDE read-back did not match imported trajectory");
    }
    Ok(())
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

fn generated_rows_absent(import: &CursorIdeImport) -> Result<bool> {
    let connection = Connection::open_with_flags(
        &import.database,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    connection.pragma_update(None, "query_only", "ON")?;
    let header_count = connection.query_row(
        "SELECT COUNT(*) FROM composerHeaders WHERE composerId = ?1",
        [&import.target.id],
        |row| row.get::<_, i64>(0),
    )?;
    if header_count != 0 {
        return Ok(false);
    }
    for key in import.records.keys() {
        let count = connection.query_row(
            "SELECT COUNT(*) FROM cursorDiskKV WHERE key = ?1",
            [key],
            |row| row.get::<_, i64>(0),
        )?;
        if count != 0 {
            return Ok(false);
        }
    }
    Ok(true)
}

fn validate_schema(transaction: &Transaction<'_>) -> Result<()> {
    let mut schema = String::new();
    for table in ["composerHeaders", "cursorDiskKV"] {
        let sql = transaction
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?1",
                [table],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .with_context(|| format!("Cursor IDE metadata omitted `{table}` table"))?;
        schema.push_str(table);
        schema.push(':');
        schema.push_str(&normalize_sql(&sql));
        if table != "cursorDiskKV" {
            schema.push('\n');
        }
    }
    let actual = hex::encode(Sha256::digest(schema.as_bytes()));
    if actual != SUPPORTED_CURSOR_IDE_SCHEMA_SHA256 {
        bail!("Cursor IDE metadata schema fingerprint is not verified");
    }
    Ok(())
}

fn normalize_sql(sql: &str) -> String {
    sql.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn combine_rollback_error(error: anyhow::Error, rollback: Result<()>) -> anyhow::Error {
    match rollback {
        Ok(()) => error,
        Err(rollback_error) => error.context(format!(
            "Cursor IDE import failed and exact-row rollback also failed: {rollback_error}"
        )),
    }
}

#[cfg(target_os = "linux")]
fn ensure_cursor_idle() -> Result<()> {
    let processes = fs::read_dir("/proc").context("inspecting Cursor IDE process state")?;
    for process in processes.flatten() {
        let Some(pid) = process
            .file_name()
            .to_str()
            .and_then(|value| value.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(comm) = fs::read_to_string(process.path().join("comm")) else {
            continue;
        };
        let comm = comm.trim().to_ascii_lowercase();
        let executable_is_cursor = fs::read_link(process.path().join("exe"))
            .ok()
            .and_then(|path| {
                path.file_name()
                    .map(|name| name.to_string_lossy().to_lowercase())
            })
            .is_some_and(|name| name.contains("cursor"));
        if !comm.contains("cursor") && !executable_is_cursor {
            continue;
        }
        let state = fs::read_to_string(process.path().join("stat")).unwrap_or_default();
        if state.split_whitespace().nth(2) == Some("Z") {
            continue;
        }
        bail!("Cursor IDE process {pid} is running; close Cursor before native materialization");
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn ensure_cursor_idle() -> Result<()> {
    bail!("Cursor IDE active-writer detection is only verified on Linux")
}

fn safe_database_path(metadata_root: &Path) -> Result<PathBuf> {
    let candidate = metadata_root.join("globalStorage/state.vscdb");
    let metadata =
        fs::symlink_metadata(&candidate).context("Cursor IDE metadata database was not found")?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        bail!("Cursor IDE metadata database is not a safe regular file");
    }
    let database = fs::canonicalize(candidate)?;
    if !database.starts_with(metadata_root) {
        bail!("Cursor IDE metadata database escaped configured root");
    }
    Ok(database)
}

#[cfg(target_os = "linux")]
fn exact_workspace_id(_metadata_root: &Path, cwd: &Path) -> Result<String> {
    linux_workspace_id(cwd)
}

#[cfg(not(target_os = "linux"))]
fn exact_workspace_id(metadata_root: &Path, cwd: &Path) -> Result<String> {
    let workspace_root = metadata_root.join("workspaceStorage");
    let metadata = fs::symlink_metadata(&workspace_root)
        .context("Cursor IDE workspace metadata was not found")?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        bail!("Cursor IDE workspace metadata is not a safe directory");
    }
    let mut matches = Vec::new();
    for entry in fs::read_dir(&workspace_root)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if !file_type.is_dir() || file_type.is_symlink() {
            continue;
        }
        let workspace_json = entry.path().join("workspace.json");
        let Ok(metadata) = fs::symlink_metadata(&workspace_json) else {
            continue;
        };
        if !metadata.is_file()
            || metadata.file_type().is_symlink()
            || metadata.len() > MAX_BUILD_METADATA_SIZE
        {
            continue;
        }
        let value: Value = match serde_json::from_reader(fs::File::open(&workspace_json)?) {
            Ok(value) => value,
            Err(_) => continue,
        };
        let Some(recorded) = value
            .get("folder")
            .or_else(|| value.get("workspace"))
            .and_then(Value::as_str)
            .and_then(file_uri_path)
        else {
            continue;
        };
        if fs::canonicalize(recorded).is_ok_and(|recorded| recorded == cwd) {
            let id = entry
                .file_name()
                .to_str()
                .filter(|id| !id.is_empty())
                .context("Cursor IDE workspace ID is not UTF-8")?
                .to_owned();
            matches.push(id);
        }
    }
    match matches.as_slice() {
        [id] => Ok(id.clone()),
        [] => bail!(
            "Cursor IDE has no workspace metadata matching `{}`",
            cwd.display()
        ),
        _ => bail!(
            "Cursor IDE has multiple workspace records matching `{}`",
            cwd.display()
        ),
    }
}

#[cfg(target_os = "linux")]
fn linux_workspace_id(cwd: &Path) -> Result<String> {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;

    let metadata = fs::metadata(cwd).context("reading Cursor IDE target workspace metadata")?;
    let mut digest = Md5::new();
    digest.update(cwd.as_os_str().as_bytes());
    digest.update(metadata.ino().to_string().as_bytes());
    Ok(hex::encode(digest.finalize()))
}

fn workspace_uri(cwd: &Path) -> Result<Value> {
    let path = cwd
        .to_str()
        .context("Cursor IDE native import requires a UTF-8 workspace path")?;
    Ok(json!({"$mid": 1, "path": path, "scheme": "file"}))
}

#[cfg(not(target_os = "linux"))]
fn file_uri_path(uri: &str) -> Option<PathBuf> {
    let encoded = uri.strip_prefix("file://")?;
    let bytes = encoded.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let high = hex_value(*bytes.get(index + 1)?)?;
            let low = hex_value(*bytes.get(index + 2)?)?;
            decoded.push((high << 4) | low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok().map(PathBuf::from)
}

#[cfg(not(target_os = "linux"))]
const fn hex_value(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn cursor_ide_root() -> Result<PathBuf> {
    if let Some(root) = env::var_os("CURSOR_IDE_HOME").filter(|value| !value.is_empty()) {
        let root = PathBuf::from(root);
        if !root.is_absolute() {
            bail!("CURSOR_IDE_HOME must be an absolute path");
        }
        return Ok(root);
    }
    BaseDirs::new()
        .map(|directories| directories.config_dir().join("Cursor/User"))
        .context("home directory is unavailable")
}

#[cfg(test)]
pub(crate) fn create_fixture_store(metadata_root: &Path, workspace: &Path) -> Result<()> {
    let workspace_id = "cursor-ide-fixture";
    fs::create_dir_all(metadata_root.join("globalStorage"))?;
    fs::create_dir_all(metadata_root.join("workspaceStorage").join(workspace_id))?;
    fs::create_dir_all(workspace)?;
    fs::write(
        metadata_root
            .join("workspaceStorage")
            .join(workspace_id)
            .join("workspace.json"),
        serde_json::to_vec(&json!({
            "folder": format!("file://{}", workspace.display())
        }))?,
    )?;
    let connection = Connection::open(metadata_root.join("globalStorage/state.vscdb"))?;
    connection.execute_batch(
        "PRAGMA user_version = 1;
         CREATE TABLE composerHeaders (composerId TEXT PRIMARY KEY, workspaceId TEXT, createdAt INTEGER, lastUpdatedAt INTEGER, isArchived INTEGER, isSubagent INTEGER, recency INTEGER, checkpointAt INTEGER, value TEXT);
         CREATE TABLE cursorDiskKV (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB);
         CREATE TABLE ItemTable (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB);",
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use chrono::Utc;
    use omnis_ir::{
        EventKind, EventSource, GitState, OmniEvent, ReplayPolicy, SCHEMA_VERSION, Sensitivity,
        WorkspaceSnapshot,
    };

    use super::*;

    #[test]
    fn native_store_round_trip_and_exact_rollback() {
        let fixture = fixture_store();
        let snapshot = fixture_snapshot(&fixture.workspace);
        let import = build_with_root(&snapshot, &fixture.workspace, fixture.root.clone())
            .expect("build Cursor IDE import");

        materialize_store(&import).expect("materialize Cursor IDE import");
        assert!(
            CursorIdeAdapter::with_root(&fixture.root)
                .read_session(&import.target)
                .is_ok()
        );
        rollback_store(&import).expect("exact-row rollback");
        assert!(generated_rows_absent(&import).expect("rollback read-back"));
    }

    #[test]
    fn materialize_refuses_existing_key_without_overwrite() {
        let fixture = fixture_store();
        let snapshot = fixture_snapshot(&fixture.workspace);
        let import = build_with_root(&snapshot, &fixture.workspace, fixture.root.clone())
            .expect("build Cursor IDE import");
        let key = import.records.keys().next().expect("generated key");
        let connection = Connection::open(&import.database).expect("open fixture database");
        connection
            .execute(
                "INSERT INTO cursorDiskKV(key, value) VALUES (?1, ?2)",
                params![key, b"foreign"],
            )
            .expect("foreign row");

        assert!(materialize_store(&import).is_err());
        let value: Vec<u8> = connection
            .query_row(
                "SELECT value FROM cursorDiskKV WHERE key = ?1",
                [key],
                |row| row.get(0),
            )
            .expect("preserved foreign row");
        assert_eq!(value, b"foreign");
    }

    #[test]
    fn readback_failure_rolls_back_exact_generated_rows() {
        let fixture = fixture_store();
        let snapshot = fixture_snapshot(&fixture.workspace);
        let mut import = build_with_root(&snapshot, &fixture.workspace, fixture.root.clone())
            .expect("build Cursor IDE import");
        import.expected_messages[0].text = "forced mismatch".to_owned();

        assert!(materialize_store(&import).is_err());
        assert!(generated_rows_absent(&import).expect("failed import rollback"));
    }

    #[test]
    fn assistant_first_boundary_is_excluded_from_readback() {
        let fixture = fixture_store();
        let mut snapshot = fixture_snapshot(&fixture.workspace);
        snapshot
            .events
            .retain(|event| event.kind == EventKind::MessageAssistant);
        let import = build_with_root(&snapshot, &fixture.workspace, fixture.root.clone())
            .expect("build assistant-first import");

        materialize_store(&import).expect("materialize assistant-first import");
        assert_eq!(import.expected_messages.len(), 1);
        rollback_store(&import).expect("assistant-first rollback");
    }

    #[test]
    fn rollback_refuses_changed_generated_row() {
        let fixture = fixture_store();
        let snapshot = fixture_snapshot(&fixture.workspace);
        let import = build_with_root(&snapshot, &fixture.workspace, fixture.root.clone())
            .expect("build Cursor IDE import");
        materialize_store(&import).expect("materialize Cursor IDE import");
        let key = import.records.keys().next().expect("generated key");
        let connection = Connection::open(&import.database).expect("open fixture database");
        connection
            .execute(
                "UPDATE cursorDiskKV SET value = ?2 WHERE key = ?1",
                params![key, b"changed"],
            )
            .expect("change generated row");

        assert!(rollback_store(&import).is_err());
        assert!(
            connection
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM composerHeaders WHERE composerId = ?1)",
                    [&import.target.id],
                    |row| row.get::<_, bool>(0),
                )
                .expect("generated header still exists")
        );
    }

    #[test]
    fn version_gate_rejects_unverified_bundle() {
        let temporary = tempfile::tempdir().expect("temporary Cursor bundle");
        let binary = temporary.path().join("cursor");
        fs::write(&binary, b"not Cursor").expect("fake Cursor binary");
        assert!(ensure_supported(&binary).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn unopened_workspace_uses_cursor_linux_identity() {
        let fixture = fixture_store();
        let unopened = fixture
            .workspace
            .parent()
            .expect("fixture root")
            .join("workspace-never-opened-in-cursor");
        fs::create_dir(&unopened).expect("unopened workspace");
        let snapshot = fixture_snapshot(&unopened);

        let import = build_with_root(&snapshot, &unopened, fixture.root.clone())
            .expect("build unopened Cursor IDE workspace import");

        assert_eq!(
            import.workspace_id,
            linux_workspace_id(&unopened).expect("Cursor Linux workspace ID")
        );
        materialize_store(&import).expect("materialize unopened workspace import");
        assert!(
            CursorIdeAdapter::with_root(&fixture.root)
                .read_session(&import.target)
                .is_ok()
        );
        rollback_store(&import).expect("rollback unopened workspace import");
    }

    struct FixtureStore {
        _temporary: tempfile::TempDir,
        root: PathBuf,
        workspace: PathBuf,
    }

    fn fixture_store() -> FixtureStore {
        let temporary = tempfile::tempdir().expect("temporary Cursor IDE root");
        let root = temporary.path().join("Cursor/User");
        let workspace = temporary.path().join("workspace");
        create_fixture_store(&root, &workspace).expect("Cursor IDE fixture store");
        FixtureStore {
            _temporary: temporary,
            root,
            workspace,
        }
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
            timestamp: Some(Utc::now()),
            source: EventSource {
                provider: Provider::Codex,
                native_session_id: "synthetic-source".to_owned(),
                provider_version: Some("synthetic".to_owned()),
                raw_record_type: Some("synthetic".to_owned()),
            },
            kind,
            payload: json!({"text": text}),
            raw_blob_hash: None,
            sensitivity: Sensitivity::Normal,
            replay_policy,
        };
        CanonicalSnapshot {
            schema_version: SCHEMA_VERSION.to_owned(),
            session: SessionRef::new(Provider::Codex, "synthetic-source"),
            thread_id,
            branch_id,
            title: Some("Synthetic Cursor IDE import".to_owned()),
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
            events: vec![
                event(
                    0,
                    EventKind::MessageUser,
                    "Question",
                    ReplayPolicy::Contextual,
                ),
                event(
                    1,
                    EventKind::MessageAssistant,
                    "Answer",
                    ReplayPolicy::Contextual,
                ),
            ],
        }
    }
}
