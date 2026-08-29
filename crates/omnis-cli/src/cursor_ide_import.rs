use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    path::{Path, PathBuf},
    sync::Mutex,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use chrono::{SecondsFormat, Utc};
#[cfg(not(target_os = "windows"))]
use directories::BaseDirs;
#[cfg(target_os = "linux")]
use md5::Md5;
use omnis_adapters::{CursorIdeAdapter, ProviderAdapter};
use omnis_core::{
    HandoffMessage, HandoffRole, TrajectoryItemKind, import_trajectory, redact_secrets,
};
use omnis_ir::{CanonicalSnapshot, Provider, SessionRef};
use prost::{Message, Oneof};
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
    types::Value as SqlValue,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::private_store_lock::{self, PrivateStoreGuard};

pub(crate) type CursorIdeWriteGuard = PrivateStoreGuard;

#[cfg(target_os = "macos")]
use std::process::Command;

const MINIMUM_CURSOR_IDE_VERSION: &str = "3.12.17";
const SUPPORTED_CURSOR_IDE_SCHEMA_SHA256: &str =
    "5d50f2db30802e6508fce608f1185107e993abdc2e6c5e94d7f902f74264af96";
const CURSOR_IDE_COMPOSER_VERSION: i64 = 17;
const CURSOR_IDE_BUBBLE_VERSION: i64 = 3;
const MAX_BUILD_METADATA_SIZE: u64 = 4 * 1024 * 1024;
const MAX_CURSOR_NATIVE_GRAPH_RECORDS: usize = 100_000;
const MAX_CURSOR_NATIVE_RECORD_SIZE: usize = 16 * 1024 * 1024;
const CURSOR_BLOB_PREFIX: &str = "agentKv:blob:";
const CURSOR_WORKSPACE_SELECTION_KEY: &str = "composer.composerData";
const CURSOR_WORKSPACE_ITEM_TABLE_SCHEMA: &str =
    "CREATE TABLE ItemTable (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB)";
const CURSOR_IDE_LOCK_NAMESPACE: &str = "cursor-ide";

pub struct CursorIdeImport {
    pub target: SessionRef,
    pub expected_messages: Vec<HandoffMessage>,
    pub history_items: usize,
    pub tool_events: usize,
    pub truncated: bool,
    metadata_root: PathBuf,
    database: PathBuf,
    workspace_database: Option<PathBuf>,
    workspace_id: String,
    created_at: i64,
    header_value: String,
    records: BTreeMap<String, Vec<u8>>,
    lock_root: Option<PathBuf>,
    created_record_keys: Mutex<BTreeSet<String>>,
    previous_workspace_selection: Mutex<WorkspaceSelectionState>,
}

#[derive(Clone)]
enum WorkspaceSelectionState {
    NotCaptured,
    Missing,
    Present(SqlValue),
}

#[derive(Clone)]
struct WorkspaceDeletionPlan {
    database: PathBuf,
    original: SqlValue,
    updated: SqlValue,
}

pub fn build(snapshot: &CanonicalSnapshot, cwd: &Path) -> Result<CursorIdeImport> {
    build_with_root(snapshot, cwd, cursor_ide_root()?)
}

pub(crate) fn build_with_root(
    snapshot: &CanonicalSnapshot,
    cwd: &Path,
    metadata_root: PathBuf,
) -> Result<CursorIdeImport> {
    build_with_roots(snapshot, cwd, metadata_root, None)
}

#[cfg(test)]
fn build_with_lock_root(
    snapshot: &CanonicalSnapshot,
    cwd: &Path,
    metadata_root: PathBuf,
    lock_root: PathBuf,
) -> Result<CursorIdeImport> {
    build_with_roots(snapshot, cwd, metadata_root, Some(lock_root))
}

