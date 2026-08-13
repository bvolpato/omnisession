use std::{
    env, fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::OnceLock,
    time::{Duration, SystemTime},
};

use anyhow::{Context, Result, bail};
#[cfg(test)]
use chrono::Utc;
use directories::BaseDirs;
use omnis_adapters::{HermesAdapter, ProviderAdapter};
use omnis_core::{
    HandoffMessage, HandoffRole, TrajectoryItemKind, import_trajectory, redact_secrets,
    safe_terminal_line,
};
use omnis_ir::{CanonicalSnapshot, Provider, SessionRef};
#[cfg(test)]
use rusqlite::{Connection, OpenFlags, TransactionBehavior, params};
use serde_json::json;
#[cfg(test)]
use std::collections::HashSet;
use uuid::Uuid;
use wait_timeout::ChildExt;

const MINIMUM_HERMES_VERSION: &str = "0.19.1";
const HERMES_TITLE_BASE_CHARACTER_LIMIT: usize = 88;
#[cfg(test)]
const MINIMUM_SCHEMA_VERSION: i64 = 23;

pub struct HermesImport {
    pub target: SessionRef,
    pub expected_messages: Vec<HandoffMessage>,
    pub history_items: usize,
    pub tool_events: usize,
    pub truncated: bool,
    root: PathBuf,
    #[cfg(test)]
    database: PathBuf,
    title: Option<String>,
    resolved_title: OnceLock<Option<String>>,
    cwd: PathBuf,
    git_branch: Option<String>,
    git_repo_root: Option<String>,
    parent_session_id: Option<String>,
    model_config: String,
    started_at: f64,
}

pub fn build(snapshot: &CanonicalSnapshot, cwd: &Path) -> Result<HermesImport> {
    build_with_root(snapshot, cwd, hermes_root()?)
}

