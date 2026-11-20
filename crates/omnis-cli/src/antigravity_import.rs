use std::{
    env, fs,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use chrono::{SecondsFormat, Utc};
use directories::BaseDirs;
use omnis_adapters::{AntigravityAdapter, ProviderAdapter};
use omnis_core::{
    HandoffRole, NativeTrajectoryItem, native_trajectory_items, native_trajectory_signature,
    readback_trajectory, redact_secrets,
};
use omnis_ir::{CanonicalSnapshot, Provider, SessionRef};
use prost::Message;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use tempfile::{NamedTempFile, TempPath};
use uuid::Uuid;

use crate::recovery::ImportSource;
use crate::{
    native_path,
    private_store_lock::{self, PrivateStoreGuard},
    provider_compatibility::MINIMUM_ANTIGRAVITY_VERSION,
};

pub(crate) type AntigravityWriteGuard = PrivateStoreGuard;
const TRAJECTORY_TYPE_CASCADE: i64 = 4;
const TRAJECTORY_SOURCE_CLI: i64 = 17;
const STEP_USER_INPUT: i32 = 14;
const STEP_PLANNER_RESPONSE: i32 = 15;
const STEP_MCP_TOOL: i32 = 38;
const STEP_STATUS_DONE: i32 = 3;
const STEP_STATUS_ERROR: i32 = 7;
const STEP_SOURCE_MODEL: i32 = 2;
const STEP_SOURCE_USER_EXPLICIT: i32 = 4;
const MCP_SERVER_NAME: &str = "omnisession";
const ANTIGRAVITY_LOCK_NAMESPACE: &str = "antigravity";
const SUMMARY_WRITE_BUSY_TIMEOUT: Duration = Duration::from_secs(5);

pub struct AntigravityImport {
    pub target: SessionRef,
    pub expected_items: Vec<NativeTrajectoryItem>,
    pub history_items: usize,
    pub tool_events: usize,
    pub native_tool_records: usize,
    pub truncated: bool,
    root: PathBuf,
    conversations_root: PathBuf,
    annotations_root: PathBuf,
    summary_path: PathBuf,
    target_path: PathBuf,
    annotation_path: PathBuf,
    document: Vec<u8>,
    annotation: Vec<u8>,
    summary: SummaryRow,
    lock_root: Option<PathBuf>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SummaryRow {
    conversation_id: String,
    title: String,
    preview: String,
    step_count: i64,
    last_modified_time: String,
    workspace_uris: String,
    status: String,
    source: String,
    project_id: String,
    agent_name: String,
    parent_conversation_id: String,
    nesting_depth: i64,
    battle_id: String,
    winning_conversation_id: String,
    not_fully_idle: bool,
    killed: bool,
    last_user_input_time: String,
    last_user_input_step_index: i64,
    app_data_dir: String,
    raw_summary: Option<Vec<u8>>,
    group_id: String,
}

pub fn build(source: &dyn ImportSource, cwd: &Path) -> Result<AntigravityImport> {
    build_with_root(source, cwd, data_root()?)
}

pub(crate) fn build_with_root(
    source: &dyn ImportSource,
    cwd: &Path,
    root: PathBuf,
) -> Result<AntigravityImport> {
    build_with_roots(source, cwd, root, None)
}

#[cfg(test)]
fn build_with_lock_root(
    source: &dyn ImportSource,
    cwd: &Path,
    root: PathBuf,
    lock_root: PathBuf,
) -> Result<AntigravityImport> {
    build_with_roots(source, cwd, root, Some(lock_root))
}

fn build_with_roots(
    source: &dyn ImportSource,
    cwd: &Path,
    root: PathBuf,
    lock_root: Option<PathBuf>,
) -> Result<AntigravityImport> {
    if !cwd.is_absolute() {
        bail!("Antigravity native import requires an absolute workspace path");
    }
    let canonical_cwd = fs::canonicalize(cwd)
        .with_context(|| format!("canonicalizing Antigravity workspace `{}`", cwd.display()))?;
    if !canonical_cwd.is_dir() {
        bail!("Antigravity native import workspace is not a directory");
    }
    let (snapshot, trajectory) = source.history()?;
    if trajectory.items.is_empty() {
        bail!("source has no visible trajectory eligible for Antigravity import");
    }
    let history_items = trajectory.items.len();
    let native_items = native_trajectory_items(&trajectory);
    let native_tool_records = native_items
        .iter()
        .filter(|item| matches!(item, NativeTrajectoryItem::Tool { .. }))
        .count();
    let expected_items = native_trajectory_signature(&trajectory);
    let id = Uuid::new_v4().to_string();
    let target = SessionRef::new(Provider::Antigravity, &id);
    let trajectory_id = Uuid::new_v4().to_string();
    let initialization_state_id = Uuid::new_v4().to_string();
    let workspace_uri = native_path::file_uri(&canonical_cwd)?;
    let (git_root_uri, branch_name) =
        workspace_git_metadata(snapshot, &canonical_cwd, &workspace_uri);
    let now = Utc::now();
    let timestamp = now.to_rfc3339_opts(SecondsFormat::Nanos, true);
    let database = build_database(
        &target.id,
        &trajectory_id,
        &initialization_state_id,
        &workspace_uri,
        &git_root_uri,
        &branch_name,
        now.timestamp(),
        now.timestamp_subsec_nanos(),
        &native_items,
    )?;
    let summary_title = snapshot
        .title
        .as_deref()
        .map(redact_secrets)
        .filter(|title| !title.trim().is_empty())
        .unwrap_or_else(|| format!("Imported from {}", snapshot.session));
    let annotation = build_title_annotation(&summary_title);
    let summary = SummaryRow {
        conversation_id: id.clone(),
        title: summary_title,
        preview: String::new(),
        step_count: database.step_count,
        last_modified_time: timestamp.clone(),
        workspace_uris: serde_json::to_string(&[workspace_uri])?,
        status: String::new(),
        source: String::new(),
        project_id: String::new(),
        agent_name: String::new(),
        parent_conversation_id: String::new(),
        nesting_depth: 0,
        battle_id: String::new(),
        winning_conversation_id: String::new(),
        not_fully_idle: false,
        killed: false,
        last_user_input_time: timestamp,
        last_user_input_step_index: database.last_user_input_step_index,
        app_data_dir: "antigravity-cli".to_owned(),
        raw_summary: None,
        group_id: String::new(),
    };
    let conversations_root = root.join("conversations");
    let annotations_root = root.join("annotations");
    let summary_path = root.join("conversation_summaries.db");
    let target_path = conversations_root.join(format!("{id}.db"));
    let annotation_path = annotations_root.join(format!("{id}.pbtxt"));
    Ok(AntigravityImport {
        target,
        expected_items,
        history_items,
        tool_events: trajectory.tool_events,
        native_tool_records,
        truncated: trajectory.truncated,
        root,
        conversations_root,
        annotations_root,
        summary_path,
        target_path,
        annotation_path,
        document: database.document,
        annotation,
        summary,
        lock_root,
    })
}

fn workspace_git_metadata(
    snapshot: &CanonicalSnapshot,
    canonical_cwd: &Path,
    workspace_uri: &str,
) -> (String, String) {
    let source_root = snapshot
        .workspace
        .git
        .worktree
        .as_deref()
        .unwrap_or(&snapshot.workspace.root);
    let matched_root = (!source_root.as_os_str().is_empty()
        && !omnis_core::may_reach_network(source_root))
    .then(|| omnis_core::canonicalize_path(source_root).ok())
    .flatten()
    .filter(|git_root| {
        git_root.is_dir()
            && canonical_cwd.starts_with(git_root)
            && omnis_core::workspace_root(canonical_cwd)
                .is_ok_and(|target_root| target_root == *git_root)
    });
    let git_root_uri = matched_root
        .as_deref()
        .and_then(|git_root| native_path::file_uri(git_root).ok())
        .unwrap_or_else(|| workspace_uri.to_owned());
    let branch_name = matched_root
        .and(snapshot.workspace.git.branch.as_deref())
        .map(redact_secrets)
        .filter(|branch| !branch.trim().is_empty())
        .unwrap_or_default();
    (git_root_uri, branch_name)
}

pub fn ensure_supported(binary: &Path) -> Result<String> {
    if !cfg!(any(target_os = "linux", target_os = "macos")) {
        bail!("native Antigravity CLI import is supported only on Linux and macOS");
    }
    let version = crate::version_gate::installed_version(binary, Provider::Antigravity)?;
    if !is_supported_version(&version) {
        bail!(
            "Antigravity CLI {version} is too old for native trajectory import; supported versions: >= {MINIMUM_ANTIGRAVITY_VERSION}"
        );
    }
    Ok(version)
}

fn is_supported_version(version: &str) -> bool {
    crate::version_gate::is_release_at_least(version, MINIMUM_ANTIGRAVITY_VERSION)
}

/// Publishes the import as a new Antigravity CLI conversation. Antigravity CLI may be running:
/// concurrent CLI sessions share `conversation_summaries.db` through SQLite transactions while
/// each conversation owns a fresh UUID database and annotation file. Rollback and deletion
/// require Antigravity CLI to be closed, because unchanged database bytes do not exclude live
/// connections or uncommitted writes. Rollback also requires the generated artifacts to match.
pub fn materialize(import: &AntigravityImport, binary: &Path) -> Result<AntigravityWriteGuard> {
    ensure_supported(binary)?;
    validate_import_paths(import, "initializing")?;
    ensure_directory(&import.root)?;
    let guard = lock_import_root(import)?;
    materialize_store_locked(import)?;
    Ok(guard)
}

#[cfg(test)]
pub(crate) fn materialize_store(import: &AntigravityImport) -> Result<()> {
    materialize_store_with_verification(import, verify_materialized, || Ok(()))
}

#[cfg(test)]
fn materialize_store_with_lock(import: &AntigravityImport) -> Result<()> {
    let _guard = lock_import_root(import)?;
    materialize_store(import)
}

fn materialize_store_locked(import: &AntigravityImport) -> Result<()> {
    materialize_store_with_verification(
        import,
        verify_materialized,
        ensure_no_active_antigravity_process,
    )
}

fn materialize_store_with_verification(
    import: &AntigravityImport,
    verify: impl FnOnce(&AntigravityImport) -> Result<()>,
    check_rollback: impl FnOnce() -> Result<()>,
) -> Result<()> {
    validate_import_paths(import, "writing")?;
    ensure_directory(&import.root)?;
    validate_optional_directory_for(&import.conversations_root, &import.root, "writing")?;
    validate_optional_directory_for(&import.annotations_root, &import.root, "writing")?;
    let fresh_summary = summary_database_is_missing(import)?;
    if !fresh_summary {
        let connection = open_summary_readonly(&import.summary_path)?;
        if summary_row(&connection, &import.target.id)?.is_some() {
            bail!("generated Antigravity summary already exists");
        }
    }
    if path_entry_exists(&import.target_path)? {
        bail!("generated Antigravity target session already exists");
    }
    if path_entry_exists(&import.annotation_path)? {
        bail!("generated Antigravity target annotation already exists");
    }
    ensure_directory(&import.conversations_root)?;
    validate_directory_chain(&import.conversations_root, &import.root, "writing")?;
    ensure_directory(&import.annotations_root)?;
    validate_directory_chain(&import.annotations_root, &import.root, "writing")?;

    write_atomic_private_file(
        &import.conversations_root,
        &import.target_path,
        &import.document,
        "Antigravity conversation database",
    )?;
    let mut published_annotation = false;
    let mut published_summary = false;
    let result = (|| {
        sync_directory(&import.conversations_root)?;
        write_atomic_private_file(
            &import.annotations_root,
            &import.annotation_path,
            &import.annotation,
            "Antigravity conversation annotation",
        )?;
        published_annotation = true;
        sync_directory(&import.annotations_root)?;
        if fresh_summary {
            let temporary = build_summary_database(import)?;
            match temporary.persist_noclobber(&import.summary_path) {
                Ok(_) => {
                    published_summary = true;
                    sync_directory(&import.root)?;
                }
                Err(persist_error)
                    if persist_error.error.kind() == std::io::ErrorKind::AlreadyExists =>
                {
                    validate_summary_database(import)?;
                    insert_summary_into_existing(import)?;
                    published_summary = true;
                }
                Err(persist_error) => {
                    return Err(persist_error.error)
                        .context("publishing Antigravity summary database");
                }
            }
        } else {
            insert_summary_into_existing(import)?;
            published_summary = true;
        }
        verify(import)?;
        Ok(())
    })();
    if let Err(error) = result {
        return rollback_after_publish(
            import,
            published_annotation,
            published_summary,
            error,
            check_rollback,
        );
    }
    Ok(())
}

fn write_atomic_private_file(
    directory: &Path,
    destination: &Path,
    bytes: &[u8],
    description: &str,
) -> Result<()> {
    let mut temporary = NamedTempFile::new_in(directory)
        .with_context(|| format!("creating temporary {description}"))?;
    #[cfg(unix)]
    set_private_permissions(temporary.as_file())?;
    std::io::Write::write_all(&mut temporary, bytes)
        .with_context(|| format!("writing {description}"))?;
    std::io::Write::flush(&mut temporary)?;
    temporary.as_file().sync_all()?;
    temporary
        .persist_noclobber(destination)
        .map_err(|error| error.error)
        .with_context(|| format!("publishing {description}"))?;
    Ok(())
}

fn insert_summary_into_existing(import: &AntigravityImport) -> Result<()> {
    let mut connection = Connection::open_with_flags(
        &import.summary_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
    )
    .context("opening Antigravity summary database")?;
    connection.busy_timeout(SUMMARY_WRITE_BUSY_TIMEOUT)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    validate_summary_connection(&transaction)?;
    if summary_row(&transaction, &import.target.id)?.is_some() {
        bail!("generated Antigravity summary already exists");
    }
    insert_summary(&transaction, &import.summary)?;
    transaction.commit()?;
    Ok(())
}

fn open_summary_readonly(summary_path: &Path) -> Result<Connection> {
    let connection =
        Connection::open_with_flags(summary_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    connection.busy_timeout(SUMMARY_WRITE_BUSY_TIMEOUT)?;
    Ok(connection)
}

fn summary_database_is_missing(import: &AntigravityImport) -> Result<bool> {
    match fs::symlink_metadata(&import.summary_path) {
        Ok(_) => {
            validate_summary_database(import)?;
            Ok(false)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            ensure_no_summary_sidecars(&import.root)?;
            Ok(true)
        }
        Err(error) => Err(error).context("checking Antigravity summary database"),
    }
}

fn ensure_no_summary_sidecars(root: &Path) -> Result<()> {
    for suffix in ["-wal", "-shm", "-journal"] {
        let path = root.join(format!("conversation_summaries.db{suffix}"));
        match fs::symlink_metadata(path) {
            Ok(_) => bail!(
                "refusing to initialize or remove Antigravity summaries while a database sidecar exists"
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).context("checking Antigravity summary sidecars");
            }
        }
    }
    Ok(())
}

fn build_summary_database(import: &AntigravityImport) -> Result<NamedTempFile> {
    let temporary = NamedTempFile::new_in(&import.root)
        .context("creating temporary Antigravity summary database")?;
    #[cfg(unix)]
    set_private_permissions(temporary.as_file())?;
    {
        let mut connection = Connection::open(temporary.path())?;
        connection.execute_batch(SUMMARY_SCHEMA)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        insert_summary(&transaction, &import.summary)?;
        transaction.commit()?;
    }
    validate_summary_database_at(&import.root, temporary.path())?;
    temporary.as_file().sync_all()?;
    Ok(temporary)
}

pub(crate) fn rollback_locked(
    import: &AntigravityImport,
    _guard: &PrivateStoreGuard,
) -> Result<()> {
    ensure_no_active_antigravity_process()?;
    rollback_store_locked(import)
}

/// Deletes one selected Antigravity conversation and exact native payloads.
pub fn delete_session(session: &SessionRef, binary: &Path) -> Result<PrivateStoreGuard> {
    ensure_deletion_platform_supported()?;
    ensure_supported(binary)?;
    ensure_no_active_antigravity_process()?;
    let root = data_root()?;
    let guard = lock_root(&root, None)?;
    ensure_no_active_antigravity_process()?;
    delete_session_at_locked(session, &root)?;
    Ok(guard)
}

fn ensure_deletion_platform_supported() -> Result<()> {
    if cfg!(any(target_os = "linux", target_os = "macos")) {
        Ok(())
    } else {
        bail!("native Antigravity CLI deletion is supported only on Linux and macOS")
    }
}

#[cfg(test)]
fn delete_session_at(session: &SessionRef, root: &Path) -> Result<()> {
    delete_session_at_locked(session, root)
}

#[cfg(test)]
fn delete_session_at_with_lock_root(
    session: &SessionRef,
    root: &Path,
    configured_lock_root: &Path,
) -> Result<()> {
    let _guard = lock_root(root, Some(configured_lock_root))?;
    delete_session_at_locked(session, root)
}

struct DeletionPayloads {
    conversations_root: PathBuf,
    annotations_root: PathBuf,
    brain_root: PathBuf,
    conversation: PathBuf,
    staged_conversation: PathBuf,
    annotation: PathBuf,
    staged_annotation: PathBuf,
    brain: PathBuf,
    staged_brain: PathBuf,
    staged_conversation_exists: bool,
    staged_annotation_exists: bool,
    staged_brain_exists: bool,
}

impl DeletionPayloads {
    fn prepare(root: &Path, id: &str, summary_path: &Path) -> Result<Self> {
        let conversations_root = root.join("conversations");
        validate_optional_directory(&conversations_root, root)?;
        let annotations_root = root.join("annotations");
        validate_optional_directory(&annotations_root, root)?;
        let brain_root = root.join("brain");
        validate_optional_directory(&brain_root, root)?;
        retry_pending_antigravity_deletions(
            &conversations_root,
            &annotations_root,
            &brain_root,
            summary_path,
        )?;
        let payloads = Self {
            conversation: conversations_root.join(format!("{id}.db")),
            staged_conversation: conversations_root.join(format!(".omnisession-delete-{id}.db")),
            annotation: annotations_root.join(format!("{id}.pbtxt")),
            staged_annotation: annotations_root.join(format!(".omnisession-delete-{id}.pbtxt")),
            brain: brain_root.join(id),
            staged_brain: brain_root.join(format!(".omnisession-delete-{id}")),
            conversations_root,
            annotations_root,
            brain_root,
            staged_conversation_exists: false,
            staged_annotation_exists: false,
            staged_brain_exists: false,
        };
        if path_entry_exists(&payloads.staged_conversation)?
            || path_entry_exists(&payloads.staged_annotation)?
            || path_entry_exists(&payloads.staged_brain)?
        {
            bail!("Antigravity deletion staging path already exists");
        }
        validate_optional_payload(&payloads.conversation, false)?;
        validate_optional_payload(&payloads.annotation, false)?;
        validate_optional_payload(&payloads.brain, true)?;
        Ok(payloads)
    }

    fn stage(&mut self) -> Result<()> {
        if self.conversation.exists() {
            fs::rename(&self.conversation, &self.staged_conversation)
                .context("staging Antigravity conversation for deletion")?;
            self.staged_conversation_exists = true;
        }
        if self.annotation.exists() {
            fs::rename(&self.annotation, &self.staged_annotation)
                .context("staging Antigravity conversation annotation for deletion")?;
            self.staged_annotation_exists = true;
        }
        if self.brain.exists() {
            fs::rename(&self.brain, &self.staged_brain)
                .context("staging Antigravity transcript directory for deletion")?;
            self.staged_brain_exists = true;
        }
        Ok(())
    }

    fn restore(&self) -> Result<()> {
        if self.staged_brain_exists {
            fs::rename(&self.staged_brain, &self.brain)
                .context("restoring Antigravity transcript after failed deletion")?;
        }
        if self.staged_annotation_exists {
            fs::rename(&self.staged_annotation, &self.annotation)
                .context("restoring Antigravity conversation annotation after failed deletion")?;
        }
        if self.staged_conversation_exists {
            fs::rename(&self.staged_conversation, &self.conversation)
                .context("restoring Antigravity conversation after failed deletion")?;
        }
        Ok(())
    }

    fn cleanup(&self) {
        if self.staged_conversation_exists {
            let _ =
                remove_staged_payload(&self.staged_conversation, false, &self.conversations_root);
        }
        if self.staged_annotation_exists {
            let _ = remove_staged_payload(&self.staged_annotation, false, &self.annotations_root);
        }
        if self.staged_brain_exists {
            let _ = remove_staged_payload(&self.staged_brain, true, &self.brain_root);
        }
    }
}

fn delete_session_at_locked(session: &SessionRef, root: &Path) -> Result<()> {
    if session.provider != Provider::Antigravity || Uuid::parse_str(&session.id).is_err() {
        bail!("refusing Antigravity deletion with invalid session identity");
    }
    let root_metadata =
        fs::symlink_metadata(root).context("Antigravity data root was not found")?;
    if !root_metadata.is_dir() || root_metadata.file_type().is_symlink() {
        bail!("Antigravity data root is not a safe directory");
    }
    let root = fs::canonicalize(root).context("canonicalizing Antigravity data root")?;
    validate_directory_chain(&root, &root, "deleting")?;
    let summary_path = root.join("conversation_summaries.db");
    validate_summary_database_at(&root, &summary_path)?;
    let mut payloads = DeletionPayloads::prepare(&root, &session.id, &summary_path)?;

    let result = (|| -> Result<()> {
        let mut connection = Connection::open(&summary_path)
            .context("opening Antigravity summary database for deletion")?;
        connection.busy_timeout(Duration::ZERO)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        validate_summary_connection(&transaction)?;
        summary_row(&transaction, &session.id)?
            .context("selected Antigravity session no longer exists")?;
        payloads.stage()?;
        let deleted = transaction.execute(
            "DELETE FROM conversation_summaries WHERE conversation_id = ?1",
            [&session.id],
        )?;
        if deleted != 1 {
            bail!("Antigravity deletion did not remove exact selected summary row");
        }
        transaction
            .commit()
            .context("committing Antigravity session deletion")
    })();
    if let Err(error) = result {
        payloads.restore()?;
        return Err(error);
    }
    payloads.cleanup();
    let connection =
        Connection::open_with_flags(&summary_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    if summary_row(&connection, &session.id)?.is_some() {
        bail!("Antigravity deletion read-back found selected summary row");
    }
    Ok(())
}

fn retry_pending_antigravity_deletions(
    conversations_root: &Path,
    annotations_root: &Path,
    brain_root: &Path,
    summary_path: &Path,
) -> Result<()> {
    let connection =
        Connection::open_with_flags(summary_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    retry_staged_entries(
        &connection,
        conversations_root,
        ".omnisession-delete-",
        ".db",
        false,
    )?;
    retry_staged_entries(
        &connection,
        annotations_root,
        ".omnisession-delete-",
        ".pbtxt",
        false,
    )?;
    retry_staged_entries(&connection, brain_root, ".omnisession-delete-", "", true)
}

fn retry_staged_entries(
    connection: &Connection,
    parent: &Path,
    prefix: &str,
    suffix: &str,
    directory: bool,
) -> Result<()> {
    let entries = match fs::read_dir(parent) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Some(id) = name
            .strip_prefix(prefix)
            .and_then(|name| name.strip_suffix(suffix))
            .filter(|id| Uuid::parse_str(id).is_ok())
        else {
            continue;
        };
        if summary_row(connection, id)?.is_some() {
            continue;
        }
        let path = entry.path();
        if validate_optional_payload(&path, directory).is_ok() {
            let _ = remove_staged_payload(&path, directory, parent);
        }
    }
    Ok(())
}

fn remove_staged_payload(path: &Path, directory: bool, parent: &Path) -> Result<()> {
    if directory {
        fs::remove_dir_all(path).context("removing staged Antigravity transcript directory")?;
    } else {
        fs::remove_file(path).context("removing staged Antigravity payload")?;
    }
    sync_directory(parent)
}

fn validate_optional_directory(path: &Path, root: &Path) -> Result<()> {
    validate_optional_directory_for(path, root, "deleting")
}

fn validate_optional_directory_for(path: &Path, root: &Path, operation: &str) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => validate_directory_chain(path, root, operation),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("reading `{}`", path.display())),
    }
}