fn build_with_roots(
    snapshot: &CanonicalSnapshot,
    cwd: &Path,
    metadata_root: PathBuf,
    lock_root: Option<PathBuf>,
) -> Result<CursorIdeImport> {
    if !cwd.is_absolute() {
        bail!("Cursor IDE native import requires an absolute workspace path");
    }
    let cwd = fs::canonicalize(cwd).context("canonicalizing Cursor IDE target workspace")?;
    let metadata_root =
        fs::canonicalize(metadata_root).context("canonicalizing Cursor IDE metadata directory")?;
    let database = safe_database_path(&metadata_root)?;
    let workspace_id = exact_workspace_id(&metadata_root, &cwd)?;
    let workspace_database = safe_workspace_database(&metadata_root, &workspace_id)?;
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
        workspace_database,
        workspace_id,
        created_at: material.created_at,
        header_value: material.header_value,
        records: material.records,
        lock_root,
        created_record_keys: Mutex::new(BTreeSet::new()),
        previous_workspace_selection: Mutex::new(WorkspaceSelectionState::NotCaptured),
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

struct PersistedMessage {
    bubble_id: String,
    message: HandoffMessage,
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
    #[prost(oneof = "CursorConversationTurn", tags = "1, 2")]
    turn: Option<CursorConversationTurn>,
}

#[derive(Clone, PartialEq, Oneof)]
enum CursorConversationTurn {
    #[prost(message, tag = "1")]
    Agent(CursorAgentConversationTurnStructure),
    #[prost(message, tag = "2")]
    Shell(CursorOpaqueMessage),
}

#[derive(Clone, PartialEq, Message)]
struct CursorAgentConversationTurnStructure {
    #[prost(bytes = "vec", tag = "1")]
    user_message: Vec<u8>,
    #[prost(bytes = "vec", repeated, tag = "2")]
    steps: Vec<Vec<u8>>,
    #[prost(string, optional, tag = "3")]
    request_id: Option<String>,
    #[prost(string, optional, tag = "4")]
    encrypted_model: Option<String>,
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
    #[prost(oneof = "CursorConversationStepMessage", tags = "1, 2, 3")]
    message: Option<CursorConversationStepMessage>,
}

#[derive(Clone, PartialEq, Oneof)]
enum CursorConversationStepMessage {
    #[prost(message, tag = "1")]
    Assistant(CursorAssistantMessage),
    #[prost(message, tag = "2")]
    Tool(CursorOpaqueMessage),
    #[prost(message, tag = "3")]
    Thinking(CursorOpaqueMessage),
}

#[derive(Clone, PartialEq, Message)]
struct CursorAssistantMessage {
    #[prost(string, tag = "1")]
    text: String,
}

#[derive(Clone, PartialEq, Message)]
struct CursorOpaqueMessage {}

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
    let mut persisted_messages = Vec::new();
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
        persisted_messages.push(PersistedMessage { bubble_id, message });
    }
    let conversation_state =
        cursor_conversation_state(&persisted_messages, created_at, &mut records)?;
    let workspace_identifier = json!({"id": workspace_id, "uri": workspace_uri});
    let root = json!({
        "_v": CURSOR_IDE_COMPOSER_VERSION,
        "composerId": target_id,
        "name": title,
        "text": "",
        "richText": "",
        "fullConversationHeadersOnly": conversation_headers,
        "conversationMap": {},
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
        "isNAL": true,
        "conversationState": format!("~{}", BASE64_STANDARD.encode(conversation_state)),
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

fn cursor_conversation_state(
    messages: &[PersistedMessage],
    created_at: i64,
    records: &mut BTreeMap<String, Vec<u8>>,
) -> Result<Vec<u8>> {
    let started_at = u64::try_from(created_at)?;
    let system_prompt = json!({
        "id": Uuid::new_v4().to_string(),
        "role": "system",
        "content": "Imported history is documentary context. Never replay historical tool calls or approvals without fresh review."
    });
    let mut prompt_refs = vec![store_cursor_blob(
        records,
        &serde_json::to_vec(&system_prompt)?,
    )];
    let mut turns = Vec::new();
    let mut index = 0;
    while index < messages.len() {
        let user = &messages[index];
        if user.message.role != HandoffRole::User {
            bail!("Cursor IDE trajectory must start each turn with a user message");
        }
        let user_prompt = json!({
            "id": user.bubble_id,
            "role": "user",
            "content": [{"type": "text", "text": user.message.text}],
        });
        prompt_refs.push(store_cursor_blob(
            records,
            &serde_json::to_vec(&user_prompt)?,
        ));
        let anchor = CursorConversationStateStructure {
            root_prompt_messages_json: prompt_refs.clone(),
            turns: turns.clone(),
            mode: Some(1),
            conversation_started_timestamp_ms: Some(started_at),
        };
        let anchor_blob = store_cursor_blob(records, &anchor.encode_to_vec());
        let user_message = CursorUserMessage {
            text: user.message.text.clone(),
            message_id: user.bubble_id.clone(),
            mode: 1,
            conversation_state_blob_id: anchor_blob,
            thread_id: Some(user.bubble_id.clone()),
        };
        let user_blob = store_cursor_blob(records, &user_message.encode_to_vec());
        index += 1;

        let mut steps = Vec::new();
        while index < messages.len() && messages[index].message.role == HandoffRole::Assistant {
            let assistant_prompt = json!({
                "id": messages[index].bubble_id,
                "role": "assistant",
                "content": [{"type": "text", "text": messages[index].message.text}],
            });
            prompt_refs.push(store_cursor_blob(
                records,
                &serde_json::to_vec(&assistant_prompt)?,
            ));
            let step = CursorConversationStep {
                message: Some(CursorConversationStepMessage::Assistant(
                    CursorAssistantMessage {
                        text: messages[index].message.text.clone(),
                    },
                )),
            };
            steps.push(store_cursor_blob(records, &step.encode_to_vec()));
            index += 1;
        }
        let turn = CursorConversationTurnStructure {
            turn: Some(CursorConversationTurn::Agent(
                CursorAgentConversationTurnStructure {
                    user_message: user_blob,
                    steps,
                    request_id: Some(Uuid::new_v4().to_string()),
                    encrypted_model: None,
                },
            )),
        };
        turns.push(store_cursor_blob(records, &turn.encode_to_vec()));
    }
    Ok(CursorConversationStateStructure {
        root_prompt_messages_json: prompt_refs,
        turns,
        mode: Some(1),
        conversation_started_timestamp_ms: Some(started_at),
    }
    .encode_to_vec())
}

fn store_cursor_blob(records: &mut BTreeMap<String, Vec<u8>>, value: &[u8]) -> Vec<u8> {
    let id = Sha256::digest(value).to_vec();
    records.insert(
        format!("{CURSOR_BLOB_PREFIX}{}", hex::encode(&id)),
        value.to_vec(),
    );
    id
}

pub fn ensure_supported(binary: &Path) -> Result<String> {
    let binary = fs::canonicalize(binary)
        .with_context(|| format!("canonicalizing Cursor IDE binary `{}`", binary.display()))?;
    let version = if let Some(version) = version_from_path(&binary) {
        version
    } else {
        installed_bundle_version(&binary)?
            .context("Cursor IDE version was not found in binary name or product metadata")?
    };
    if !is_supported_version(&version) {
        bail!(
            "Cursor IDE {version} is too old for native trajectory import; supported versions: >= {MINIMUM_CURSOR_IDE_VERSION}"
        );
    }
    Ok(version)
}

fn is_supported_version(version: &str) -> bool {
    crate::version_gate::is_at_least(version, MINIMUM_CURSOR_IDE_VERSION)
}

fn version_from_path(binary: &Path) -> Option<String> {
    binary
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(parse_version)
}

fn installed_bundle_version(binary: &Path) -> Result<Option<String>> {
    let mut products = Vec::new();
    for ancestor in binary.ancestors().take(8) {
        for product in [
            ancestor.join("product.json"),
            ancestor.join("resources/app/product.json"),
            ancestor.join("Resources/app/product.json"),
        ] {
            if !products.contains(&product) {
                products.push(product);
            }
        }
    }
    for product in products {
        if !product.is_file() {
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
        return Ok(value
            .get("version")
            .and_then(Value::as_str)
            .and_then(parse_version));
    }
    Ok(None)
}

fn parse_version(output: &str) -> Option<String> {
    output
        .split(|character: char| !character.is_ascii_digit() && character != '.')
        .find(|candidate| {
            let components = candidate.split('.').collect::<Vec<_>>();
            components.len() == 3
                && components
                    .iter()
                    .all(|component| component.parse::<u64>().is_ok())
        })
        .map(str::to_owned)
}

pub fn materialize(import: &CursorIdeImport, binary: &Path) -> Result<CursorIdeWriteGuard> {
    ensure_supported(binary)?;
    ensure_cursor_idle()?;
    let guard = lock_metadata_root(import)?;
    ensure_cursor_idle()?;
    materialize_store_locked(import)?;
    Ok(guard)
}

#[cfg(test)]
pub(crate) fn materialize_store(import: &CursorIdeImport) -> Result<()> {
    materialize_store_locked(import)
}

#[cfg(test)]
fn materialize_store_with_lock(import: &CursorIdeImport) -> Result<()> {
    let _guard = lock_metadata_root(import)?;
    materialize_store_locked(import)
}

fn materialize_store_locked(import: &CursorIdeImport) -> Result<()> {
    let mut connection = open_write_database(import)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    validate_schema(&transaction)?;
    let created_record_keys = created_record_keys(&transaction, import)?;
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
        for key in &created_record_keys {
            statement.execute(params![key, &import.records[key]])?;
        }
    }
    transaction.commit()?;
    *import
        .created_record_keys
        .lock()
        .map_err(|_| anyhow::anyhow!("Cursor IDE import state lock is poisoned"))? =
        created_record_keys;
    drop(connection);

    if let Err(error) = materialize_workspace_selection(import) {
        return Err(combine_rollback_error(
            error.context("selecting imported Cursor IDE chat"),
            rollback_store_locked(import),
        ));
    }

    if let Err(error) = verify_readback(import).and_then(|()| verify_workspace_selection(import)) {
        return Err(combine_rollback_error(
            error.context("verifying Cursor IDE native import"),
            rollback_store_locked(import),
        ));
    }
    Ok(())
}

pub(crate) fn rollback_locked(import: &CursorIdeImport, _guard: &PrivateStoreGuard) -> Result<()> {
    ensure_cursor_idle()?;
    rollback_store_locked(import)
}

/// Deletes one selected Cursor IDE composer and exact namespaced records.
pub fn delete_session(session: &SessionRef, binary: &Path) -> Result<CursorIdeWriteGuard> {
    ensure_supported(binary)?;
    ensure_cursor_idle()?;
    let metadata_root = cursor_ide_root()?;
    let guard = lock_root(&metadata_root, None)?;
    ensure_cursor_idle()?;
    delete_session_at_locked(session, &metadata_root)?;
    Ok(guard)
}

#[cfg(test)]
fn delete_session_at(session: &SessionRef, metadata_root: &Path) -> Result<()> {
    delete_session_at_locked(session, metadata_root)
}

#[cfg(test)]
fn delete_session_at_with_lock_root(
    session: &SessionRef,
    metadata_root: &Path,
    configured_lock_root: &Path,
) -> Result<()> {
    let _guard = lock_root(metadata_root, Some(configured_lock_root))?;
    delete_session_at_locked(session, metadata_root)
}

fn delete_session_at_locked(session: &SessionRef, metadata_root: &Path) -> Result<()> {
    if session.provider != Provider::CursorIde || Uuid::parse_str(&session.id).is_err() {
        bail!("refusing Cursor IDE deletion with invalid session identity");
    }
    let root_metadata =
        fs::symlink_metadata(metadata_root).context("Cursor IDE metadata root was not found")?;
    if !root_metadata.is_dir() || root_metadata.file_type().is_symlink() {
        bail!("Cursor IDE metadata root is not a safe directory");
    }
    let metadata_root =
        fs::canonicalize(metadata_root).context("canonicalizing Cursor IDE metadata root")?;
    let database = safe_database_path(&metadata_root)?;
    let mut connection = Connection::open_with_flags(
        &database,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .context("opening Cursor IDE metadata for deletion")?;
    connection.busy_timeout(Duration::ZERO)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    validate_schema(&transaction)?;
    let (ids, header_ids, workspace_ids) = cursor_deletion_set(&transaction, &session.id)?;
    let workspace_databases = workspace_ids
        .iter()
        .filter_map(|workspace_id| {
            safe_workspace_database(&metadata_root, workspace_id).transpose()
        })
        .collect::<Result<BTreeSet<_>>>()?;
    let workspace_plans = workspace_databases
        .iter()
        .filter_map(|database| plan_workspace_selection_deletion(database, &ids).transpose())
        .collect::<Result<Vec<_>>>()?;
    for id in &ids {
        delete_cursor_namespaced_records(&transaction, id)?;
        let deleted =
            transaction.execute("DELETE FROM composerHeaders WHERE composerId = ?1", [id])?;
        let expected = usize::from(header_ids.contains(id));
        if deleted != expected {
            bail!("Cursor IDE deletion did not remove exact composer header `{id}`");
        }
    }
    apply_workspace_deletion_plans(&workspace_plans, &ids)?;
    if let Err(error) = transaction.commit() {
        return Err(combine_workspace_restore_error(
            error.into(),
            restore_workspace_deletion_plans(&workspace_plans),
        ));
    }
    drop(connection);
    verify_cursor_deletion(&database, &ids)
}

fn cursor_deletion_set(
    transaction: &Transaction<'_>,
    selected_id: &str,
) -> Result<(BTreeSet<String>, BTreeSet<String>, BTreeSet<String>)> {
    let mut statement = transaction.prepare(
        "SELECT composerId, workspaceId, value FROM composerHeaders ORDER BY composerId",
    )?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, SqlValue>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if rows.len() > MAX_CURSOR_NATIVE_GRAPH_RECORDS {
        bail!("Cursor IDE deletion exceeds safe composer-header limit");
    }
    let header_ids = rows
        .iter()
        .map(|(id, _, _)| id.clone())
        .collect::<BTreeSet<_>>();
    if !header_ids.contains(selected_id) {
        bail!("selected Cursor IDE session no longer exists");
    }
    let roots = cursor_deletion_roots(transaction, &header_ids)?;
    let mut ids = BTreeSet::from([selected_id.to_owned()]);
    loop {
        let before = ids.len();
        for (id, _, raw) in &rows {
            let header = parse_cursor_delete_value(raw)?;
            let root = roots.get(id);
            if [header.as_ref(), root]
                .into_iter()
                .flatten()
                .any(|value| cursor_parent(value).is_some_and(|parent| ids.contains(parent)))
            {
                validate_cursor_descendant_id(id)?;
                ids.insert(id.clone());
            }
            if ids.contains(id) {
                for value in [header.as_ref(), root].into_iter().flatten() {
                    for child in cursor_children(value) {
                        validate_cursor_descendant_id(child)?;
                        ids.insert(child.to_owned());
                    }
                }
            }
        }
        if ids.len() == before {
            break;
        }
        if ids.len() > MAX_CURSOR_NATIVE_GRAPH_RECORDS {
            bail!("Cursor IDE deletion exceeds safe descendant limit");
        }
    }
    let workspace_ids = rows
        .iter()
        .filter(|(id, _, _)| ids.contains(id))
        .filter_map(|(_, workspace_id, _)| workspace_id.clone())
        .collect();
    Ok((ids, header_ids, workspace_ids))
}

fn cursor_deletion_roots(
    transaction: &Transaction<'_>,
    header_ids: &BTreeSet<String>,
) -> Result<BTreeMap<String, Value>> {
    let mut roots = BTreeMap::new();
    for id in header_ids {
        let key = format!("composerData:{id}");
        let Some(raw) = transaction
            .query_row(
                "SELECT value FROM cursorDiskKV WHERE key = ?1",
                [&key],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()?
        else {
            continue;
        };
        if raw.len() > MAX_CURSOR_NATIVE_RECORD_SIZE {
            bail!("Cursor IDE composer root exceeds safe record limit");
        }
        let value = serde_json::from_slice(&raw)
            .with_context(|| format!("Cursor IDE composer root `{id}` is invalid JSON"))?;
        roots.insert(id.clone(), value);
    }
    Ok(roots)
}

fn parse_cursor_delete_value(raw: &SqlValue) -> Result<Option<Value>> {
    let bytes = match raw {
        SqlValue::Text(raw) => raw.as_bytes(),
        SqlValue::Blob(raw) => raw.as_slice(),
        _ => return Ok(None),
    };
    if bytes.len() > MAX_CURSOR_NATIVE_RECORD_SIZE {
        bail!("Cursor IDE composer header exceeds safe record limit");
    }
    Ok(serde_json::from_slice(bytes).ok())
}

fn cursor_parent(value: &Value) -> Option<&str> {
    value
        .get("parentComposerId")
        .or_else(|| value.get("rootComposerId"))
        .and_then(Value::as_str)
}

fn cursor_children(value: &Value) -> impl Iterator<Item = &str> {
    ["subComposerIds", "subagentComposerIds"]
        .into_iter()
        .filter_map(|key| value.get(key).and_then(Value::as_array))
        .flatten()
        .filter_map(Value::as_str)
}

fn validate_cursor_descendant_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 256
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        bail!("Cursor IDE descendant composer has unsafe identity");
    }
    Ok(())
}

fn cursor_namespaced_key_prefix(prefix: &str, id: &str) -> String {
    format!("{prefix}:{id}:")
}

fn delete_cursor_namespaced_records(transaction: &Transaction<'_>, id: &str) -> Result<()> {
    let root = format!("composerData:{id}");
    transaction.execute("DELETE FROM cursorDiskKV WHERE key = ?1", [&root])?;
    for prefix in [
        "bubbleId",
        "checkpointId",
        "codeBlockDiff",
        "codeBlockPartialInlineDiffFates",
        "ofsContent",
    ] {
        let prefix = cursor_namespaced_key_prefix(prefix, id);
        transaction.execute(
            "DELETE FROM cursorDiskKV
             WHERE substr(key, 1, length(?1)) = ?1 COLLATE BINARY",
            [&prefix],
        )?;
    }
    Ok(())
}

fn apply_workspace_deletion_plans(
    plans: &[WorkspaceDeletionPlan],
    ids: &BTreeSet<String>,
) -> Result<()> {
    let mut applied = Vec::new();
    for plan in plans {
        if let Err(error) = apply_workspace_deletion_plan(plan) {
            return Err(combine_workspace_restore_error(
                error,
                restore_workspace_deletion_plans(&applied),
            ));
        }
        applied.push(plan.clone());
        if let Err(error) = verify_workspace_selection_deleted(&plan.database, ids) {
            return Err(combine_workspace_restore_error(
                error,
                restore_workspace_deletion_plans(&applied),
            ));
        }
    }
    Ok(())
}

fn apply_workspace_deletion_plan(plan: &WorkspaceDeletionPlan) -> Result<()> {
    let mut connection = open_workspace_database(&plan.database)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    validate_workspace_schema(&transaction)?;
    let changed = transaction.execute(
        "UPDATE ItemTable SET value = ?2 WHERE key = ?1 AND value = ?3",
        params![
            CURSOR_WORKSPACE_SELECTION_KEY,
            &plan.updated,
            &plan.original
        ],
    )?;
    if changed != 1 {
        bail!("Cursor IDE workspace selection changed during deletion");
    }
    transaction.commit()?;
    Ok(())
}

fn plan_workspace_selection_deletion(
    database: &Path,
    ids: &BTreeSet<String>,
) -> Result<Option<WorkspaceDeletionPlan>> {
    let connection = Connection::open_with_flags(
        database,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    connection.pragma_update(None, "query_only", "ON")?;
    validate_workspace_schema(&connection)?;
    let Some(original) = connection
        .query_row(
            "SELECT value FROM ItemTable WHERE key = ?1",
            [CURSOR_WORKSPACE_SELECTION_KEY],
            |row| row.get::<_, SqlValue>(0),
        )
        .optional()?
    else {
        return Ok(None);
    };
    let Some(updated) = updated_workspace_selection(&original, ids)? else {
        return Ok(None);
    };
    Ok(Some(WorkspaceDeletionPlan {
        database: database.to_path_buf(),
        original,
        updated,
    }))
}

fn updated_workspace_selection(
    value: &SqlValue,
    ids: &BTreeSet<String>,
) -> Result<Option<SqlValue>> {
    let (raw, was_blob) = match value {
        SqlValue::Text(value) => (value.as_bytes(), false),
        SqlValue::Blob(value) => (value.as_slice(), true),
        _ => bail!("Cursor IDE workspace selection has unsupported SQLite type"),
    };
    if raw.len() > MAX_CURSOR_NATIVE_RECORD_SIZE {
        bail!("Cursor IDE workspace selection exceeds safe record limit");
    }
    let mut document = serde_json::from_slice::<Value>(raw)
        .context("Cursor IDE workspace selection is invalid JSON")?;
    let mut changed = false;
    for key in ["selectedComposerIds", "lastFocusedComposerIds"] {
        let Some(values) = document.get_mut(key).and_then(Value::as_array_mut) else {
            continue;
        };
        let before = values.len();
        values.retain(|value| value.as_str().is_none_or(|id| !ids.contains(id)));
        changed |= values.len() != before;
    }
    if !changed {
        return Ok(None);
    }
    let encoded = serde_json::to_vec(&document)?;
    Ok(Some(if was_blob {
        SqlValue::Blob(encoded)
    } else {
        SqlValue::Text(String::from_utf8(encoded)?)
    }))
}

fn verify_workspace_selection_deleted(database: &Path, ids: &BTreeSet<String>) -> Result<()> {
    let connection = Connection::open_with_flags(
        database,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    connection.pragma_update(None, "query_only", "ON")?;
    let Some(value) = connection
        .query_row(
            "SELECT value FROM ItemTable WHERE key = ?1",
            [CURSOR_WORKSPACE_SELECTION_KEY],
            |row| row.get::<_, SqlValue>(0),
        )
        .optional()?
    else {
        return Ok(());
    };
    if updated_workspace_selection(&value, ids)?.is_some() {
        bail!("Cursor IDE workspace selection still references deleted composer");
    }
    Ok(())
}

fn restore_workspace_deletion_plans(plans: &[WorkspaceDeletionPlan]) -> Result<()> {
    for plan in plans.iter().rev() {
        let mut connection = open_workspace_database(&plan.database)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        validate_workspace_schema(&transaction)?;
        let restored = transaction.execute(
            "UPDATE ItemTable SET value = ?2 WHERE key = ?1 AND value = ?3",
            params![
                CURSOR_WORKSPACE_SELECTION_KEY,
                &plan.original,
                &plan.updated
            ],
        )?;
        if restored != 1 {
            bail!("Cursor IDE workspace selection could not be restored");
        }
        transaction.commit()?;
    }
    Ok(())
}

fn combine_workspace_restore_error(error: anyhow::Error, restore: Result<()>) -> anyhow::Error {
    match restore {
        Ok(()) => error,
        Err(restore_error) => error.context(format!(
            "Cursor IDE deletion failed and workspace selection rollback also failed: {restore_error}"
        )),
    }
}

fn verify_cursor_deletion(database: &Path, ids: &BTreeSet<String>) -> Result<()> {
    let connection = Connection::open_with_flags(
        database,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    connection.pragma_update(None, "query_only", "ON")?;
    for id in ids {
        let patterns = [
            cursor_namespaced_key_prefix("bubbleId", id),
            cursor_namespaced_key_prefix("checkpointId", id),
            cursor_namespaced_key_prefix("codeBlockDiff", id),
            cursor_namespaced_key_prefix("codeBlockPartialInlineDiffFates", id),
            cursor_namespaced_key_prefix("ofsContent", id),
        ];
        let headers: i64 = connection.query_row(
            "SELECT COUNT(*) FROM composerHeaders WHERE composerId = ?1",
            [id],
            |row| row.get(0),
        )?;
        let namespaced: i64 = connection.query_row(
            "SELECT COUNT(*) FROM cursorDiskKV
             WHERE key = ?1
                OR substr(key, 1, length(?2)) = ?2 COLLATE BINARY
                OR substr(key, 1, length(?3)) = ?3 COLLATE BINARY
                OR substr(key, 1, length(?4)) = ?4 COLLATE BINARY
                OR substr(key, 1, length(?5)) = ?5 COLLATE BINARY
                OR substr(key, 1, length(?6)) = ?6 COLLATE BINARY",
            params![
                format!("composerData:{id}"),
                &patterns[0],
                &patterns[1],
                &patterns[2],
                &patterns[3],
                &patterns[4],
            ],
            |row| row.get(0),
        )?;
        if headers != 0 || namespaced != 0 {
            bail!("Cursor IDE deletion read-back found selected composer records");
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn rollback_store(import: &CursorIdeImport) -> Result<()> {
    rollback_store_locked(import)
}

#[cfg(test)]
fn rollback_store_with_lock(import: &CursorIdeImport) -> Result<()> {
    let _guard = lock_metadata_root(import)?;
    rollback_store_locked(import)
}

fn rollback_store_locked(import: &CursorIdeImport) -> Result<()> {
    let mut connection = open_write_database(import)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    validate_schema(&transaction)?;
    validate_exact_rows(&transaction, import)?;
    restore_workspace_selection(import)?;
    let created_record_keys = import
        .created_record_keys
        .lock()
        .map_err(|_| anyhow::anyhow!("Cursor IDE import state lock is poisoned"))?
        .clone();
    if created_record_keys.is_empty() {
        bail!("Cursor IDE rollback has no materialization record");
    }
    let other_references = other_composer_blob_references(&transaction, &import.target.id)?;
    for key in &created_record_keys {
        if key.starts_with(CURSOR_BLOB_PREFIX)
            && other_references
                .as_ref()
                .is_none_or(|references| references.contains(key))
        {
            continue;
        }
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

fn lock_metadata_root(import: &CursorIdeImport) -> Result<PrivateStoreGuard> {
    lock_root(&import.metadata_root, import.lock_root.as_deref())
}

fn lock_root(root: &Path, configured_lock_root: Option<&Path>) -> Result<PrivateStoreGuard> {
    private_store_lock::acquire(
        root,
        CURSOR_IDE_LOCK_NAMESPACE,
        "Cursor IDE",
        configured_lock_root,
    )
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

fn materialize_workspace_selection(import: &CursorIdeImport) -> Result<()> {
    let Some(database) = &import.workspace_database else {
        return Ok(());
    };
    let mut connection = open_workspace_database(database)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    validate_workspace_schema(&transaction)?;
    let previous = transaction
        .query_row(
            "SELECT value FROM ItemTable WHERE key = ?1",
            [CURSOR_WORKSPACE_SELECTION_KEY],
            |row| row.get::<_, SqlValue>(0),
        )
        .optional()?;
    let previous = match previous {
        Some(value) => WorkspaceSelectionState::Present(value),
        None => WorkspaceSelectionState::Missing,
    };
    *import
        .previous_workspace_selection
        .lock()
        .map_err(|_| anyhow::anyhow!("Cursor IDE workspace selection lock is poisoned"))? =
        previous;
    transaction.execute(
        "INSERT OR REPLACE INTO ItemTable(key, value) VALUES (?1, ?2)",
        params![CURSOR_WORKSPACE_SELECTION_KEY, workspace_selection(import)?],
    )?;
    transaction.commit()?;
    Ok(())
}

fn verify_workspace_selection(import: &CursorIdeImport) -> Result<()> {
    let Some(database) = &import.workspace_database else {
        return Ok(());
    };
    let selection_was_not_captured = matches!(
        *import
            .previous_workspace_selection
            .lock()
            .map_err(|_| anyhow::anyhow!("Cursor IDE workspace selection lock is poisoned"))?,
        WorkspaceSelectionState::NotCaptured
    );
    if selection_was_not_captured {
        return Ok(());
    }
    let connection = open_workspace_database(database)?;
    validate_workspace_schema(&connection)?;
    let actual = connection
        .query_row(
            "SELECT value FROM ItemTable WHERE key = ?1",
            [CURSOR_WORKSPACE_SELECTION_KEY],
            |row| row.get::<_, SqlValue>(0),
        )
        .optional()?;
    if actual != Some(SqlValue::Text(workspace_selection(import)?)) {
        bail!("Cursor IDE workspace did not retain imported chat selection");
    }
    Ok(())
}

fn restore_workspace_selection(import: &CursorIdeImport) -> Result<()> {
    let Some(database) = &import.workspace_database else {
        return Ok(());
    };
    let previous = import
        .previous_workspace_selection
        .lock()
        .map_err(|_| anyhow::anyhow!("Cursor IDE workspace selection lock is poisoned"))?
        .clone();
    if matches!(previous, WorkspaceSelectionState::NotCaptured) {
        return Ok(());
    }
    let mut connection = open_workspace_database(database)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    validate_workspace_schema(&transaction)?;
    let actual = transaction
        .query_row(
            "SELECT value FROM ItemTable WHERE key = ?1",
            [CURSOR_WORKSPACE_SELECTION_KEY],
            |row| row.get::<_, SqlValue>(0),
        )
        .optional()?;
    let original = match &previous {
        WorkspaceSelectionState::Present(value) => Some(value.clone()),
        WorkspaceSelectionState::Missing => None,
        WorkspaceSelectionState::NotCaptured => unreachable!(),
    };
    if actual == original {
        return Ok(());
    }
    if actual != Some(SqlValue::Text(workspace_selection(import)?)) {
        bail!("Cursor IDE workspace selection changed after import");
    }
    match previous {
        WorkspaceSelectionState::Present(value) => {
            transaction.execute(
                "INSERT OR REPLACE INTO ItemTable(key, value) VALUES (?1, ?2)",
                params![CURSOR_WORKSPACE_SELECTION_KEY, value],
            )?;
        }
        WorkspaceSelectionState::Missing => {
            transaction.execute(
                "DELETE FROM ItemTable WHERE key = ?1",
                [CURSOR_WORKSPACE_SELECTION_KEY],
            )?;
        }
        WorkspaceSelectionState::NotCaptured => unreachable!(),
    }
    transaction.commit()?;
    Ok(())
}

fn workspace_selection(import: &CursorIdeImport) -> Result<String> {
    Ok(serde_json::to_string(&json!({
        "selectedComposerIds": [import.target.id],
        "lastFocusedComposerIds": [import.target.id],
        "hasMigratedComposerData": true,
        "hasMigratedMultipleComposers": true,
    }))?)
}

fn open_workspace_database(database: &Path) -> Result<Connection> {
    let connection = Connection::open_with_flags(
        database,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .context("opening Cursor IDE workspace state for guarded write")?;
    connection.busy_timeout(Duration::ZERO)?;
    Ok(connection)
}

fn validate_workspace_schema(connection: &Connection) -> Result<()> {
    let sql = connection
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'ItemTable'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .context("Cursor IDE workspace state omitted `ItemTable`")?;
    if normalize_sql(&sql) != CURSOR_WORKSPACE_ITEM_TABLE_SCHEMA {
        bail!("Cursor IDE workspace state schema is not verified");
    }
    Ok(())
}

fn created_record_keys(
    transaction: &Transaction<'_>,
    import: &CursorIdeImport,
) -> Result<BTreeSet<String>> {
    let header_exists: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM composerHeaders WHERE composerId = ?1)",
        [&import.target.id],
        |row| row.get(0),
    )?;
    if header_exists {
        bail!("generated Cursor IDE target header already exists");
    }
    let mut created = BTreeSet::new();
    for (key, expected) in &import.records {
        let actual = transaction
            .query_row(
                "SELECT value FROM cursorDiskKV WHERE key = ?1",
                [key],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()?;
        match actual {
            None => {
                created.insert(key.clone());
            }
            Some(actual) if key.starts_with(CURSOR_BLOB_PREFIX) && actual == *expected => {}
            Some(_) => bail!("generated Cursor IDE target key already exists"),
        }
    }
    Ok(created)
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
    let created_record_keys = import
        .created_record_keys
        .lock()
        .map_err(|_| anyhow::anyhow!("Cursor IDE import state lock is poisoned"))?
        .clone();
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
    let other_references = other_composer_blob_references(&connection, &import.target.id)?;
    for key in &created_record_keys {
        let count = connection.query_row(
            "SELECT COUNT(*) FROM cursorDiskKV WHERE key = ?1",
            [key],
            |row| row.get::<_, i64>(0),
        )?;
        let may_remain_shared = key.starts_with(CURSOR_BLOB_PREFIX)
            && other_references
                .as_ref()
                .is_none_or(|references| references.contains(key));
        if count != 0 && !may_remain_shared {
            return Ok(false);
        }
    }
    Ok(true)
}

fn other_composer_blob_references(
    connection: &Connection,
    excluded_composer_id: &str,
) -> Result<Option<BTreeSet<String>>> {
    let mut statement = connection.prepare(
        "SELECT key, value FROM cursorDiskKV
         WHERE key LIKE 'composerData:%' AND key <> ?1",
    )?;
    let excluded_key = format!("composerData:{excluded_composer_id}");
    let mut rows = statement.query([excluded_key])?;
    let mut roots = 0usize;
    let mut references = BTreeSet::new();
    while let Some(row) = rows.next()? {
        roots += 1;
        if roots > MAX_CURSOR_NATIVE_GRAPH_RECORDS {
            return Ok(None);
        }
        let bytes = row.get::<_, Vec<u8>>(1)?;
        if bytes.len() > MAX_CURSOR_NATIVE_RECORD_SIZE {
            return Ok(None);
        }
        let Ok(root) = serde_json::from_slice::<Value>(&bytes) else {
            return Ok(None);
        };
        let Some(encoded) = root
            .get("conversationState")
            .and_then(Value::as_str)
            .and_then(|value| value.strip_prefix('~'))
        else {
            continue;
        };
        let Ok(state_bytes) = BASE64_STANDARD.decode(encoded) else {
            return Ok(None);
        };
        let Ok(state) = CursorConversationStateStructure::decode(state_bytes.as_slice()) else {
            return Ok(None);
        };
        if state.turns.len() > MAX_CURSOR_NATIVE_GRAPH_RECORDS {
            return Ok(None);
        }
        for blob_id in state.root_prompt_messages_json {
            references.insert(format!("{CURSOR_BLOB_PREFIX}{}", hex::encode(blob_id)));
        }
        for turn_id in state.turns {
            let turn_key = format!("{CURSOR_BLOB_PREFIX}{}", hex::encode(&turn_id));
            references.insert(turn_key.clone());
            let turn_bytes = connection
                .query_row(
                    "SELECT value FROM cursorDiskKV WHERE key = ?1",
                    [&turn_key],
                    |row| row.get::<_, Vec<u8>>(0),
                )
                .optional()?;
            let Some(turn_bytes) = turn_bytes else {
                return Ok(None);
            };
            let Ok(turn) = CursorConversationTurnStructure::decode(turn_bytes.as_slice()) else {
                return Ok(None);
            };
            if let Some(CursorConversationTurn::Agent(turn)) = turn.turn {
                let user_key = format!("{CURSOR_BLOB_PREFIX}{}", hex::encode(turn.user_message));
                references.insert(user_key.clone());
                let user_bytes = connection
                    .query_row(
                        "SELECT value FROM cursorDiskKV WHERE key = ?1",
                        [&user_key],
                        |row| row.get::<_, Vec<u8>>(0),
                    )
                    .optional()?;
                let Some(user_bytes) = user_bytes else {
                    return Ok(None);
                };
                let Ok(user) = CursorUserMessage::decode(user_bytes.as_slice()) else {
                    return Ok(None);
                };
                if !user.conversation_state_blob_id.is_empty() {
                    references.insert(format!(
                        "{CURSOR_BLOB_PREFIX}{}",
                        hex::encode(user.conversation_state_blob_id)
                    ));
                }
                for step in turn.steps {
                    references.insert(format!("{CURSOR_BLOB_PREFIX}{}", hex::encode(step)));
                }
            }
        }
    }
    Ok(Some(references))
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
        bail!("Cursor IDE process {pid} is running; close Cursor before native store mutation");
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn ensure_cursor_idle() -> Result<()> {
    let output = Command::new("/bin/ps")
        .args(["-axo", "pid=,comm="])
        .output()
        .context("inspecting Cursor IDE process state")?;
    if !output.status.success() {
        bail!("could not inspect Cursor IDE process state");
    }
    if let Some(pid) = cursor_pid_from_macos_ps(&String::from_utf8_lossy(&output.stdout)) {
        bail!("Cursor IDE process {pid} is running; close Cursor before native store mutation");
    }
    Ok(())
}

#[cfg(any(target_os = "macos", test))]
fn cursor_pid_from_macos_ps(output: &str) -> Option<u32> {
    let own_pid = std::process::id();
    output.lines().find_map(|line| {
        let line = line.trim_start();
        let split = line.find(char::is_whitespace)?;
        let pid = line[..split].parse::<u32>().ok()?;
        if pid == own_pid {
            return None;
        }
        let command = line[split..].trim().to_ascii_lowercase();
        let executable = Path::new(&command)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(&command);
        (command.contains("/cursor.app/")
            || executable == "cursor"
            || executable.starts_with("cursor helper"))
        .then_some(pid)
    })
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn ensure_cursor_idle() -> Result<()> {
    bail!("Cursor IDE active-writer detection is supported on Linux and macOS")
}

fn safe_database_path(metadata_root: &Path) -> Result<PathBuf> {
    let global_storage = metadata_root.join("globalStorage");
    validate_cursor_directory(&global_storage, metadata_root)?;
    let candidate = global_storage.join("state.vscdb");
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

fn safe_workspace_database(metadata_root: &Path, workspace_id: &str) -> Result<Option<PathBuf>> {
    let mut components = Path::new(workspace_id).components();
    if workspace_id.is_empty()
        || !matches!(components.next(), Some(std::path::Component::Normal(_)))
        || components.next().is_some()
    {
        bail!("Cursor IDE workspace ID is not a safe path component");
    }
    let workspace_storage = metadata_root.join("workspaceStorage");
    if fs::symlink_metadata(&workspace_storage)
        .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
    {
        return Ok(None);
    }
    validate_cursor_directory(&workspace_storage, metadata_root)?;
    let workspace_root = workspace_storage.join(workspace_id);
    let workspace_metadata = match fs::symlink_metadata(&workspace_root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !workspace_metadata.is_dir() || workspace_metadata.file_type().is_symlink() {
        bail!("Cursor IDE workspace metadata is not a safe directory");
    }
    let workspace_root = fs::canonicalize(workspace_root)?;
    if !workspace_root.starts_with(&workspace_storage) {
        bail!("Cursor IDE workspace metadata escaped configured root");
    }
    let candidate = workspace_root.join("state.vscdb");
    let Ok(metadata) = fs::symlink_metadata(&candidate) else {
        return Ok(None);
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        bail!("Cursor IDE workspace database is not a safe regular file");
    }
    let database = fs::canonicalize(candidate)?;
    if !database.starts_with(metadata_root) {
        bail!("Cursor IDE workspace database escaped configured root");
    }
    Ok(Some(database))
}

fn validate_cursor_directory(path: &Path, metadata_root: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("Cursor IDE directory `{}` was not found", path.display()))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        bail!("Cursor IDE directory `{}` is not safe", path.display());
    }
    let canonical = fs::canonicalize(path)?;
    if !canonical.starts_with(metadata_root) {
        bail!("Cursor IDE directory escaped configured root");
    }
    Ok(())
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
    Ok(json!({
        "$mid": 1,
        "external": file_uri(path),
        "fsPath": path,
        "path": path,
        "scheme": "file",
    }))
}

pub fn launch_args(workspace: &Path, target: &SessionRef) -> Result<Vec<String>> {
    if target.provider != Provider::CursorIde {
        bail!("Cursor IDE launch requires a cursor-ide session");
    }
    let path = workspace
        .to_str()
        .context("Cursor IDE launch requires a UTF-8 workspace path")?;
    Ok(vec!["--folder-uri".to_owned(), file_uri(path)])
}

pub fn opens_imported_chat(import: &CursorIdeImport) -> bool {
    import.workspace_database.is_some()
}

fn file_uri(path: &str) -> String {
    let mut uri = String::from("file://");
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_' | b'.' | b'~') {
            uri.push(char::from(byte));
        } else {
            use std::fmt::Write as _;
            write!(uri, "%{byte:02X}").expect("writing to String cannot fail");
        }
    }
    uri
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
    default_cursor_ide_root().context("home directory is unavailable")
}

#[cfg(target_os = "windows")]
fn default_cursor_ide_root() -> Option<PathBuf> {
    env::var_os("APPDATA")
        .map(PathBuf::from)
        .map(|root| root.join("Cursor/User"))
}

#[cfg(target_os = "macos")]
fn default_cursor_ide_root() -> Option<PathBuf> {
    BaseDirs::new().map(|directories| {
        directories
            .home_dir()
            .join("Library/Application Support/Cursor/User")
    })
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn default_cursor_ide_root() -> Option<PathBuf> {
    BaseDirs::new().map(|directories| directories.config_dir().join("Cursor/User"))
}

#[cfg(test)]
pub(crate) fn create_fixture_store(metadata_root: &Path, workspace: &Path) -> Result<()> {
    fs::create_dir_all(workspace)?;
    #[cfg(target_os = "linux")]
    let workspace_id = linux_workspace_id(workspace)?;
    #[cfg(not(target_os = "linux"))]
    let workspace_id = "cursor-ide-fixture".to_owned();
    fs::create_dir_all(metadata_root.join("globalStorage"))?;
    fs::create_dir_all(metadata_root.join("workspaceStorage").join(&workspace_id))?;
    fs::write(
        metadata_root
            .join("workspaceStorage")
            .join(&workspace_id)
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
    let workspace_connection = Connection::open(
        metadata_root
            .join("workspaceStorage")
            .join(workspace_id)
            .join("state.vscdb"),
    )?;
    workspace_connection.execute_batch(
        "CREATE TABLE ItemTable (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB);",
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        path::PathBuf,
        sync::{Arc, mpsc},
        time::Duration,
    };

    use chrono::Utc;
    use omnis_ir::{
        EventKind, EventSource, GitState, OmniEvent, ReplayPolicy, SCHEMA_VERSION, Sensitivity,
        WorkspaceSnapshot,
    };

    use super::*;

    fn assert_mutation_waits<F>(metadata_root: &Path, configured_locks: &Path, mutation: F)
    where
        F: FnOnce() -> Result<()> + Send + 'static,
    {
        let guard = lock_root(metadata_root, Some(configured_locks))
            .expect("hold Cursor IDE provider root lock");
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            sender.send(mutation()).expect("report private mutation");
        });
        assert!(matches!(
            receiver.recv_timeout(Duration::from_millis(100)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        drop(guard);
        receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("private mutation unblocked")
            .expect("complete private mutation");
    }

    #[test]
    fn private_mutations_wait_for_provider_root_lock() {
        let fixture = fixture_store();
        let snapshot = fixture_snapshot(&fixture.workspace);
        let configured_locks = fixture.temporary.path().join("locks/cursor-ide");
        let import = Arc::new(
            build_with_lock_root(
                &snapshot,
                &fixture.workspace,
                fixture.root.clone(),
                configured_locks.clone(),
            )
            .expect("build Cursor IDE import"),
        );

        let materialize_import = Arc::clone(&import);
        assert_mutation_waits(&fixture.root, &configured_locks, move || {
            materialize_store_with_lock(&materialize_import)
        });

        let rollback_import = Arc::clone(&import);
        assert_mutation_waits(&fixture.root, &configured_locks, move || {
            rollback_store_with_lock(&rollback_import)
        });

        let deletion_import = build_with_lock_root(
            &snapshot,
            &fixture.workspace,
            fixture.root.clone(),
            configured_locks.clone(),
        )
        .expect("build Cursor IDE deletion fixture");
        materialize_store(&deletion_import).expect("materialize deletion fixture");
        let session = deletion_import.target.clone();
        let metadata_root = fixture.root.clone();
        let deletion_lock_root = configured_locks.clone();
        assert_mutation_waits(&fixture.root, &configured_locks, move || {
            delete_session_at_with_lock_root(&session, &metadata_root, &deletion_lock_root)
        });
        assert!(
            CursorIdeAdapter::with_root(&fixture.root)
                .list_sessions(None)
                .expect("deletion read-back")
                .iter()
                .all(|candidate| candidate.session != deletion_import.target)
        );
        assert!(!fixture.root.join(".omnisession.lock").exists());
    }

    #[test]
    fn native_store_round_trip_and_exact_rollback() {
        let fixture = fixture_store();
        let snapshot = fixture_snapshot(&fixture.workspace);
        let import = build_with_root(&snapshot, &fixture.workspace, fixture.root.clone())
            .expect("build Cursor IDE import");
        fs::remove_file(
            fixture
                .root
                .join("workspaceStorage")
                .join(&import.workspace_id)
                .join("workspace.json"),
        )
        .expect("remove external workspace mapping");

        materialize_store(&import).expect("materialize Cursor IDE import");
        let readback = CursorIdeAdapter::with_root(&fixture.root)
            .read_session(&import.target)
            .expect("read Cursor IDE import");
        assert_eq!(
            readback.workspace.root,
            fs::canonicalize(&fixture.workspace).expect("canonical workspace")
        );
        rollback_store(&import).expect("exact-row rollback");
        assert!(generated_rows_absent(&import).expect("rollback read-back"));
    }

    #[test]
    fn deletes_exact_cursor_ide_composer_and_keeps_blob_store() {
        let fixture = fixture_store();
        let snapshot = fixture_snapshot(&fixture.workspace);
        let import = build_with_root(&snapshot, &fixture.workspace, fixture.root.clone())
            .expect("build Cursor IDE import");
        materialize_store(&import).expect("materialize Cursor IDE import");
        let blob_count_before: i64 = Connection::open(&import.database)
            .expect("open global database")
            .query_row(
                "SELECT COUNT(*) FROM cursorDiskKV WHERE key LIKE 'agentKv:blob:%'",
                [],
                |row| row.get(0),
            )
            .expect("blob count");

        delete_session_at(&import.target, &fixture.root)
            .expect("delete selected Cursor IDE composer");

        assert!(
            CursorIdeAdapter::with_root(&fixture.root)
                .list_sessions(None)
                .expect("list Cursor IDE sessions")
                .iter()
                .all(|session| session.session != import.target)
        );
        let connection = Connection::open(&import.database).expect("open global database");
        let blob_count_after: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM cursorDiskKV WHERE key LIKE 'agentKv:blob:%'",
                [],
                |row| row.get(0),
            )
            .expect("blob count");
        assert_eq!(blob_count_after, blob_count_before);
    }

    #[test]
    fn deletion_follows_subcomposers_from_composer_root() {
        let fixture = fixture_store();
        let snapshot = fixture_snapshot(&fixture.workspace);
        let parent = build_with_root(&snapshot, &fixture.workspace, fixture.root.clone())
            .expect("build parent Cursor IDE import");
        let child = build_with_root(&snapshot, &fixture.workspace, fixture.root.clone())
            .expect("build child Cursor IDE import");
        materialize_store(&parent).expect("materialize parent");
        materialize_store(&child).expect("materialize child");
        let connection = Connection::open(&parent.database).expect("global database");
        let parent_key = format!("composerData:{}", parent.target.id);
        let raw: Vec<u8> = connection
            .query_row(
                "SELECT value FROM cursorDiskKV WHERE key = ?1",
                [&parent_key],
                |row| row.get(0),
            )
            .expect("parent composer root");
        let mut root: Value = serde_json::from_slice(&raw).expect("parent root JSON");
        root["subComposerIds"] = json!([child.target.id.clone()]);
        connection
            .execute(
                "UPDATE cursorDiskKV SET value = ?2 WHERE key = ?1",
                params![parent_key, serde_json::to_vec(&root).expect("updated root")],
            )
            .expect("link child from composer root");

        delete_session_at(&parent.target, &fixture.root).expect("delete parent and child");

        for id in [&parent.target.id, &child.target.id] {
            let headers: i64 = connection
                .query_row(
                    "SELECT COUNT(*) FROM composerHeaders WHERE composerId = ?1",
                    [id],
                    |row| row.get(0),
                )
                .expect("header count");
            let roots: i64 = connection
                .query_row(
                    "SELECT COUNT(*) FROM cursorDiskKV WHERE key = ?1",
                    [format!("composerData:{id}")],
                    |row| row.get(0),
                )
                .expect("root count");
            assert_eq!((headers, roots), (0, 0));
        }
    }

    #[test]
    fn deletion_matches_descendant_ids_case_sensitively() {
        let fixture = fixture_store();
        let snapshot = fixture_snapshot(&fixture.workspace);
        let parent = build_with_root(&snapshot, &fixture.workspace, fixture.root.clone())
            .expect("build parent Cursor IDE import");
        materialize_store(&parent).expect("materialize parent");

        let child_id = "child_a";
        let ids = [child_id, "childXa", "CHILD_A"];
        let prefixes = [
            "bubbleId",
            "checkpointId",
            "codeBlockDiff",
            "codeBlockPartialInlineDiffFates",
            "ofsContent",
        ];
        let connection = Connection::open(&parent.database).expect("open global database");
        let parent_key = format!("composerData:{}", parent.target.id);
        let raw: Vec<u8> = connection
            .query_row(
                "SELECT value FROM cursorDiskKV WHERE key = ?1",
                [&parent_key],
                |row| row.get(0),
            )
            .expect("parent composer root");
        let mut root: Value = serde_json::from_slice(&raw).expect("parent root JSON");
        root["subComposerIds"] = json!([child_id]);
        connection
            .execute(
                "UPDATE cursorDiskKV SET value = ?2 WHERE key = ?1",
                params![parent_key, serde_json::to_vec(&root).expect("updated root")],
            )
            .expect("link child from composer root");

        for prefix in prefixes {
            for id in ids {
                connection
                    .execute(
                        "INSERT INTO cursorDiskKV(key, value) VALUES (?1, ?2)",
                        params![format!("{prefix}:{id}:synthetic"), id.as_bytes()],
                    )
                    .expect("insert synthetic namespaced record");
            }
        }
        for id in ids {
            connection
                .execute(
                    "INSERT INTO cursorDiskKV(key, value) VALUES (?1, ?2)",
                    params![format!("composerData:{id}"), id.as_bytes()],
                )
                .expect("insert synthetic composer root");
        }
        drop(connection);

        delete_session_at(&parent.target, &fixture.root)
            .expect("delete selected Cursor IDE composer and descendant");

        let connection = Connection::open(&parent.database).expect("reopen global database");
        for prefix in prefixes {
            for id in ids {
                let expected_count = i64::from(id != child_id);
                let count: i64 = connection
                    .query_row(
                        "SELECT COUNT(*) FROM cursorDiskKV WHERE key = ?1",
                        [format!("{prefix}:{id}:synthetic")],
                        |row| row.get(0),
                    )
                    .expect("check synthetic namespaced record");
                assert_eq!(count, expected_count, "unexpected count for {prefix}:{id}");
            }
        }
        for id in ids {
            let expected_count = i64::from(id != child_id);
            let count: i64 = connection
                .query_row(
                    "SELECT COUNT(*) FROM cursorDiskKV WHERE key = ?1",
                    [format!("composerData:{id}")],
                    |row| row.get(0),
                )
                .expect("check synthetic composer root");
            assert_eq!(
                count, expected_count,
                "unexpected composer root count for {id}"
            );
        }
    }

    #[test]
    fn deletion_refuses_unverified_workspace_schema_before_global_mutation() {
        let fixture = fixture_store();
        let snapshot = fixture_snapshot(&fixture.workspace);
        let import = build_with_root(&snapshot, &fixture.workspace, fixture.root.clone())
            .expect("build Cursor IDE import");
        materialize_store(&import).expect("materialize Cursor IDE import");
        let workspace_database = import
            .workspace_database
            .as_ref()
            .expect("fixture workspace database");
        Connection::open(workspace_database)
            .expect("workspace database")
            .execute_batch(
                "DROP TABLE ItemTable;
                 CREATE TABLE ItemTable (key TEXT PRIMARY KEY, value BLOB, unknown TEXT);",
            )
            .expect("replace workspace schema");

        let error = delete_session_at(&import.target, &fixture.root)
            .expect_err("unverified workspace schema must be rejected");

        assert!(error.to_string().contains("schema is not verified"));
        let headers: i64 = Connection::open(&import.database)
            .expect("global database")
            .query_row(
                "SELECT COUNT(*) FROM composerHeaders WHERE composerId = ?1",
                [&import.target.id],
                |row| row.get(0),
            )
            .expect("header count");
        assert_eq!(headers, 1);
    }

    #[test]
    fn workspace_failure_restores_already_applied_selection_plans() {
        let fixture = fixture_store();
        let id = Uuid::new_v4().to_string();
        let ids = BTreeSet::from([id.clone()]);
        let original = serde_json::to_string(&json!({
            "selectedComposerIds": [id.clone()],
            "lastFocusedComposerIds": [id],
        }))
        .expect("original workspace selection");
        let import = build_with_root(
            &fixture_snapshot(&fixture.workspace),
            &fixture.workspace,
            fixture.root.clone(),
        )
        .expect("build Cursor IDE import");
        let first_database = import
            .workspace_database
            .expect("fixture workspace database");
        Connection::open(&first_database)
            .expect("first workspace database")
            .execute(
                "INSERT INTO ItemTable(key, value) VALUES (?1, ?2)",
                params![CURSOR_WORKSPACE_SELECTION_KEY, &original],
            )
            .expect("first workspace selection");
        let second_root = fixture.root.join("workspaceStorage/second");
        fs::create_dir(&second_root).expect("second workspace root");
        let second_database = second_root.join("state.vscdb");
        let second_connection =
            Connection::open(&second_database).expect("second workspace database");
        second_connection
            .execute_batch(CURSOR_WORKSPACE_ITEM_TABLE_SCHEMA)
            .expect("second workspace schema");
        second_connection
            .execute(
                "INSERT INTO ItemTable(key, value) VALUES (?1, ?2)",
                params![CURSOR_WORKSPACE_SELECTION_KEY, &original],
            )
            .expect("second workspace selection");
        drop(second_connection);
        let plans = [&first_database, &second_database]
            .into_iter()
            .map(|database| {
                plan_workspace_selection_deletion(database, &ids)
                    .expect("workspace plan")
                    .expect("changed workspace plan")
            })
            .collect::<Vec<_>>();
        Connection::open(&second_database)
            .expect("second workspace database")
            .execute_batch(
                "CREATE TRIGGER refuse_selection_update
                 BEFORE UPDATE ON ItemTable
                 BEGIN SELECT RAISE(ABORT, 'synthetic workspace failure'); END;",
            )
            .expect("failure trigger");

        apply_workspace_deletion_plans(&plans, &ids)
            .expect_err("second workspace failure must roll back first");

        let restored: String = Connection::open(&first_database)
            .expect("first workspace database")
            .query_row(
                "SELECT value FROM ItemTable WHERE key = ?1",
                [CURSOR_WORKSPACE_SELECTION_KEY],
                |row| row.get(0),
            )
            .expect("restored workspace selection");
        assert_eq!(restored, original);
    }

    #[test]
    fn workspace_selection_opens_target_and_rolls_back() {
        let fixture = fixture_store();
        let snapshot = fixture_snapshot(&fixture.workspace);
        let import = build_with_root(&snapshot, &fixture.workspace, fixture.root.clone())
            .expect("build Cursor IDE import");
        let database = import
            .workspace_database
            .as_ref()
            .expect("fixture workspace database");
        let connection = Connection::open(database).expect("open workspace database");
        connection
            .execute(
                "INSERT INTO ItemTable(key, value) VALUES (?1, ?2)",
                params![CURSOR_WORKSPACE_SELECTION_KEY, b"previous-selection"],
            )
            .expect("previous selection");

        materialize_store(&import).expect("materialize Cursor IDE import");
        let selected: String = connection
            .query_row(
                "SELECT value FROM ItemTable WHERE key = ?1",
                [CURSOR_WORKSPACE_SELECTION_KEY],
                |row| row.get(0),
            )
            .expect("selected imported chat");
        assert_eq!(
            selected,
            workspace_selection(&import).expect("selection JSON")
        );

        rollback_store(&import).expect("rollback Cursor IDE import");
        let restored: Vec<u8> = connection
            .query_row(
                "SELECT value FROM ItemTable WHERE key = ?1",
                [CURSOR_WORKSPACE_SELECTION_KEY],
                |row| row.get(0),
            )
            .expect("restored previous selection");
        assert_eq!(restored, b"previous-selection");
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
    fn workspace_selection_failure_rolls_back_global_rows() {
        let fixture = fixture_store();
        let snapshot = fixture_snapshot(&fixture.workspace);
        let import = build_with_root(&snapshot, &fixture.workspace, fixture.root.clone())
            .expect("build Cursor IDE import");
        let database = import
            .workspace_database
            .as_ref()
            .expect("fixture workspace database");
        Connection::open(database)
            .expect("open workspace database")
            .execute_batch(
                "CREATE TRIGGER reject_selection BEFORE INSERT ON ItemTable
                 BEGIN SELECT RAISE(ABORT, 'synthetic selection failure'); END;",
            )
            .expect("selection failure trigger");

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
    fn rollback_preserves_rewind_anchor_shared_by_another_composer() {
        let fixture = fixture_store();
        let snapshot = fixture_snapshot(&fixture.workspace);
        let import = build_with_root(&snapshot, &fixture.workspace, fixture.root.clone())
            .expect("build Cursor IDE import");
        materialize_store(&import).expect("materialize Cursor IDE import");
        let root_key = format!("composerData:{}", import.target.id);
        let root_bytes = import.records.get(&root_key).expect("composer root");
        let root: Value = serde_json::from_slice(root_bytes).expect("parse composer root");
        let state = CursorConversationStateStructure::decode(
            BASE64_STANDARD
                .decode(
                    root.get("conversationState")
                        .and_then(Value::as_str)
                        .and_then(|value| value.strip_prefix('~'))
                        .expect("conversation state"),
                )
                .expect("decode state")
                .as_slice(),
        )
        .expect("parse state");
        let turn = CursorConversationTurnStructure::decode(
            import.records[&format!("{CURSOR_BLOB_PREFIX}{}", hex::encode(&state.turns[0]))]
                .as_slice(),
        )
        .expect("parse turn");
        let Some(CursorConversationTurn::Agent(turn)) = turn.turn else {
            panic!("agent turn");
        };
        let user = CursorUserMessage::decode(
            import.records[&format!("{CURSOR_BLOB_PREFIX}{}", hex::encode(turn.user_message))]
                .as_slice(),
        )
        .expect("parse user message");
        let anchor_key = format!(
            "{CURSOR_BLOB_PREFIX}{}",
            hex::encode(user.conversation_state_blob_id)
        );
        let connection = Connection::open(&import.database).expect("open global database");
        connection
            .execute(
                "INSERT INTO cursorDiskKV(key, value) VALUES (?1, ?2)",
                params!["composerData:synthetic-fork", root_bytes],
            )
            .expect("fork composer root");

        rollback_store(&import).expect("rollback imported composer");
        assert_eq!(
            connection
                .query_row(
                    "SELECT COUNT(*) FROM cursorDiskKV WHERE key = ?1",
                    [&anchor_key],
                    |row| row.get::<_, i64>(0),
                )
                .expect("shared anchor count"),
            1
        );
    }

    #[test]
    fn version_probe_rejects_invalid_executable() {
        let temporary = tempfile::tempdir().expect("temporary Cursor bundle");
        let binary = temporary.path().join("cursor");
        fs::write(&binary, b"not Cursor").expect("fake Cursor binary");
        assert!(ensure_supported(&binary).is_err());
    }

    #[test]
    fn version_gate_accepts_newer_cursor_ide_releases() {
        assert!(!is_supported_version("3.12.16"));
        assert!(is_supported_version("3.12.17"));
        assert!(is_supported_version("3.13.0"));
    }

    #[test]
    fn appimage_name_exposes_version_without_launching() {
        assert_eq!(
            version_from_path(Path::new("/opt/Cursor-3.13.0-x86_64.AppImage")).as_deref(),
            Some("3.13.0")
        );
    }

    #[test]
    fn macos_bundle_exposes_product_version_without_launching() {
        let temporary = tempfile::tempdir().expect("temporary Cursor bundle");
        let contents = temporary.path().join("Cursor.app/Contents");
        let binary = contents.join("MacOS/Cursor");
        let product = contents.join("Resources/app/product.json");
        fs::create_dir_all(binary.parent().expect("binary parent")).expect("binary directory");
        fs::create_dir_all(product.parent().expect("product parent")).expect("product directory");
        fs::write(&binary, b"synthetic Cursor binary").expect("synthetic binary");
        fs::write(&product, br#"{"version":"3.13.2"}"#).expect("product metadata");

        assert_eq!(
            installed_bundle_version(&binary)
                .expect("bundle version")
                .as_deref(),
            Some("3.13.2")
        );
    }

    #[test]
    fn macos_process_scan_matches_cursor_ide_but_not_cursor_agent() {
        assert_eq!(
            cursor_pid_from_macos_ps("987650 /Applications/Cursor.app/Contents/MacOS/Cursor\n"),
            Some(987_650)
        );
        assert_eq!(
            cursor_pid_from_macos_ps(
                "987651 /Applications/Cursor.app/Contents/Frameworks/Cursor Helper.app/Contents/MacOS/Cursor Helper\n"
            ),
            Some(987_651)
        );
        assert_eq!(
            cursor_pid_from_macos_ps("987652 /Users/dev/.local/bin/cursor-agent\n"),
            None
        );
    }

    #[test]
    fn native_material_matches_cursor_root_contract() {
        let fixture = fixture_store();
        let snapshot = fixture_snapshot(&fixture.workspace);
        let import = build_with_root(&snapshot, &fixture.workspace, fixture.root.clone())
            .expect("build Cursor IDE import");
        let root: Value = serde_json::from_slice(
            import
                .records
                .get(&format!("composerData:{}", import.target.id))
                .expect("composer root"),
        )
        .expect("parse composer root");

        assert_eq!(root.get("_v").and_then(Value::as_i64), Some(17));
        assert_eq!(root.get("isNAL").and_then(Value::as_bool), Some(true));
        let encoded_state = root
            .get("conversationState")
            .and_then(Value::as_str)
            .and_then(|value| value.strip_prefix('~'))
            .expect("encoded conversation state");
        let state = CursorConversationStateStructure::decode(
            BASE64_STANDARD
                .decode(encoded_state)
                .expect("base64 conversation state")
                .as_slice(),
        )
        .expect("protobuf conversation state");
        assert_eq!(state.mode, Some(1));
        assert_eq!(state.root_prompt_messages_json.len(), 3);
        assert_eq!(state.turns.len(), 1);

        let turn = CursorConversationTurnStructure::decode(
            import.records[&format!("{CURSOR_BLOB_PREFIX}{}", hex::encode(&state.turns[0]))]
                .as_slice(),
        )
        .expect("protobuf conversation turn");
        let Some(CursorConversationTurn::Agent(turn)) = turn.turn else {
            panic!("agent conversation turn");
        };
        assert_eq!(turn.steps.len(), 1);
        let user = CursorUserMessage::decode(
            import.records[&format!("{CURSOR_BLOB_PREFIX}{}", hex::encode(&turn.user_message))]
                .as_slice(),
        )
        .expect("protobuf user message");
        assert_eq!(user.text, "Question");
        assert_eq!(user.mode, 1);
        assert!(!user.conversation_state_blob_id.is_empty());
        let step = CursorConversationStep::decode(
            import.records[&format!("{CURSOR_BLOB_PREFIX}{}", hex::encode(&turn.steps[0]))]
                .as_slice(),
        )
        .expect("protobuf assistant step");
        let Some(CursorConversationStepMessage::Assistant(assistant)) = step.message else {
            panic!("assistant conversation step");
        };
        assert_eq!(assistant.text, "Answer");
        assert_eq!(root.get("conversationMap"), Some(&json!({})));
        assert!(root.get("workspaceIdentifier").is_none());

        for value in import
            .records
            .iter()
            .filter(|(key, _)| key.starts_with("bubbleId:"))
            .map(|(_, value)| serde_json::from_slice::<Value>(value).expect("parse Cursor bubble"))
        {
            assert_eq!(value.get("isAgentic").and_then(Value::as_bool), Some(true));
            assert_eq!(
                value.get("unifiedMode").and_then(Value::as_str),
                Some("agent")
            );
        }
    }

    #[test]
    fn launch_targets_exact_composer_in_workspace() {
        let target = SessionRef::new(Provider::CursorIde, "synthetic-target");
        let args = launch_args(Path::new("/tmp/workspace with spaces"), &target)
            .expect("Cursor IDE launch arguments");

        assert_eq!(
            args,
            vec!["--folder-uri", "file:///tmp/workspace%20with%20spaces"]
        );
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
        temporary: tempfile::TempDir,
        root: PathBuf,
        workspace: PathBuf,
    }

    fn fixture_store() -> FixtureStore {
        let temporary = tempfile::tempdir().expect("temporary Cursor IDE root");
        let root = temporary.path().join("Cursor/User");
        let workspace = temporary.path().join("workspace");
        create_fixture_store(&root, &workspace).expect("Cursor IDE fixture store");
        FixtureStore {
            temporary,
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
