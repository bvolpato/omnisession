use std::{
    env, fs,
    io::{Read, Seek},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use chrono::{SecondsFormat, Utc};
use directories::BaseDirs;
use omnis_adapters::{AntigravityAdapter, ProviderAdapter};
use omnis_core::{HandoffMessage, HandoffRole, TrajectoryItemKind, import_trajectory};
use omnis_ir::{CanonicalSnapshot, Provider, SessionRef};
use prost::Message;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use sha2::{Digest, Sha256};
use tempfile::{NamedTempFile, TempPath};
use uuid::Uuid;
use wait_timeout::ChildExt;

const SUPPORTED_VERSION: &str = "1.1.8";
const SUPPORTED_SHA256: &str = "90464ef203a5ba44e18bd779bb4b5920536374a8328a4648f1e8702b7a4342e6";
const MAX_VERSION_OUTPUT: u64 = 8 * 1024;
const TRAJECTORY_TYPE_CASCADE: i64 = 4;
const TRAJECTORY_SOURCE_CLI: i64 = 17;
const STEP_USER_INPUT: i32 = 14;
const STEP_PLANNER_RESPONSE: i32 = 15;
const STEP_STATUS_DONE: i32 = 3;
const STEP_SOURCE_MODEL: i32 = 2;
const STEP_SOURCE_USER_EXPLICIT: i32 = 4;

pub struct AntigravityImport {
    pub target: SessionRef,
    pub expected_messages: Vec<HandoffMessage>,
    pub history_items: usize,
    pub tool_events: usize,
    pub truncated: bool,
    root: PathBuf,
    conversations_root: PathBuf,
    summary_path: PathBuf,
    target_path: PathBuf,
    document: Vec<u8>,
    summary: SummaryRow,
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
}

pub fn build(snapshot: &CanonicalSnapshot, cwd: &Path) -> Result<AntigravityImport> {
    build_with_root(snapshot, cwd, data_root()?)
}

pub(crate) fn build_with_root(
    snapshot: &CanonicalSnapshot,
    cwd: &Path,
    root: PathBuf,
) -> Result<AntigravityImport> {
    if !cwd.is_absolute() {
        bail!("Antigravity native import requires an absolute workspace path");
    }
    let canonical_cwd = fs::canonicalize(cwd)
        .with_context(|| format!("canonicalizing Antigravity workspace `{}`", cwd.display()))?;
    if !canonical_cwd.is_dir() {
        bail!("Antigravity native import workspace is not a directory");
    }
    let cwd = canonical_cwd
        .to_str()
        .context("Antigravity native import requires a UTF-8 workspace path")?;
    let trajectory = import_trajectory(snapshot);
    if trajectory.items.is_empty() {
        bail!("source has no visible trajectory eligible for Antigravity import");
    }
    let history_items = trajectory.items.len();
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
    let target = SessionRef::new(Provider::Antigravity, &id);
    let trajectory_id = Uuid::new_v4().to_string();
    let initialization_state_id = Uuid::new_v4().to_string();
    let workspace_uri = file_uri(cwd);
    let now = Utc::now();
    let timestamp = now.to_rfc3339_opts(SecondsFormat::Nanos, true);
    let document = build_database(
        &target.id,
        &trajectory_id,
        &initialization_state_id,
        &workspace_uri,
        now.timestamp(),
        now.timestamp_subsec_nanos(),
        &expected_messages,
    )?;
    let summary = SummaryRow {
        conversation_id: id.clone(),
        title: format!("Imported from {}", snapshot.session),
        preview: String::new(),
        step_count: i64::try_from(expected_messages.len())?,
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
        last_user_input_step_index: -1,
        app_data_dir: "antigravity-cli".to_owned(),
    };
    let conversations_root = root.join("conversations");
    let summary_path = root.join("conversation_summaries.db");
    let target_path = conversations_root.join(format!("{id}.db"));
    Ok(AntigravityImport {
        target,
        expected_messages,
        history_items,
        tool_events: trajectory.tool_events,
        truncated: trajectory.truncated,
        root,
        conversations_root,
        summary_path,
        target_path,
        document,
        summary,
    })
}

pub fn ensure_supported(binary: &Path) -> Result<String> {
    if !cfg!(target_os = "linux") {
        bail!("native Antigravity import is currently supported only on Linux");
    }
    let version = installed_version(binary)?;
    if version != SUPPORTED_VERSION {
        bail!(
            "Antigravity CLI {version} is not verified for native trajectory import; supported version: {SUPPORTED_VERSION}"
        );
    }
    let actual = sha256_file(binary)?;
    if actual != SUPPORTED_SHA256 {
        bail!("Antigravity CLI {version} binary fingerprint is not verified");
    }
    Ok(version)
}

pub fn materialize(import: &AntigravityImport, binary: &Path) -> Result<()> {
    ensure_supported(binary)?;
    ensure_no_active_antigravity_process()?;
    materialize_store(import)
}

pub(crate) fn materialize_store(import: &AntigravityImport) -> Result<()> {
    validate_import_paths(import, "writing")?;
    validate_summary_database(import)?;
    let connection = Connection::open(&import.summary_path)?;
    connection.busy_timeout(Duration::ZERO)?;
    if summary_row(&connection, &import.target.id)?.is_some() {
        bail!("generated Antigravity summary already exists");
    }
    drop(connection);
    ensure_directory(&import.conversations_root)?;
    validate_directory_chain(&import.conversations_root, &import.root, "writing")?;
    if import.target_path.exists() {
        bail!("generated Antigravity target session already exists");
    }

    let mut temporary = NamedTempFile::new_in(&import.conversations_root)
        .context("creating temporary Antigravity conversation database")?;
    set_private_permissions(temporary.as_file())?;
    std::io::Write::write_all(&mut temporary, &import.document)
        .context("writing Antigravity conversation database")?;
    std::io::Write::flush(&mut temporary)?;
    temporary.as_file().sync_all()?;
    temporary
        .persist_noclobber(&import.target_path)
        .map_err(|error| error.error)
        .context("publishing Antigravity conversation database")?;
    sync_directory(&import.conversations_root)?;

    let result = (|| {
        let mut connection = Connection::open(&import.summary_path)
            .context("opening Antigravity summary database")?;
        connection.busy_timeout(Duration::ZERO)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if summary_row(&transaction, &import.target.id)?.is_some() {
            bail!("generated Antigravity summary already exists");
        }
        insert_summary(&transaction, &import.summary)?;
        transaction.commit()?;
        verify_materialized(import)?;
        Ok(())
    })();
    if let Err(error) = result {
        return rollback_after_publish(import, error);
    }
    Ok(())
}

pub fn rollback(import: &AntigravityImport) -> Result<()> {
    ensure_no_active_antigravity_process()?;
    rollback_store(import)
}

pub(crate) fn rollback_store(import: &AntigravityImport) -> Result<()> {
    validate_import_paths(import, "rolling back")?;
    validate_generated_file(import)?;
    let mut connection = Connection::open(&import.summary_path)
        .context("opening Antigravity summary database for rollback")?;
    connection.busy_timeout(Duration::ZERO)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if summary_row(&transaction, &import.target.id)?.as_ref() != Some(&import.summary) {
        bail!("refusing to delete changed Antigravity summary row");
    }
    let rollback_path = import
        .conversations_root
        .join(format!(".omnisession-rollback-{}.db", import.target.id));
    if rollback_path.exists() {
        bail!("Antigravity rollback staging path already exists");
    }
    fs::rename(&import.target_path, &rollback_path)
        .context("staging Antigravity target for rollback")?;
    if let Err(error) = transaction
        .execute(
            "DELETE FROM conversation_summaries WHERE conversation_id = ?1",
            [&import.target.id],
        )
        .and_then(|deleted| {
            if deleted == 1 {
                Ok(())
            } else {
                Err(rusqlite::Error::QueryReturnedNoRows)
            }
        })
        .and_then(|()| transaction.commit())
    {
        fs::rename(&rollback_path, &import.target_path)
            .context("restoring Antigravity target after rollback failure")?;
        return Err(error).context("deleting Antigravity summary row");
    }
    fs::remove_file(&rollback_path).context("removing generated Antigravity target")?;
    sync_directory(&import.conversations_root)
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

fn verify_materialized(import: &AntigravityImport) -> Result<()> {
    validate_generated_file(import)?;
    let connection = Connection::open(&import.summary_path)?;
    if summary_row(&connection, &import.target.id)?.as_ref() != Some(&import.summary) {
        bail!("Antigravity summary read-back changed imported metadata");
    }
    let snapshot = AntigravityAdapter::with_root(&import.root)
        .read_session(&import.target)
        .context("reading back imported Antigravity trajectory")?;
    if !readback_matches(&snapshot, &import.expected_messages) {
        bail!("Antigravity trajectory read-back did not match imported history");
    }
    Ok(())
}

fn rollback_after_publish(import: &AntigravityImport, error: anyhow::Error) -> Result<()> {
    let stored = stored_summary(import)?;
    let rollback = if stored.as_ref() == Some(&import.summary) {
        rollback_store(import)
    } else {
        validate_generated_file(import).and_then(|()| {
            fs::remove_file(&import.target_path)
                .context("removing unpublished Antigravity target")?;
            sync_directory(&import.conversations_root)
        })
    };
    match rollback {
        Ok(()) => Err(error),
        Err(rollback_error) => Err(error).context(format!(
            "Antigravity import failed and exact rollback also failed: {rollback_error}"
        )),
    }
}

fn stored_summary(import: &AntigravityImport) -> Result<Option<SummaryRow>> {
    let connection = Connection::open(&import.summary_path)?;
    summary_row(&connection, &import.target.id)
}

fn validate_generated_file(import: &AntigravityImport) -> Result<()> {
    validate_import_paths(import, "validating")?;
    let metadata = fs::symlink_metadata(&import.target_path)
        .context("reading generated Antigravity target metadata")?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        bail!("generated Antigravity target is not a regular file");
    }
    let content = fs::read(&import.target_path)
        .context("reading generated Antigravity conversation database")?;
    if content != import.document {
        bail!("generated Antigravity target changed after materialization");
    }
    let connection = Connection::open_with_flags(
        &import.target_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
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

fn validate_import_paths(import: &AntigravityImport, operation: &str) -> Result<()> {
    if !import.root.is_absolute()
        || import.target.provider != Provider::Antigravity
        || Uuid::parse_str(&import.target.id).is_err()
        || import.summary.conversation_id != import.target.id
        || import.conversations_root != import.root.join("conversations")
        || import.summary_path != import.root.join("conversation_summaries.db")
        || import.target_path
            != import
                .conversations_root
                .join(format!("{}.db", import.target.id))
    {
        bail!("refusing {operation} with unverified Antigravity target paths");
    }
    Ok(())
}

fn validate_summary_database(import: &AntigravityImport) -> Result<()> {
    validate_directory_chain(&import.root, &import.root, "opening")?;
    let metadata = fs::symlink_metadata(&import.summary_path)
        .context("Antigravity summary database does not exist")?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        bail!("Antigravity summary database is not a regular file");
    }
    let connection = Connection::open_with_flags(
        &import.summary_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let version = connection.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))?;
    if version != 1 {
        bail!("Antigravity summary schema version {version} is not supported");
    }
    connection.prepare(
        "SELECT conversation_id, title, preview, step_count, last_modified_time, workspace_uris, \
         status, source, project_id, agent_name, parent_conversation_id, nesting_depth, battle_id, \
         winning_conversation_id, not_fully_idle, killed, last_user_input_time, \
         last_user_input_step_index, app_data_dir FROM conversation_summaries LIMIT 0",
    )?;
    Ok(())
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
    connection
        .query_row(
            "SELECT conversation_id, title, preview, step_count, last_modified_time, workspace_uris,
                    status, source, project_id, agent_name, parent_conversation_id, nesting_depth,
                    battle_id, winning_conversation_id, not_fully_idle, killed,
                    last_user_input_time, last_user_input_step_index, app_data_dir
             FROM conversation_summaries WHERE conversation_id = ?1",
            [id],
            |row| {
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
                })
            },
        )
        .optional()
        .map_err(Into::into)
}

