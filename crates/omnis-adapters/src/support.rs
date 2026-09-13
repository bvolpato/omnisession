use std::{
    collections::VecDeque,
    env,
    ffi::OsStr,
    fs,
    fs::File,
    io::{self, BufRead, BufReader, ErrorKind, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

use anyhow::{Result, anyhow};
use chrono::{DateTime, Utc};
use omnis_core::workspace_paths_match;
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
const MAX_PREVIEW_TAIL_SIZE: u64 = 4 * 1024 * 1024;
pub(crate) const MAX_STREAMED_TRANSCRIPT_FILE_SIZE: u64 = 4 * 1024 * 1024 * 1024;
// Readers that keep every record in memory stop at a smaller file size.
pub(crate) const MAX_COLLECTED_TRANSCRIPT_FILE_SIZE: u64 = 512 * 1024 * 1024;
const MAX_STREAMED_TRANSCRIPT_LINE_SIZE: u64 = 16 * 1024 * 1024;
// Index readers fold records into per-session maps, so record count does not bound memory.
// This budget keeps listings fast. Larger index files keep their newest suffix.
const MAX_INDEX_FILE_SIZE: u64 = 256 * 1024 * 1024;
const MAX_DISCOVERED_FILES: usize = 10_000;
const MAX_DISCOVERY_ENTRIES: usize = 200_000;
const MAX_METADATA_FILE_SIZE: u64 = 4 * 1024 * 1024;
// Snapshots copy the database and WAL to private temporary storage, so this bounds disk use, not
// memory. It matches the streamed transcript budget. `OMNI_SNAPSHOT_MAX_BYTES` overrides it.
const DEFAULT_SQLITE_SNAPSHOT_MAX_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const SQLITE_SNAPSHOT_MAX_BYTES_ENV: &str = "OMNI_SNAPSHOT_MAX_BYTES";
// Free space a snapshot copy leaves on the temporary volume.
const SQLITE_SNAPSHOT_FREE_SPACE_RESERVE: u64 = 256 * 1024 * 1024;
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
        return (candidate.is_file() && !is_omnisession_shim(candidate))
            .then(|| candidate.to_path_buf());
    }

    let paths = env::var_os("PATH")?;
    env::split_paths(&paths)
        .flat_map(|directory| executable_candidates(&directory, name))
        .find(|path| path.is_file() && !is_omnisession_shim(path))
}

/// Same PATH candidates as the CLI shim resolver.
#[cfg(not(windows))]
fn executable_candidates(directory: &Path, name: &str) -> Vec<PathBuf> {
    vec![directory.join(name)]
}

/// Windows tries only `PATHEXT` extensions, so npm's extension-less shell script is never
/// selected.
#[cfg(windows)]
fn executable_candidates(directory: &Path, name: &str) -> Vec<PathBuf> {
    windows_executable_candidates(directory, name, env::var_os("PATHEXT").as_deref())
}

/// Whether `path` is a `.cmd` or `.bat` launcher, which Windows runs through `cmd.exe`.
pub(crate) fn is_batch_launcher(path: &Path) -> bool {
    path.extension()
        .and_then(std::ffi::OsStr::to_str)
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("cmd") || extension.eq_ignore_ascii_case("bat")
        })
}

#[cfg(any(test, windows))]
fn windows_executable_candidates(
    directory: &Path,
    name: &str,
    path_extensions: Option<&std::ffi::OsStr>,
) -> Vec<PathBuf> {
    let path_extensions = path_extensions
        .filter(|extensions| !extensions.is_empty())
        .map_or_else(
            || ".COM;.EXE;.BAT;.CMD".into(),
            std::ffi::OsStr::to_string_lossy,
        );
    path_extensions
        .split(';')
        .filter(|extension| !extension.is_empty())
        .map(|extension| directory.join(format!("{name}{extension}")))
        .collect()
}

/// Resolves a provider executable with the CLI's rules for real provider binaries.
///
/// A non-empty `OMNI_*_BIN` override must name an absolute executable that is not an `OmniSession`
/// shim. An invalid override means the provider is not installed; PATH is not searched.
pub(crate) fn provider_executable(provider: Provider) -> Option<PathBuf> {
    let name = provider.command()?;
    let Some(value) = binary_override(provider)
        .and_then(env::var_os)
        .filter(|value| !value.is_empty())
    else {
        return executable(name);
    };
    let candidate = Path::new(&value);
    if !candidate.is_absolute() || !is_executable(candidate) {
        return None;
    }
    let binary = fs::canonicalize(candidate).ok()?;
    let current_executable = env::current_exe().and_then(fs::canonicalize).ok();
    let is_shim =
        current_executable.as_deref() == Some(binary.as_path()) || is_omnisession_shim(&binary);
    let names_provider =
        provider != Provider::CursorCli || cursor_agent_binary_name_matches(&binary);
    (!is_shim && names_provider).then_some(binary)
}

/// Same variables as the CLI shim resolver.
const fn binary_override(provider: Provider) -> Option<&'static str> {
    match provider {
        Provider::Claude => Some("OMNI_CLAUDE_BIN"),
        Provider::Codex => Some("OMNI_CODEX_BIN"),
        Provider::OpenCode => Some("OMNI_OPENCODE_BIN"),
        Provider::Grok => Some("OMNI_GROK_BIN"),
        Provider::Hermes => Some("OMNI_HERMES_BIN"),
        Provider::Antigravity => Some("OMNI_ANTIGRAVITY_BIN"),
        Provider::Pi => Some("OMNI_PI_BIN"),
        Provider::CursorCli => Some("OMNI_CURSOR_AGENT_BIN"),
        Provider::AntigravityIde
        | Provider::CursorIde
        | Provider::GenericAcp
        | Provider::Imported => None,
    }
}

/// Same Cursor Agent name check as the CLI shim resolver.
fn cursor_agent_binary_name_matches(binary: &Path) -> bool {
    let Some(name) = binary.file_stem().and_then(std::ffi::OsStr::to_str) else {
        return false;
    };
    if name.eq_ignore_ascii_case("cursor-agent") {
        return true;
    }
    cfg!(windows)
        && name.eq_ignore_ascii_case("agent")
        && binary
            .parent()
            .and_then(Path::file_name)
            .and_then(std::ffi::OsStr::to_str)
            .is_some_and(|parent| parent.eq_ignore_ascii_case("cursor-agent"))
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    path.metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

fn is_omnisession_shim(candidate: &Path) -> bool {
    let state_root = env::var_os("OMNISESSION_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            directories::BaseDirs::new()
                .map(|directories| directories.home_dir().join(".omnisession"))
        });
    state_root.is_some_and(|root| same_parent(candidate, &root.join("shims")))
}