fn path_entry_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("reading `{}`", path.display())),
    }
}

fn validate_optional_payload(path: &Path, directory: bool) -> Result<()> {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return Ok(());
    };
    if metadata.file_type().is_symlink()
        || (directory && !metadata.is_dir())
        || (!directory && !metadata.is_file())
    {
        bail!("Antigravity session payload is not a safe native entry");
    }
    if directory {
        for entry in walkdir::WalkDir::new(path).follow_links(false) {
            let entry = entry.context("reading Antigravity transcript directory")?;
            let metadata = fs::symlink_metadata(entry.path())?;
            if metadata.file_type().is_symlink() || (!metadata.is_dir() && !metadata.is_file()) {
                bail!("Antigravity transcript directory contains unsafe entry");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn rollback_store(import: &AntigravityImport) -> Result<()> {
    rollback_store_locked(import)
}

#[cfg(test)]
fn rollback_store_with_lock(import: &AntigravityImport) -> Result<()> {
    let _guard = lock_import_root(import)?;
    rollback_store_locked(import)
}

fn rollback_store_locked(import: &AntigravityImport) -> Result<()> {
    validate_import_paths(import, "rolling back")?;
    validate_directory_chain(&import.conversations_root, &import.root, "rolling back")?;
    validate_directory_chain(&import.annotations_root, &import.root, "rolling back")?;
    let mut connection = Connection::open(&import.summary_path)
        .context("opening Antigravity summary database for rollback")?;
    connection.busy_timeout(SUMMARY_WRITE_BUSY_TIMEOUT)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    validate_summary_connection(&transaction)?;
    if summary_row(&transaction, &import.target.id)?.as_ref() != Some(&import.summary) {
        bail!("refusing to delete changed Antigravity summary row");
    }
    let staged_target = import
        .conversations_root
        .join(format!(".omnisession-rollback-{}.db", import.target.id));
    let staged_annotation = import
        .annotations_root
        .join(format!(".omnisession-rollback-{}.pbtxt", import.target.id));
    if path_entry_exists(&staged_target)? || path_entry_exists(&staged_annotation)? {
        bail!("Antigravity rollback staging path already exists");
    }
    fs::rename(&import.target_path, &staged_target)
        .context("staging Antigravity target for rollback")?;
    if let Err(error) = fs::rename(&import.annotation_path, &staged_annotation)
        .context("staging Antigravity target annotation for rollback")
    {
        return Err(
            match restore_staged_file(&staged_target, &import.target_path) {
                Ok(()) => error,
                Err(restore) => error.context(format!(
                    "restoring `{}` failed: {restore}",
                    staged_target.display()
                )),
            },
        );
    }
    let commit_result = (|| -> Result<()> {
        validate_generated_target_file(import, &staged_target)?;
        validate_generated_annotation_file(import, &staged_annotation)?;
        let deleted = transaction
            .execute(
                "DELETE FROM conversation_summaries WHERE conversation_id = ?1",
                [&import.target.id],
            )
            .context("deleting Antigravity summary row")?;
        if deleted != 1 {
            bail!("Antigravity rollback did not remove exact summary row");
        }
        transaction
            .commit()
            .context("committing Antigravity summary rollback")
    })();
    if let Err(error) = commit_result {
        let restore_annotation = restore_staged_file(&staged_annotation, &import.annotation_path);
        let restore_target = restore_staged_file(&staged_target, &import.target_path);
        let mut combined = error;
        if let Err(restore) = restore_annotation {
            combined = combined.context(format!(
                "restoring `{}` failed: {restore}",
                staged_annotation.display()
            ));
        }
        if let Err(restore) = restore_target {
            combined = combined.context(format!(
                "restoring `{}` failed: {restore}",
                staged_target.display()
            ));
        }
        return Err(combined);
    }
    fs::remove_file(&staged_annotation)
        .context("removing generated Antigravity target annotation")?;
    sync_directory(&import.annotations_root)?;
    fs::remove_file(&staged_target).context("removing generated Antigravity target")?;
    sync_directory(&import.conversations_root)
}

fn restore_staged_file(staged: &Path, original: &Path) -> std::io::Result<()> {
    fs::hard_link(staged, original)?;
    fs::remove_file(staged)
}

pub fn readback_matches(snapshot: &CanonicalSnapshot, expected: &[NativeTrajectoryItem]) -> bool {
    readback_trajectory(snapshot)
        .is_some_and(|trajectory| native_trajectory_signature(&trajectory) == expected)
}

fn verify_materialized(import: &AntigravityImport) -> Result<()> {
    validate_generated_files(import)?;
    let connection = open_summary_readonly(&import.summary_path)?;
    if summary_row(&connection, &import.target.id)?.as_ref() != Some(&import.summary) {
        bail!("Antigravity summary read-back changed imported metadata");
    }
    let snapshot = AntigravityAdapter::with_root(&import.root)
        .read_session(&import.target)
        .context("reading back imported Antigravity trajectory")?;
    if !readback_matches(&snapshot, &import.expected_items) {
        bail!("Antigravity trajectory read-back did not match imported history");
    }
    Ok(())
}

fn rollback_after_publish(
    import: &AntigravityImport,
    published_annotation: bool,
    published_summary: bool,
    error: anyhow::Error,
    check_rollback: impl FnOnce() -> Result<()>,
) -> Result<()> {
    // An unreadable summary is a failed rollback too: the published conversation may remain.
    let rollback = (|| {
        check_rollback()?;
        let stored = stored_summary(import)?;
        if published_summary || stored.is_some() {
            if !published_annotation {
                bail!("Antigravity summary row exists without generated annotation");
            }
            rollback_store_locked(import)
        } else {
            validate_import_paths(import, "rolling back")?;
            validate_generated_target_file(import, &import.target_path)?;
            if published_annotation {
                validate_generated_annotation_file(import, &import.annotation_path)?;
                fs::remove_file(&import.annotation_path)
                    .context("removing unpublished Antigravity target annotation")?;
                sync_directory(&import.annotations_root)?;
            }
            fs::remove_file(&import.target_path)
                .context("removing unpublished Antigravity target")?;
            sync_directory(&import.conversations_root)
        }
    })();
    match rollback {
        Ok(()) => Err(error),
        Err(rollback_error) => Err(crate::transfer::rollback_failure(
            error,
            format!("Antigravity import failed and exact rollback also failed: {rollback_error}"),
        )),
    }
}

fn lock_import_root(import: &AntigravityImport) -> Result<PrivateStoreGuard> {
    lock_root(&import.root, import.lock_root.as_deref())
}

fn lock_root(root: &Path, configured_lock_root: Option<&Path>) -> Result<PrivateStoreGuard> {
    private_store_lock::acquire(
        root,
        ANTIGRAVITY_LOCK_NAMESPACE,
        "Antigravity",
        configured_lock_root,
    )
}

fn stored_summary(import: &AntigravityImport) -> Result<Option<SummaryRow>> {
    match fs::symlink_metadata(&import.summary_path) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("checking Antigravity summary database"),
    }
    let connection = open_summary_readonly(&import.summary_path)?;
    summary_row(&connection, &import.target.id)
}

fn validate_generated_files(import: &AntigravityImport) -> Result<()> {
    validate_import_paths(import, "validating")?;
    validate_generated_target_file(import, &import.target_path)?;
    validate_generated_annotation_file(import, &import.annotation_path)
}

fn validate_generated_target_file(import: &AntigravityImport, path: &Path) -> Result<()> {
    let metadata =
        fs::symlink_metadata(path).context("reading generated Antigravity target metadata")?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        bail!("generated Antigravity target is not a regular file");
    }
    let content = fs::read(path).context("reading generated Antigravity conversation database")?;
    if content != import.document {
        bail!("generated Antigravity target changed after materialization");
    }
    let connection = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let identity = connection.query_row(
        "SELECT trajectory_id, cascade_id, trajectory_type, source FROM trajectory_meta",
        [],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
            ))
        },
    )?;
    if identity.1 != import.target.id
        || identity.2 != TRAJECTORY_TYPE_CASCADE
        || identity.3 != TRAJECTORY_SOURCE_CLI
        || Uuid::parse_str(&identity.0).is_err()
    {
        bail!("generated Antigravity target failed exact identity validation");
    }
    Ok(())
}