fn build_database(
    cascade_id: &str,
    trajectory_id: &str,
    initialization_state_id: &str,
    workspace_uri: &str,
    seconds: i64,
    nanos: u32,
    messages: &[HandoffMessage],
) -> Result<Vec<u8>> {
    let temporary = NamedTempFile::new().context("creating Antigravity database image")?;
    let temporary_path = temporary.into_temp_path();
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
                git_root_absolute_uri: workspace_uri.to_owned(),
            }],
            created_at: Some(ProtoTimestamp {
                seconds,
                nanos: i32::try_from(nanos)?,
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
        for (index, message) in messages.iter().enumerate() {
            let (step_type, source, step) = match message.role {
                HandoffRole::User => (
                    STEP_USER_INPUT,
                    STEP_SOURCE_USER_EXPLICIT,
                    proto_step::Step::UserInput(ProtoUserInput {
                        query: message.text.clone(),
                    }),
                ),
                HandoffRole::Assistant => (
                    STEP_PLANNER_RESPONSE,
                    STEP_SOURCE_MODEL,
                    proto_step::Step::PlannerResponse(ProtoPlannerResponse {
                        response: message.text.clone(),
                    }),
                ),
            };
            let step_seconds = seconds.saturating_add(i64::try_from(index)?);
            let metadata = ProtoStepMetadata {
                created_at: Some(ProtoTimestamp {
                    seconds: step_seconds,
                    nanos: i32::try_from(nanos)?,
                }),
                source,
                execution_id: Uuid::new_v4().to_string(),
            };
            let metadata_bytes = metadata.encode_to_vec();
            let payload = ProtoStep {
                r#type: step_type,
                status: STEP_STATUS_DONE,
                metadata: Some(metadata),
                step: Some(step),
            }
            .encode_to_vec();
            transaction.execute(
                "INSERT INTO steps (
                    idx, step_type, status, has_subtrajectory, metadata, step_payload, step_format
                 ) VALUES (?1, ?2, ?3, false, ?4, ?5, 0)",
                params![
                    i64::try_from(index)?,
                    step_type,
                    STEP_STATUS_DONE,
                    metadata_bytes,
                    payload
                ],
            )?;
        }
        transaction.commit()?;
        connection.pragma_update(None, "user_version", 1)?;
        connection.execute_batch("PRAGMA optimize;")?;
    }
    let bytes = fs::read(&temporary_path)?;
    TempPath::close(temporary_path)?;
    Ok(bytes)
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

