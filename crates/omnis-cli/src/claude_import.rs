use std::{
    env, fs,
    io::{BufRead, BufReader, Write},
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
use unicode_normalization::UnicodeNormalization;
use uuid::Uuid;
use wait_timeout::ChildExt;

const SUPPORTED_CLAUDE_VERSION: &str = "2.1.220";
const BASE36_DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";

pub struct ClaudeImport {
    pub target: SessionRef,
    pub expected_messages: Vec<HandoffMessage>,
    pub history_items: usize,
    pub tool_events: usize,
    pub truncated: bool,
    records: Vec<Value>,
    target_path: PathBuf,
    projects_root: PathBuf,
}

pub fn build(snapshot: &CanonicalSnapshot, cwd: &Path) -> Result<ClaudeImport> {
    build_with_root(snapshot, cwd, projects_root()?)
}

pub(crate) fn build_with_root(
    snapshot: &CanonicalSnapshot,
    cwd: &Path,
    projects_root: PathBuf,
) -> Result<ClaudeImport> {
    let trajectory = import_trajectory(snapshot);
    if trajectory.items.is_empty() {
        bail!("source has no visible trajectory eligible for Claude import");
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
    let target = SessionRef::new(Provider::Claude, &id);
    let cwd_text = cwd
        .to_str()
        .context("Claude native import requires a UTF-8 workspace path")?;
    let project_key = project_key(cwd_text);
    let target_path = projects_root.join(project_key).join(format!("{id}.jsonl"));
    let timestamp = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
    let git_branch = snapshot.workspace.git.branch.clone();
    let mut parent_uuid: Option<String> = None;
    let records = expected_messages
        .iter()
        .map(|message| {
            let uuid = Uuid::new_v4().to_string();
            let record = match message.role {
                HandoffRole::User => json!({
                    "parentUuid": parent_uuid,
                    "isSidechain": false,
                    "type": "user",
                    "message": { "role": "user", "content": message.text },
                    "uuid": uuid,
                    "timestamp": timestamp,
                    "userType": "external",
                    "cwd": cwd_text,
                    "sessionId": id,
                    "version": SUPPORTED_CLAUDE_VERSION,
                    "gitBranch": git_branch
                }),
                HandoffRole::Assistant => json!({
                    "parentUuid": parent_uuid,
                    "isSidechain": false,
                    "type": "assistant",
                    "message": {
                        "id": Uuid::new_v4().to_string(),
                        "type": "message",
                        "role": "assistant",
                        "model": "<synthetic>",
                        "content": [{ "type": "text", "text": message.text }],
                        "stop_reason": "stop_sequence",
                        "stop_sequence": null,
                        "stop_details": null,
                        "usage": {
                            "input_tokens": 0,
                            "cache_creation_input_tokens": 0,
                            "cache_read_input_tokens": 0,
                            "output_tokens": 0
                        }
                    },
                    "type": "assistant",
                    "uuid": uuid,
                    "timestamp": timestamp,
                    "userType": "external",
                    "cwd": cwd_text,
                    "sessionId": id,
                    "version": SUPPORTED_CLAUDE_VERSION,
                    "gitBranch": git_branch,
                    "isApiErrorMessage": false
                }),
            };
            parent_uuid = record
                .get("uuid")
                .and_then(Value::as_str)
                .map(str::to_owned);
            record
        })
        .collect();

    Ok(ClaudeImport {
        target,
        expected_messages,
        history_items,
        tool_events: trajectory.tool_events,
        truncated: trajectory.truncated,
        records,
        target_path,
        projects_root,
    })
}

pub fn ensure_supported(binary: &Path) -> Result<String> {
    let version = installed_version(binary)?;
    if version != SUPPORTED_CLAUDE_VERSION {
        bail!(
            "Claude {version} is not verified for native trajectory import; supported version: {SUPPORTED_CLAUDE_VERSION}"
        );
    }
    Ok(version)
}

pub fn materialize(import: &ClaudeImport, binary: &Path) -> Result<()> {
    ensure_supported(binary)?;
    materialize_records(import)
}

pub(crate) fn materialize_records(import: &ClaudeImport) -> Result<()> {
    let project_dir = import
        .target_path
        .parent()
        .context("Claude target session path has no project directory")?;
    ensure_directory(&import.projects_root)?;
    ensure_directory(project_dir)?;
    validate_directory_chain(project_dir, "writing")?;
    let cwd = import.records[0]["cwd"]
        .as_str()
        .context("generated Claude transcript omitted workspace")?;
    verify_project_identity(project_dir, cwd)?;
    if import.target_path.exists() {
        bail!("generated Claude target session already exists");
    }

    let mut temporary = NamedTempFile::new_in(project_dir)
        .context("creating temporary Claude session transcript")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))
            .context("setting Claude transcript permissions")?;
    }
    for record in &import.records {
        serde_json::to_writer(&mut temporary, record).context("serializing Claude transcript")?;
        temporary
            .write_all(b"\n")
            .context("terminating Claude transcript record")?;
    }
    temporary.flush().context("flushing Claude transcript")?;
    temporary
        .as_file()
        .sync_all()
        .context("syncing Claude transcript")?;
    temporary
        .persist_noclobber(&import.target_path)
        .map_err(|error| error.error)
        .context("publishing generated Claude transcript")?;
    if let Err(error) =
        sync_directory(project_dir).context("syncing Claude project session directory")
    {
        return match rollback(import) {
            Ok(()) => Err(error),
            Err(rollback_error) => Err(error).context(format!(
                "Claude directory sync failed and rollback also failed: {rollback_error}"
            )),
        };
    }
    Ok(())
}