fn validate_generated_annotation_file(import: &AntigravityImport, path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .context("reading generated Antigravity target annotation metadata")?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        bail!("generated Antigravity target annotation is not a regular file");
    }
    let content =
        fs::read(path).context("reading generated Antigravity conversation annotation")?;
    if content != import.annotation {
        bail!("generated Antigravity target annotation changed after materialization");
    }
    Ok(())
}

fn validate_import_paths(import: &AntigravityImport, operation: &str) -> Result<()> {
    if !import.root.is_absolute()
        || import.target.provider != Provider::Antigravity
        || Uuid::parse_str(&import.target.id).is_err()
        || import.summary.conversation_id != import.target.id
        || import.conversations_root != import.root.join("conversations")
        || import.annotations_root != import.root.join("annotations")
        || import.summary_path != import.root.join("conversation_summaries.db")
        || import.target_path
            != import
                .conversations_root
                .join(format!("{}.db", import.target.id))
        || import.annotation_path
            != import
                .annotations_root
                .join(format!("{}.pbtxt", import.target.id))
    {
        bail!("refusing {operation} with unverified Antigravity target paths");
    }
    Ok(())
}

fn validate_summary_database(import: &AntigravityImport) -> Result<()> {
    validate_summary_database_at(&import.root, &import.summary_path)
}

fn validate_summary_database_at(root: &Path, summary_path: &Path) -> Result<()> {
    validate_directory_chain(root, root, "opening")?;
    let metadata = fs::symlink_metadata(summary_path)
        .context("Antigravity summary database does not exist")?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        bail!("Antigravity summary database is not a regular file");
    }
    let connection = open_summary_readonly(summary_path)?;
    validate_summary_connection(&connection)?;
    Ok(())
}

fn validate_summary_connection(connection: &Connection) -> Result<i64> {
    let version = connection.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))?;
    if !matches!(version, 1..=3) {
        bail!("Antigravity summary schema version {version} is not supported");
    }
    validate_summary_schema(connection, version)?;
    Ok(version)
}

fn validate_summary_schema(connection: &Connection, version: i64) -> Result<()> {
    const BASE_EXPECTED: [(&str, &str, bool, Option<&str>, bool); 19] = [
        ("conversation_id", "text", false, None, true),
        ("title", "text", true, Some(""), false),
        ("preview", "text", true, Some(""), false),
        ("step_count", "integer", true, Some("0"), false),
        ("last_modified_time", "datetime", true, None, false),
        ("workspace_uris", "text", true, None, false),
        ("status", "text", true, Some(""), false),
        ("source", "text", true, Some(""), false),
        ("project_id", "text", true, Some(""), false),
        ("agent_name", "text", true, Some(""), false),
        ("parent_conversation_id", "text", true, Some(""), false),
        ("nesting_depth", "integer", true, Some("0"), false),
        ("battle_id", "text", true, Some(""), false),
        ("winning_conversation_id", "text", true, Some(""), false),
        ("not_fully_idle", "numeric", true, Some("false"), false),
        ("killed", "numeric", true, Some("false"), false),
        ("last_user_input_time", "datetime", true, None, false),
        (
            "last_user_input_step_index",
            "integer",
            true,
            Some("-1"),
            false,
        ),
        ("app_data_dir", "text", true, Some(""), false),
    ];
    let mut statement = connection.prepare("PRAGMA table_info(conversation_summaries)")?;
    let actual = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?.to_ascii_lowercase(),
                row.get::<_, i64>(3)? != 0,
                row.get::<_, Option<String>>(4)?
                    .map(|value| normalize_sql_default(&value)),
                row.get::<_, i64>(5)? != 0,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut expected_columns = BASE_EXPECTED.to_vec();
    if version >= 2 {
        expected_columns.push(("raw_summary", "blob", false, None, false));
    }
    if version >= 3 {
        expected_columns.push(("group_id", "text", true, Some(""), false));
    }
    let expected = expected_columns
        .into_iter()
        .map(|(name, kind, not_null, default, primary_key)| {
            (
                name.to_owned(),
                kind.to_owned(),
                not_null,
                default.map(str::to_owned),
                primary_key,
            )
        })
        .collect::<Vec<_>>();
    if actual != expected {
        bail!("Antigravity summary database schema is not verified");
    }
    Ok(())
}

