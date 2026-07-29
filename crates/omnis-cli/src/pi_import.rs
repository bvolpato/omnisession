use std::{
    collections::HashSet,
    env, fs,
    io::{Read, Seek, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use chrono::{SecondsFormat, Utc};
use directories::BaseDirs;
use omnis_core::{HandoffMessage, HandoffRole, TrajectoryItemKind, import_trajectory};
use omnis_ir::{CanonicalSnapshot, Provider, SessionRef};
use serde_json::{Value, json};
use tempfile::NamedTempFile;
use uuid::Uuid;
use wait_timeout::ChildExt;

const PI_SESSION_VERSION: u64 = 3;
const SUPPORTED_PI_MAJOR: u64 = 0;
const SUPPORTED_PI_MINOR: u64 = 82;
const MAX_VERSION_OUTPUT: u64 = 8 * 1024;

/// Pi v3 JSONL session staged for one exclusive native write.
pub struct PiImport {
    pub target: SessionRef,
    pub expected_messages: Vec<HandoffMessage>,
    pub history_items: usize,
    pub tool_events: usize,
    pub truncated: bool,
    records: Vec<Value>,
    document: Vec<u8>,
    sessions_root: PathBuf,
    target_dir: PathBuf,
    target_path: PathBuf,
    cwd: String,
}

pub fn build(snapshot: &CanonicalSnapshot, cwd: &Path) -> Result<PiImport> {
    build_with_root(snapshot, cwd, sessions_root()?)
}

pub(crate) fn build_with_root(
    snapshot: &CanonicalSnapshot,
    cwd: &Path,
    sessions_root: PathBuf,
) -> Result<PiImport> {
    if !sessions_root.is_absolute() {
        bail!("Pi native import requires an absolute session root");
    }
    let trajectory = import_trajectory(snapshot);
    if trajectory.items.is_empty() {
        bail!("source has no visible trajectory eligible for Pi import");
    }
    if !cwd.is_absolute() {
        bail!("Pi native import requires an absolute workspace path");
    }
    let canonical_cwd = fs::canonicalize(cwd)
        .with_context(|| format!("canonicalizing Pi workspace `{}`", cwd.display()))?;
    if !canonical_cwd.is_dir() {
        bail!("Pi native import workspace is not a directory");
    }
    let cwd = canonical_cwd
        .to_str()
        .context("Pi native import requires a UTF-8 workspace path")?
        .to_owned();
    let history_items = trajectory.items.len();
    let expected_messages = trajectory_messages(trajectory.items);
    let target = SessionRef::new(Provider::Pi, Uuid::new_v4().to_string());
    let timestamp = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
    let target_dir = sessions_root.join(session_directory_name(&cwd));
    let filename = format!("{}_{}.jsonl", timestamp.replace([':', '.'], "-"), target.id);
    let target_path = target_dir.join(filename);
    let records = native_records(&target, &cwd, &timestamp, snapshot, &expected_messages)?;
    let document = serialize_records(&records)?;
    Ok(PiImport {
        target,
        expected_messages,
        history_items,
        tool_events: trajectory.tool_events,
        truncated: trajectory.truncated,
        records,
        document,
        sessions_root,
        target_dir,
        target_path,
        cwd,
    })
}

fn trajectory_messages(items: Vec<omnis_core::TrajectoryItem>) -> Vec<HandoffMessage> {
    items
        .into_iter()
        .map(|item| HandoffMessage {
            role: match item.kind {
                TrajectoryItemKind::User => HandoffRole::User,
                TrajectoryItemKind::Assistant | TrajectoryItemKind::Tool => HandoffRole::Assistant,
            },
            text: item.text,
        })
        .collect()
}

fn native_records(
    target: &SessionRef,
    cwd: &str,
    timestamp: &str,
    snapshot: &CanonicalSnapshot,
    messages: &[HandoffMessage],
) -> Result<Vec<Value>> {
    let mut records = Vec::with_capacity(messages.len() + 2);
    records.push(json!({
        "type": "session",
        "version": PI_SESSION_VERSION,
        "id": target.id,
        "timestamp": timestamp,
        "cwd": cwd,
    }));
    let mut ids = HashSet::new();
    let mut parent_id = None;
    let timestamp_ms = Utc::now().timestamp_millis();
    for (index, message) in messages.iter().enumerate() {
        let id = entry_id(&mut ids);
        let entry_timestamp = timestamp_ms.saturating_add(i64::try_from(index)?);
        let native_message = match message.role {
            HandoffRole::User => json!({
                "role": "user",
                "content": message.text,
                "timestamp": entry_timestamp,
            }),
            HandoffRole::Assistant => json!({
                "role": "assistant",
                "content": [{ "type": "text", "text": message.text }],
                "api": "omnisession",
                "provider": "omnisession",
                "model": "historical",
                "usage": zero_usage(),
                "stopReason": "stop",
                "timestamp": entry_timestamp,
            }),
        };
        records.push(json!({
            "type": "message",
            "id": id,
            "parentId": parent_id,
            "timestamp": timestamp,
            "message": native_message,
        }));
        parent_id = records
            .last()
            .and_then(|record| record.get("id"))
            .and_then(Value::as_str)
            .map(str::to_owned);
    }
    let title = format!("Imported from {}", snapshot.session);
    records.push(json!({
        "type": "session_info",
        "id": entry_id(&mut ids),
        "parentId": parent_id,
        "timestamp": timestamp,
        "name": title,
    }));
    Ok(records)
}

fn zero_usage() -> Value {
    json!({
        "input": 0,
        "output": 0,
        "cacheRead": 0,
        "cacheWrite": 0,
        "totalTokens": 0,
        "cost": {
            "input": 0,
            "output": 0,
            "cacheRead": 0,
            "cacheWrite": 0,
            "total": 0,
        },
    })
}

fn entry_id(ids: &mut HashSet<String>) -> String {
    loop {
        let id = Uuid::new_v4().simple().to_string()[..8].to_owned();
        if ids.insert(id.clone()) {
            return id;
        }
    }
}

fn serialize_records(records: &[Value]) -> Result<Vec<u8>> {
    let mut document = Vec::new();
    for record in records {
        serde_json::to_writer(&mut document, record).context("serializing Pi session record")?;
        document.push(b'\n');
    }
    Ok(document)
}

/// Validates Pi CLI version against Pi's currently documented v3 session format.
///
/// Pi's 0.x minor releases may make incompatible changes. This writer accepts
/// patch releases from the source-compatible 0.82 line only.
pub fn ensure_supported(binary: &Path) -> Result<String> {
    let version = installed_version(binary)?;
    let (major, minor, _) =
        parse_version(&version).context("Pi returned an unrecognized version")?;
    if (major, minor) != (SUPPORTED_PI_MAJOR, SUPPORTED_PI_MINOR) {
        bail!(
            "Pi {version} is not verified for native v{PI_SESSION_VERSION} import; supported release line: {SUPPORTED_PI_MAJOR}.{SUPPORTED_PI_MINOR}.x"
        );
    }
    Ok(version)
}

pub fn materialize(import: &PiImport, binary: &Path) -> Result<()> {
    ensure_supported(binary)?;
    materialize_records(import)
}

/// Writes exactly one new Pi v3 session file with exclusive publication.
pub(crate) fn materialize_records(import: &PiImport) -> Result<()> {
    ensure_directory(&import.sessions_root)?;
    ensure_directory(&import.target_dir)?;
    validate_directory_chain(&import.target_dir, "writing")?;
    verify_session_directory_identity(&import.target_dir, &import.cwd)?;
    if import.target_path.exists() {
        bail!("generated Pi target session already exists");
    }

    let mut temporary = NamedTempFile::new_in(&import.target_dir)
        .context("creating temporary Pi session transcript")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))
            .context("setting Pi transcript permissions")?;
    }
    temporary
        .write_all(&import.document)
        .context("writing Pi session transcript")?;
    temporary
        .flush()
        .context("flushing Pi session transcript")?;
    temporary
        .as_file()
        .sync_all()
        .context("syncing Pi session transcript")?;
    temporary
        .persist_noclobber(&import.target_path)
        .map_err(|error| error.error)
        .context("publishing generated Pi session transcript")?;
    if let Err(error) = sync_directory(&import.target_dir).context("syncing Pi session directory") {
        return rollback_after_publish(import, error);
    }
    validate_generated_file(import)
}