#[cfg(not(unix))]
fn set_private_permissions(_file: &fs::File) -> Result<()> {
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

fn installed_version(binary: &Path) -> Result<String> {
    let mut output = NamedTempFile::new().context("creating Antigravity version buffer")?;
    let mut child = Command::new(binary)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::from(output.reopen()?))
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("executing `{}`", binary.display()))?;
    let Some(status) = child
        .wait_timeout(Duration::from_secs(5))
        .context("waiting for Antigravity version")?
    else {
        child.kill().context("stopping Antigravity version probe")?;
        let _ = child.wait();
        bail!("Antigravity version probe timed out");
    };
    if !status.success() {
        bail!("Antigravity version probe exited with status {status}");
    }
    if output.as_file().metadata()?.len() > MAX_VERSION_OUTPUT {
        bail!("Antigravity version output exceeds safe limit");
    }
    output.as_file_mut().rewind()?;
    let mut text = String::new();
    output
        .as_file_mut()
        .take(MAX_VERSION_OUTPUT + 1)
        .read_to_string(&mut text)?;
    parse_version(&text).context("Antigravity returned an unrecognized version")
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

fn sha256_file(path: &Path) -> Result<String> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        bail!("Antigravity binary is not a regular file");
    }
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
        let is_antigravity = comm.trim() == "agy"
            || (comm.trim().starts_with("language_server")
                && cmdline
                    .windows(b"antigravity-cli".len())
                    .any(|window| window == b"antigravity-cli"));
        if !is_antigravity {
            continue;
        }
        let status = fs::read_to_string(process.join("status")).unwrap_or_default();
        let zombie = status
            .lines()
            .find(|line| line.starts_with("State:"))
            .is_some_and(|line| line.split_whitespace().nth(1) == Some("Z"));
        if !zombie {
            bail!("refusing native Antigravity store mutation while Antigravity is running");
        }
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn ensure_no_active_antigravity_process() -> Result<()> {
    bail!("native Antigravity import process-safety guard is currently supported only on Linux")
}

