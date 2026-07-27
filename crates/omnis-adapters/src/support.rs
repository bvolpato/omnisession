use std::{
    env, fs,
    fs::File,
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
};

use anyhow::{Result, anyhow};
use chrono::{DateTime, Utc};
use omnis_ir::{
    CanonicalSnapshot, EventKind, EventSource, GitState, OmniEvent, Provider, ReplayPolicy,
    SCHEMA_VERSION, Sensitivity, SessionRef, WorkspaceSnapshot,
};
use rusqlite::{Connection, OpenFlags};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use uuid::Uuid;

const MAX_PROVIDER_RECORDS: usize = 100_000;
const MAX_DISCOVERED_FILES: usize = 10_000;
const MAX_METADATA_FILE_SIZE: u64 = 4 * 1024 * 1024;
const MAX_SQLITE_SNAPSHOT_SIZE: u64 = 256 * 1024 * 1024;
pub(crate) const MAX_TRANSCRIPT_FILE_SIZE: u64 = 32 * 1024 * 1024;
pub(crate) const MAX_TRANSCRIPT_LINE_SIZE: u64 = 2 * 1024 * 1024;

pub(crate) fn provider_root(environment: &str, default_suffix: &[&str]) -> Option<PathBuf> {
    if let Some(value) = env::var_os(environment).filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(value));
    }
    let mut root = directories::BaseDirs::new()?.home_dir().to_path_buf();
    root.extend(default_suffix);
    Some(root)
}