/// Removes only byte-for-byte generated Pi session file.
pub fn rollback(import: &PiImport) -> Result<()> {
    validate_generated_file(import)?;
    fs::remove_file(&import.target_path).context("removing generated Pi target session")?;
    sync_directory(&import.target_dir).context("syncing Pi session directory after rollback")
}

fn rollback_after_publish(import: &PiImport, error: anyhow::Error) -> Result<()> {
    match rollback(import) {
        Ok(()) => Err(error),
        Err(rollback_error) => Err(error).context(format!(
            "Pi session publish failed and exact rollback also failed: {rollback_error}"
        )),
    }
}

pub fn readback_matches(snapshot: &CanonicalSnapshot, expected: &[HandoffMessage]) -> bool {
    let trajectory = import_trajectory(snapshot);
    let actual = trajectory_messages(trajectory.items);
    !trajectory.truncated && actual == expected
}

fn sessions_root() -> Result<PathBuf> {
    if let Some(root) = env::var_os("PI_CODING_AGENT_SESSION_DIR").filter(|value| !value.is_empty())
    {
        let root = PathBuf::from(root);
        if !root.is_absolute() {
            bail!("PI_CODING_AGENT_SESSION_DIR must be an absolute path for native import");
        }
        return Ok(root);
    }
    if let Some(agent_root) = env::var_os("PI_CODING_AGENT_DIR").filter(|value| !value.is_empty()) {
        let agent_root = PathBuf::from(agent_root);
        if !agent_root.is_absolute() {
            bail!("PI_CODING_AGENT_DIR must be an absolute path for native import");
        }
        return Ok(agent_root.join("sessions"));
    }
    BaseDirs::new()
        .map(|directories| {
            directories
                .home_dir()
                .join(".pi")
                .join("agent")
                .join("sessions")
        })
        .context("home directory is unavailable")
}