#[derive(Clone, PartialEq, Message)]
struct ProtoStep {
    #[prost(int32, tag = "1")]
    r#type: i32,
    #[prost(int32, tag = "4")]
    status: i32,
    #[prost(message, optional, tag = "5")]
    metadata: Option<ProtoStepMetadata>,
    #[prost(oneof = "proto_step::Step", tags = "19, 20")]
    step: Option<proto_step::Step>,
}

mod proto_step {
    use prost::Oneof;

    use super::{ProtoPlannerResponse, ProtoUserInput};

    #[derive(Clone, PartialEq, Oneof)]
    pub(super) enum Step {
        #[prost(message, tag = "19")]
        UserInput(ProtoUserInput),
        #[prost(message, tag = "20")]
        PlannerResponse(ProtoPlannerResponse),
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
    #[prost(string, tag = "12")]
    execution_id: String,
}

#[derive(Clone, PartialEq, Message)]
struct ProtoUserInput {
    #[prost(string, tag = "1")]
    query: String,
}

#[derive(Clone, PartialEq, Message)]
struct ProtoPlannerResponse {
    #[prost(string, tag = "1")]
    response: String,
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
}

#[cfg(test)]
pub(crate) fn create_fixture_store(root: &Path) -> Result<()> {
    ensure_directory(root)?;
    ensure_directory(&root.join("conversations"))?;
    let connection = Connection::open(root.join("conversation_summaries.db"))?;
    connection.execute_batch(SUMMARY_SCHEMA)?;
    Ok(())
}

#[cfg(test)]
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
    use std::path::PathBuf;

    use chrono::Utc;
    use omnis_ir::{
        EventKind, EventSource, GitState, OmniEvent, ReplayPolicy, SCHEMA_VERSION, Sensitivity,
        WorkspaceSnapshot,
    };
    use serde_json::json;

    use super::*;

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
        let readback = AntigravityAdapter::with_root(temporary.path())
            .read_session(&import.target)
            .expect("Antigravity readback");
        assert!(readback_matches(&readback, &import.expected_messages));
        rollback_store(&import).expect("exact rollback");
        assert!(!import.target_path.exists());
        let connection = Connection::open(&import.summary_path).expect("summary database");
        assert!(
            summary_row(&connection, &import.target.id)
                .expect("summary read")
                .is_none()
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
    fn version_parser_requires_full_semver() {
        assert_eq!(parse_version("agy 1.1.8"), Some("1.1.8".to_owned()));
        assert_eq!(parse_version("1.1"), None);
    }
}