pub(crate) fn executable(name: &str) -> Option<PathBuf> {
    let candidate = Path::new(name);
    if candidate.components().count() > 1 {
        return candidate.is_file().then(|| candidate.to_path_buf());
    }

    let paths = env::var_os("PATH")?;
    #[cfg(windows)]
    let extensions: Vec<String> = env::var_os("PATHEXT")
        .map(|value| {
            value
                .to_string_lossy()
                .split(';')
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_else(|| vec![".EXE".to_owned(), ".CMD".to_owned(), ".BAT".to_owned()]);

    for directory in env::split_paths(&paths) {
        let path = directory.join(name);
        if path.is_file() {
            return Some(path);
        }
        #[cfg(windows)]
        for extension in &extensions {
            let path = directory.join(format!("{name}{extension}"));
            if path.is_file() {
                return Some(path);
            }
        }
    }
    None
}

pub(crate) fn nested_files(root: &Path, depth: usize, filename: Option<&str>) -> Vec<PathBuf> {
    fn visit(
        directory: &Path,
        canonical_root: &Path,
        depth: usize,
        filename: Option<&str>,
        output: &mut Vec<PathBuf>,
    ) {
        let Ok(entries) = fs::read_dir(directory) else {
            return;
        };
        for entry in entries.flatten() {
            if output.len() >= MAX_DISCOVERED_FILES {
                return;
            }
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() && depth > 0 {
                visit(&path, canonical_root, depth - 1, filename, output);
            } else if file_type.is_file()
                && filename.is_none_or(|expected| {
                    path.file_name().is_some_and(|actual| actual == expected)
                })
                && fs::canonicalize(&path)
                    .is_ok_and(|candidate| candidate.starts_with(canonical_root))
            {
                output.push(path);
            }
        }
    }

    let mut output = Vec::new();
    if let Ok(canonical_root) = fs::canonicalize(root) {
        visit(root, &canonical_root, depth, filename, &mut output);
    }
    output.sort();
    output
}

pub(crate) fn provider_file(root: &Path, candidate: &Path) -> Option<PathBuf> {
    if candidate.symlink_metadata().ok()?.file_type().is_symlink() {
        return None;
    }
    let root = fs::canonicalize(root).ok()?;
    let candidate = fs::canonicalize(candidate).ok()?;
    (candidate.is_file() && candidate.starts_with(root)).then_some(candidate)
}

pub(crate) struct SqliteSnapshot {
    pub(crate) connection: Connection,
    _directory: TempDir,
}

#[derive(Eq, PartialEq)]
struct SqliteSignature {
    database: [u8; 32],
    wal: Option<[u8; 32]>,
}

pub(crate) fn sqlite_snapshot(root: &Path, database: &Path) -> Result<SqliteSnapshot> {
    let database = provider_file(root, database)
        .ok_or_else(|| anyhow!("provider database is outside its allowed root"))?;
    for _ in 0..3 {
        let before = sqlite_signature(root, &database)?;
        let directory = tempfile::tempdir()?;
        let snapshot = directory.path().join("snapshot.sqlite");
        copy_limited(&database, &snapshot)?;
        if before.wal.is_some() {
            let source_wal = provider_file(root, &sidecar(&database, "-wal"))
                .ok_or_else(|| anyhow!("provider WAL changed during snapshot"))?;
            copy_limited(&source_wal, &sidecar(&snapshot, "-wal"))?;
        }
        let after = sqlite_signature(root, &database)?;
        let copied = sqlite_signature(directory.path(), &snapshot)?;
        if before != after || before != copied {
            continue;
        }
        let Ok(connection) = Connection::open_with_flags(
            snapshot,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        ) else {
            continue;
        };
        let integrity = connection
            .query_row("PRAGMA quick_check(1)", [], |row| row.get::<_, String>(0))
            .unwrap_or_default();
        if integrity != "ok" {
            continue;
        }
        connection.pragma_update(None, "query_only", "ON")?;
        return Ok(SqliteSnapshot {
            connection,
            _directory: directory,
        });
    }
    Err(anyhow!("provider database changed during snapshot"))
}

fn sqlite_signature(root: &Path, database: &Path) -> Result<SqliteSignature> {
    let database = provider_file(root, database)
        .ok_or_else(|| anyhow!("provider database changed during snapshot"))?;
    let wal = provider_file(root, &sidecar(&database, "-wal"))
        .map(|path| hash_limited(&path))
        .transpose()?;
    Ok(SqliteSignature {
        database: hash_limited(&database)?,
        wal,
    })
}

fn sidecar(database: &Path, suffix: &str) -> PathBuf {
    let mut path = database.as_os_str().to_os_string();
    path.push(suffix);
    PathBuf::from(path)
}

fn hash_limited(path: &Path) -> Result<[u8; 32]> {
    let file = File::open(path)?;
    if file.metadata()?.len() > MAX_SQLITE_SNAPSHOT_SIZE {
        return Err(anyhow!("provider SQLite file exceeds safe snapshot limit"));
    }
    let mut reader = file.take(MAX_SQLITE_SNAPSHOT_SIZE + 1);
    let mut digest = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    let mut total = 0_u64;
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        total += u64::try_from(read)?;
        if total > MAX_SQLITE_SNAPSHOT_SIZE {
            return Err(anyhow!("provider SQLite file exceeds safe snapshot limit"));
        }
        digest.update(&buffer[..read]);
    }
    Ok(digest.finalize().into())
}

fn copy_limited(source: &Path, target: &Path) -> Result<()> {
    let mut source = File::open(source)?.take(MAX_SQLITE_SNAPSHOT_SIZE + 1);
    let mut target = File::create(target)?;
    let copied = std::io::copy(&mut source, &mut target)?;
    if copied > MAX_SQLITE_SNAPSHOT_SIZE {
        return Err(anyhow!("provider SQLite file exceeds safe snapshot limit"));
    }
    target.sync_all()?;
    Ok(())
}

pub(crate) fn json_lines(path: &Path) -> Result<Vec<Value>> {
    read_json_lines(path, MAX_PROVIDER_RECORDS, true)
}

pub(crate) fn json_lines_prefix(path: &Path, limit: usize) -> Result<Vec<Value>> {
    read_json_lines(path, limit, false)
}

fn read_json_lines(path: &Path, limit: usize, require_eof: bool) -> Result<Vec<Value>> {
    let file = File::open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || require_eof && metadata.len() > MAX_TRANSCRIPT_FILE_SIZE {
        return Err(anyhow!("provider file exceeds safe read limit"));
    }
    let mut reader = BufReader::new(file);
    let mut records = Vec::new();
    let mut lines = 0_usize;
    loop {
        if lines >= limit {
            if require_eof && !reader.fill_buf()?.is_empty() {
                return Err(anyhow!("provider file exceeds safe record limit"));
            }
            break;
        }
        let mut line = String::new();
        let mut bounded = reader.take(MAX_TRANSCRIPT_LINE_SIZE + 1);
        let read = bounded.read_line(&mut line)?;
        reader = bounded.into_inner();
        if read == 0 {
            break;
        }
        if read as u64 > MAX_TRANSCRIPT_LINE_SIZE {
            return Err(anyhow!("provider record exceeds safe line limit"));
        }
        lines += 1;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(record) = serde_json::from_str(line) {
            records.push(record);
        }
    }
    Ok(records)
}

pub(crate) fn read_json(path: &Path) -> Result<Value> {
    let file = File::open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > MAX_METADATA_FILE_SIZE {
        return Err(anyhow!("provider metadata exceeds safe read limit"));
    }
    serde_json::from_reader(BufReader::new(file)).map_err(Into::into)
}

pub(crate) fn parse_timestamp(value: Option<&Value>) -> Option<DateTime<Utc>> {
    match value? {
        Value::String(value) => DateTime::parse_from_rfc3339(value)
            .ok()
            .map(|timestamp| timestamp.with_timezone(&Utc)),
        Value::Number(value) => {
            let raw = value.as_i64()?;
            if raw.unsigned_abs() >= 100_000_000_000 {
                DateTime::from_timestamp_millis(raw)
            } else {
                DateTime::from_timestamp(raw, 0)
            }
        }
        _ => None,
    }
}

pub(crate) fn string_at<'a>(value: &'a Value, paths: &[&[&str]]) -> Option<&'a str> {
    paths.iter().find_map(|path| {
        let mut current = value;
        for key in *path {
            current = current.get(*key)?;
        }
        current.as_str().filter(|value| !value.is_empty())
    })
}