fn normalize_sql_default(value: &str) -> String {
    let value = value.trim();
    if value.len() >= 2
        && ((value.starts_with('"') && value.ends_with('"'))
            || (value.starts_with('\'') && value.ends_with('\'')))
    {
        value[1..value.len() - 1].to_owned()
    } else {
        value.to_ascii_lowercase()
    }
}

fn insert_summary(connection: &Connection, row: &SummaryRow) -> Result<()> {
    connection.execute(
        "INSERT INTO conversation_summaries (
            conversation_id, title, preview, step_count, last_modified_time, workspace_uris,
            status, source, project_id, agent_name, parent_conversation_id, nesting_depth,
            battle_id, winning_conversation_id, not_fully_idle, killed, last_user_input_time,
            last_user_input_step_index, app_data_dir
         ) VALUES (
            ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19
         )",
        params![
            row.conversation_id,
            row.title,
            row.preview,
            row.step_count,
            row.last_modified_time,
            row.workspace_uris,
            row.status,
            row.source,
            row.project_id,
            row.agent_name,
            row.parent_conversation_id,
            row.nesting_depth,
            row.battle_id,
            row.winning_conversation_id,
            row.not_fully_idle,
            row.killed,
            row.last_user_input_time,
            row.last_user_input_step_index,
            row.app_data_dir,
        ],
    )?;
    Ok(())
}

fn summary_row(connection: &Connection, id: &str) -> Result<Option<SummaryRow>> {
    let version = validate_summary_connection(connection)?;
    let query = match version {
        1 => {
            "SELECT conversation_id, title, preview, step_count, last_modified_time, workspace_uris,
                     status, source, project_id, agent_name, parent_conversation_id, nesting_depth,
                     battle_id, winning_conversation_id, not_fully_idle, killed,
                     last_user_input_time, last_user_input_step_index, app_data_dir,
                     NULL AS raw_summary, '' AS group_id
              FROM conversation_summaries WHERE conversation_id = ?1"
        }
        2 => {
            "SELECT conversation_id, title, preview, step_count, last_modified_time, workspace_uris,
                     status, source, project_id, agent_name, parent_conversation_id, nesting_depth,
                     battle_id, winning_conversation_id, not_fully_idle, killed,
                     last_user_input_time, last_user_input_step_index, app_data_dir,
                     raw_summary, '' AS group_id
              FROM conversation_summaries WHERE conversation_id = ?1"
        }
        3 => {
            "SELECT conversation_id, title, preview, step_count, last_modified_time, workspace_uris,
                     status, source, project_id, agent_name, parent_conversation_id, nesting_depth,
                     battle_id, winning_conversation_id, not_fully_idle, killed,
                     last_user_input_time, last_user_input_step_index, app_data_dir,
                     raw_summary, group_id
              FROM conversation_summaries WHERE conversation_id = ?1"
        }
        _ => bail!("Antigravity summary schema version {version} is not supported"),
    };
    connection
        .query_row(query, [id], |row| {
            Ok(SummaryRow {
                conversation_id: row.get(0)?,
                title: row.get(1)?,
                preview: row.get(2)?,
                step_count: row.get(3)?,
                last_modified_time: row.get(4)?,
                workspace_uris: row.get(5)?,
                status: row.get(6)?,
                source: row.get(7)?,
                project_id: row.get(8)?,
                agent_name: row.get(9)?,
                parent_conversation_id: row.get(10)?,
                nesting_depth: row.get(11)?,
                battle_id: row.get(12)?,
                winning_conversation_id: row.get(13)?,
                not_fully_idle: row.get(14)?,
                killed: row.get(15)?,
                last_user_input_time: row.get(16)?,
                last_user_input_step_index: row.get(17)?,
                app_data_dir: row.get(18)?,
                raw_summary: row.get(19)?,
                group_id: row.get(20)?,
            })
        })
        .optional()
        .map_err(Into::into)
}

fn build_title_annotation(title: &str) -> Vec<u8> {
    use std::fmt::Write;

    let mut escaped = String::with_capacity(title.len() + 10);
    escaped.push_str("title: \"");
    for character in title.chars() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            control if control.is_ascii_control() => {
                let _ = write!(escaped, "\\{:03o}", u32::from(control));
            }
            other => escaped.push(other),
        }
    }
    escaped.push_str("\"\n");
    escaped.into_bytes()
}

struct BuiltDatabase {
    document: Vec<u8>,
    step_count: i64,
    last_user_input_step_index: i64,
}

#[allow(clippy::too_many_arguments)]
fn build_database(
    cascade_id: &str,
    trajectory_id: &str,
    initialization_state_id: &str,
    workspace_uri: &str,
    git_root_uri: &str,
    branch_name: &str,
    seconds: i64,
    nanos: u32,
    items: &[NativeTrajectoryItem],
) -> Result<BuiltDatabase> {
    let nanos_i32 = i32::try_from(nanos)?;
    let total_steps = items
        .iter()
        .map(|item| match item {
            NativeTrajectoryItem::Message { .. } => 1usize,
            NativeTrajectoryItem::Tool { .. } => 2usize,
        })
        .sum::<usize>();
    let base_seconds = seconds.saturating_sub(i64::try_from(total_steps.saturating_sub(1))?);
    let temporary = NamedTempFile::new().context("creating Antigravity database image")?;
    let temporary_path = temporary.into_temp_path();
    let mut step_index = 0usize;
    let mut last_user_input_step_index = -1i64;
    {
        let mut connection = Connection::open(&temporary_path)?;
        connection.execute_batch(CONVERSATION_SCHEMA)?;
        let transaction = connection.transaction()?;
        transaction.execute(
            "INSERT INTO trajectory_meta VALUES (?1, ?2, ?3, ?4)",
            params![
                trajectory_id,
                cascade_id,
                TRAJECTORY_TYPE_CASCADE,
                TRAJECTORY_SOURCE_CLI
            ],
        )?;
        let metadata = ProtoTrajectoryMetadata {
            workspaces: vec![ProtoWorkspaceMetadata {
                workspace_folder_absolute_uri: workspace_uri.to_owned(),
                git_root_absolute_uri: git_root_uri.to_owned(),
                branch_name: branch_name.to_owned(),
            }],
            created_at: Some(ProtoTimestamp {
                seconds,
                nanos: nanos_i32,
            }),
            initialization_state_id: initialization_state_id.to_owned(),
            root_conversation_id: cascade_id.to_owned(),
            workspace_uris: vec![workspace_uri.to_owned()],
        }
        .encode_to_vec();
        transaction.execute(
            "INSERT INTO trajectory_metadata_blob (id, data) VALUES ('main', ?1)",
            [&metadata],
        )?;
        for item in items {
            insert_native_item(
                &transaction,
                item,
                base_seconds,
                nanos_i32,
                &mut step_index,
                &mut last_user_input_step_index,
            )?;
        }
        transaction.commit()?;
        connection.pragma_update(None, "user_version", 1)?;
        connection.execute_batch("PRAGMA optimize;")?;
    }
    let document = fs::read(&temporary_path)?;
    TempPath::close(temporary_path)?;
    Ok(BuiltDatabase {
        document,
        step_count: i64::try_from(step_index)?,
        last_user_input_step_index,
    })
}

fn make_step_metadata(
    base_seconds: i64,
    nanos: i32,
    index: usize,
    source: i32,
    tool_call: Option<ProtoToolCall>,
) -> Result<ProtoStepMetadata> {
    Ok(ProtoStepMetadata {
        created_at: Some(ProtoTimestamp {
            seconds: base_seconds.saturating_add(i64::try_from(index)?),
            nanos,
        }),
        source,
        tool_call,
        execution_id: Uuid::new_v4().to_string(),
    })
}