pub(crate) fn build_with_root(
    snapshot: &CanonicalSnapshot,
    cwd: &Path,
    root: PathBuf,
) -> Result<HermesImport> {
    let trajectory = import_trajectory(snapshot);
    if trajectory.items.is_empty() {
        bail!("source has no visible trajectory eligible for Hermes import");
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
    let cwd = cwd
        .canonicalize()
        .with_context(|| format!("canonicalizing Hermes workspace `{}`", cwd.display()))?;
    if !cwd.is_dir() {
        bail!("Hermes native import workspace is not a directory")
    }
    let target = SessionRef::new(
        Provider::Hermes,
        format!("omni_{}", Uuid::new_v4().simple()),
    );
    let parent_session_id =
        (snapshot.session.provider == Provider::Hermes).then(|| snapshot.session.id.clone());
    let model_config = if let Some(parent) = &parent_session_id {
        json!({
            "_branched_from": parent,
            "omnisession_source": snapshot.session.to_string(),
        })
    } else {
        json!({ "omnisession_source": snapshot.session.to_string() })
    }
    .to_string();
    let title = imported_title(snapshot.title.as_deref());
    let git_repo_root = snapshot
        .workspace
        .git
        .remote_fingerprint
        .as_ref()
        .and_then(|_| cwd.to_str().map(str::to_owned));
    #[cfg(test)]
    let database = root.join("state.db");
    Ok(HermesImport {
        target,
        expected_messages,
        history_items,
        tool_events: trajectory.tool_events,
        truncated: trajectory.truncated,
        root,
        #[cfg(test)]
        database,
        title,
        resolved_title: OnceLock::new(),
        cwd,
        git_branch: snapshot.workspace.git.branch.clone(),
        git_repo_root,
        parent_session_id,
        model_config,
        started_at: SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .context("system clock predates Unix epoch")?
            .as_secs_f64(),
    })
}

fn imported_title(source: Option<&str>) -> Option<String> {
    let source = source.map(redact_secrets).map(|title| {
        safe_terminal_line(&title)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    })?;
    if source.is_empty() {
        return None;
    }

    let source = source
        .chars()
        .take(HERMES_TITLE_BASE_CHARACTER_LIMIT)
        .collect::<String>();
    let source = source.trim();
    if source.is_empty() {
        None
    } else {
        Some(source.to_owned())
    }
}

fn effective_title(import: &HermesImport) -> Option<&str> {
    import
        .resolved_title
        .get()
        .and_then(Option::as_deref)
        .or(import.title.as_deref())
}

pub fn ensure_supported(binary: &Path) -> Result<String> {
    let version = installed_version(binary)?;
    if !is_supported_version(&version) {
        bail!(
            "Hermes {version} is too old for native session import; supported versions: >= {MINIMUM_HERMES_VERSION}"
        );
    }
    let _ = python_interpreter(binary)?;
    Ok(version)
}

pub fn materialize(import: &HermesImport, binary: &Path) -> Result<()> {
    ensure_supported(binary)?;
    validate_import_root(import)?;
    if HermesAdapter::with_root(&import.root)
        .read_session(&import.target)
        .is_ok()
    {
        bail!("generated Hermes target session already exists")
    }
    let messages = import
        .expected_messages
        .iter()
        .enumerate()
        .map(|(index, message)| -> Result<serde_json::Value> {
            let offset = f64::from(u32::try_from(index)?);
            Ok(json!({
                "role": match message.role {
                    HandoffRole::User => "user",
                    HandoffRole::Assistant => "assistant",
                },
                "content": message.text,
                "timestamp": import.started_at + (offset + 1.0) / 1_000.0,
                "observed": true,
            }))
        })
        .collect::<Result<Vec<_>>>()?;
    let payload = json!([{
        "id": import.target.id,
        "source": "cli",
        "model_config": import.model_config,
        "parent_session_id": import.parent_session_id,
        "started_at": import.started_at,
        "message_count": import.expected_messages.len(),
        "cwd": import.cwd,
        "git_branch": import.git_branch,
        "git_repo_root": import.git_repo_root,
        "title": import.title,
        "messages": messages,
    }]);
    let result = run_provider_import(binary, &import.root, &serde_json::to_vec(&payload)?)?;
    let resolved_title = result
        .get("resolved_titles")
        .and_then(serde_json::Value::as_object)
        .and_then(|titles| titles.get(&import.target.id))
        .context("Hermes provider importer omitted resolved title")?;
    let resolved_title = serde_json::from_value::<Option<String>>(resolved_title.clone())
        .context("Hermes provider importer returned invalid resolved title")?;
    import
        .resolved_title
        .set(resolved_title)
        .map_err(|_| anyhow::anyhow!("Hermes import title was already resolved"))?;
    let imported = result
        .get("imported_ids")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|ids| ids.len() == 1 && ids[0].as_str() == Some(import.target.id.as_str()));
    if result.get("ok").and_then(serde_json::Value::as_bool) != Some(true) || !imported {
        bail!("Hermes provider importer did not create exact target session")
    }
    if import.parent_session_id.is_some()
        && result.get("detached").and_then(serde_json::Value::as_u64) != Some(0)
    {
        return Err(combine_rollback_error(
            anyhow::anyhow!("Hermes provider importer detached native fork parent"),
            rollback(import, binary),
        ));
    }
    if let Err(error) = verify(import) {
        return Err(combine_rollback_error(
            error.context("Hermes import failed read-back verification"),
            rollback(import, binary),
        ));
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn materialize_store(import: &HermesImport) -> Result<()> {
    validate_import_paths(import)?;
    let mut connection = open_database(&import.database)?;
    validate_schema(&connection)?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .context("locking Hermes state for native import")?;
    if transaction
        .query_row(
            "SELECT 1 FROM sessions WHERE id = ?1 LIMIT 1",
            [&import.target.id],
            |_| Ok(()),
        )
        .is_ok()
    {
        bail!("generated Hermes target session already exists")
    }
    if let Some(parent) = &import.parent_session_id
        && transaction
            .query_row(
                "SELECT 1 FROM sessions WHERE id = ?1 LIMIT 1",
                [parent],
                |_| Ok(()),
            )
            .is_err()
    {
        bail!("Hermes fork parent `{parent}` no longer exists")
    }
    let resolved_title = next_store_title(&transaction, import.title.as_deref())?;
    import
        .resolved_title
        .set(resolved_title.clone())
        .map_err(|_| anyhow::anyhow!("Hermes import title was already resolved"))?;
    transaction.execute(
        "INSERT INTO sessions (
           id, source, model_config, parent_session_id, started_at, message_count,
           tool_call_count, cwd, git_branch, git_repo_root, title, archived
         ) VALUES (?1, 'cli', ?2, ?3, ?4, ?5, 0, ?6, ?7, ?8, ?9, 0)",
        params![
            import.target.id,
            import.model_config,
            import.parent_session_id,
            import.started_at,
            i64::try_from(import.expected_messages.len())?,
            import
                .cwd
                .to_str()
                .context("Hermes workspace path is not UTF-8")?,
            import.git_branch,
            import.git_repo_root,
            resolved_title,
        ],
    )?;
    for (index, message) in import.expected_messages.iter().enumerate() {
        let role = match message.role {
            HandoffRole::User => "user",
            HandoffRole::Assistant => "assistant",
        };
        transaction.execute(
            "INSERT INTO messages (
               session_id, role, content, timestamp, observed, active, compacted
             ) VALUES (?1, ?2, ?3, ?4, 1, 1, 0)",
            params![
                import.target.id,
                role,
                message.text,
                import.started_at + (f64::from(u32::try_from(index)?) + 1.0) / 1_000.0,
            ],
        )?;
    }
    transaction
        .commit()
        .context("committing Hermes native import")?;

    if let Err(error) = verify(import) {
        return Err(combine_rollback_error(
            error.context("Hermes import failed read-back verification"),
            rollback_store(import),
        ));
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn rollback_store(import: &HermesImport) -> Result<()> {
    validate_import_paths(import)?;
    verify_owned_rows(import).context("refusing to delete changed Hermes target session")?;
    let mut connection = open_database(&import.database)?;
    validate_schema(&connection)?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .context("locking Hermes state for rollback")?;
    let removed_messages = transaction.execute(
        "DELETE FROM messages WHERE session_id = ?1",
        [&import.target.id],
    )?;
    let removed_session =
        transaction.execute("DELETE FROM sessions WHERE id = ?1", [&import.target.id])?;
    if removed_session != 1 || removed_messages != import.expected_messages.len() {
        bail!("Hermes rollback did not match generated session")
    }
    transaction.commit().context("committing Hermes rollback")?;
    if HermesAdapter::with_root(&import.root)
        .read_session(&import.target)
        .is_ok()
    {
        bail!("Hermes rollback left generated session discoverable")
    }
    Ok(())
}

pub fn rollback(import: &HermesImport, binary: &Path) -> Result<()> {
    verify_owned(import).context("refusing to delete changed Hermes target session")?;
    let mut child = Command::new(binary)
        .args(["sessions", "delete", &import.target.id, "--yes"])
        .env("HERMES_HOME", &import.root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("executing `{}`", binary.display()))?;
    let status = child
        .wait_timeout(Duration::from_secs(30))
        .context("waiting for Hermes rollback")?;
    let Some(status) = status else {
        child.kill().context("stopping Hermes rollback")?;
        let _ = child.wait();
        bail!("Hermes rollback timed out")
    };
    let output = child
        .wait_with_output()
        .context("reading Hermes rollback")?;
    if !status.success() {
        bail!(
            "Hermes rollback exited with status {status}: {}",
            redact_secrets(&String::from_utf8_lossy(&output.stderr))
        )
    }
    if HermesAdapter::with_root(&import.root)
        .read_session(&import.target)
        .is_ok()
    {
        bail!("Hermes rollback left generated session discoverable")
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

fn verify(import: &HermesImport) -> Result<()> {
    let snapshot = verify_owned(import)?;
    let metadata = imported_metadata(&snapshot).context("Hermes import metadata was not found")?;
    if metadata
        .get("parent_session_id")
        .and_then(serde_json::Value::as_str)
        != import.parent_session_id.as_deref()
    {
        bail!("Hermes import did not preserve native parent")
    }
    if metadata
        .get("branched_from")
        .and_then(serde_json::Value::as_str)
        != import.parent_session_id.as_deref()
    {
        bail!("Hermes import did not preserve fork lineage marker")
    }
    Ok(())
}

fn verify_owned(import: &HermesImport) -> Result<CanonicalSnapshot> {
    let snapshot = HermesAdapter::with_root(&import.root)
        .read_session(&import.target)
        .context("reading generated Hermes target")?;
    if !readback_matches(&snapshot, &import.expected_messages) {
        bail!("Hermes imported history did not match generated trajectory")
    }
    if snapshot.title.as_deref() != effective_title(import) {
        bail!("Hermes imported title did not match generated target")
    }
    let expected_source = serde_json::from_str::<serde_json::Value>(&import.model_config)
        .ok()
        .and_then(|config| {
            config
                .get("omnisession_source")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .context("Hermes import source marker was not generated")?;
    let metadata = imported_metadata(&snapshot).context("Hermes import metadata was not found")?;
    if metadata
        .get("omnisession_source")
        .and_then(serde_json::Value::as_str)
        != Some(expected_source.as_str())
    {
        bail!("Hermes import source marker did not match generated target")
    }
    Ok(snapshot)
}

fn imported_metadata(snapshot: &CanonicalSnapshot) -> Option<&serde_json::Value> {
    snapshot
        .events
        .iter()
        .find(|event| {
            event.kind == omnis_ir::EventKind::ProviderEvent
                && event.source.raw_record_type.as_deref() == Some("omnisession.session_metadata")
        })
        .map(|event| &event.payload)
}

#[cfg(test)]
fn verify_owned_rows(import: &HermesImport) -> Result<()> {
    let connection = open_database(&import.database)?;
    validate_schema(&connection)?;
    let session = connection
        .query_row(
            "SELECT model_config, parent_session_id, started_at, message_count, cwd,
                    git_branch, git_repo_root, title, archived
             FROM sessions WHERE id = ?1",
            [&import.target.id],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, f64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, Option<String>>(7)?,
                    row.get::<_, i64>(8)?,
                ))
            },
        )
        .context("generated Hermes target session was not found")?;
    let expected_cwd = import
        .cwd
        .to_str()
        .context("Hermes workspace path is not UTF-8")?;
    if session.0.as_deref() != Some(import.model_config.as_str())
        || session.1 != import.parent_session_id
        || session.2.to_bits() != import.started_at.to_bits()
        || session.3 != i64::try_from(import.expected_messages.len())?
        || session.4.as_deref() != Some(expected_cwd)
        || session.5 != import.git_branch
        || session.6 != import.git_repo_root
        || session.7.as_deref() != effective_title(import)
        || session.8 != 0
    {
        bail!("generated Hermes target metadata changed after import")
    }

    let mut statement = connection.prepare(
        "SELECT role, content, timestamp, observed, active, compacted
         FROM messages WHERE session_id = ?1 ORDER BY id",
    )?;
    let rows = statement
        .query_map([&import.target.id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, f64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if rows.len() != import.expected_messages.len() {
        bail!("generated Hermes target message count changed after import")
    }
    for (index, (row, expected)) in rows.iter().zip(&import.expected_messages).enumerate() {
        let role = match expected.role {
            HandoffRole::User => "user",
            HandoffRole::Assistant => "assistant",
        };
        let timestamp = import.started_at + (f64::from(u32::try_from(index)?) + 1.0) / 1_000.0;
        if row.0 != role
            || row.1.as_deref() != Some(expected.text.as_str())
            || row.2.to_bits() != timestamp.to_bits()
            || (row.3, row.4, row.5) != (1, 1, 0)
        {
            bail!("generated Hermes target message changed after import")
        }
    }
    Ok(())
}

#[cfg(test)]
fn validate_import_paths(import: &HermesImport) -> Result<()> {
    let root = validate_import_root(import)?;
    let database_metadata =
        fs::symlink_metadata(&import.database).context("Hermes state database was not found")?;
    if !database_metadata.is_file() || database_metadata.file_type().is_symlink() {
        bail!("Hermes state database is not a safe regular file")
    }
    let database = import
        .database
        .canonicalize()
        .context("canonicalizing Hermes state database")?;
    if !database.starts_with(&root) || database.parent() != Some(root.as_path()) {
        bail!("Hermes state database is outside its allowed root")
    }
    Ok(())
}

fn validate_import_root(import: &HermesImport) -> Result<PathBuf> {
    let root_metadata = fs::symlink_metadata(&import.root).context("reading Hermes data root")?;
    if !root_metadata.is_dir() || root_metadata.file_type().is_symlink() {
        bail!("Hermes data root is not a safe directory")
    }
    let root = import
        .root
        .canonicalize()
        .context("canonicalizing Hermes data root")?;
    Ok(root)
}

#[cfg(test)]
fn open_database(path: &Path) -> Result<Connection> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .context("opening Hermes state database")?;
    connection
        .busy_timeout(Duration::from_secs(5))
        .context("configuring Hermes state lock timeout")?;
    connection
        .pragma_update(None, "foreign_keys", "ON")
        .context("enabling Hermes foreign-key checks")?;
    Ok(connection)
}

#[cfg(test)]
fn validate_schema(connection: &Connection) -> Result<()> {
    let version = connection
        .query_row("SELECT version FROM schema_version LIMIT 1", [], |row| {
            row.get::<_, i64>(0)
        })
        .context("reading Hermes schema version")?;
    if version < MINIMUM_SCHEMA_VERSION {
        bail!(
            "Hermes schema {version} is too old for native import; supported schemas: >= {MINIMUM_SCHEMA_VERSION}"
        )
    }
    for (table, required) in [
        (
            "sessions",
            &[
                "id",
                "source",
                "model_config",
                "parent_session_id",
                "started_at",
                "message_count",
                "tool_call_count",
                "cwd",
                "git_branch",
                "git_repo_root",
                "title",
                "archived",
            ][..],
        ),
        (
            "messages",
            &[
                "id",
                "session_id",
                "role",
                "content",
                "timestamp",
                "observed",
                "active",
                "compacted",
            ][..],
        ),
    ] {
        let mut statement = connection.prepare(&format!("PRAGMA table_info({table})"))?;
        let columns = statement
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<rusqlite::Result<HashSet<_>>>()?;
        if let Some(column) = required.iter().find(|column| !columns.contains(**column)) {
            bail!("Hermes database is missing required `{table}.{column}` column")
        }
    }
    Ok(())
}

#[cfg(test)]
fn next_store_title(
    transaction: &rusqlite::Transaction<'_>,
    base: Option<&str>,
) -> Result<Option<String>> {
    let Some(base) = base else {
        return Ok(None);
    };
    let escaped = base
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    let mut statement = transaction.prepare(
        "SELECT title FROM sessions
         WHERE title = ?1 OR title LIKE ?2 ESCAPE '\\'",
    )?;
    let titles = statement
        .query_map(params![base, format!("{escaped} #%")], |row| {
            row.get::<_, String>(0)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if titles.is_empty() {
        return Ok(Some(base.to_owned()));
    }
    let prefix = format!("{base} #");
    let number = titles
        .iter()
        .filter_map(|title| title.strip_prefix(&prefix))
        .filter_map(|number| number.parse::<u64>().ok())
        .max()
        .unwrap_or(1)
        .saturating_add(1);
    Ok(Some(format!("{base} #{number}")))
}

fn hermes_root() -> Result<PathBuf> {
    if let Some(root) = env::var_os("HERMES_HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(root));
    }
    Ok(BaseDirs::new()
        .context("resolving home directory")?
        .home_dir()
        .join(".hermes"))
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
        .context("waiting for Hermes version")?;
    let Some(status) = status else {
        child.kill().context("stopping Hermes version probe")?;
        let _ = child.wait();
        bail!("Hermes version probe timed out");
    };
    let output = child.wait_with_output().context("reading Hermes version")?;
    if !status.success() {
        bail!("Hermes version probe exited with status {status}")
    }
    let mut text = String::from_utf8(output.stdout).context("Hermes version was not UTF-8")?;
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    parse_version(&text).context("Hermes returned an unrecognized version")
}

fn run_provider_import(binary: &Path, root: &Path, payload: &[u8]) -> Result<serde_json::Value> {
    const SCRIPT: &str = r#"import json, sqlite3, sys
from hermes_state import SessionDB
db = SessionDB()
try:
    payload = json.load(sys.stdin)
    record = payload[0]
    base_title = record.get("title")
    resolved_title = base_title
    for _ in range(16):
        if base_title:
            resolved_title = db.get_next_title_in_lineage(base_title)
            record["title"] = resolved_title
        try:
            result = db.import_sessions(payload)
            break
        except sqlite3.IntegrityError as error:
            if not base_title or "sessions.title" not in str(error):
                raise
    else:
        raise RuntimeError("could not allocate a unique Hermes session title")
    result["resolved_titles"] = {record["id"]: resolved_title}
finally:
    db.close()
json.dump(result, sys.stdout)
"#;
    let (program, interpreter_args) = python_interpreter(binary)?;
    let mut child = Command::new(&program)
        .args(interpreter_args)
        .arg("-c")
        .arg(SCRIPT)
        .env("HERMES_HOME", root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| {
            format!(
                "starting Hermes provider importer through `{}`",
                program.display()
            )
        })?;
    child
        .stdin
        .take()
        .context("opening Hermes provider importer input")?
        .write_all(payload)
        .context("writing Hermes provider import payload")?;
    let status = child
        .wait_timeout(Duration::from_secs(30))
        .context("waiting for Hermes provider importer")?;
    let Some(status) = status else {
        child.kill().context("stopping Hermes provider importer")?;
        let _ = child.wait();
        bail!("Hermes provider importer timed out")
    };
    let output = child
        .wait_with_output()
        .context("reading Hermes provider importer")?;
    if !status.success() {
        bail!(
            "Hermes provider importer exited with status {status}: {}",
            redact_secrets(&String::from_utf8_lossy(&output.stderr))
        )
    }
    serde_json::from_slice(&output.stdout).context("Hermes provider importer returned invalid JSON")
}

fn python_interpreter(binary: &Path) -> Result<(PathBuf, Vec<String>)> {
    let mut file = fs::File::open(binary)
        .with_context(|| format!("opening Hermes launcher `{}`", binary.display()))?;
    let mut bytes = vec![0; 4_096];
    let read = file.read(&mut bytes)?;
    let launcher = std::str::from_utf8(&bytes[..read]).context("Hermes launcher is not UTF-8")?;
    let first_line = launcher
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("#!"))
        .context("Hermes launcher has no shebang")?;
    let mut parts = first_line.split_whitespace();
    let program = PathBuf::from(
        parts
            .next()
            .context("Hermes launcher has an empty shebang")?,
    );
    let arguments = parts.map(str::to_owned).collect::<Vec<_>>();
    let program_name = program
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    let python_launcher = program_name.starts_with("python")
        || (program_name == "env"
            && arguments
                .iter()
                .any(|argument| argument.starts_with("python")));
    if program.is_absolute() && python_launcher {
        return Ok((program, arguments));
    }
    if matches!(program_name, "bash" | "sh" | "env")
        && let Some(program) = official_wrapper_python(launcher)
    {
        return Ok((program, Vec::new()));
    }
    bail!("Hermes launcher does not expose its Python runtime")
}

fn official_wrapper_python(launcher: &str) -> Option<PathBuf> {
    let arguments = launcher.lines().find_map(|line| {
        let line = line.trim();
        line.strip_prefix("exec ").and_then(quoted_arguments)
    })?;
    if arguments.len() != 3 || arguments[2] != "$@" {
        return None;
    }
    let python = PathBuf::from(&arguments[0]);
    let script = PathBuf::from(&arguments[1]);
    let root = script.parent()?;
    let python_name = python.file_name()?.to_str()?;
    if !python.is_absolute()
        || !script.is_absolute()
        || script.file_name()?.to_str()? != "hermes"
        || !python_name.starts_with("python")
        || !python.starts_with(root.join("venv"))
        || !python.is_file()
    {
        return None;
    }
    Some(python)
}

fn quoted_arguments(mut input: &str) -> Option<Vec<String>> {
    let mut arguments = Vec::new();
    while !input.trim().is_empty() {
        input = input.trim_start();
        let value = input.strip_prefix('"')?;
        let end = value.find('"')?;
        arguments.push(value[..end].to_owned());
        input = &value[end + 1..];
    }
    Some(arguments)
}

fn parse_version(output: &str) -> Option<String> {
    output
        .split_whitespace()
        .map(|part| {
            part.trim_matches(|character: char| !character.is_ascii_digit() && character != '.')
        })
        .find(|part| {
            let mut components = part.split('.');
            components.clone().count() == 3
                && components.all(|component| component.parse::<u64>().is_ok())
        })
        .map(str::to_owned)
}

fn is_supported_version(version: &str) -> bool {
    crate::version_gate::is_at_least(version, MINIMUM_HERMES_VERSION)
}

fn combine_rollback_error(error: anyhow::Error, rollback: Result<()>) -> anyhow::Error {
    match rollback {
        Ok(()) => error,
        Err(rollback_error) => error.context(format!(
            "Hermes import failed and rollback also failed: {}",
            redact_secrets(&rollback_error.to_string())
        )),
    }
}

#[cfg(test)]
pub(crate) fn create_fixture_store(root: &Path) -> Result<()> {
    fs::create_dir_all(root).context("creating Hermes fixture root")?;
    let connection = Connection::open(root.join("state.db"))?;
    connection.execute_batch(
        "PRAGMA foreign_keys = ON;
         CREATE TABLE schema_version (version INTEGER NOT NULL);
         INSERT INTO schema_version VALUES (23);
         CREATE TABLE sessions (
           id TEXT PRIMARY KEY, source TEXT NOT NULL, model_config TEXT,
           parent_session_id TEXT REFERENCES sessions(id), started_at REAL NOT NULL,
           ended_at REAL, message_count INTEGER DEFAULT 0, tool_call_count INTEGER DEFAULT 0,
           cwd TEXT, git_branch TEXT, git_repo_root TEXT, title TEXT, model TEXT,
           input_tokens INTEGER DEFAULT 0, output_tokens INTEGER DEFAULT 0,
           cache_read_tokens INTEGER DEFAULT 0, cache_write_tokens INTEGER DEFAULT 0,
           reasoning_tokens INTEGER DEFAULT 0, archived INTEGER DEFAULT 0
         );
         CREATE TABLE messages (
           id INTEGER PRIMARY KEY AUTOINCREMENT,
           session_id TEXT NOT NULL REFERENCES sessions(id), role TEXT NOT NULL,
           content TEXT, tool_call_id TEXT, tool_calls TEXT, tool_name TEXT,
           effect_disposition TEXT, timestamp REAL NOT NULL, finish_reason TEXT,
           observed INTEGER DEFAULT 0, active INTEGER DEFAULT 1,
           compacted INTEGER DEFAULT 0
         );
         CREATE UNIQUE INDEX idx_sessions_title_unique
           ON sessions(title) WHERE title IS NOT NULL;",
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use omnis_ir::{
        EventKind, EventSource, GitState, OmniEvent, ReplayPolicy, SCHEMA_VERSION, Sensitivity,
        WorkspaceSnapshot,
    };

    fn fixture(root: &Path) {
        create_fixture_store(root).expect("Hermes fixture schema");
    }

    fn snapshot(workspace: &Path, provider: Provider, id: &str) -> CanonicalSnapshot {
        let thread_id = Uuid::new_v4();
        let event = |sequence, kind, text: &str| OmniEvent {
            schema_version: SCHEMA_VERSION.to_owned(),
            event_id: Uuid::new_v4(),
            thread_id,
            branch_id: thread_id,
            sequence,
            timestamp: None,
            source: EventSource {
                provider,
                native_session_id: id.to_owned(),
                provider_version: None,
                raw_record_type: None,
            },
            kind,
            payload: json!({"text": text}),
            raw_blob_hash: None,
            sensitivity: Sensitivity::Normal,
            replay_policy: ReplayPolicy::Contextual,
        };
        CanonicalSnapshot {
            schema_version: SCHEMA_VERSION.to_owned(),
            session: SessionRef::new(provider, id),
            thread_id,
            branch_id: thread_id,
            title: Some("Hermes import fixture".to_owned()),
            captured_at: Utc::now(),
            workspace: WorkspaceSnapshot {
                schema_version: SCHEMA_VERSION.to_owned(),
                captured_at: Utc::now(),
                root: workspace.to_path_buf(),
                current_dir: workspace.to_path_buf(),
                git: GitState {
                    branch: Some("main".to_owned()),
                    ..GitState::default()
                },
                instruction_files: Vec::new(),
                environment_names: Vec::new(),
                available_tools: Vec::new(),
            },
            events: vec![
                event(0, EventKind::MessageUser, "question"),
                event(1, EventKind::MessageAssistant, "answer"),
            ],
        }
    }

    #[test]
    fn native_import_round_trips_and_rolls_back() {
        let temporary = tempfile::tempdir().expect("Hermes import root");
        fixture(temporary.path());
        let workspace = temporary.path().join("workspace");
        fs::create_dir(&workspace).expect("workspace");
        let source = snapshot(&workspace, Provider::Claude, "source");
        let import = build_with_root(&source, &workspace, temporary.path().to_path_buf())
            .expect("Hermes import");
        materialize_store(&import).expect("materialize Hermes import");
        let readback = HermesAdapter::with_root(temporary.path())
            .read_session(&import.target)
            .expect("Hermes readback");
        assert!(readback_matches(&readback, &import.expected_messages));
        rollback_store(&import).expect("Hermes rollback");
        assert!(
            HermesAdapter::with_root(temporary.path())
                .read_session(&import.target)
                .is_err()
        );
    }

    #[test]
    fn repeated_source_titles_remain_importable() {
        let temporary = tempfile::tempdir().expect("Hermes import root");
        fixture(temporary.path());
        let workspace = temporary.path().join("workspace");
        fs::create_dir(&workspace).expect("workspace");
        let source = snapshot(&workspace, Provider::Claude, "source");
        let first = build_with_root(&source, &workspace, temporary.path().to_path_buf())
            .expect("first Hermes import");
        let second = build_with_root(&source, &workspace, temporary.path().to_path_buf())
            .expect("second Hermes import");

        materialize_store(&first).expect("materialize first Hermes title");
        materialize_store(&second).expect("materialize repeated Hermes title");
        assert_eq!(effective_title(&first), Some("Hermes import fixture"));
        assert_eq!(effective_title(&second), Some("Hermes import fixture #2"));
    }

    #[test]
    fn imported_title_is_bounded_and_terminal_safe() {
        let source = format!("  {}\nunsafe\u{1b}[31m  ", "x".repeat(160));
        let title = imported_title(Some(&source)).expect("Hermes title");

        assert_eq!(title.chars().count(), HERMES_TITLE_BASE_CHARACTER_LIMIT);
        assert!(!title.contains('\n'));
        assert!(!title.contains('\u{1b}'));
    }

    #[test]
    fn same_provider_import_records_native_parent() {
        let temporary = tempfile::tempdir().expect("Hermes fork root");
        fixture(temporary.path());
        let workspace = temporary.path().join("workspace");
        fs::create_dir(&workspace).expect("workspace");
        let connection =
            Connection::open(temporary.path().join("state.db")).expect("fixture database");
        connection.execute(
            "INSERT INTO sessions (id, source, started_at, cwd) VALUES ('parent', 'cli', 1, ?1)",
            [workspace.to_str().expect("workspace path")],
        ).expect("Hermes parent");
        let source = snapshot(&workspace, Provider::Hermes, "parent");
        let import = build_with_root(&source, &workspace, temporary.path().to_path_buf())
            .expect("Hermes fork import");
        materialize_store(&import).expect("materialize Hermes fork");
        let parent: String = connection
            .query_row(
                "SELECT parent_session_id FROM sessions WHERE id = ?1",
                [&import.target.id],
                |row| row.get(0),
            )
            .expect("Hermes parent readback");
        assert_eq!(parent, "parent");
    }

    #[test]
    fn version_gate_accepts_newer_hermes_releases() {
        assert_eq!(
            parse_version("Hermes Agent v0.19.1"),
            Some("0.19.1".to_owned())
        );
        assert!(!is_supported_version("0.19.0"));
        assert!(is_supported_version("0.19.1"));
        assert!(is_supported_version("0.20.0"));
        assert!(is_supported_version("1.0.0"));
    }

    #[cfg(unix)]
    #[test]
    fn version_probe_uses_fast_version_flag() {
        use std::os::unix::fs::PermissionsExt;

        let temporary = tempfile::tempdir().expect("Hermes version probe root");
        let binary = temporary.path().join("hermes");
        fs::write(
            &binary,
            "#!/usr/bin/env sh\ntest \"$1\" = \"--version\" || exit 64\nprintf '%s\\n' 'Hermes Agent v0.20.0 (2026.8.3)'\n",
        )
        .expect("Hermes version probe");
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755))
            .expect("Hermes version probe permissions");

        assert_eq!(
            installed_version(&binary).expect("Hermes version"),
            "0.20.0"
        );
    }

    #[test]
    fn official_install_wrapper_exposes_python_runtime() {
        let temporary = tempfile::tempdir().expect("Hermes launcher root");
        let install = temporary.path().join("hermes-agent");
        let runtime = install.join("venv/bin/python3");
        let script = install.join("hermes");
        let launcher = temporary.path().join("hermes");
        fs::create_dir_all(runtime.parent().expect("runtime parent")).expect("runtime directory");
        fs::write(&runtime, []).expect("runtime placeholder");
        fs::write(&script, "#!/usr/bin/env python3\n").expect("Hermes script");
        fs::write(
            &launcher,
            format!(
                "#!/usr/bin/env bash\nunset PYTHONPATH\nunset PYTHONHOME\nexec \"{}\" \"{}\" \"$@\"\n",
                runtime.display(),
                script.display()
            ),
        )
        .expect("Hermes wrapper");

        let (program, arguments) = python_interpreter(&launcher).expect("wrapper runtime");
        assert_eq!(program, runtime);
        assert!(arguments.is_empty());
    }
}