pub(crate) fn value_at<'a>(value: &'a Value, paths: &[&[&str]]) -> Option<&'a Value> {
    paths.iter().find_map(|path| {
        let mut current = value;
        for key in *path {
            current = current.get(*key)?;
        }
        Some(current)
    })
}

pub(crate) fn validate_provider(session: &SessionRef, provider: Provider) -> Result<()> {
    if session.provider == provider {
        Ok(())
    } else {
        Err(anyhow!(
            "session `{session}` does not belong to provider `{provider}`"
        ))
    }
}

pub(crate) fn paths_match(recorded: &Path, requested: &Path) -> bool {
    match (fs::canonicalize(recorded), fs::canonicalize(requested)) {
        (Ok(recorded), Ok(requested)) => recorded == requested,
        _ => recorded == requested,
    }
}

pub(crate) struct EventBuilder {
    provider: Provider,
    session_id: String,
    provider_version: Option<String>,
    thread_id: Uuid,
    branch_id: Uuid,
    events: Vec<OmniEvent>,
}

impl EventBuilder {
    pub(crate) fn new(provider: Provider, session_id: &str) -> Self {
        let thread_id = Uuid::parse_str(session_id).unwrap_or_else(|_| {
            Uuid::new_v5(
                &Uuid::NAMESPACE_URL,
                format!("https://omnisession.dev/{provider}/{session_id}").as_bytes(),
            )
        });
        Self {
            provider,
            session_id: session_id.to_owned(),
            provider_version: None,
            thread_id,
            branch_id: thread_id,
            events: Vec::new(),
        }
    }

    pub(crate) fn set_provider_version(&mut self, provider_version: Option<String>) {
        self.provider_version = provider_version;
    }

    pub(crate) fn push(
        &mut self,
        kind: EventKind,
        payload: Value,
        timestamp: Option<DateTime<Utc>>,
        replay_policy: ReplayPolicy,
        raw_record_type: Option<String>,
        event_id: Option<Uuid>,
    ) {
        let sequence = self.events.len() as u64;
        let event_seed = event_id.map_or_else(
            || sequence.to_string(),
            |native_id| format!("{sequence}:{native_id}"),
        );
        self.events.push(OmniEvent {
            schema_version: SCHEMA_VERSION.to_owned(),
            event_id: Uuid::new_v5(&self.thread_id, event_seed.as_bytes()),
            thread_id: self.thread_id,
            branch_id: self.branch_id,
            sequence,
            timestamp,
            source: EventSource {
                provider: self.provider,
                native_session_id: self.session_id.clone(),
                provider_version: self.provider_version.clone(),
                raw_record_type,
            },
            kind,
            payload,
            raw_blob_hash: None,
            sensitivity: Sensitivity::Normal,
            replay_policy,
        });
    }

    pub(crate) fn snapshot(
        self,
        session: SessionRef,
        title: Option<String>,
        project_path: Option<PathBuf>,
        git_branch: Option<String>,
        captured_at: DateTime<Utc>,
    ) -> CanonicalSnapshot {
        let current_dir = project_path.unwrap_or_default();
        CanonicalSnapshot {
            schema_version: SCHEMA_VERSION.to_owned(),
            session,
            thread_id: self.thread_id,
            branch_id: self.branch_id,
            title,
            captured_at,
            workspace: WorkspaceSnapshot {
                schema_version: SCHEMA_VERSION.to_owned(),
                captured_at,
                root: current_dir.clone(),
                current_dir,
                git: GitState {
                    branch: git_branch,
                    ..GitState::default()
                },
                instruction_files: Vec::new(),
                environment_names: Vec::new(),
                available_tools: Vec::new(),
            },
            events: self.events,
        }
    }
}

pub(crate) fn selected_metadata(value: &Value) -> Value {
    const SAFE_FIELDS: &[&str] = &[
        "id",
        "sessionId",
        "session_id",
        "title",
        "name",
        "cwd",
        "directory",
        "projectPath",
        "project_path",
        "gitBranch",
        "git_branch",
        "createdAt",
        "created_at",
        "updatedAt",
        "updated_at",
    ];
    let mut selected = Map::new();
    if let Some(object) = value.as_object() {
        for field in SAFE_FIELDS {
            if let Some(value) = object.get(*field) {
                selected.insert((*field).to_owned(), value.clone());
            }
        }
    }
    Value::Object(selected)
}

pub(crate) fn sort_sessions(sessions: &mut [crate::NativeSession]) {
    sessions.sort_by(|left, right| {
        right
            .updated_at
            .cmp(&left.updated_at)
            .then_with(|| left.session.id.cmp(&right.session.id))
    });
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::{MAX_TRANSCRIPT_LINE_SIZE, json_lines};

    #[test]
    fn rejects_oversized_provider_record() {
        let temporary = tempdir().expect("temporary directory");
        let path = temporary.path().join("oversized.jsonl");
        let size = usize::try_from(MAX_TRANSCRIPT_LINE_SIZE + 1).expect("line limit fits usize");
        let mut record = vec![b'x'; size];
        record.push(b'\n');
        std::fs::write(&path, record).expect("oversized fixture");

        assert!(json_lines(&path).is_err());
    }
}