fn session_directory_name(cwd: &str) -> String {
    let path = cwd.trim_start_matches(['/', '\\']);
    format!("--{}--", path.replace(['/', '\\', ':'], "-"))
}

fn ensure_directory(path: &Path) -> Result<()> {
    if path.exists() {
        let metadata =
            fs::symlink_metadata(path).with_context(|| format!("reading `{}`", path.display()))?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            bail!("`{}` is not a safe directory", path.display());
        }
        return Ok(());
    }
    let parent = path.parent().context("Pi target directory has no parent")?;
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
        Err(error) => Err(error).with_context(|| format!("creating `{}`", path.display())),
    }
}

fn verify_session_directory_identity(target_dir: &Path, cwd: &str) -> Result<()> {
    for entry in fs::read_dir(target_dir).context("reading Pi session directory")? {
        let entry = entry.context("reading Pi session entry")?;
        let file_type = entry.file_type().context("reading Pi session entry type")?;
        if !file_type.is_file()
            || entry
                .path()
                .extension()
                .is_none_or(|extension| extension != "jsonl")
        {
            continue;
        }
        let recorded_cwd = read_session_cwd(&entry.path())?;
        if recorded_cwd != cwd {
            bail!(
                "Pi session directory collision: existing session records `{recorded_cwd}`, target is `{cwd}`"
            );
        }
    }
    Ok(())
}

fn read_session_cwd(path: &Path) -> Result<String> {
    let file = fs::File::open(path).context("reading Pi session identity")?;
    let mut reader = std::io::BufReader::new(file.take(1024 * 1024));
    let mut bytes = Vec::new();
    std::io::BufRead::read_until(&mut reader, b'\n', &mut bytes)?;
    let record: Value = serde_json::from_slice(&bytes)
        .context("cannot verify Pi session identity from malformed header")?;
    if record.get("type").and_then(Value::as_str) != Some("session")
        || record.get("version").and_then(Value::as_u64) != Some(PI_SESSION_VERSION)
    {
        bail!("cannot verify Pi session identity from unsupported header");
    }
    record
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|cwd| !cwd.is_empty())
        .map(str::to_owned)
        .context("cannot verify Pi session identity without working directory")
}