fn insert_native_item(
    transaction: &rusqlite::Transaction<'_>,
    item: &NativeTrajectoryItem,
    base_seconds: i64,
    nanos: i32,
    step_index: &mut usize,
    last_user_input_step_index: &mut i64,
) -> Result<()> {
    match item {
        NativeTrajectoryItem::Message {
            role: HandoffRole::User,
            text,
        } => {
            *last_user_input_step_index = i64::try_from(*step_index)?;
            let metadata = make_step_metadata(
                base_seconds,
                nanos,
                *step_index,
                STEP_SOURCE_USER_EXPLICIT,
                None,
            )?;
            insert_step(
                transaction,
                *step_index,
                STEP_USER_INPUT,
                STEP_STATUS_DONE,
                metadata,
                None,
                proto_step::Step::UserInput(ProtoUserInput {
                    query: text.clone(),
                    user_response: text.clone(),
                    items: vec![ProtoTextOrScopeItem { text: text.clone() }],
                }),
            )?;
            *step_index += 1;
        }
        NativeTrajectoryItem::Message {
            role: HandoffRole::Assistant,
            text,
        } => {
            let metadata =
                make_step_metadata(base_seconds, nanos, *step_index, STEP_SOURCE_MODEL, None)?;
            insert_step(
                transaction,
                *step_index,
                STEP_PLANNER_RESPONSE,
                STEP_STATUS_DONE,
                metadata,
                None,
                proto_step::Step::PlannerResponse(ProtoPlannerResponse {
                    response: text.clone(),
                    tool_calls: Vec::new(),
                }),
            )?;
            *step_index += 1;
        }
        NativeTrajectoryItem::Tool {
            call_id,
            name,
            input,
            output,
            is_error,
        } => {
            insert_native_tool_pair(
                transaction,
                base_seconds,
                nanos,
                step_index,
                call_id,
                name,
                input,
                output,
                *is_error,
            )?;
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn insert_native_tool_pair(
    transaction: &rusqlite::Transaction<'_>,
    base_seconds: i64,
    nanos: i32,
    step_index: &mut usize,
    call_id: &str,
    name: &str,
    input: &serde_json::Value,
    output: &str,
    is_error: bool,
) -> Result<()> {
    let tool_call = ProtoToolCall {
        id: call_id.to_owned(),
        name: name.to_owned(),
        arguments_json: serde_json::to_string(input)?,
    };
    let call_metadata =
        make_step_metadata(base_seconds, nanos, *step_index, STEP_SOURCE_MODEL, None)?;
    insert_step(
        transaction,
        *step_index,
        STEP_PLANNER_RESPONSE,
        STEP_STATUS_DONE,
        call_metadata,
        None,
        proto_step::Step::PlannerResponse(ProtoPlannerResponse {
            response: String::new(),
            tool_calls: vec![tool_call.clone()],
        }),
    )?;
    *step_index += 1;

    let result_metadata = make_step_metadata(
        base_seconds,
        nanos,
        *step_index,
        STEP_SOURCE_MODEL,
        Some(tool_call.clone()),
    )?;
    let (status, error_details) = if is_error {
        let message = if output.is_empty() {
            format!("Historical tool `{name}` failed")
        } else {
            output.to_owned()
        };
        (
            STEP_STATUS_ERROR,
            Some(ProtoErrorDetails {
                user_error_message: message.clone(),
                model_error_message: message,
                short_error: "historical_tool_failed".to_owned(),
                full_error: output.to_owned(),
            }),
        )
    } else {
        (STEP_STATUS_DONE, None)
    };
    insert_step(
        transaction,
        *step_index,
        STEP_MCP_TOOL,
        status,
        result_metadata,
        error_details,
        proto_step::Step::McpTool(ProtoMcpTool {
            server_name: MCP_SERVER_NAME.to_owned(),
            tool_call: Some(tool_call),
            result_string: output.to_owned(),
        }),
    )?;
    *step_index += 1;
    Ok(())
}

fn insert_step(
    transaction: &rusqlite::Transaction<'_>,
    index: usize,
    step_type: i32,
    status: i32,
    metadata: ProtoStepMetadata,
    error_details: Option<ProtoErrorDetails>,
    step: proto_step::Step,
) -> Result<()> {
    let metadata_bytes = metadata.encode_to_vec();
    let error_bytes = error_details.as_ref().map(Message::encode_to_vec);
    let payload = ProtoStep {
        r#type: step_type,
        status,
        metadata: Some(metadata),
        error: error_details,
        step: Some(step),
    }
    .encode_to_vec();
    transaction.execute(
        "INSERT INTO steps (
            idx, step_type, status, has_subtrajectory, metadata, error_details, step_payload, step_format
         ) VALUES (?1, ?2, ?3, false, ?4, ?5, ?6, 0)",
        params![
            i64::try_from(index)?,
            step_type,
            status,
            metadata_bytes,
            error_bytes,
            payload
        ],
    )?;
    Ok(())
}

const CONVERSATION_SCHEMA: &str = r#"
CREATE TABLE `battle_mode_infos` (`idx` integer,`data` blob,PRIMARY KEY (`idx`));
CREATE TABLE `executor_metadata` (`idx` integer,`data` blob,PRIMARY KEY (`idx`));
CREATE TABLE `gen_metadata` (`idx` integer,`data` blob,`size` integer NOT NULL DEFAULT 0,PRIMARY KEY (`idx`));
CREATE TABLE `parent_references` (`idx` integer,`data` blob,PRIMARY KEY (`idx`));
CREATE TABLE `steps` (`idx` integer,`step_type` integer NOT NULL DEFAULT 0,`status` integer NOT NULL DEFAULT 0,`has_subtrajectory` numeric NOT NULL DEFAULT false,`metadata` blob,`error_details` blob,`permissions` blob,`task_details` blob,`render_info` blob,`step_payload` blob,`step_format` integer NOT NULL DEFAULT 0,PRIMARY KEY (`idx`));
CREATE INDEX `idx_steps_status` ON `steps` (`status`);
CREATE INDEX `idx_steps_step_type` ON `steps` (`step_type`);
CREATE TABLE `trajectory_meta` (`trajectory_id` text,`cascade_id` text,`trajectory_type` integer,`source` integer,PRIMARY KEY (`trajectory_id`));
CREATE TABLE `trajectory_metadata_blob` (`id` text DEFAULT "main",`data` blob,PRIMARY KEY (`id`));
PRAGMA user_version = 1;
"#;

fn data_root() -> Result<PathBuf> {
    if let Some(root) = env::var_os("ANTIGRAVITY_CLI_HOME").filter(|value| !value.is_empty()) {
        let root = PathBuf::from(root);
        if !root.is_absolute() {
            bail!("ANTIGRAVITY_CLI_HOME must be an absolute path for native import");
        }
        return Ok(root);
    }
    BaseDirs::new()
        .map(|directories| {
            directories
                .home_dir()
                .join(".gemini")
                .join("antigravity-cli")
        })
        .context("home directory is unavailable")
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
        .context("Antigravity target directory has no parent")?;
    ensure_directory(parent)?;
    #[cfg(unix)]
    let builder = {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = fs::DirBuilder::new();
        builder.mode(0o700);
        builder
    };
    #[cfg(not(unix))]
    let builder = fs::DirBuilder::new();
    match builder.create(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => ensure_directory(path),
        Err(error) => Err(error.into()),
    }
}

fn validate_directory_chain(path: &Path, root: &Path, operation: &str) -> Result<()> {
    let mut current = Some(path);
    while let Some(directory) = current {
        let metadata = fs::symlink_metadata(directory)
            .with_context(|| format!("reading `{}`", directory.display()))?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            bail!(
                "refusing {operation} through unsafe directory `{}`",
                directory.display()
            );
        }
        if directory == root {
            return Ok(());
        }
        current = directory.parent();
    }
    bail!("Antigravity target path is outside data root")
}

#[cfg(unix)]
fn set_private_permissions(file: &fs::File) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
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

#[cfg(target_os = "linux")]
fn ensure_no_active_antigravity_process() -> Result<()> {
    let own_pid = std::process::id().to_string();
    for entry in fs::read_dir("/proc").context("checking active Antigravity processes")? {
        let entry = entry?;
        let Some(pid) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if pid == own_pid || !pid.bytes().all(|byte| byte.is_ascii_digit()) {
            continue;
        }
        let process = entry.path();
        let comm = fs::read_to_string(process.join("comm")).unwrap_or_default();
        let cmdline = fs::read(process.join("cmdline")).unwrap_or_default();
        let command = String::from_utf8_lossy(&cmdline);
        let is_antigravity =
            comm.trim() == "agy" || is_antigravity_cli_language_server(comm.trim(), &command);
        if !is_antigravity {
            continue;
        }
        let status = fs::read_to_string(process.join("status")).unwrap_or_default();
        let zombie = status
            .lines()
            .find(|line| line.starts_with("State:"))
            .is_some_and(|line| line.split_whitespace().nth(1) == Some("Z"));
        if !zombie {
            bail!(
                "refusing native Antigravity CLI store mutation while Antigravity CLI is running (pid {pid})"
            );
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn ensure_no_active_antigravity_process() -> Result<()> {
    let output = std::process::Command::new("/bin/ps")
        .args(["-ww", "-x", "-o", "pid=,ucomm=,args="])
        .output()
        .context("checking active Antigravity CLI processes")?;
    if !output.status.success() {
        bail!("could not inspect Antigravity CLI process state");
    }
    if let Some(pid) =
        antigravity_pid_from_macos_ps(&String::from_utf8_lossy(&output.stdout), std::process::id())
    {
        bail!(
            "refusing native Antigravity CLI store mutation while Antigravity CLI is running (pid {pid})"
        );
    }
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn ensure_no_active_antigravity_process() -> Result<()> {
    bail!("Antigravity CLI active-writer detection is supported only on Linux and macOS")
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn is_antigravity_cli_language_server(command_name: &str, command: &str) -> bool {
    let language_server =
        command_name == "language_server" || command_name.starts_with("language_server_");
    language_server
        && command
            .split(|character: char| {
                character.is_whitespace()
                    || matches!(character, '/' | '\\' | '=' | '\"' | '\'' | '\0')
            })
            .any(|component| component == "antigravity-cli")
}

#[cfg(any(target_os = "macos", test))]
fn antigravity_pid_from_macos_ps(output: &str, own_pid: u32) -> Option<u32> {
    output.lines().find_map(|line| {
        let line = line.trim_start();
        let split = line.find(char::is_whitespace)?;
        let pid = line[..split].parse::<u32>().ok()?;
        if pid == own_pid {
            return None;
        }
        let fields = line[split..].trim_start();
        let name_end = fields.find(char::is_whitespace).unwrap_or(fields.len());
        let command_name = &fields[..name_end];
        let command = fields[name_end..].trim_start();
        (command_name == "agy" || is_antigravity_cli_language_server(command_name, command))
            .then_some(pid)
    })
}

#[derive(Clone, PartialEq, Message)]
struct ProtoStep {
    #[prost(int32, tag = "1")]
    r#type: i32,
    #[prost(int32, tag = "4")]
    status: i32,
    #[prost(message, optional, tag = "5")]
    metadata: Option<ProtoStepMetadata>,
    #[prost(message, optional, tag = "31")]
    error: Option<ProtoErrorDetails>,
    #[prost(oneof = "proto_step::Step", tags = "19, 20, 47")]
    step: Option<proto_step::Step>,
}

mod proto_step {
    use prost::Oneof;

    use super::{ProtoMcpTool, ProtoPlannerResponse, ProtoUserInput};

    #[derive(Clone, PartialEq, Oneof)]
    pub(super) enum Step {
        #[prost(message, tag = "19")]
        UserInput(ProtoUserInput),
        #[prost(message, tag = "20")]
        PlannerResponse(ProtoPlannerResponse),
        #[prost(message, tag = "47")]
        McpTool(ProtoMcpTool),
    }
}

#[derive(Clone, PartialEq, Message)]
struct ProtoTimestamp {
    #[prost(int64, tag = "1")]
    seconds: i64,
    #[prost(int32, tag = "2")]
    nanos: i32,
}

#[derive(Clone, PartialEq, Message)]
struct ProtoStepMetadata {
    #[prost(message, optional, tag = "1")]
    created_at: Option<ProtoTimestamp>,
    #[prost(int32, tag = "3")]
    source: i32,
    #[prost(message, optional, tag = "4")]
    tool_call: Option<ProtoToolCall>,
    #[prost(string, tag = "12")]
    execution_id: String,
}

#[derive(Clone, PartialEq, Message)]
struct ProtoUserInput {
    #[prost(string, tag = "1")]
    query: String,
    #[prost(string, tag = "2")]
    user_response: String,
    #[prost(message, repeated, tag = "3")]
    items: Vec<ProtoTextOrScopeItem>,
}

#[derive(Clone, PartialEq, Message)]
struct ProtoTextOrScopeItem {
    #[prost(string, tag = "1")]
    text: String,
}

#[derive(Clone, PartialEq, Message)]
struct ProtoPlannerResponse {
    #[prost(string, tag = "1")]
    response: String,
    #[prost(message, repeated, tag = "7")]
    tool_calls: Vec<ProtoToolCall>,
}

#[derive(Clone, PartialEq, Message)]
struct ProtoToolCall {
    #[prost(string, tag = "1")]
    id: String,
    #[prost(string, tag = "2")]
    name: String,
    #[prost(string, tag = "3")]
    arguments_json: String,
}

#[derive(Clone, PartialEq, Message)]
struct ProtoMcpTool {
    #[prost(string, tag = "1")]
    server_name: String,
    #[prost(message, optional, tag = "2")]
    tool_call: Option<ProtoToolCall>,
    #[prost(string, tag = "3")]
    result_string: String,
}

#[derive(Clone, PartialEq, Message)]
struct ProtoErrorDetails {
    #[prost(string, tag = "1")]
    user_error_message: String,
    #[prost(string, tag = "2")]
    model_error_message: String,
    #[prost(string, tag = "3")]
    short_error: String,
    #[prost(string, tag = "9")]
    full_error: String,
}

#[derive(Clone, PartialEq, Message)]
struct ProtoTrajectoryMetadata {
    #[prost(message, repeated, tag = "1")]
    workspaces: Vec<ProtoWorkspaceMetadata>,
    #[prost(message, optional, tag = "2")]
    created_at: Option<ProtoTimestamp>,
    #[prost(string, tag = "3")]
    initialization_state_id: String,
    #[prost(string, tag = "6")]
    root_conversation_id: String,
    #[prost(string, repeated, tag = "7")]
    workspace_uris: Vec<String>,
}

#[derive(Clone, PartialEq, Message)]
struct ProtoWorkspaceMetadata {
    #[prost(string, tag = "1")]
    workspace_folder_absolute_uri: String,
    #[prost(string, tag = "2")]
    git_root_absolute_uri: String,
    #[prost(string, tag = "4")]
    branch_name: String,
}

#[cfg(test)]
pub(crate) fn create_fixture_store(root: &Path) -> Result<()> {
    ensure_directory(root)?;
    ensure_directory(&root.join("conversations"))?;
    let connection = Connection::open(root.join("conversation_summaries.db"))?;
    connection.execute_batch(SUMMARY_SCHEMA)?;
    Ok(())
}

const SUMMARY_SCHEMA: &str = r#"
CREATE TABLE conversation_summaries (
    conversation_id text PRIMARY KEY, title text NOT NULL DEFAULT "",
    preview text NOT NULL DEFAULT "", step_count integer NOT NULL DEFAULT 0,
    last_modified_time datetime NOT NULL, workspace_uris text NOT NULL,
    status text NOT NULL DEFAULT "", source text NOT NULL DEFAULT "",
    project_id text NOT NULL DEFAULT "", agent_name text NOT NULL DEFAULT "",
    parent_conversation_id text NOT NULL DEFAULT "", nesting_depth integer NOT NULL DEFAULT 0,
    battle_id text NOT NULL DEFAULT "", winning_conversation_id text NOT NULL DEFAULT "",
    not_fully_idle numeric NOT NULL DEFAULT false, killed numeric NOT NULL DEFAULT false,
    last_user_input_time datetime NOT NULL, last_user_input_step_index integer NOT NULL DEFAULT -1,
    app_data_dir text NOT NULL DEFAULT ""
);
CREATE INDEX idx_conversation_summaries_last_user_input_time
    ON conversation_summaries (last_user_input_time DESC);
CREATE INDEX idx_conversation_summaries_last_modified_time
    ON conversation_summaries (last_modified_time DESC);
PRAGMA user_version = 1;
"#;

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, sync::Arc};

    use chrono::Utc;
    use omnis_ir::{
        EventKind, EventSource, GitState, OmniEvent, ReplayPolicy, SCHEMA_VERSION, Sensitivity,
        WorkspaceSnapshot,
    };
    use serde_json::json;

    use super::*;
    use crate::private_store_lock::test_support;

    fn assert_mutation_waits<F>(provider_root: &Path, configured_locks: &Path, mutation: F)
    where
        F: FnOnce() -> Result<()> + Send + 'static,
    {
        let guard = lock_root(provider_root, Some(configured_locks))
            .expect("hold Antigravity provider root lock");
        test_support::assert_waits_for_release(mutation, move || drop(guard))
            .expect("complete private mutation");
    }

    #[test]
    fn private_mutations_wait_for_provider_root_lock() {
        let temporary = tempfile::tempdir().expect("temporary provider root");
        let lock_state = tempfile::tempdir().expect("temporary lock root");
        let provider_root =
            fs::canonicalize(temporary.path()).expect("canonical temporary provider root");
        let lock_state_root =
            fs::canonicalize(lock_state.path()).expect("canonical temporary lock root");
        let configured_locks = lock_state_root.join("locks/antigravity");
        let workspace = provider_root.join("workspace");
        fs::create_dir(&workspace).expect("workspace");
        create_summary_database(&provider_root);
        let import = Arc::new(
            build_with_lock_root(
                &snapshot(),
                &workspace,
                provider_root.clone(),
                configured_locks.clone(),
            )
            .expect("build Antigravity import"),
        );

        let materialize_import = Arc::clone(&import);
        assert_mutation_waits(&provider_root, &configured_locks, move || {
            materialize_store_with_lock(&materialize_import)
        });

        let rollback_import = Arc::clone(&import);
        assert_mutation_waits(&provider_root, &configured_locks, move || {
            rollback_store_with_lock(&rollback_import)
        });

        let deletion_import = build_with_lock_root(
            &snapshot(),
            &workspace,
            provider_root.clone(),
            configured_locks.clone(),
        )
        .expect("build Antigravity deletion fixture");
        materialize_store(&deletion_import).expect("materialize deletion fixture");
        let session = deletion_import.target.clone();
        let deletion_provider_root = provider_root.clone();
        let deletion_lock_root = configured_locks.clone();
        assert_mutation_waits(&provider_root, &configured_locks, move || {
            delete_session_at_with_lock_root(&session, &deletion_provider_root, &deletion_lock_root)
        });
        assert!(!deletion_import.target_path.exists());
        assert!(!provider_root.join(".omnisession.lock").exists());
    }

    fn snapshot() -> CanonicalSnapshot {
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
                native_session_id: "synthetic-source".to_owned(),
                provider_version: None,
                raw_record_type: None,
            },
            kind,
            payload: json!({"text": text}),
            raw_blob_hash: None,
            sensitivity: Sensitivity::Normal,
            replay_policy,
        };
        CanonicalSnapshot {
            schema_version: SCHEMA_VERSION.to_owned(),
            session: SessionRef::new(Provider::Claude, "synthetic-source"),
            thread_id,
            branch_id,
            title: Some("Synthetic import".to_owned()),
            captured_at: Utc::now(),
            workspace: WorkspaceSnapshot {
                schema_version: SCHEMA_VERSION.to_owned(),
                captured_at: Utc::now(),
                root: PathBuf::from("/synthetic"),
                current_dir: PathBuf::from("/synthetic"),
                git: GitState::default(),
                instruction_files: Vec::new(),
                environment_names: Vec::new(),
                available_tools: Vec::new(),
            },
            events: vec![
                event(
                    0,
                    EventKind::MessageUser,
                    "question",
                    ReplayPolicy::Contextual,
                ),
                event(
                    1,
                    EventKind::ToolCompleted,
                    "documentary tool",
                    ReplayPolicy::HistoricalOnly,
                ),
                event(
                    2,
                    EventKind::MessageAssistant,
                    "answer",
                    ReplayPolicy::Contextual,
                ),
            ],
        }
    }

    fn create_summary_database(root: &Path) {
        create_fixture_store(root).expect("summary schema");
    }

    #[test]
    fn materializes_reads_back_and_exactly_rolls_back() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let workspace = temporary.path().join("workspace");
        fs::create_dir(&workspace).expect("workspace");
        create_summary_database(temporary.path());
        let import = build_with_root(&snapshot(), &workspace, temporary.path().to_path_buf())
            .expect("Antigravity import");

        materialize_store(&import).expect("materialize Antigravity import");
        assert_eq!(
            fs::read(&import.annotation_path).expect("annotation file"),
            b"title: \"Synthetic import\"\n"
        );
        let readback = AntigravityAdapter::with_root(temporary.path())
            .read_session(&import.target)
            .expect("Antigravity readback");
        assert!(readback_matches(&readback, &import.expected_items));
        assert_eq!(readback.title.as_deref(), Some("Synthetic import"));
        rollback_store(&import).expect("exact rollback");
        assert!(!import.target_path.exists());
        assert!(!import.annotation_path.exists());
        let connection = Connection::open(&import.summary_path).expect("summary database");
        assert!(
            summary_row(&connection, &import.target.id)
                .expect("summary read")
                .is_none()
        );
    }

    #[test]
    fn fresh_profile_materializes_without_a_seeded_summary_database() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let workspace = temporary.path().join("workspace");
        let root = temporary.path().join("fresh-profile");
        fs::create_dir(&workspace).expect("workspace");
        let import = build_with_root(&snapshot(), &workspace, root.clone()).expect("fresh import");

        materialize_store(&import)
            .expect("fresh profile must not require a seeded summary database");
        let adapter = AntigravityAdapter::with_root(&root);
        assert!(readback_matches(
            &adapter.read_session(&import.target).unwrap(),
            &import.expected_items
        ));
        validate_summary_database(&import).expect("verified initialized schema");
        let conversation = fs::read(&import.target_path).unwrap();
        assert!(
            materialize_store(&import).is_err(),
            "a repeated generated ID must not replace a session"
        );
        assert_eq!(fs::read(&import.target_path).unwrap(), conversation);
        let second = build_with_root(&snapshot(), &workspace, root).expect("second import");
        materialize_store(&second).expect("a subsequent import uses the initialized store");
        assert_eq!(adapter.list_sessions(Some(&workspace)).unwrap().len(), 2);
        assert_eq!(fs::read(&import.target_path).unwrap(), conversation);
        rollback_store(&second).expect("second exact rollback");
        rollback_store(&import).expect("first exact rollback");
        assert!(adapter.list_sessions(None).unwrap().is_empty());
    }

    #[test]
    fn fresh_profile_failed_readback_retains_catalog_for_live_connections_and_allows_retry() {
        let temporary = tempfile::tempdir().unwrap();
        let workspace = temporary.path().join("workspace");
        let root = temporary.path().join("fresh-profile");
        fs::create_dir(&workspace).unwrap();
        fs::create_dir(&root).unwrap();
        let sentinel = root.join("unrelated-file");
        fs::write(&sentinel, b"synthetic unrelated data").unwrap();
        let import = build_with_root(&snapshot(), &workspace, root.clone()).unwrap();
        let mut live_provider = None;
        let error = materialize_store_with_verification(
            &import,
            |import| {
                verify_materialized(import)?;
                let connection = Connection::open(&import.summary_path)?;
                assert!(summary_row(&connection, &import.target.id)?.is_some());
                live_provider = Some(connection);
                bail!("synthetic read-back failure")
            },
            || Ok(()),
        )
        .expect_err("the injected read-back failure must abort publication");
        assert!(!crate::rollback_failed(&error), "{error:#}");
        assert!(import.summary_path.is_file());
        assert!(!import.target_path.exists());
        assert!(!import.annotation_path.exists());
        assert_eq!(fs::read(&sentinel).unwrap(), b"synthetic unrelated data");
        assert!(
            fs::read_dir(&import.conversations_root)
                .unwrap()
                .next()
                .is_none()
        );
        assert!(
            fs::read_dir(&import.annotations_root)
                .unwrap()
                .next()
                .is_none()
        );
        assert_eq!(
            fs::read_dir(&root).unwrap().count(),
            4,
            "temporary database and annotation images must be cleaned up while retaining the catalog"
        );
        let live_provider = live_provider.expect("opened live provider connection");
        assert!(
            summary_row(&live_provider, &import.target.id)
                .unwrap()
                .is_none()
        );
        let mut live_row = import.summary.clone();
        live_row.conversation_id = Uuid::new_v4().to_string();
        insert_summary(&live_provider, &live_row).expect(
            "retained catalog must remain writable through an existing provider connection",
        );
        drop(live_provider);

        materialize_store(&import)
            .expect("the same generated import can retry after exact row rollback");
        assert!(readback_matches(
            &AntigravityAdapter::with_root(&root)
                .read_session(&import.target)
                .unwrap(),
            &import.expected_items
        ));
        let verify_conn = Connection::open(&import.summary_path).unwrap();
        assert!(
            summary_row(&verify_conn, &live_row.conversation_id)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn fresh_profile_initialization_does_not_replace_conflicting_store_entries() {
        let temporary = tempfile::tempdir().unwrap();
        let workspace = temporary.path().join("workspace");
        fs::create_dir(&workspace).unwrap();
        for conflict in [
            "schema",
            "malformed",
            "sidecar",
            "conversations",
            "annotations",
        ] {
            let root = temporary.path().join(conflict);
            fs::create_dir(&root).unwrap();
            let path = match conflict {
                "schema" => {
                    let path = root.join("conversation_summaries.db");
                    let connection = Connection::open(&path).unwrap();
                    connection
                        .execute_batch("PRAGMA user_version = 99;")
                        .unwrap();
                    path
                }
                "malformed" => root.join("conversation_summaries.db"),
                "sidecar" => root.join("conversation_summaries.db-wal"),
                "annotations" => root.join("annotations"),
                _ => root.join("conversations"),
            };
            if conflict != "schema" {
                fs::write(&path, b"synthetic conflicting entry").unwrap();
            }
            let before = fs::read(&path).unwrap();
            let import = build_with_root(&snapshot(), &workspace, root.clone()).unwrap();
            assert!(
                materialize_store(&import).is_err(),
                "{conflict} must fail closed"
            );
            assert_eq!(
                fs::read(&path).unwrap(),
                before,
                "{conflict} must remain unchanged"
            );
            assert_eq!(
                fs::read_dir(&root).unwrap().count(),
                1,
                "{conflict} must not publish new files"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn unsupported_provider_version_does_not_create_a_fresh_profile() {
        use std::os::unix::fs::PermissionsExt;

        let temporary = tempfile::tempdir().unwrap();
        let workspace = temporary.path().join("workspace");
        fs::create_dir(&workspace).unwrap();
        let binary = temporary.path().join("agy-version-fixture");
        fs::write(&binary, "#!/bin/sh\nprintf '0.0.1\\n'\n").unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
        let root = temporary.path().join("fresh-profile");
        let import = build_with_root(&snapshot(), &workspace, root.clone()).unwrap();
        let error =
            materialize(&import, &binary).expect_err("an old provider cannot initialize the store");
        assert!(format!("{error:#}").contains("too old"));
        assert!(!root.exists());
    }

    #[test]
    fn failed_publish_rollback_is_tagged_so_fallbacks_refuse_to_launch() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let workspace = temporary.path().join("workspace");
        fs::create_dir(&workspace).expect("workspace");
        create_summary_database(temporary.path());

        let import = build_with_root(&snapshot(), &workspace, temporary.path().to_path_buf())
            .expect("Antigravity import");
        materialize_store(&import).expect("materialize Antigravity import");
        let rolled_back = rollback_after_publish(
            &import,
            true,
            true,
            anyhow::anyhow!("verify failed"),
            || Ok(()),
        )
        .expect_err("publish failure stays an error");
        assert!(!crate::rollback_failed(&rolled_back));
        assert!(!import.target_path.exists());
        assert!(!import.annotation_path.exists());

        let stranded = build_with_root(&snapshot(), &workspace, temporary.path().to_path_buf())
            .expect("Antigravity import");
        materialize_store(&stranded).expect("materialize Antigravity import");
        fs::write(&stranded.target_path, b"changed").expect("tamper target");
        let error = rollback_after_publish(
            &stranded,
            true,
            true,
            anyhow::anyhow!("verify failed"),
            || Ok(()),
        )
        .expect_err("publish failure stays an error");
        assert!(crate::rollback_failed(&error));
        assert!(
            format!("{error:#}")
                .contains("Antigravity import failed and exact rollback also failed")
        );
    }

    #[test]
    fn rollback_refuses_changed_target() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let workspace = temporary.path().join("workspace");
        fs::create_dir(&workspace).expect("workspace");
        create_summary_database(temporary.path());
        let import = build_with_root(&snapshot(), &workspace, temporary.path().to_path_buf())
            .expect("Antigravity import");
        materialize_store(&import).expect("materialize Antigravity import");
        fs::write(&import.target_path, b"changed").expect("tamper target");
        assert!(rollback_store(&import).is_err());
        assert!(import.target_path.exists());
        assert!(import.annotation_path.exists());
    }

    #[test]
    fn rollback_refuses_changed_annotation_and_restores_staged_files() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let workspace = temporary.path().join("workspace");
        fs::create_dir(&workspace).expect("workspace");
        create_summary_database(temporary.path());
        let import = build_with_root(&snapshot(), &workspace, temporary.path().to_path_buf())
            .expect("Antigravity import");
        materialize_store(&import).expect("materialize Antigravity import");
        fs::write(&import.annotation_path, b"title: \"live edit\"\n").expect("tamper annotation");
        let error = rollback_store(&import).expect_err("changed annotation must block rollback");
        assert!(format!("{error:#}").contains("annotation changed"));
        assert_eq!(fs::read(&import.target_path).unwrap(), import.document);
        assert_eq!(
            fs::read(&import.annotation_path).unwrap(),
            b"title: \"live edit\"\n"
        );
        for parent in [&import.conversations_root, &import.annotations_root] {
            let leftovers = fs::read_dir(parent)
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .contains(".omnisession-rollback-")
                })
                .count();
            assert_eq!(leftovers, 0, "staged rollback files must not remain");
        }
    }

    #[test]
    fn deletes_exact_antigravity_summary_and_payloads() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let workspace = temporary.path().join("workspace");
        fs::create_dir(&workspace).expect("workspace");
        create_summary_database(temporary.path());
        let selected = build_with_root(&snapshot(), &workspace, temporary.path().to_path_buf())
            .expect("selected Antigravity import");
        let sibling = build_with_root(&snapshot(), &workspace, temporary.path().to_path_buf())
            .expect("sibling Antigravity import");
        materialize_store(&selected).expect("materialize selected");
        materialize_store(&sibling).expect("materialize sibling");
        let transcript = temporary
            .path()
            .join("brain")
            .join(&selected.target.id)
            .join(".system_generated/logs");
        fs::create_dir_all(&transcript).expect("transcript directory");
        fs::write(transcript.join("transcript.jsonl"), b"{}\n").expect("transcript");

        delete_session_at(&selected.target, temporary.path())
            .expect("delete selected Antigravity session");

        assert!(!selected.target_path.exists());
        assert!(!selected.annotation_path.exists());
        assert!(
            !temporary
                .path()
                .join("brain")
                .join(&selected.target.id)
                .exists()
        );
        assert!(sibling.target_path.exists());
        assert!(sibling.annotation_path.exists());
        let connection = Connection::open(&selected.summary_path).expect("summary database");
        assert!(
            summary_row(&connection, &selected.target.id)
                .expect("selected row")
                .is_none()
        );
        assert!(
            summary_row(&connection, &sibling.target.id)
                .expect("sibling row")
                .is_some()
        );
    }

    #[cfg(unix)]
    #[test]
    fn deletion_refuses_symlinked_payload_parent() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().expect("temporary root");
        let outside = tempfile::tempdir().expect("outside root");
        let workspace = temporary.path().join("workspace");
        fs::create_dir(&workspace).expect("workspace");
        create_summary_database(temporary.path());
        let selected = build_with_root(&snapshot(), &workspace, temporary.path().to_path_buf())
            .expect("selected Antigravity import");
        materialize_store(&selected).expect("materialize selected");
        let outside_conversations = outside.path().join("conversations");
        fs::rename(
            temporary.path().join("conversations"),
            &outside_conversations,
        )
        .expect("move conversations outside root");
        symlink(
            &outside_conversations,
            temporary.path().join("conversations"),
        )
        .expect("symlink conversations root");

        let error = delete_session_at(&selected.target, temporary.path())
            .expect_err("symlinked payload parent must be rejected");

        assert!(error.to_string().contains("unsafe directory"));
        assert!(
            outside_conversations
                .join(format!("{}.db", selected.target.id))
                .exists()
        );
        let connection = Connection::open(&selected.summary_path).expect("summary database");
        assert!(
            summary_row(&connection, &selected.target.id)
                .expect("selected row")
                .is_some()
        );
    }

    #[test]
    fn deletion_refuses_unverified_summary_schema() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let workspace = temporary.path().join("workspace");
        fs::create_dir(&workspace).expect("workspace");
        create_summary_database(temporary.path());
        let selected = build_with_root(&snapshot(), &workspace, temporary.path().to_path_buf())
            .expect("selected Antigravity import");
        materialize_store(&selected).expect("materialize selected");
        Connection::open(&selected.summary_path)
            .expect("summary database")
            .execute(
                "ALTER TABLE conversation_summaries ADD COLUMN unknown text",
                [],
            )
            .expect("alter summary schema");

        let error = delete_session_at(&selected.target, temporary.path())
            .expect_err("unverified schema must be rejected");

        assert!(error.to_string().contains("schema is not verified"));
        assert!(selected.target_path.exists());
    }

    #[test]
    fn retries_committed_antigravity_staging_cleanup() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let workspace = temporary.path().join("workspace");
        fs::create_dir(&workspace).expect("workspace");
        create_summary_database(temporary.path());
        let selected = build_with_root(&snapshot(), &workspace, temporary.path().to_path_buf())
            .expect("selected Antigravity import");
        materialize_store(&selected).expect("materialize selected");
        let staged = selected
            .conversations_root
            .join(format!(".omnisession-delete-{}.db", selected.target.id));
        let staged_annotation = selected
            .annotations_root
            .join(format!(".omnisession-delete-{}.pbtxt", selected.target.id));
        fs::rename(&selected.target_path, &staged).expect("stage conversation");
        fs::rename(&selected.annotation_path, &staged_annotation).expect("stage annotation");
        Connection::open(&selected.summary_path)
            .expect("summary database")
            .execute(
                "DELETE FROM conversation_summaries WHERE conversation_id = ?1",
                [&selected.target.id],
            )
            .expect("commit summary deletion");

        retry_pending_antigravity_deletions(
            &selected.conversations_root,
            &selected.annotations_root,
            &temporary.path().join("brain"),
            &selected.summary_path,
        )
        .expect("retry staged cleanup");

        assert!(!staged.exists());
        assert!(!staged_annotation.exists());
    }

    #[test]
    fn rollback_refuses_changed_summary() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let workspace = temporary.path().join("workspace");
        fs::create_dir(&workspace).expect("workspace");
        create_summary_database(temporary.path());
        let import = build_with_root(&snapshot(), &workspace, temporary.path().to_path_buf())
            .expect("Antigravity import");
        materialize_store(&import).expect("materialize Antigravity import");
        let connection = Connection::open(&import.summary_path).expect("summary database");
        connection
            .execute(
                "UPDATE conversation_summaries SET title = 'changed' WHERE conversation_id = ?1",
                [&import.target.id],
            )
            .expect("tamper summary");
        drop(connection);
        assert!(rollback_store(&import).is_err());
        assert!(import.target_path.exists());
        assert!(
            summary_row(
                &Connection::open(&import.summary_path).expect("summary database"),
                &import.target.id,
            )
            .expect("summary read")
            .is_some()
        );
    }

    #[test]
    fn materializes_and_deletes_across_summary_schema_versions_v2_and_v3() {
        for (label, ddl) in [
            (
                "v2",
                "ALTER TABLE conversation_summaries ADD COLUMN raw_summary blob;\nPRAGMA user_version = 2;\n",
            ),
            (
                "v3-double-quoted-default",
                "PRAGMA journal_mode = WAL;\nALTER TABLE conversation_summaries ADD COLUMN raw_summary blob;\nALTER TABLE conversation_summaries ADD COLUMN group_id text NOT NULL DEFAULT \"\";\nPRAGMA user_version = 3;\n",
            ),
            (
                "v3-single-quoted-default",
                "PRAGMA journal_mode = WAL;\nALTER TABLE conversation_summaries ADD COLUMN raw_summary blob;\nALTER TABLE conversation_summaries ADD COLUMN group_id text NOT NULL DEFAULT '';\nPRAGMA user_version = 3;\n",
            ),
        ] {
            let temporary = tempfile::tempdir().expect("temporary root");
            let workspace = temporary.path().join("workspace");
            fs::create_dir(&workspace).expect("workspace");
            create_summary_database(temporary.path());
            let connection = Connection::open(temporary.path().join("conversation_summaries.db"))
                .expect("open summary database");
            connection.execute_batch(ddl).expect(label);
            drop(connection);

            let first = build_with_root(&snapshot(), &workspace, temporary.path().to_path_buf())
                .expect(label);
            materialize_store(&first).expect(label);
            let readback = AntigravityAdapter::with_root(temporary.path())
                .read_session(&first.target)
                .expect(label);
            assert!(readback_matches(&readback, &first.expected_items));
            rollback_store(&first).expect(label);
            assert!(!first.target_path.exists());
            assert!(!first.annotation_path.exists());

            let second = build_with_root(&snapshot(), &workspace, temporary.path().to_path_buf())
                .expect(label);
            materialize_store(&second).expect(label);
            Connection::open(&second.summary_path)
                .expect(label)
                .execute(
                    "UPDATE conversation_summaries SET raw_summary = X'0801' WHERE conversation_id = ?1",
                    [&second.target.id],
                )
                .expect(label);
            assert!(
                rollback_store(&second).is_err(),
                "{label}: live raw_summary cache update must block rollback"
            );
            assert!(second.target_path.exists());

            delete_session_at(&second.target, temporary.path()).expect(label);
            assert!(!second.target_path.exists());
            assert!(!second.annotation_path.exists());
        }
    }

    #[test]
    fn writes_escaped_title_annotation_and_populates_user_input_items() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let workspace = temporary.path().join("workspace");
        fs::create_dir(&workspace).expect("workspace");
        create_summary_database(temporary.path());
        let mut custom = snapshot();
        custom.title = Some("Fix \"Agy\" \\ path\nline\t\x07 café".to_owned());
        let import = build_with_root(&custom, &workspace, temporary.path().to_path_buf())
            .expect("Antigravity import");
        assert_eq!(import.summary.last_user_input_step_index, 0);
        assert_eq!(
            String::from_utf8_lossy(&import.annotation),
            "title: \"Fix \\\"Agy\\\" \\\\ path\\nline\\t\\007 café\"\n"
        );
        materialize_store(&import).expect("materialize Antigravity import");

        let connection = Connection::open(&import.target_path).expect("open target database");
        let payload: Vec<u8> = connection
            .query_row("SELECT step_payload FROM steps WHERE idx = 0", [], |row| {
                row.get(0)
            })
            .expect("first step payload");
        let decoded = ProtoStep::decode(payload.as_slice()).expect("decode user step");
        assert_eq!(
            decoded.step,
            Some(proto_step::Step::UserInput(ProtoUserInput {
                query: "question".to_owned(),
                user_response: "question".to_owned(),
                items: vec![ProtoTextOrScopeItem {
                    text: "question".to_owned(),
                }],
            }))
        );
    }

    fn codex_test_event(
        sequence: u64,
        kind: EventKind,
        payload: serde_json::Value,
        replay_policy: ReplayPolicy,
    ) -> OmniEvent {
        OmniEvent {
            schema_version: SCHEMA_VERSION.to_owned(),
            event_id: Uuid::new_v4(),
            thread_id: Uuid::nil(),
            branch_id: Uuid::nil(),
            sequence,
            timestamp: None,
            source: EventSource {
                provider: Provider::Codex,
                native_session_id: "codex-source".to_owned(),
                provider_version: None,
                raw_record_type: None,
            },
            kind,
            payload,
            raw_blob_hash: None,
            sensitivity: Sensitivity::Normal,
            replay_policy,
        }
    }

    #[test]
    fn materializes_paired_tool_calls_as_native_steps_and_keeps_orphans_documentary() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let workspace = temporary.path().join("workspace");
        fs::create_dir(&workspace).expect("workspace");
        create_summary_database(temporary.path());
        let mut custom = snapshot();
        custom.session = SessionRef::new(Provider::Codex, "codex-source");
        custom.workspace.root.clone_from(&workspace);
        custom.workspace.current_dir.clone_from(&workspace);
        custom.workspace.git.branch = Some("feature/native-tools".to_owned());
        custom.events = vec![
            codex_test_event(
                0,
                EventKind::MessageUser,
                json!({"text": "inspect workspace"}),
                ReplayPolicy::Contextual,
            ),
            codex_test_event(
                1,
                EventKind::ToolCalled,
                json!({"name": "exec_command", "call_id": "call_1", "arguments": {"cmd": "cargo check"}}),
                ReplayPolicy::HistoricalOnly,
            ),
            codex_test_event(
                2,
                EventKind::ToolCompleted,
                json!({"name": "exec_command", "call_id": "call_1", "output": "ok"}),
                ReplayPolicy::HistoricalOnly,
            ),
            codex_test_event(
                3,
                EventKind::ToolCalled,
                json!({"name": "exec_command", "call_id": "call_2", "arguments": {"cmd": "false"}}),
                ReplayPolicy::HistoricalOnly,
            ),
            codex_test_event(
                4,
                EventKind::ToolFailed,
                json!({"name": "exec_command", "call_id": "call_2", "output": ""}),
                ReplayPolicy::HistoricalOnly,
            ),
            codex_test_event(
                5,
                EventKind::ToolCompleted,
                json!({"text": "orphan documentary output"}),
                ReplayPolicy::HistoricalOnly,
            ),
            codex_test_event(
                6,
                EventKind::MessageAssistant,
                json!({"text": "done"}),
                ReplayPolicy::Contextual,
            ),
        ];

        let import = build_with_root(&custom, &workspace, temporary.path().to_path_buf())
            .expect("Antigravity import with paired tool calls");
        assert_eq!(import.native_tool_records, 2);
        assert_eq!(import.summary.step_count, 7);
        materialize_store(&import).expect("materialize Antigravity import");

        let readback = AntigravityAdapter::with_root(temporary.path())
            .read_session(&import.target)
            .expect("Antigravity readback");
        assert!(readback_matches(&readback, &import.expected_items));

        let connection = Connection::open(&import.target_path).expect("open target database");
        let meta_bytes: Vec<u8> = connection
            .query_row(
                "SELECT data FROM trajectory_metadata_blob WHERE id = 'main'",
                [],
                |r| r.get(0),
            )
            .expect("trajectory metadata");
        drop(connection);
        let meta = ProtoTrajectoryMetadata::decode(meta_bytes.as_slice()).expect("decode metadata");
        assert_eq!(meta.workspaces[0].branch_name, "feature/native-tools");
        assert!(!meta.workspaces[0].git_root_absolute_uri.is_empty());

        rollback_store(&import).expect("exact rollback");
    }

    #[test]
    fn workspace_git_metadata_does_not_leak_across_workspaces_or_parent_dirs() {
        let temporary = tempfile::tempdir().expect("temporary workspace root");
        let parent = omnis_core::canonicalize_path(temporary.path()).expect("canonical temp root");
        let source_repo = parent.join("repo-a");
        let other_repo = parent.join("repo-b");
        let nested_repo = parent.join("nested-repo");
        for dir in [&source_repo, &other_repo, &nested_repo] {
            fs::create_dir(dir).expect("create workspace");
        }
        let mut custom = snapshot();
        custom.workspace.root.clone_from(&source_repo);
        custom.workspace.current_dir.clone_from(&source_repo);
        custom.workspace.git.branch = Some("feature/source-only".to_owned());

        // Cross-workspace import into another directory must not copy the source branch.
        let other_uri = native_path::file_uri(&other_repo).expect("other uri");
        let (git_root_uri, branch_name) = workspace_git_metadata(&custom, &other_repo, &other_uri);
        assert_eq!(git_root_uri, other_uri);
        assert!(branch_name.is_empty());

        // Importing from a non-git parent directory into a nested directory must not treat the
        // parent as the nested directory's git root.
        custom.workspace.root = parent;
        custom.workspace.git.branch = Some("stale-parent-branch".to_owned());
        let nested_uri = native_path::file_uri(&nested_repo).expect("nested uri");
        let (nested_git_root, nested_branch) =
            workspace_git_metadata(&custom, &nested_repo, &nested_uri);
        assert_eq!(nested_git_root, nested_uri);
        assert!(nested_branch.is_empty());
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    struct RunningAntigravity(std::process::Child);

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    impl Drop for RunningAntigravity {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn start_synthetic_antigravity(root: &Path) -> RunningAntigravity {
        use std::{
            process::{Command, Stdio},
            time::Instant,
        };

        let source = root.join("agy.rs");
        let process = root.join("agy");
        fs::write(
            &source,
            "fn main() { let _ = std::io::stdin().read_line(&mut String::new()); }\n",
        )
        .expect("synthetic Antigravity source");
        let rustc = env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
        let compiled = Command::new(rustc)
            .arg(&source)
            .arg("-o")
            .arg(&process)
            .output()
            .expect("compile synthetic Antigravity binary");
        assert!(
            compiled.status.success(),
            "compiling synthetic Antigravity binary failed: {}",
            String::from_utf8_lossy(&compiled.stderr)
        );
        let running = RunningAntigravity(
            Command::new(&process)
                .stdin(Stdio::piped())
                .spawn()
                .expect("start synthetic Antigravity process"),
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        while ensure_no_active_antigravity_process().is_ok() {
            assert!(
                Instant::now() < deadline,
                "synthetic Antigravity CLI process never appeared"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        running
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn import_publishes_but_preserves_live_session_on_rollback() {
        use std::{os::unix::fs::PermissionsExt, process::Command};

        let temporary = tempfile::tempdir().expect("temporary Antigravity home");
        let root = temporary.path().canonicalize().expect("canonical home");
        let running = start_synthetic_antigravity(&root);

        let binary = root.join("agy-launcher");
        fs::write(&binary, "#!/bin/sh\necho '1.5.7'\n").expect("launcher");
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).expect("launcher mode");
        crate::test_support::output_after_write(Command::new(&binary).arg("--version"));

        let workspace = root.join("workspace");
        fs::create_dir(&workspace).expect("workspace");
        let data_root = root.join("store");
        let lock_root = root.join("locks/antigravity");
        let import = build_with_lock_root(&snapshot(), &workspace, data_root, lock_root)
            .expect("build import");

        let guard = materialize(&import, &binary).expect("import while Antigravity CLI is running");
        assert!(import.target_path.is_file());
        assert!(import.annotation_path.is_file());
        let connection = Connection::open(&import.target_path).expect("live conversation writer");
        connection
            .execute_batch(
                "BEGIN IMMEDIATE; UPDATE steps SET step_payload = X'0801' WHERE idx = 0;",
            )
            .expect("begin live conversation update");
        assert_eq!(fs::read(&import.target_path).unwrap(), import.document);
        rollback_locked(&import, &guard).expect_err("active provider must block rollback");
        assert!(import.target_path.is_file());
        assert_eq!(
            fs::read(&import.annotation_path).unwrap(),
            import.annotation
        );
        assert_eq!(
            stored_summary(&import).unwrap(),
            Some(import.summary.clone())
        );
        connection
            .execute_batch("COMMIT")
            .expect("live update must remain writable after refused rollback");
        let stored: Vec<u8> = Connection::open(&import.target_path)
            .unwrap()
            .query_row("SELECT step_payload FROM steps WHERE idx = 0", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(stored, [8, 1]);

        let failed = build_with_root(&snapshot(), &workspace, import.root.clone()).unwrap();
        let error = materialize_store_with_verification(
            &failed,
            |_| bail!("synthetic read-back failure"),
            ensure_no_active_antigravity_process,
        )
        .expect_err("failed verification must preserve a published live conversation");
        assert!(crate::rollback_failed(&error));
        assert_eq!(fs::read(&failed.target_path).unwrap(), failed.document);
        assert_eq!(
            fs::read(&failed.annotation_path).unwrap(),
            failed.annotation
        );
        assert_eq!(
            stored_summary(&failed).unwrap(),
            Some(failed.summary.clone())
        );

        let unpublished = build_with_root(&snapshot(), &workspace, import.root.clone()).unwrap();
        write_atomic_private_file(
            &unpublished.conversations_root,
            &unpublished.target_path,
            &unpublished.document,
            "synthetic unpublished conversation",
        )
        .unwrap();
        let error = rollback_after_publish(
            &unpublished,
            false,
            false,
            anyhow::anyhow!("synthetic annotation publication failure"),
            ensure_no_active_antigravity_process,
        )
        .expect_err("failed publication must preserve a live conversation database");
        assert!(crate::rollback_failed(&error));
        assert_eq!(
            fs::read(&unpublished.target_path).unwrap(),
            unpublished.document
        );
        assert!(!unpublished.annotation_path.exists());
        assert!(stored_summary(&unpublished).unwrap().is_none());

        let refusal =
            ensure_no_active_antigravity_process().expect_err("deletion guard still refuses");
        assert!(
            refusal
                .to_string()
                .contains("while Antigravity CLI is running")
        );
        drop(running);
        rollback_store(&failed).expect("unchanged conversation can be rolled back after shutdown");
        assert!(!failed.target_path.exists());
        assert!(!failed.annotation_path.exists());
        assert!(stored_summary(&failed).unwrap().is_none());
    }

    #[test]
    fn macos_process_parser_matches_antigravity_cli_only() {
        let cli = "  4294967294 agy /Users/demo/.local/bin/agy resume session\n";
        assert_eq!(antigravity_pid_from_macos_ps(cli, 10), Some(4_294_967_294));

        let language_server = "  321 language_server_ /Users/demo/.gemini/antigravity-cli/bin/language_server_macos_arm64 --app_data_dir=/Users/demo/.gemini/antigravity-cli\n";
        assert_eq!(
            antigravity_pid_from_macos_ps(language_server, 10),
            Some(321)
        );

        assert_eq!(antigravity_pid_from_macos_ps(cli, 4_294_967_294), None);
    }

    #[test]
    fn macos_process_parser_handles_spaced_executable_paths() {
        let output = r"
  401 agy /Users/First Last/.local/bin/agy resume session
  402 language_server_ /Users/First Last/.gemini/antigravity-cli/bin/language_server_macos_arm64 --app_data_dir=/Users/First Last/.gemini/antigravity-cli
";

        assert_eq!(antigravity_pid_from_macos_ps(output, 10), Some(401));
        assert_eq!(antigravity_pid_from_macos_ps(output, 401), Some(402));
    }

    #[test]
    fn macos_process_parser_excludes_ide_and_unrelated_processes() {
        let output = r"
  100 Antigravity /Applications/Antigravity.app/Contents/MacOS/Antigravity
  101 language_server_ /Applications/Antigravity.app/Contents/Resources/app/language_server_macos_arm64 --app_data_dir=/Users/demo/.antigravity
  102 agy-helper /usr/local/bin/agy-helper
  103 vim /usr/bin/vim agy
  104 language_server_ /usr/local/bin/language_server_macos_arm64 --app_data_dir=/tmp/unrelated
  105 language_server_ /usr/local/bin/language_server_macos_arm64 --app_data_dir=/tmp/antigravity-cli-backup
  invalid agy /Users/demo/.local/bin/agy
";

        assert_eq!(antigravity_pid_from_macos_ps(output, 10), None);
    }

    #[test]
    fn private_deletion_requires_verified_writer_detection() {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        assert!(ensure_deletion_platform_supported().is_ok());

        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        assert_eq!(
            ensure_deletion_platform_supported()
                .expect_err("private deletion must be rejected outside Linux and macOS")
                .to_string(),
            "native Antigravity CLI deletion is supported only on Linux and macOS"
        );
    }

    #[test]
    fn version_gate_accepts_newer_antigravity_releases() {
        assert!(!is_supported_version("1.1.7"));
        assert!(is_supported_version("1.1.8"));
        assert!(is_supported_version("1.2.0"));
    }
}