pub fn rollback(import: &ClaudeImport) -> Result<()> {
    validate_generated_file(import)?;
    fs::remove_file(&import.target_path).context("removing generated Claude target session")?;
    if let Some(parent) = import.target_path.parent() {
        sync_directory(parent).context("syncing Claude project directory after rollback")?;
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

fn projects_root() -> Result<PathBuf> {
    if let Some(root) = env::var_os("CLAUDE_CONFIG_DIR").filter(|value| !value.is_empty()) {
        let root = PathBuf::from(root);
        if !root.is_absolute() {
            bail!("CLAUDE_CONFIG_DIR must be an absolute path for native import");
        }
        return Ok(root.join("projects"));
    }
    BaseDirs::new()
        .map(|directories| directories.home_dir().join(".claude").join("projects"))
        .context("home directory is unavailable")
}

fn project_key(cwd: &str) -> String {
    let normalized = cwd.nfc().collect::<String>();
    let units = normalized.encode_utf16().collect::<Vec<_>>();
    let mut key = units
        .iter()
        .map(|unit| {
            if u8::try_from(*unit).is_ok_and(|byte| byte.is_ascii_alphanumeric()) {
                char::from_u32(u32::from(*unit)).unwrap_or('-')
            } else {
                '-'
            }
        })
        .collect::<String>();
    if key.len() > 200 {
        let hash = units.iter().fold(0i32, |hash, unit| {
            hash.wrapping_mul(31).wrapping_add(i32::from(*unit))
        });
        key.truncate(200);
        key.push('-');
        key.push_str(&base36(i64::from(hash).unsigned_abs()));
    }
    key
}

fn base36(mut value: u64) -> String {
    if value == 0 {
        return "0".to_owned();
    }
    let mut encoded = Vec::new();
    while value > 0 {
        encoded.push(char::from(BASE36_DIGITS[(value % 36) as usize]));
        value /= 36;
    }
    encoded.into_iter().rev().collect()
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
    let parent = path
        .parent()
        .context("Claude target directory has no parent")?;
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
    builder
        .create(path)
        .with_context(|| format!("creating `{}`", path.display()))
}

fn verify_project_identity(project_dir: &Path, cwd: &str) -> Result<()> {
    for entry in fs::read_dir(project_dir).context("reading Claude project session directory")? {
        let entry = entry.context("reading Claude project session entry")?;
        let file_type = entry
            .file_type()
            .context("reading Claude session file type")?;
        if !file_type.is_file()
            || entry
                .path()
                .extension()
                .is_none_or(|extension| extension != "jsonl")
        {
            continue;
        }
        let file = fs::File::open(entry.path()).context("reading Claude session identity")?;
        let recorded_cwd = BufReader::new(file)
            .lines()
            .take(128)
            .find_map(|line| {
                serde_json::from_str::<Value>(&line.ok()?)
                    .ok()?
                    .get("cwd")?
                    .as_str()
                    .map(str::to_owned)
            })
            .with_context(|| {
                format!(
                    "cannot verify Claude project identity from `{}`",
                    entry.path().display()
                )
            })?;
        if recorded_cwd != cwd {
            bail!(
                "Claude project key collision: existing transcript records `{recorded_cwd}`, target is `{cwd}`"
            );
        }
    }
    Ok(())
}

fn validate_generated_file(import: &ClaudeImport) -> Result<()> {
    let project_dir = import
        .target_path
        .parent()
        .context("Claude target session path has no project directory")?;
    validate_directory_chain(project_dir, "rolling back")?;
    if import.target.provider != Provider::Claude
        || Uuid::parse_str(&import.target.id).is_err()
        || import
            .target_path
            .file_name()
            .and_then(|name| name.to_str())
            != Some(format!("{}.jsonl", import.target.id).as_str())
        || !import.target_path.starts_with(&import.projects_root)
    {
        bail!("refusing to remove unverified Claude target path");
    }
    let content = fs::read_to_string(&import.target_path)
        .context("reading generated Claude target session")?;
    if !content.ends_with('\n') {
        bail!("generated Claude target session lost its record terminator");
    }
    let mut records = 0usize;
    for (index, line) in content.lines().enumerate() {
        let record: Value = serde_json::from_str(line)
            .context("generated Claude target session failed identity validation")?;
        if record.get("sessionId").and_then(Value::as_str) != Some(import.target.id.as_str())
            || import.records.get(index) != Some(&record)
        {
            bail!("generated Claude target session contains a foreign record");
        }
        records += 1;
    }
    if records != import.records.len() {
        bail!("generated Claude target session changed after materialization");
    }
    Ok(())
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
        .context("waiting for Claude version")?;
    let Some(status) = status else {
        child.kill().context("stopping Claude version probe")?;
        let _ = child.wait();
        bail!("Claude version probe timed out");
    };
    let output = child.wait_with_output().context("reading Claude version")?;
    if !status.success() {
        bail!("Claude version probe exited with status {status}");
    }
    let stdout = String::from_utf8(output.stdout).context("Claude version was not UTF-8")?;
    parse_version(&stdout).context("Claude returned an unrecognized version")
}

fn parse_version(output: &str) -> Option<String> {
    output
        .split_whitespace()
        .find(|part| {
            let mut components = part.split('.');
            components.clone().count() == 3
                && components.all(|component| component.parse::<u64>().is_ok())
        })
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_key_matches_claude_directory_name() {
        assert_eq!(project_key("/home/dev/repo.name"), "-home-dev-repo-name");
    }

    #[test]
    fn project_key_normalizes_unicode_to_nfc() {
        assert_eq!(
            project_key("/tmp/cafe\u{301}"),
            project_key("/tmp/caf\u{e9}")
        );
    }

    #[test]
    fn project_identity_rejects_transcripts_without_workspace() {
        let directory = tempfile::tempdir().expect("temporary project directory");
        fs::write(
            directory.path().join("existing.jsonl"),
            r#"{"type":"summary","summary":"missing cwd"}"#,
        )
        .expect("fixture transcript");

        let error = verify_project_identity(directory.path(), "/workspace/demo")
            .expect_err("unknown project identity must fail closed");
        assert!(
            error
                .to_string()
                .contains("cannot verify Claude project identity")
        );
    }

    #[cfg(unix)]
    #[test]
    fn directory_creation_rejects_symlinked_ancestor() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().expect("temporary directory");
        let real = directory.path().join("real");
        fs::create_dir(&real).expect("real directory");
        let alias = directory.path().join("alias");
        symlink(&real, &alias).expect("directory symlink");

        let error = ensure_directory(&alias.join("projects"))
            .expect_err("symlinked ancestor must fail closed");
        assert!(error.to_string().contains("not a safe directory"));
    }

    #[test]
    fn long_project_key_uses_claude_hash_suffix() {
        let path = format!("/{}", "workspace/".repeat(25));
        let key = project_key(&path);
        assert!(key.len() > 201);
        assert_eq!(key.chars().nth(200), Some('-'));
        assert!(
            key[201..]
                .chars()
                .all(|character| character.is_ascii_alphanumeric())
        );
    }

    #[test]
    fn version_parser_reads_installed_shape() {
        assert_eq!(
            parse_version("2.1.220 (Claude Code)"),
            Some("2.1.220".to_owned())
        );
    }
}