fn validate_generated_file(import: &PiImport) -> Result<()> {
    validate_directory_chain(&import.target_dir, "rolling back")?;
    if import.target.provider != Provider::Pi
        || Uuid::parse_str(&import.target.id).is_err()
        || !import.target_path.starts_with(&import.sessions_root)
        || import.target_path.parent() != Some(import.target_dir.as_path())
        || !import
            .target_path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with(&format!("_{}.jsonl", import.target.id)))
    {
        bail!("refusing to remove unverified Pi target path");
    }
    let metadata = fs::symlink_metadata(&import.target_path)
        .context("reading generated Pi target session metadata")?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        bail!("generated Pi target session is not a regular file");
    }
    let content = fs::read(&import.target_path).context("reading generated Pi target session")?;
    if content != import.document {
        bail!("generated Pi target session changed after materialization");
    }
    let records = parse_document(&content)?;
    if records != import.records
        || records
            .first()
            .and_then(|record| record.get("id"))
            .and_then(Value::as_str)
            != Some(import.target.id.as_str())
        || records
            .first()
            .and_then(|record| record.get("version"))
            .and_then(Value::as_u64)
            != Some(PI_SESSION_VERSION)
    {
        bail!("generated Pi target session failed exact identity validation");
    }
    Ok(())
}

fn parse_document(document: &[u8]) -> Result<Vec<Value>> {
    if !document.ends_with(b"\n") {
        bail!("generated Pi target session lost its record terminator");
    }
    document
        .split_inclusive(|byte| *byte == b'\n')
        .map(|line| {
            let line = line.strip_suffix(b"\n").unwrap_or(line);
            serde_json::from_slice(line).context("generated Pi target session has invalid JSONL")
        })
        .collect()
}

fn validate_directory_chain(path: &Path, operation: &str) -> Result<()> {
    for directory in path.ancestors() {
        if directory.as_os_str().is_empty() {
            break;
        }
        let metadata = fs::symlink_metadata(directory)
            .with_context(|| format!("reading `{}`", directory.display()))?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            bail!(
                "refusing {operation} through unsafe directory `{}`",
                directory.display()
            );
        }
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

fn installed_version(binary: &Path) -> Result<String> {
    let mut output = NamedTempFile::new().context("creating Pi version output buffer")?;
    let mut child = Command::new(binary)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::from(output.reopen()?))
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("executing `{}`", binary.display()))?;
    let Some(status) = child
        .wait_timeout(Duration::from_secs(5))
        .context("waiting for Pi version")?
    else {
        child.kill().context("stopping Pi version probe")?;
        let _ = child.wait();
        bail!("Pi version probe timed out");
    };
    if !status.success() {
        bail!("Pi version probe exited with status {status}");
    }
    if output.as_file().metadata()?.len() > MAX_VERSION_OUTPUT {
        bail!("Pi version output exceeds safe limit");
    }
    output.as_file_mut().rewind()?;
    let mut version = String::new();
    output
        .as_file_mut()
        .take(MAX_VERSION_OUTPUT + 1)
        .read_to_string(&mut version)?;
    Ok(version)
}