fn same_parent(candidate: &Path, directory: &Path) -> bool {
    let Some(parent) = candidate.parent() else {
        return false;
    };
    let parent = fs::canonicalize(parent).unwrap_or_else(|_| parent.to_path_buf());
    let directory = fs::canonicalize(directory).unwrap_or_else(|_| directory.to_path_buf());
    parent == directory
}

pub(crate) fn nested_files(root: &Path, depth: usize, filename: Option<&str>) -> Vec<PathBuf> {
    nested_files_matching(root, depth, &|path, is_dir| {
        is_dir
            || filename
                .is_none_or(|expected| path.file_name().is_some_and(|actual| actual == expected))
    })
}

/// Walks provider files. `include` decides whether to enter a directory (`true`) or accept a file.
pub(crate) fn nested_files_matching(
    root: &Path,
    depth: usize,
    include: &dyn Fn(&Path, bool) -> bool,
) -> Vec<PathBuf> {
    nested_files_with_limit(
        root,
        depth,
        MAX_DISCOVERED_FILES,
        MAX_DISCOVERY_ENTRIES,
        include,
    )
}

fn nested_files_with_limit(
    root: &Path,
    depth: usize,
    file_limit: usize,
    entry_limit: usize,
    include: &dyn Fn(&Path, bool) -> bool,
) -> Vec<PathBuf> {
    fn visit(
        directory: &Path,
        canonical_root: &Path,
        depth: usize,
        file_limit: usize,
        entries_left: &mut usize,
        include: &dyn Fn(&Path, bool) -> bool,
        output: &mut Vec<PathBuf>,
    ) {
        let Ok(entries) = fs::read_dir(directory) else {
            return;
        };
        for entry in entries.flatten() {
            if output.len() >= file_limit || *entries_left == 0 {
                return;
            }
            *entries_left -= 1;
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() && depth > 0 && include(&path, true) {
                visit(
                    &path,
                    canonical_root,
                    depth - 1,
                    file_limit,
                    entries_left,
                    include,
                    output,
                );
            } else if file_type.is_file()
                && include(&path, false)
                && fs::canonicalize(&path)
                    .is_ok_and(|candidate| candidate.starts_with(canonical_root))
            {
                output.push(path);
            }
        }
    }

    let mut output = Vec::new();
    let mut entries_left = entry_limit;
    if let Ok(canonical_root) = fs::canonicalize(root) {
        visit(
            root,
            &canonical_root,
            depth,
            file_limit,
            &mut entries_left,
            include,
            &mut output,
        );
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

/// Whether a provider store is absent. A missing store means "not installed", not a failure.
pub(crate) fn store_is_missing(path: &Path) -> bool {
    path.symlink_metadata()
        .is_err_and(|error| error.kind() == ErrorKind::NotFound)
}

pub(crate) struct SqliteSnapshot {
    pub(crate) connection: Connection,
    _directory: TempDir,
}

/// Bounds for the private copy a SQLite snapshot makes.
#[derive(Clone, Copy, Debug)]
struct SnapshotLimits {
    /// Database plus WAL bytes the copy may hold.
    max_bytes: u64,
    /// Free space the copy must leave on the temporary volume.
    free_space_reserve: u64,
}

impl SnapshotLimits {
    fn from_environment() -> Result<Self> {
        Ok(Self {
            max_bytes: snapshot_max_bytes(env::var_os(SQLITE_SNAPSHOT_MAX_BYTES_ENV).as_deref())?,
            free_space_reserve: SQLITE_SNAPSHOT_FREE_SPACE_RESERVE,
        })
    }
}

/// Parses the snapshot bound setting. An invalid value fails closed instead of falling back.
fn snapshot_max_bytes(value: Option<&OsStr>) -> Result<u64> {
    let Some(value) = value.filter(|value| !value.is_empty()) else {
        return Ok(DEFAULT_SQLITE_SNAPSHOT_MAX_BYTES);
    };
    value
        .to_str()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|bytes| *bytes > 0)
        .ok_or_else(|| {
            anyhow!("{SQLITE_SNAPSHOT_MAX_BYTES_ENV} must be a positive whole number of bytes")
        })
}

#[derive(Eq, PartialEq)]
struct SqliteSignature {
    bytes: u64,
    database: [u8; 32],
    wal: Option<[u8; 32]>,
}

/// Copies a provider database and its WAL into a private temporary directory and opens the copy.
///
/// SQLite never opens the provider files, so locks and `-shm` activity stay in the copy. Reads
/// stream through fixed buffers, so memory does not grow with store size. Disk use is bounded by
/// `OMNI_SNAPSHOT_MAX_BYTES`, and the copy must leave a free-space reserve on the temporary volume.
pub(crate) fn sqlite_snapshot(root: &Path, database: &Path) -> Result<SqliteSnapshot> {
    sqlite_snapshot_with_limits(root, database, SnapshotLimits::from_environment()?)
}

fn sqlite_snapshot_with_limits(
    root: &Path,
    database: &Path,
    limits: SnapshotLimits,
) -> Result<SqliteSnapshot> {
    let database = provider_file(root, database)
        .ok_or_else(|| anyhow!("provider database is outside its allowed root"))?;
    for _ in 0..3 {
        let before = sqlite_signature(root, &database, limits.max_bytes)?;
        let directory = tempfile::tempdir()?;
        ensure_snapshot_space(directory.path(), before.bytes, limits.free_space_reserve)?;
        let snapshot = directory.path().join("snapshot.sqlite");
        let source_wal = if before.wal.is_some() {
            Some(
                provider_file(root, &sidecar(&database, "-wal"))
                    .ok_or_else(|| anyhow!("provider WAL changed during snapshot"))?,
            )
        } else {
            None
        };
        if !copy_within_budget(&database, source_wal.as_deref(), &snapshot, before.bytes)? {
            continue;
        }
        let after = sqlite_signature(root, &database, limits.max_bytes)?;
        let copied = sqlite_signature(directory.path(), &snapshot, limits.max_bytes)?;
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

/// Hashes a database and its WAL after checking their combined size against the snapshot bound.
fn sqlite_signature(root: &Path, database: &Path, max_bytes: u64) -> Result<SqliteSignature> {
    let database = provider_file(root, database)
        .ok_or_else(|| anyhow!("provider database changed during snapshot"))?;
    let wal = provider_file(root, &sidecar(&database, "-wal"));
    let mut bytes = database.metadata()?.len();
    if let Some(wal) = &wal {
        bytes = bytes.saturating_add(wal.metadata()?.len());
    }
    if bytes > max_bytes {
        return Err(anyhow!(
            "provider SQLite database and WAL need {bytes} bytes, above the {max_bytes}-byte \
             snapshot limit; set {SQLITE_SNAPSHOT_MAX_BYTES_ENV} to raise it"
        ));
    }
    Ok(SqliteSignature {
        bytes,
        database: hash_limited(&database, max_bytes)?,
        wal: wal.map(|wal| hash_limited(&wal, max_bytes)).transpose()?,
    })
}

/// Refuses a copy that would leave less than `reserve` bytes free on the temporary volume.
fn ensure_snapshot_space(directory: &Path, bytes: u64, reserve: u64) -> Result<()> {
    let available = fs2::available_space(directory)?;
    if available < bytes.saturating_add(reserve) {
        return Err(io::Error::new(
            ErrorKind::StorageFull,
            format!(
                "provider SQLite snapshot needs {bytes} bytes plus {reserve} bytes of headroom, \
                 but temporary storage has {available} bytes available"
            ),
        )
        .into());
    }
    Ok(())
}

fn sidecar(database: &Path, suffix: &str) -> PathBuf {
    let mut path = database.as_os_str().to_os_string();
    path.push(suffix);
    PathBuf::from(path)
}

fn hash_limited(path: &Path, max_bytes: u64) -> Result<[u8; 32]> {
    let mut reader = File::open(path)?.take(max_bytes.saturating_add(1));
    let mut digest = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    let mut total = 0_u64;
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        total += u64::try_from(read)?;
        digest.update(&buffer[..read]);
    }
    // The size passed its check before reading, so a longer read means the file grew.
    if total > max_bytes {
        return Err(anyhow!("provider database changed during snapshot"));
    }
    Ok(digest.finalize().into())
}

/// Copies the database and its WAL into `snapshot` within one byte budget.
///
/// Returns `false` when the sources outgrew the budget, which means they changed after it was
/// checked.
fn copy_within_budget(
    database: &Path,
    wal: Option<&Path>,
    snapshot: &Path,
    budget: u64,
) -> Result<bool> {
    let Some(copied) = copy_limited(database, snapshot, budget)? else {
        return Ok(false);
    };
    if let Some(wal) = wal {
        // The WAL gets only what the database left, so a growing WAL can't double disk use.
        if copy_limited(wal, &sidecar(snapshot, "-wal"), budget - copied)?.is_none() {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Copies at most `max_bytes` and returns the bytes copied, or `None` when the source is longer.
fn copy_limited(source: &Path, target: &Path, max_bytes: u64) -> Result<Option<u64>> {
    let mut source = File::open(source)?.take(max_bytes.saturating_add(1));
    let mut target = File::create(target)?;
    let copied = io::copy(&mut source, &mut target)?;
    if copied > max_bytes {
        return Ok(None);
    }
    target.sync_all()?;
    Ok(Some(copied))
}

/// Budgets for streamed JSONL reads.
///
/// Line size counts the line terminator when present, so a final unterminated line may hold one
/// more content byte than a terminated one.
#[derive(Clone, Copy, Debug)]
pub(crate) struct JsonLinesLimits {
    pub(crate) file_bytes: u64,
    pub(crate) records: usize,
    pub(crate) line_bytes: u64,
}

impl JsonLinesLimits {
    const fn streamed(file_bytes: u64) -> Self {
        Self {
            file_bytes,
            records: MAX_PROVIDER_RECORDS,
            line_bytes: MAX_STREAMED_TRANSCRIPT_LINE_SIZE,
        }
    }
}

/// Streams transcript records and returns how many oversized records were skipped.
pub(crate) fn visit_json_lines(
    path: &Path,
    file_limit: u64,
    visit: impl FnMut(Value) -> Result<()>,
) -> Result<usize> {
    visit_json_lines_with_limits(path, JsonLinesLimits::streamed(file_limit), visit)
}

/// Streams records for visitors that fold them instead of collecting them.
///
/// Only byte budgets apply, so the visitor must bound what it retains. Long rollouts then read to
/// the end instead of failing at the collected record budget.
pub(crate) fn visit_streamed_json_lines(
    path: &Path,
    file_limit: u64,
    visit: impl FnMut(Value) -> Result<()>,
) -> Result<usize> {
    visit_json_lines_with_limits(
        path,
        JsonLinesLimits {
            records: usize::MAX,
            ..JsonLinesLimits::streamed(file_limit)
        },
        visit,
    )
}

/// Visible text for a user turn made only of images, so the turn stays in history.
pub(crate) fn omitted_images_text(count: usize) -> String {
    if count == 1 {
        "[1 image omitted]".to_owned()
    } else {
        format!("[{count} images omitted]")
    }
}

fn visit_json_lines_with_limits(
    path: &Path,
    limits: JsonLinesLimits,
    mut visit: impl FnMut(Value) -> Result<()>,
) -> Result<usize> {
    let file = File::open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > limits.file_bytes {
        return Err(anyhow!("provider file exceeds safe streaming limit"));
    }
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    let mut records = 0_usize;
    let mut oversized_records = 0_usize;
    loop {
        if records >= limits.records {
            if !reader.fill_buf()?.is_empty() {
                return Err(anyhow!("provider file exceeds safe record limit"));
            }
            return Ok(oversized_records);
        }
        let Some((line_kind, _)) = read_bounded_line(&mut reader, limits.line_bytes, &mut line)?
        else {
            return Ok(oversized_records);
        };
        records += 1;
        match line_kind {
            BoundedLine::Oversized => oversized_records += 1,
            BoundedLine::Complete => {
                if let Ok(record) = serde_json::from_slice(&line) {
                    visit(record)?;
                }
            }
        }
    }
}

/// What an index scan left out. Index scans never fail on budgets.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct IndexScan {
    /// Records above the line budget, skipped whole.
    pub(crate) oversized_records: usize,
    /// Oldest bytes left unread because the file exceeds its byte budget.
    pub(crate) skipped_bytes: u64,
}

impl IndexScan {
    /// Discovery notes naming what was skipped. `label` names the provider file.
    pub(crate) fn notes(self, label: &str) -> Vec<String> {
        let mut notes = Vec::new();
        if self.skipped_bytes > 0 {
            notes.push(format!(
                "{label} exceeds {} MiB; its oldest {} byte(s) were not indexed.",
                MAX_INDEX_FILE_SIZE / (1024 * 1024),
                self.skipped_bytes
            ));
        }
        if self.oversized_records > 0 {
            notes.push(format!(
                "{label} skipped {} oversized record(s) above {} MiB.",
                self.oversized_records,
                MAX_TRANSCRIPT_LINE_SIZE / (1024 * 1024)
            ));
        }
        notes
    }
}

/// Streams an append-only index where later records update earlier ones.
///
/// Files above the byte budget keep their newest suffix, starting at the first complete record.
/// Oversized records are skipped and counted. Only open or read failures are errors.
pub(crate) fn visit_index_json_lines(path: &Path, visit: impl FnMut(Value)) -> Result<IndexScan> {
    visit_index_json_lines_with_limits(path, MAX_INDEX_FILE_SIZE, MAX_TRANSCRIPT_LINE_SIZE, visit)
}

fn visit_index_json_lines_with_limits(
    path: &Path,
    file_limit: u64,
    line_limit: u64,
    mut visit: impl FnMut(Value),
) -> Result<IndexScan> {
    let mut file = File::open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(anyhow!("provider path is not a file"));
    }
    let length = metadata.len();
    let start = length.saturating_sub(file_limit);
    // Start one byte early so a suffix that begins on a record boundary keeps that record.
    let aligned = start.saturating_sub(1);
    file.seek(SeekFrom::Start(aligned))?;
    let mut reader = BufReader::new(file.take(length - aligned));
    let mut scan = IndexScan::default();
    if start > 0 {
        scan.skipped_bytes = aligned + u64::try_from(reader.skip_until(b'\n')?)?;
    }
    let mut line = Vec::new();
    while let Some((line_kind, _)) = read_bounded_line(&mut reader, line_limit, &mut line)? {
        match line_kind {
            BoundedLine::Oversized => scan.oversized_records += 1,
            BoundedLine::Complete => {
                if let Ok(record) = serde_json::from_slice(&line) {
                    visit(record);
                }
            }
        }
    }
    Ok(scan)
}

enum BoundedLine {
    Complete,
    Oversized,
}

/// Reads one line into `line` and returns its kind and consumed bytes, or `None` at the end.
///
/// Oversized lines are consumed through their terminator without buffering the rest.
fn read_bounded_line(
    reader: &mut impl BufRead,
    line_limit: u64,
    line: &mut Vec<u8>,
) -> io::Result<Option<(BoundedLine, u64)>> {
    line.clear();
    let read = reader
        .by_ref()
        .take(line_limit.saturating_add(1))
        .read_until(b'\n', line)? as u64;
    if read == 0 {
        return Ok(None);
    }
    if read <= line_limit {
        return Ok(Some((BoundedLine::Complete, read)));
    }
    let rest = if line.last() == Some(&b'\n') {
        0
    } else {
        reader.skip_until(b'\n')? as u64
    };
    Ok(Some((BoundedLine::Oversized, read + rest)))
}

pub(crate) fn json_lines_prefix(path: &Path, limit: usize) -> Result<Vec<Value>> {
    read_json_lines(path, limit)
}

pub(crate) fn json_lines_preview(path: &Path, sample_records: usize) -> Result<Vec<Value>> {
    let metadata = File::open(path)?.metadata()?;
    if !metadata.is_file() {
        return Err(anyhow!("provider path is not a file"));
    }
    if metadata.len() <= MAX_PREVIEW_TAIL_SIZE {
        return read_json_lines(path, MAX_PROVIDER_RECORDS);
    }

    let (mut records, prefix_end) = read_json_lines_with_end(path, sample_records)?;
    records.extend(
        json_lines_tail_with_offsets(path, sample_records)?
            .into_iter()
            .filter_map(|(offset, record)| (offset >= prefix_end).then_some(record)),
    );
    Ok(records)
}

fn json_lines_tail_with_offsets(path: &Path, limit: usize) -> Result<Vec<(u64, Value)>> {
    let mut file = File::open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(anyhow!("provider path is not a file"));
    }
    let start = metadata.len().saturating_sub(MAX_PREVIEW_TAIL_SIZE);
    file.seek(SeekFrom::Start(start))?;
    let mut reader = BufReader::new(file.take(MAX_PREVIEW_TAIL_SIZE));
    let mut offset = start;
    if start > 0 {
        let mut partial = Vec::new();
        offset += u64::try_from(reader.read_until(b'\n', &mut partial)?)?;
    }
    let mut records = VecDeque::with_capacity(limit);
    let mut line = Vec::new();
    loop {
        let line_offset = offset;
        line.clear();
        let read = reader.read_until(b'\n', &mut line)?;
        if read == 0 {
            break;
        }
        offset += u64::try_from(read)?;
        // Previews skip oversized records the way streamed reads do.
        if read as u64 > MAX_TRANSCRIPT_LINE_SIZE {
            continue;
        }
        if let Ok(record) = serde_json::from_slice(&line) {
            if limit > 0 {
                if records.len() == limit {
                    records.pop_front();
                }
                records.push_back((line_offset, record));
            }
        }
    }
    Ok(records.into_iter().collect())
}

fn read_json_lines(path: &Path, limit: usize) -> Result<Vec<Value>> {
    Ok(read_json_lines_with_end(path, limit)?.0)
}

/// Reads up to `limit` leading lines and returns their records and consumed bytes.
fn read_json_lines_with_end(path: &Path, limit: usize) -> Result<(Vec<Value>, u64)> {
    let file = File::open(path)?;
    if !file.metadata()?.is_file() {
        return Err(anyhow!("provider path is not a file"));
    }
    let mut reader = BufReader::new(file);
    let mut records = Vec::new();
    let mut line = Vec::new();
    let mut consumed = 0_u64;
    for _ in 0..limit {
        let Some((line_kind, read)) =
            read_bounded_line(&mut reader, MAX_TRANSCRIPT_LINE_SIZE, &mut line)?
        else {
            break;
        };
        consumed += read;
        // Previews skip oversized records the way streamed reads do.
        match line_kind {
            BoundedLine::Oversized => {}
            BoundedLine::Complete => {
                if let Ok(record) = serde_json::from_slice(&line) {
                    records.push(record);
                }
            }
        }
    }
    Ok((records, consumed))
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
    workspace_paths_match(recorded, requested)
}

pub(crate) struct EventBuilder {
    provider: Provider,
    session_id: String,
    provider_version: Option<String>,
    thread_id: Uuid,
    branch_id: Uuid,
    next_sequence: u64,
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
            next_sequence: 0,
            events: Vec::new(),
        }
    }

    pub(crate) fn set_provider_version(&mut self, provider_version: Option<String>) {
        self.provider_version = provider_version;
    }

    pub(crate) fn checkpoint(&self) -> u64 {
        self.next_sequence
    }

    pub(crate) fn truncate_from(&mut self, checkpoint: u64) {
        self.events.retain(|event| event.sequence < checkpoint);
    }

    pub(crate) fn retain_preview_events(&mut self, limit: usize, timestamp: DateTime<Utc>) {
        if self.events.len() <= limit {
            return;
        }
        let omitted = self.events.len() - limit;
        self.events.drain(limit / 2..limit / 2 + omitted);
        self.push(
            EventKind::ProviderEvent,
            serde_json::json!({"omitted_events": omitted, "retained_preview_events": limit}),
            Some(timestamp),
            ReplayPolicy::HistoricalOnly,
            Some("omnisession.preview_limit".to_owned()),
            None,
        );
    }

    pub(crate) fn push_oversized_record_notice(
        &mut self,
        omitted_records: usize,
        timestamp: Option<DateTime<Utc>>,
    ) {
        if omitted_records == 0 {
            return;
        }
        self.push(
            EventKind::ProviderEvent,
            serde_json::json!({
                "omitted_events": omitted_records,
                "omitted_records": omitted_records,
                "max_record_bytes": MAX_STREAMED_TRANSCRIPT_LINE_SIZE,
            }),
            timestamp,
            ReplayPolicy::HistoricalOnly,
            Some("omnisession.record_size_limit".to_owned()),
            None,
        );
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
        let sequence = self.next_sequence;
        self.next_sequence += 1;
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

    pub(crate) fn retain_latest_tool_events(&mut self, limit: usize) -> usize {
        let tool_events = self
            .events
            .iter()
            .filter(|event| {
                matches!(
                    event.kind,
                    EventKind::ToolCalled
                        | EventKind::ToolCompleted
                        | EventKind::ToolFailed
                        | EventKind::CommandExecuted
                )
            })
            .count();
        let mut remaining = tool_events.saturating_sub(limit);
        let removed = remaining;
        self.events.retain(|event| {
            if remaining > 0
                && matches!(
                    event.kind,
                    EventKind::ToolCalled
                        | EventKind::ToolCompleted
                        | EventKind::ToolFailed
                        | EventKind::CommandExecuted
                )
            {
                remaining -= 1;
                false
            } else {
                true
            }
        });
        removed
    }

    /// Drops the oldest events above `limit` and returns how many were dropped.
    pub(crate) fn retain_latest_events(&mut self, limit: usize) -> usize {
        let omitted = self.events.len().saturating_sub(limit);
        self.events.drain(..omitted);
        omitted
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
    use std::{
        collections::HashSet,
        fmt::Write,
        path::{Path, PathBuf},
    };

    use omnis_ir::{EventKind, Provider, ReplayPolicy};
    use proptest::prelude::*;
    use serde_json::json;
    use tempfile::{TempDir, tempdir};

    use super::{
        DEFAULT_SQLITE_SNAPSHOT_MAX_BYTES, EventBuilder, IndexScan, JsonLinesLimits,
        MAX_TRANSCRIPT_LINE_SIZE, SnapshotLimits, json_lines_prefix, json_lines_preview,
        json_lines_tail_with_offsets, nested_files_with_limit, same_parent, snapshot_max_bytes,
        sqlite_snapshot, sqlite_snapshot_with_limits, visit_index_json_lines_with_limits,
        visit_json_lines_with_limits,
    };

    /// A WAL-mode store with one row in the database file and one only in the WAL.
    fn wal_store() -> (TempDir, PathBuf, rusqlite::Connection) {
        let temporary = tempdir().expect("temporary directory");
        let database = temporary.path().join("store.db");
        let writer = rusqlite::Connection::open(&database).expect("fixture database");
        writer
            .execute_batch(
                "CREATE TABLE blobs (id INTEGER PRIMARY KEY, data BLOB NOT NULL);
                 INSERT INTO blobs (data) VALUES (zeroblob(16384));",
            )
            .expect("fixture schema");
        let mode: String = writer
            .query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))
            .expect("WAL mode");
        assert_eq!(mode, "wal");
        writer
            .pragma_update(None, "wal_autocheckpoint", 0)
            .expect("no automatic checkpoint");
        writer
            .execute("INSERT INTO blobs (data) VALUES (zeroblob(16384))", [])
            .expect("WAL-only row");
        (temporary, database, writer)
    }

    fn directory_bytes(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
        let mut files = std::fs::read_dir(root)
            .expect("fixture directory")
            .map(|entry| {
                let path = entry.expect("fixture entry").path();
                let bytes = std::fs::read(&path).expect("fixture file");
                (path, bytes)
            })
            .collect::<Vec<_>>();
        files.sort();
        files
    }

    #[test]
    fn sqlite_snapshot_bounds_database_and_wal_and_leaves_the_store_untouched() {
        let (temporary, database, _writer) = wal_store();
        let size = |path: &Path| std::fs::metadata(path).expect("fixture file").len();
        let wal_bytes = size(&temporary.path().join("store.db-wal"));
        assert!(wal_bytes > 0, "fixture row must live in the WAL");
        let bytes = size(&database) + wal_bytes;
        let limits = |max_bytes, free_space_reserve| SnapshotLimits {
            max_bytes,
            free_space_reserve,
        };
        let before = directory_bytes(temporary.path());

        let snapshot = sqlite_snapshot_with_limits(temporary.path(), &database, limits(bytes, 0))
            .expect("snapshot at its bound");
        let rows: i64 = snapshot
            .connection
            .query_row("SELECT COUNT(*) FROM blobs", [], |row| row.get(0))
            .expect("snapshot rows");
        assert_eq!(rows, 2, "snapshot must carry the WAL-only row");
        drop(snapshot);

        // The database alone fits one byte under the bound, but WAL bytes count too.
        let Err(error) =
            sqlite_snapshot_with_limits(temporary.path(), &database, limits(bytes - 1, 0))
        else {
            panic!("snapshot above its bound must fail closed");
        };
        assert!(
            error.to_string().contains(&format!("need {bytes} bytes")),
            "{error}"
        );

        let Err(error) =
            sqlite_snapshot_with_limits(temporary.path(), &database, limits(bytes, u64::MAX))
        else {
            panic!("snapshot without free-space headroom must fail closed");
        };
        assert!(
            error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::StorageFull),
            "{error}"
        );

        assert_eq!(directory_bytes(temporary.path()), before);
    }

    #[test]
    fn snapshot_copy_holds_database_and_wal_within_one_budget() {
        use super::{copy_within_budget, sidecar};

        let (temporary, database, _writer) = wal_store();
        let wal = temporary.path().join("store.db-wal");
        let size = |path: &Path| std::fs::metadata(path).expect("snapshot file").len();
        let copy = tempdir().expect("copy directory");
        let snapshot = copy.path().join("snapshot.sqlite");

        let bytes = size(&database) + size(&wal);
        assert!(
            copy_within_budget(&database, Some(&wal), &snapshot, bytes).expect("copy at budget")
        );

        // Sources that grew after their size check: each file alone fits, together they don't.
        let budget = size(&database).max(size(&wal));
        assert!(
            !copy_within_budget(&database, Some(&wal), &snapshot, budget)
                .expect("copy above budget")
        );
        let copied = size(&snapshot) + size(&sidecar(&snapshot, "-wal"));
        assert!(
            copied <= budget + 1,
            "copy used {copied} bytes for a {budget}-byte budget"
        );
    }

    #[test]
    fn snapshot_bound_setting_fails_closed_on_invalid_values() {
        use std::ffi::OsStr;

        for unset in [None, Some(OsStr::new(""))] {
            assert_eq!(
                snapshot_max_bytes(unset).ok(),
                Some(DEFAULT_SQLITE_SNAPSHOT_MAX_BYTES)
            );
        }
        assert_eq!(
            snapshot_max_bytes(Some(OsStr::new("1048576"))).ok(),
            Some(1_048_576)
        );
        for invalid in ["0", "-1", "4GiB", " 1024"] {
            assert!(
                snapshot_max_bytes(Some(OsStr::new(invalid))).is_err(),
                "{invalid}"
            );
        }
    }

    #[test]
    fn sqlite_snapshot_applies_the_configured_bound() {
        const CHILD_ROOT: &str = "OMNI_TEST_SNAPSHOT_BOUND_ROOT";
        const NAME: &str = "support::tests::sqlite_snapshot_applies_the_configured_bound";
        if let Some(root) = std::env::var_os(CHILD_ROOT) {
            let root = PathBuf::from(root);
            let Err(error) = sqlite_snapshot(&root, &root.join("store.db")) else {
                panic!("configured snapshot bound was ignored");
            };
            assert!(
                error.to_string().contains("OMNI_SNAPSHOT_MAX_BYTES"),
                "{error}"
            );
            return;
        }
        let (temporary, _database, _writer) = wal_store();
        let output = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args(["--exact", NAME, "--nocapture"])
            .env(CHILD_ROOT, temporary.path())
            .env("OMNI_SNAPSHOT_MAX_BYTES", "1024")
            .output()
            .expect("run child test");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains("1 passed"),
            "child test failed:\n{stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    const LIMITS: JsonLinesLimits = JsonLinesLimits {
        file_bytes: 1024,
        records: 16,
        line_bytes: 64,
    };

    /// A JSON record of exactly `bytes` bytes, without a terminator.
    fn record_of(bytes: usize) -> String {
        const OVERHEAD: usize = r#"{"p":""}"#.len();
        format!(r#"{{"p":"{}"}}"#, "x".repeat(bytes - OVERHEAD))
    }

    fn write_fixture(bytes: impl AsRef<[u8]>) -> (TempDir, PathBuf) {
        let temporary = tempdir().expect("temporary directory");
        let path = temporary.path().join("records.jsonl");
        std::fs::write(&path, bytes).expect("JSONL fixture");
        (temporary, path)
    }

    fn visit_all(path: &Path, limits: JsonLinesLimits) -> anyhow::Result<(usize, usize)> {
        let mut visited = 0;
        let oversized = visit_json_lines_with_limits(path, limits, |_| {
            visited += 1;
            Ok(())
        })?;
        Ok((visited, oversized))
    }

    fn index_values(path: &Path, file_limit: u64, line_limit: u64) -> (Vec<u64>, IndexScan) {
        let mut values = Vec::new();
        let scan = visit_index_json_lines_with_limits(path, file_limit, line_limit, |record| {
            values.extend(record["n"].as_u64());
        })
        .expect("index scan");
        (values, scan)
    }

    #[test]
    fn nested_file_limit_counts_only_accepted_files() {
        let temporary = tempdir().expect("temporary directory");
        let project = temporary.path().join("project");
        std::fs::create_dir_all(&project).expect("project directory");
        for index in 0..20 {
            std::fs::write(project.join(format!("tool-result-{index}.json")), "{}")
                .expect("decoy file");
        }
        let sessions = (0..3)
            .map(|index| {
                let path = project.join(format!("{index}.jsonl"));
                std::fs::write(&path, "{}\n").expect("session file");
                path
            })
            .collect::<Vec<_>>();

        let found = nested_files_with_limit(temporary.path(), 8, 3, 1_000, &|path, is_dir| {
            is_dir
                || path
                    .extension()
                    .is_some_and(|extension| extension == "jsonl")
        });

        assert_eq!(found, sessions);
    }

    #[test]
    fn nested_file_walk_stops_after_entry_budget() {
        let temporary = tempdir().expect("temporary directory");
        for index in 0..10 {
            std::fs::write(temporary.path().join(format!("{index}.json")), "{}")
                .expect("decoy file");
        }

        let found = nested_files_with_limit(temporary.path(), 8, 10, 3, &|_, _| true);

        assert_eq!(found.len(), 3);
    }

    #[test]
    fn nested_file_walk_skips_pruned_directories() {
        let temporary = tempdir().expect("temporary directory");
        let project = temporary.path().join("project");
        let subagents = project
            .join("11111111-1111-4111-8111-111111111111")
            .join("subagents");
        std::fs::create_dir_all(&subagents).expect("subagent directory");
        for index in 0..20 {
            std::fs::write(subagents.join(format!("agent-{index}.jsonl")), "{}\n")
                .expect("subagent log");
        }
        let transcript = project.join("11111111-1111-4111-8111-111111111111.jsonl");
        std::fs::write(&transcript, "{}\n").expect("transcript");

        // Pruning spends 4 of 5 entries in either read order; descending into logs exhausts them first.
        let found = nested_files_with_limit(temporary.path(), 8, 10, 5, &|path, is_dir| {
            if is_dir {
                return path.file_name().is_none_or(|name| name != "subagents");
            }
            path.extension()
                .is_some_and(|extension| extension == "jsonl")
        });

        assert_eq!(found, vec![transcript]);
    }

    #[test]
    fn provider_discovery_excludes_omnisession_shims() {
        let temporary = tempdir().expect("temporary directory");
        let shims = temporary.path().join("shims");
        let providers = temporary.path().join("providers");
        std::fs::create_dir_all(&shims).expect("shim directory");
        std::fs::create_dir_all(&providers).expect("provider directory");

        assert!(same_parent(&shims.join("pi"), &shims));
        assert!(!same_parent(&providers.join("pi"), &shims));
    }

    #[test]
    fn windows_path_lookup_never_selects_extensionless_scripts() {
        use std::ffi::OsStr;

        use super::{is_batch_launcher, windows_executable_candidates};

        let npm = Path::new(r"C:\Users\developer\AppData\Roaming\npm");
        let defaults = windows_executable_candidates(npm, "opencode", None);
        assert!(!defaults.iter().any(|path| path.ends_with("opencode")));
        // npm's `opencode.cmd` is still a candidate, but it is a batch launcher discovery refuses.
        let launcher = defaults
            .iter()
            .find(|path| path.ends_with("opencode.CMD"))
            .expect("npm launcher candidate");
        assert!(is_batch_launcher(launcher));
        assert_eq!(
            windows_executable_candidates(npm, "opencode", Some(OsStr::new(".EXE;;.CMD"))),
            [npm.join("opencode.EXE"), npm.join("opencode.CMD")]
        );
    }

    #[test]
    fn batch_launchers_are_detected_case_insensitively() {
        use super::is_batch_launcher;

        for launcher in ["opencode.cmd", "OPENCODE.CMD", r"C:\npm\opencode.Bat"] {
            assert!(is_batch_launcher(Path::new(launcher)), "{launcher}");
        }
        for direct in ["opencode.exe", "opencode", "cmd", "bat", "opencode.cmd.exe"] {
            assert!(!is_batch_launcher(Path::new(direct)), "{direct}");
        }
    }

    #[test]
    fn streamed_reader_file_budget_boundaries() {
        let limits = JsonLinesLimits {
            file_bytes: 48,
            ..LIMITS
        };
        for (file_bytes, accepted) in [(47, true), (48, true), (49, false)] {
            let (_temporary, path) = write_fixture(format!("{}\n", record_of(file_bytes - 1)));
            let result = visit_all(&path, limits);
            assert_eq!(
                result.ok(),
                accepted.then_some((1, 0)),
                "{file_bytes}-byte file"
            );
        }
    }

    #[test]
    fn streamed_reader_record_budget_boundaries() {
        let limits = JsonLinesLimits {
            records: 3,
            ..LIMITS
        };
        for terminated in [true, false] {
            for (count, accepted) in [(2, true), (3, true), (4, false)] {
                let mut document = (0..count)
                    .map(|index| format!(r#"{{"n":{index}}}"#))
                    .collect::<Vec<_>>()
                    .join("\n");
                if terminated {
                    document.push('\n');
                }
                let (_temporary, path) = write_fixture(document);
                assert_eq!(
                    visit_all(&path, limits).ok(),
                    accepted.then_some((count, 0)),
                    "{count} records, terminated: {terminated}"
                );
            }
        }
    }

    #[test]
    fn streamed_reader_line_budget_boundaries() {
        let limits = JsonLinesLimits {
            line_bytes: 32,
            ..LIMITS
        };
        for (line_bytes, visited) in [(31, 1), (32, 1), (33, 0)] {
            // A terminated line counts its newline.
            let (_temporary, path) =
                write_fixture(format!("{}\n{}\n", record_of(line_bytes - 1), record_of(8)));
            assert_eq!(
                visit_all(&path, limits).expect("streamed read"),
                (visited + 1, 1 - visited),
                "{line_bytes}-byte terminated line"
            );
            // A final unterminated line counts its content only.
            let (_temporary, path) =
                write_fixture(format!("{}\n{}", record_of(8), record_of(line_bytes)));
            assert_eq!(
                visit_all(&path, limits).expect("streamed read"),
                (visited + 1, 1 - visited),
                "{line_bytes}-byte final line"
            );
        }
    }

    #[test]
    fn index_reader_keeps_the_newest_suffix_above_its_byte_budget() {
        let mut document = String::new();
        for n in 0..5 {
            writeln!(document, "{{\"n\":{n}}}").expect("JSON line");
        }
        assert_eq!(document.len(), 40);
        let (_temporary, terminated) = write_fixture(&document);
        for (file_limit, values, skipped_bytes) in [
            (41, vec![0, 1, 2, 3, 4], 0),
            (40, vec![0, 1, 2, 3, 4], 0),
            (39, vec![1, 2, 3, 4], 8),
            // The suffix starts exactly on a record boundary.
            (32, vec![1, 2, 3, 4], 8),
            (31, vec![2, 3, 4], 16),
        ] {
            assert_eq!(
                index_values(&terminated, file_limit, 64),
                (
                    values,
                    IndexScan {
                        oversized_records: 0,
                        skipped_bytes
                    }
                ),
                "{file_limit}-byte budget"
            );
        }

        let (_temporary, unterminated) = write_fixture(document.trim_end());
        for (file_limit, values, skipped_bytes) in [
            (40, vec![0, 1, 2, 3, 4], 0),
            (39, vec![0, 1, 2, 3, 4], 0),
            (38, vec![1, 2, 3, 4], 8),
        ] {
            assert_eq!(
                index_values(&unterminated, file_limit, 64),
                (
                    values,
                    IndexScan {
                        oversized_records: 0,
                        skipped_bytes
                    }
                ),
                "{file_limit}-byte budget without final newline"
            );
        }
    }

    #[test]
    fn index_reader_skips_oversized_records_without_failing() {
        for (line_bytes, oversized_records) in [(31, 0), (32, 0), (33, 1)] {
            let (_temporary, terminated) =
                write_fixture(format!("{}\n{{\"n\":7}}\n", record_of(line_bytes - 1)));
            let (values, scan) = index_values(&terminated, 1024, 32);
            assert_eq!(values, [7], "{line_bytes}-byte terminated line");
            assert_eq!(scan.oversized_records, oversized_records);

            let (_temporary, unterminated) =
                write_fixture(format!("{{\"n\":7}}\n{}", record_of(line_bytes)));
            let (values, scan) = index_values(&unterminated, 1024, 32);
            assert_eq!(values, [7], "{line_bytes}-byte final line");
            assert_eq!(scan.oversized_records, oversized_records);
        }
    }

    #[test]
    fn bounded_event_builder_keeps_latest_tools_and_unique_sequences() {
        let mut builder =
            EventBuilder::new(Provider::Codex, "44444444-4444-4444-8444-444444444444");
        for result in 0..3 {
            builder.push(
                EventKind::ToolCompleted,
                json!({"result": result}),
                None,
                ReplayPolicy::HistoricalOnly,
                None,
                None,
            );
        }

        assert_eq!(builder.retain_latest_tool_events(2), 1);
        builder.push(
            EventKind::MessageAssistant,
            json!({"text": "after tools"}),
            None,
            ReplayPolicy::Contextual,
            None,
            None,
        );

        assert_eq!(
            builder
                .events
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            [1, 2, 3]
        );
        assert_eq!(
            builder.events[0..2]
                .iter()
                .map(|event| event.payload["result"].as_u64())
                .collect::<Vec<_>>(),
            [Some(1), Some(2)]
        );
        assert_eq!(
            builder
                .events
                .iter()
                .map(|event| event.event_id)
                .collect::<HashSet<_>>()
                .len(),
            builder.events.len()
        );
    }

    #[test]
    fn tail_reader_returns_latest_records() {
        let temporary = tempdir().expect("temporary directory");
        let path = temporary.path().join("records.jsonl");
        let mut content = String::new();
        for value in 0..10 {
            writeln!(content, "{{\"value\":{value}}}").expect("JSON line");
        }
        std::fs::write(&path, content).expect("JSONL fixture");

        let records = json_lines_tail_with_offsets(&path, 2)
            .expect("tail records")
            .into_iter()
            .map(|(_, record)| record)
            .collect::<Vec<_>>();

        assert_eq!(records[0]["value"], 8);
        assert_eq!(records[1]["value"], 9);
    }

    #[test]
    fn preview_reader_samples_head_and_latest_records() {
        let temporary = tempdir().expect("temporary directory");
        let path = temporary.path().join("large.jsonl");
        let padding = "x".repeat(900);
        let mut content = String::new();
        for value in 0..6_000 {
            writeln!(
                content,
                "{}",
                serde_json::json!({"value": value, "padding": &padding})
            )
            .expect("JSON line");
        }
        std::fs::write(&path, content).expect("large JSONL fixture");

        let records = json_lines_preview(&path, 32).expect("preview records");

        assert!(records.len() <= 64);
        assert_eq!(
            records.first().and_then(|record| record["value"].as_u64()),
            Some(0)
        );
        assert_eq!(
            records.last().and_then(|record| record["value"].as_u64()),
            Some(5_999)
        );
    }

    #[test]
    fn small_preview_skips_oversized_records() {
        let temporary = tempdir().expect("temporary directory");
        let path = temporary.path().join("small.jsonl");
        let oversized = "x"
            .repeat(usize::try_from(MAX_TRANSCRIPT_LINE_SIZE + 1).expect("line limit fits usize"));
        let mut content = String::new();
        writeln!(content, "{}", serde_json::json!({"value": "head"})).expect("JSON line");
        writeln!(content, "{}", serde_json::json!({"oversized": &oversized})).expect("JSON line");
        writeln!(content, "{}", serde_json::json!({"value": "tail"})).expect("JSON line");
        std::fs::write(&path, content).expect("small oversized JSONL fixture");

        let records = json_lines_preview(&path, 32).expect("small preview skips oversized records");

        assert_eq!(
            records
                .iter()
                .filter_map(|record| record["value"].as_str())
                .collect::<Vec<_>>(),
            ["head", "tail"]
        );
    }

    #[test]
    fn preview_reader_skips_oversized_records() {
        let temporary = tempdir().expect("temporary directory");
        let path = temporary.path().join("oversized.jsonl");
        let oversized = "x"
            .repeat(usize::try_from(MAX_TRANSCRIPT_LINE_SIZE + 1).expect("line limit fits usize"));
        let padding = "y".repeat(900);
        let mut content = String::new();
        writeln!(content, "{}", serde_json::json!({"value": "head"})).expect("JSON line");
        writeln!(content, "{}", serde_json::json!({"oversized": &oversized})).expect("JSON line");
        for value in 0..6_000 {
            writeln!(
                content,
                "{}",
                serde_json::json!({"value": value, "padding": &padding})
            )
            .expect("JSON line");
        }
        writeln!(content, "{}", serde_json::json!({"oversized": &oversized})).expect("JSON line");
        writeln!(content, "{}", serde_json::json!({"value": "tail"})).expect("JSON line");
        std::fs::write(&path, content).expect("oversized JSONL fixture");

        let records = json_lines_preview(&path, 32).expect("preview skips oversized records");

        assert_eq!(
            records.first().and_then(|record| record["value"].as_str()),
            Some("head")
        );
        assert_eq!(
            records.last().and_then(|record| record["value"].as_str()),
            Some("tail")
        );
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        #[test]
        fn jsonl_readers_never_panic_on_arbitrary_bytes(
            bytes in proptest::collection::vec(
                prop_oneof![
                    6 => any::<u8>(),
                    1 => Just(b'\n'),
                    1 => Just(b'{'),
                    1 => Just(b'}'),
                    1 => Just(b'"'),
                ],
                0..512,
            ),
            file_bytes in 0_u64..600,
            records in 0_usize..24,
            line_bytes in 0_u64..48,
        ) {
            let (_temporary, path) = write_fixture(&bytes);
            let limits = JsonLinesLimits { file_bytes, records, line_bytes };
            let _ = visit_json_lines_with_limits(&path, limits, |_| Ok(()));
            let scan = visit_index_json_lines_with_limits(&path, file_bytes, line_bytes, |_| {})
                .expect("index scan");
            prop_assert!(scan.skipped_bytes <= bytes.len() as u64);
            let _ = json_lines_prefix(&path, records);
            let _ = json_lines_preview(&path, records);
            let _ = json_lines_tail_with_offsets(&path, records);
        }

        #[test]
        fn index_reader_visits_exactly_the_records_inside_its_budget(
            paddings in proptest::collection::vec(0_usize..24, 0..32),
            file_limit in 0_u64..800,
        ) {
            let mut document = String::new();
            let mut offsets = Vec::new();
            for (index, padding) in paddings.iter().enumerate() {
                offsets.push(document.len() as u64);
                writeln!(document, "{{\"n\":{index},\"p\":\"{}\"}}", "x".repeat(*padding))
                    .expect("JSON line");
            }
            let (_temporary, path) = write_fixture(&document);
            let length = document.len() as u64;
            let start = length.saturating_sub(file_limit);
            let kept = offsets
                .iter()
                .enumerate()
                .filter(|(_, offset)| **offset >= start)
                .map(|(index, _)| index as u64)
                .collect::<Vec<_>>();
            let first_kept = offsets
                .iter()
                .copied()
                .find(|offset| *offset >= start)
                .unwrap_or(length);

            let (values, scan) = index_values(&path, file_limit, 64);

            prop_assert_eq!(values, kept);
            prop_assert_eq!(scan, IndexScan { oversized_records: 0, skipped_bytes: first_kept });
        }
    }
}