fn parse_version(output: &str) -> Option<(u64, u64, u64)> {
    output
        .split(|character: char| !character.is_ascii_digit() && character != '.')
        .find_map(|candidate| {
            let mut components = candidate.split('.');
            let major = components.next()?.parse().ok()?;
            let minor = components.next()?.parse().ok()?;
            let patch = components.next()?.parse().ok()?;
            components.next().is_none().then_some((major, minor, patch))
        })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use chrono::Utc;
    use omnis_adapters::{PiAdapter, ProviderAdapter};
    use omnis_ir::{
        EventKind, EventSource, GitState, OmniEvent, ReplayPolicy, SCHEMA_VERSION, Sensitivity,
        WorkspaceSnapshot,
    };

    use super::*;

    fn snapshot() -> CanonicalSnapshot {
        let thread_id = Uuid::new_v4();
        let branch_id = Uuid::new_v4();
        let session = SessionRef::new(Provider::Claude, "synthetic-source");
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
            payload: json!({ "text": text }),
            raw_blob_hash: None,
            sensitivity: Sensitivity::Normal,
            replay_policy,
        };
        CanonicalSnapshot {
            schema_version: SCHEMA_VERSION.to_owned(),
            session,
            thread_id,
            branch_id,
            title: Some("Synthetic import".to_owned()),
            captured_at: Utc::now(),
            workspace: WorkspaceSnapshot {
                schema_version: SCHEMA_VERSION.to_owned(),
                captured_at: Utc::now(),
                root: PathBuf::from("/workspace"),
                current_dir: PathBuf::from("/workspace"),
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
                    EventKind::ToolCompleted,
                    "documentary tool",
                    ReplayPolicy::HistoricalOnly,
                ),
                event(
                    3,
                    EventKind::MessageAssistant,
                    "answer",
                    ReplayPolicy::Contextual,
                ),
            ],
        }
    }

    #[test]
    fn materializes_v3_jsonl_and_exactly_rolls_back() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let workspace = temporary.path().join("workspace");
        fs::create_dir(&workspace).expect("workspace");
        let import = build_with_root(&snapshot(), &workspace, temporary.path().join("sessions"))
            .expect("Pi import");
        materialize_records(&import).expect("materialize Pi import");
        let record = parse_document(&fs::read(&import.target_path).expect("Pi JSONL"))
            .expect("parse Pi JSONL");
        assert_eq!(record[0]["type"], "session");
        assert_eq!(record[0]["version"], PI_SESSION_VERSION);
        assert_eq!(record[0]["id"], import.target.id);
        assert!(record.iter().all(|entry| {
            entry.get("type").and_then(Value::as_str) != Some("message")
                || entry["message"]["role"] != "toolResult"
        }));
        let readback = PiAdapter::with_root(&import.sessions_root)
            .read_session(&import.target)
            .expect("Pi readback");
        assert!(readback_matches(&readback, &import.expected_messages));
        rollback(&import).expect("exact rollback");
        assert!(!import.target_path.exists());
    }

    #[test]
    fn rollback_refuses_modified_target() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let workspace = temporary.path().join("workspace");
        fs::create_dir(&workspace).expect("workspace");
        let import = build_with_root(&snapshot(), &workspace, temporary.path().join("sessions"))
            .expect("Pi import");
        materialize_records(&import).expect("materialize Pi import");
        fs::write(&import.target_path, b"changed\n").expect("tamper Pi target");
        assert!(rollback(&import).is_err());
        assert!(import.target_path.exists());
    }

    #[test]
    fn materialization_never_clobbers_existing_target() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let workspace = temporary.path().join("workspace");
        fs::create_dir(&workspace).expect("workspace");
        let import = build_with_root(&snapshot(), &workspace, temporary.path().join("sessions"))
            .expect("Pi import");
        fs::create_dir_all(&import.target_dir).expect("Pi target directory");
        let existing = format!(
            "{{\"type\":\"session\",\"version\":3,\"id\":\"existing\",\"cwd\":\"{}\"}}\n",
            import.cwd
        );
        fs::write(&import.target_path, &existing).expect("existing Pi session");
        assert!(materialize_records(&import).is_err());
        assert_eq!(
            fs::read_to_string(&import.target_path).expect("existing target"),
            existing
        );
    }

    #[test]
    fn version_parser_requires_full_semver() {
        assert_eq!(parse_version("pi 0.82.1"), Some((0, 82, 1)));
        assert_eq!(parse_version("Pi version v0.82.2"), Some((0, 82, 2)));
        assert_eq!(parse_version("0.82"), None);
    }
}
