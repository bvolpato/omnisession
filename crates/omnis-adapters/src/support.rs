mod json_tail;

use std::{
    cell::Cell,
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    env,
    ffi::OsStr,
    fs,
    fs::File,
    io::{self, BufRead, BufReader, ErrorKind, Read, Seek, SeekFrom},
    panic::resume_unwind,
    path::{Path, PathBuf},
    sync::{Mutex, PoisonError},
    thread,
    time::SystemTime,
};

use anyhow::{Context, Result, anyhow};
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

pub(crate) const MAX_PROVIDER_RECORDS: usize = 100_000;
pub(crate) const MAX_PREVIEW_EDGE_BYTES: usize = 4 * 1024 * 1024;
pub(crate) const MAX_PREVIEW_TAIL_SIZE: u64 = MAX_PREVIEW_EDGE_BYTES as u64;
/// Maximum bytes in a full transcript read.
pub const MAX_STREAMED_TRANSCRIPT_FILE_SIZE: u64 = 4 * 1024 * 1024 * 1024;
// Readers that keep every record in memory stop at a smaller file size.
pub(crate) const MAX_COLLECTED_TRANSCRIPT_FILE_SIZE: u64 = 512 * 1024 * 1024;
pub(crate) const MAX_STREAMED_TRANSCRIPT_LINE_SIZE: u64 = 16 * 1024 * 1024;
const MAX_PREVIEW_VALIDATION_BYTES: u64 = 2 * MAX_STREAMED_TRANSCRIPT_LINE_SIZE;
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

pub(crate) fn discovery_home(root: Option<&Path>) -> io::Result<Option<PathBuf>> {
    root.map(|path| resolve_discovery_home(&std::path::absolute(path)?, &mut 40))
        .transpose()
}

fn canonical_directory(path: &Path) -> io::Result<PathBuf> {
    let resolved = path.canonicalize()?;
    if !resolved.metadata()?.is_dir() {
        return Err(ErrorKind::NotADirectory.into());
    }
    Ok(resolved)
}

fn resolve_discovery_home(path: &Path, remaining_links: &mut u8) -> io::Result<PathBuf> {
    let mut ancestor = path;
    let mut resolved = loop {
        match canonical_directory(ancestor) {
            Ok(physical) => break physical,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                ancestor = ancestor.parent().ok_or(error)?;
            }
            Err(error) => return Err(error),
        }
    };
    // A missing target can leave a readable symlink anywhere below the last existing ancestor.
    for component in path.strip_prefix(ancestor).unwrap().components() {
        let child = resolved.join(component);
        match canonical_directory(&child) {
            Ok(physical) => resolved = physical,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                if component == std::path::Component::ParentDir {
                    resolved.pop();
                    continue;
                }
                match fs::read_link(&child) {
                    Ok(target) => {
                        *remaining_links = remaining_links.checked_sub(1).ok_or_else(|| {
                            io::Error::other("too many symlinks in provider home")
                        })?;
                        resolved = resolve_discovery_home(&resolved.join(target), remaining_links)?;
                    }
                    Err(error) if error.kind() == ErrorKind::NotFound => resolved = child,
                    Err(error) => return Err(error),
                }
            }
            Err(error) => return Err(error),
        }
    }
    Ok(resolved)
}

pub(crate) fn executable(name: &str) -> Option<PathBuf> {
    let candidate = Path::new(name);
    if candidate.components().count() > 1 {
        return (candidate.is_file() && !is_legacy_omnisession_alias(candidate))
            .then(|| candidate.to_path_buf());
    }

    let paths = env::var_os("PATH")?;
    env::split_paths(&paths)
        .flat_map(|directory| executable_candidates(&directory, name))
        .find(|path| path.is_file() && !is_legacy_omnisession_alias(path))
}

/// Same PATH candidates as the CLI provider resolver.
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
#[cfg(any(test, windows))]
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
/// executable or legacy alias. An invalid override means the provider is not installed; PATH is
/// not searched.
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
    let is_omnisession_executable = current_executable.as_deref() == Some(binary.as_path())
        || is_legacy_omnisession_alias(&binary);
    let names_provider =
        provider != Provider::CursorCli || cursor_agent_binary_name_matches(&binary);
    (!is_omnisession_executable && names_provider).then_some(binary)
}

/// Same variables as the CLI provider resolver.
const fn binary_override(provider: Provider) -> Option<&'static str> {
    match provider {
        Provider::Claude => Some("OMNI_CLAUDE_BIN"),
        Provider::Codex => Some("OMNI_CODEX_BIN"),
        Provider::OpenCode => Some("OMNI_OPENCODE_BIN"),
        Provider::Grok => Some("OMNI_GROK_BIN"),
        Provider::Hermes => Some("OMNI_HERMES_BIN"),
        Provider::Antigravity => Some("OMNI_ANTIGRAVITY_BIN"),
        Provider::Pi => Some("OMNI_PI_BIN"),
        Provider::OhMyPi => Some("OMNI_OMP_BIN"),
        Provider::CursorCli => Some("OMNI_CURSOR_AGENT_BIN"),
        Provider::AntigravityIde
        | Provider::AntigravityDesktop
        | Provider::CursorIde
        | Provider::GenericAcp
        | Provider::Imported => None,
    }
}

/// Same Cursor Agent name check as the CLI provider resolver.
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

pub(crate) fn legacy_alias_directory() -> Option<PathBuf> {
    let state_root = env::var_os("OMNISESSION_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            directories::BaseDirs::new()
                .map(|directories| directories.home_dir().join(".omnisession"))
        });
    state_root.map(|root| root.join("shims"))
}

fn is_legacy_omnisession_alias(candidate: &Path) -> bool {
    legacy_alias_directory().is_some_and(|directory| same_parent(candidate, &directory))
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
    nested_file_inventory(root, depth, include).0
}

pub(crate) fn nested_file_inventory(
    root: &Path,
    depth: usize,
    include: &dyn Fn(&Path, bool) -> bool,
) -> (Vec<PathBuf>, bool) {
    nested_inventory_with_limit(
        root,
        depth,
        MAX_DISCOVERED_FILES,
        MAX_DISCOVERY_ENTRIES,
        include,
    )
}

#[cfg(test)]
fn nested_files_with_limit(
    root: &Path,
    depth: usize,
    file_limit: usize,
    entry_limit: usize,
    include: &dyn Fn(&Path, bool) -> bool,
) -> Vec<PathBuf> {
    nested_inventory_with_limit(root, depth, file_limit, entry_limit, include).0
}

fn nested_inventory_with_limit(
    root: &Path,
    depth: usize,
    file_limit: usize,
    entry_limit: usize,
    include: &dyn Fn(&Path, bool) -> bool,
) -> (Vec<PathBuf>, bool) {
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
        // Listed files are not symlinks, so each one resolves to this directory plus its name.
        // The directory is resolved for the first admitted file and confirmed for each later one.
        let mut resolved = None;
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
                && current_directory(&mut resolved, directory)
                    .is_some_and(|directory| directory.starts_with(canonical_root))
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
    let limited = output.len() == file_limit || entries_left == 0;
    (output, limited)
}

pub(crate) fn provider_file(root: &Path, candidate: &Path) -> Option<PathBuf> {
    if candidate.symlink_metadata().ok()?.file_type().is_symlink() {
        return None;
    }
    let root = fs::canonicalize(root).ok()?;
    let candidate = fs::canonicalize(candidate).ok()?;
    (candidate.is_file() && candidate.starts_with(root)).then_some(candidate)
}

/// A directory path that was resolved once, with the identity the directory had then.
///
/// [`Self::still_names`] costs one `stat` call. It fails when the path now leads to another
/// directory, for example after a rename or behind a new symlink. Platforms without a stable
/// directory identity never confirm, so every file resolves its directory again there.
struct ResolvedDirectory {
    canonical: PathBuf,
    identity: Option<(u64, u64)>,
}

impl ResolvedDirectory {
    fn resolve(directory: &Path) -> Option<Self> {
        let canonical = fs::canonicalize(directory).ok()?;
        let identity = directory_identity(directory);
        // Both paths must name one directory, or it was replaced while it was resolved.
        (directory_identity(&canonical) == identity).then_some(Self {
            canonical,
            identity,
        })
    }

    fn still_names(&self, directory: &Path) -> bool {
        self.identity.is_some() && directory_identity(directory) == self.identity
    }
}

/// Device and inode of the directory that `directory` leads to, through symlinks.
#[cfg(unix)]
fn directory_identity(directory: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;

    let metadata = fs::metadata(directory).ok()?;
    metadata.is_dir().then(|| (metadata.dev(), metadata.ino()))
}

#[cfg(not(unix))]
fn directory_identity(_directory: &Path) -> Option<(u64, u64)> {
    None
}

/// Resolved path of `directory`. `resolved` is reused while `directory` still names the directory
/// that was resolved, and replaced when it does not.
fn current_directory<'a>(
    resolved: &'a mut Option<ResolvedDirectory>,
    directory: &Path,
) -> Option<&'a Path> {
    if !resolved
        .as_ref()
        .is_some_and(|known| known.still_names(directory))
    {
        *resolved = ResolvedDirectory::resolve(directory);
    }
    resolved.as_ref().map(|known| known.canonical.as_path())
}

/// Resolves many files under one provider root, with the same result as [`provider_file`] for
/// each one.
///
/// [`provider_file`] resolves the root and every directory above the file again on each call,
/// which costs several system calls per session during discovery. This resolves the root once
/// and each parent directory once, and then confirms for each file that the parent path still
/// names the same directory. A file that is not a symlink sits at its resolved parent directory
/// plus its own name, so candidates must take their file name from a directory listing.
pub(crate) struct ProviderFiles {
    root: Option<PathBuf>,
    parents: HashMap<PathBuf, Option<ResolvedDirectory>>,
}

impl ProviderFiles {
    pub(crate) fn new(root: &Path) -> Self {
        Self {
            root: fs::canonicalize(root).ok(),
            parents: HashMap::new(),
        }
    }

    pub(crate) fn resolve(&mut self, candidate: &Path) -> Option<PathBuf> {
        let root = self.root.as_deref()?;
        // A symlink is not a regular file here, because the link itself is examined.
        if !candidate.symlink_metadata().ok()?.file_type().is_file() {
            return None;
        }
        let (parent, name) = (candidate.parent()?, candidate.file_name()?);
        if !self.parents.contains_key(parent) {
            self.parents.insert(parent.to_owned(), None);
        }
        let candidate = current_directory(self.parents.get_mut(parent)?, parent)?.join(name);
        candidate.starts_with(root).then_some(candidate)
    }
}

/// Most threads one provider uses to sample session files during discovery.
const MAX_DISCOVERY_WORKERS: usize = 8;
/// Fewest files that justify one more discovery thread.
const MIN_FILES_PER_DISCOVERY_WORKER: usize = 64;

/// Runs `sample` over consecutive slices of `files` on several threads and joins the results in
/// file order.
///
/// Discovery opens every session file. The open and close calls dominate on large stores and
/// do not depend on each other, so they run concurrently.
pub(crate) fn sample_files_concurrently<T: Sync, R: Send>(
    files: &[T],
    sample: impl Fn(&[T]) -> Vec<R> + Sync,
) -> Vec<R> {
    let workers = thread::available_parallelism()
        .map_or(1, usize::from)
        .min(MAX_DISCOVERY_WORKERS)
        .min(files.len() / MIN_FILES_PER_DISCOVERY_WORKER);
    if workers < 2 {
        return sample(files);
    }
    thread::scope(|scope| {
        let handles = files
            .chunks(files.len().div_ceil(workers))
            .map(|slice| scope.spawn(|| sample(slice)))
            .collect::<Vec<_>>();
        handles
            .into_iter()
            .flat_map(|handle| handle.join().unwrap_or_else(|panic| resume_unwind(panic)))
            .collect()
    })
}

/// Whether a provider store is absent. A missing store means "not installed", not a failure.
pub(crate) fn store_is_missing(path: &Path) -> bool {
    path.symlink_metadata()
        .is_err_and(|error| error.kind() == ErrorKind::NotFound)
}

#[derive(Debug)]
pub(crate) struct SqliteSnapshot {
    pub(crate) connection: Connection,
    _directory: TempDir,
}

/// Bounds for the private copy a `SQLite` snapshot makes.
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

#[derive(Debug, Eq, PartialEq)]
pub(super) struct SnapshotFileStamp {
    len: u64,
    pub(super) modified: SystemTime,
    created: Option<SystemTime>,
    unix_identity: Option<(u64, u64, i64, i64)>,
}

impl SnapshotFileStamp {
    pub(crate) fn revision(&self, namespace: Uuid) -> Uuid {
        let modified = DateTime::<Utc>::from(self.modified);
        let mut hash = Sha256::new();
        hash.update(self.len.to_le_bytes());
        hash.update(modified.timestamp().to_le_bytes());
        hash.update(modified.timestamp_subsec_nanos().to_le_bytes());
        if let Some(created) = self.created.map(DateTime::<Utc>::from) {
            hash.update(created.timestamp().to_le_bytes());
            hash.update(created.timestamp_subsec_nanos().to_le_bytes());
        }
        if let Some((device, inode, seconds, nanos)) = self.unix_identity {
            hash.update(device.to_le_bytes());
            hash.update(inode.to_le_bytes());
            hash.update(seconds.to_le_bytes());
            hash.update(nanos.to_le_bytes());
        }
        Uuid::new_v5(&namespace, &hash.finalize())
    }

    pub(crate) fn read(path: &Path) -> io::Result<Self> {
        Self::from_metadata(&path.metadata()?)
    }

    fn from_metadata(metadata: &fs::Metadata) -> io::Result<Self> {
        Ok(Self {
            len: metadata.len(),
            modified: metadata.modified()?,
            created: metadata.created().ok(),
            #[cfg(unix)]
            unix_identity: Some({
                use std::os::unix::fs::MetadataExt;
                (
                    metadata.dev(),
                    metadata.ino(),
                    metadata.ctime(),
                    metadata.ctime_nsec(),
                )
            }),
            #[cfg(not(unix))]
            unix_identity: None,
        })
    }

    fn has_precise_change_time(&self) -> bool {
        // Multiple writes can share a timestamp on a filesystem with millisecond resolution.
        self.unix_identity
            .is_some_and(|(_, inode, _, nanos)| inode != 0 && nanos % 1_000_000 != 0)
    }

    fn copy_unchanged(&self, path: &Path, target: &Path) -> Result<bool> {
        let Some(mut source) = open_observed_source(path)? else {
            return Ok(false);
        };
        if *self != Self::from_metadata(&source.metadata()?)? {
            return Ok(false);
        }
        #[cfg(target_os = "macos")]
        if rustix::fs::fclonefileat(
            &source,
            rustix::fs::CWD,
            target,
            rustix::fs::CloneFlags::empty(),
        )
        .is_ok()
        {
            return Ok(*self == Self::from_metadata(&source.metadata()?)?
                && fs::metadata(target)?.len() == self.len);
        }
        let mut copy = File::create(target)?;
        let bytes = io::copy(&mut (&mut source).take(self.len), &mut copy)?;
        Ok(bytes == self.len && *self == Self::from_metadata(&source.metadata()?)?)
    }
}

/// Physical identity of a selected transcript for search cache validation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TranscriptRevision {
    pub bytes: i64,
    pub modified: DateTime<Utc>,
    pub key: i64,
}

impl TranscriptRevision {
    /// Reads the path, size, write time and platform file identity.
    ///
    /// # Errors
    ///
    /// Returns an error when the file metadata cannot be read.
    pub fn read(path: &Path) -> io::Result<Self> {
        let path = path.canonicalize()?;
        let stamp = SnapshotFileStamp::read(&path)?;
        let modified = DateTime::<Utc>::from(stamp.modified);
        let mut hash = Sha256::new();
        hash.update(path.as_os_str().as_encoded_bytes());
        hash.update(stamp.len.to_le_bytes());
        hash.update(modified.timestamp().to_le_bytes());
        hash.update(modified.timestamp_subsec_nanos().to_le_bytes());
        if let Some((device, inode, seconds, nanos)) = stamp.unix_identity {
            hash.update(device.to_le_bytes());
            hash.update(inode.to_le_bytes());
            hash.update(seconds.to_le_bytes());
            hash.update(nanos.to_le_bytes());
        }
        #[cfg(not(unix))]
        if let Some(created) = stamp.created {
            let created = DateTime::<Utc>::from(created);
            hash.update(created.timestamp().to_le_bytes());
            hash.update(created.timestamp_subsec_nanos().to_le_bytes());
        }
        let digest = hash.finalize();
        let mut prefix = [0; 8];
        prefix.copy_from_slice(&digest[..8]);
        Ok(Self {
            bytes: i64::try_from(stamp.len).map_err(io::Error::other)?,
            modified,
            key: i64::from_le_bytes(prefix),
        })
    }
}

#[derive(Debug, Eq, PartialEq)]
struct SnapshotStamp {
    database: SnapshotFileStamp,
    wal: Option<SnapshotFileStamp>,
}

impl SnapshotStamp {
    fn read(root: &Path, database: &Path, max_bytes: u64, stamped: bool) -> Result<Self> {
        let wal = sidecar(database, "-wal");
        let wal = if stamped {
            anyhow::ensure!(
                provider_file(root, database).as_deref() == Some(database),
                "provider database changed during snapshot"
            );
            if let Some(journal) = optional_regular_file(&sidecar(database, "-journal"))? {
                anyhow::ensure!(
                    journal.len == 0,
                    "provider SQLite rollback journal is active"
                );
            }
            optional_regular_file(&wal)?
        } else {
            provider_file(root, &wal)
                .map(|path| SnapshotFileStamp::read(&path))
                .transpose()?
        };
        let stamp = Self {
            database: SnapshotFileStamp::read(database)?,
            wal,
        };
        let bytes = stamp.bytes();
        if bytes > max_bytes {
            return Err(anyhow!(
                "provider SQLite database and WAL exceed the {max_bytes}-byte snapshot limit"
            ));
        }
        Ok(stamp)
    }

    fn bytes(&self) -> u64 {
        self.database
            .len
            .saturating_add(self.wal.as_ref().map_or(0, |wal| wal.len))
    }

    fn has_precise_change_times(&self) -> bool {
        self.database.has_precise_change_time()
            && self
                .wal
                .as_ref()
                .is_none_or(SnapshotFileStamp::has_precise_change_time)
    }

    fn copy_snapshot(
        &self,
        root: &Path,
        database: &Path,
        limits: SnapshotLimits,
    ) -> Result<Option<SqliteSnapshot>> {
        let directory = private_snapshot_directory()?;
        ensure_snapshot_space(directory.path(), self.bytes(), limits.free_space_reserve)?;
        let copy = directory.path().join("snapshot.sqlite");
        if !self.database.copy_unchanged(database, &copy)? {
            return Ok(None);
        }
        if let Some(wal) = &self.wal
            && !wal.copy_unchanged(&sidecar(database, "-wal"), &sidecar(&copy, "-wal"))?
        {
            return Ok(None);
        }
        if *self != Self::read(root, database, limits.max_bytes, true)? {
            return Ok(None);
        }
        open_snapshot(directory)
    }
}

fn open_snapshot_source(path: &Path) -> io::Result<File> {
    #[cfg(unix)]
    let file = {
        use rustix::fs::{Mode, OFlags, open};
        File::from(open(
            path,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )?)
    };
    #[cfg(windows)]
    let file = {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        fs::OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?
    };
    #[cfg(not(any(unix, windows)))]
    let file = File::open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            "provider SQLite source must be a regular file",
        ));
    }
    Ok(file)
}

/// A file observed in a snapshot stamp can disappear before the copy opens it.
fn open_observed_source(path: &Path) -> io::Result<Option<File>> {
    match open_snapshot_source(path) {
        Ok(file) => Ok(Some(file)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// A WAL reset replaces the header salts. Appending transactions preserves the header.
struct WalPrefix {
    header: [u8; 32],
    len: u64,
}

impl WalPrefix {
    fn capture(path: &Path, len: u64) -> Result<Option<Self>> {
        if len < 32 {
            return Ok(None);
        }
        let Some(file) = open_observed_source(path)? else {
            return Ok(None);
        };
        let mut reader = BufReader::with_capacity(128 * 1024, file);
        let mut header = [0; 32];
        if !read_snapshot_bytes(&mut reader, &mut header)? {
            return Ok(None);
        }
        let magic = u32::from_be_bytes(header[0..4].try_into()?);
        let page_size = u32::from_be_bytes(header[8..12].try_into()?);
        if !matches!(magic, 0x377f_0682 | 0x377f_0683)
            || !(512..=65_536).contains(&page_size)
            || !page_size.is_power_of_two()
        {
            return Ok(None);
        }
        // SQLite can reuse a WAL without truncating it. Frames from the prior generation
        // and an unfinished write after the last commit do not belong to this snapshot.
        let frame_size = 24 + u64::from(page_size);
        let mut offset = 32;
        let mut committed = offset;
        let mut frame = [0; 24];
        while len - offset >= frame_size {
            if !read_snapshot_bytes(&mut reader, &mut frame)?
                || frame[8..16] != header[16..24]
                || frame[..4] == [0; 4]
            {
                break;
            }
            offset += frame_size;
            if frame[4..8] != [0; 4] {
                committed = offset;
            }
            reader.seek_relative(i64::from(page_size))?;
        }
        Ok(Some(Self {
            header,
            len: committed,
        }))
    }

    fn still_matches(&self, path: &Path) -> Result<bool> {
        let mut header = [0; 32];
        let Some(mut file) = open_observed_source(path)? else {
            return Ok(false);
        };
        Ok(read_snapshot_bytes(&mut file, &mut header)? && header == self.header)
    }
}

fn read_snapshot_bytes(reader: &mut impl Read, buffer: &mut [u8]) -> io::Result<bool> {
    match reader.read_exact(buffer) {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == ErrorKind::UnexpectedEof => Ok(false),
        Err(err) => Err(err),
    }
}

fn prefix_matches(source: &Path, copy: &Path, len: u64) -> Result<bool> {
    let Some(mut source) = open_observed_source(source)? else {
        return Ok(false);
    };
    let mut copy = File::open(copy)?;
    let mut left = vec![0_u8; 64 * 1024];
    let mut right = vec![0_u8; left.len()];
    let mut remaining = len;
    while remaining > 0 {
        let count = usize::try_from(remaining.min(left.len() as u64))?;
        if let Err(err) = source.read_exact(&mut left[..count]) {
            if err.kind() == ErrorKind::UnexpectedEof {
                return Ok(false);
            }
            return Err(err.into());
        }
        copy.read_exact(&mut right[..count])?;
        if left[..count] != right[..count] {
            return Ok(false);
        }
        remaining -= count as u64;
    }
    Ok(true)
}

impl SnapshotStamp {
    fn copy_live_wal(
        &self,
        root: &Path,
        database: &Path,
        limits: SnapshotLimits,
        prefix: &WalPrefix,
    ) -> Result<Option<SqliteSnapshot>> {
        let directory = private_snapshot_directory()?;
        ensure_snapshot_space(directory.path(), self.bytes(), limits.free_space_reserve)?;
        let copy = directory.path().join("snapshot.sqlite");
        let wal = sidecar(database, "-wal");
        let copy_wal = sidecar(&copy, "-wal");
        if !self.database.copy_unchanged(database, &copy)? || !prefix.still_matches(&wal)? {
            return Ok(None);
        }
        let Some(file) = open_observed_source(&wal)? else {
            return Ok(None);
        };
        let copied = io::copy(&mut file.take(prefix.len), &mut File::create(&copy_wal)?)?;
        if copied != prefix.len
            || !prefix_matches(&wal, &copy_wal, prefix.len)?
            || !prefix.still_matches(&wal)?
        {
            return Ok(None);
        }
        let latest = Self::read(root, database, limits.max_bytes, true)?;
        if latest.database != self.database || latest.wal.is_none_or(|wal| wal.len < prefix.len) {
            return Ok(None);
        }
        if !self.database.has_precise_change_time()
            && !prefix_matches(database, &copy, self.database.len)?
        {
            return Ok(None);
        }
        // SQLite recovers complete committed frames in the private WAL. An unfinished tail is ignored.
        open_snapshot(directory)
    }
}

fn optional_regular_file(path: &Path) -> Result<Option<SnapshotFileStamp>> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_file() => Ok(Some(SnapshotFileStamp::from_metadata(&meta)?)),
        Ok(_) => Err(anyhow!("provider SQLite sidecar must be a regular file")),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Identifies a cacheable source database and WAL without opening either file.
#[derive(Debug, Default, Eq, PartialEq)]
pub(crate) struct SqliteSourceStamp(Option<SnapshotStamp>);

impl SqliteSourceStamp {
    pub(crate) fn read(root: &Path, database: &Path) -> Option<Self> {
        match fs::symlink_metadata(database) {
            Ok(meta) if meta.is_file() && cfg!(unix) => {
                let database = provider_file(root, database)?;
                let limits = SnapshotLimits::from_environment().ok()?;
                let stamp = SnapshotStamp::read(root, &database, limits.max_bytes, true).ok()?;
                stamp
                    .has_precise_change_times()
                    .then_some(Self(Some(stamp)))
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {
                let wal = optional_regular_file(&sidecar(database, "-wal")).ok()?;
                let journal = optional_regular_file(&sidecar(database, "-journal")).ok()?;
                (wal.is_none() && journal.is_none_or(|file| file.len == 0)).then_some(Self(None))
            }
            _ => None,
        }
    }
}

#[derive(Debug)]
struct CachedSnapshot {
    database: PathBuf,
    stamp: SnapshotStamp,
    stamped: bool,
    snapshot: SqliteSnapshot,
}

/// Reuses one private snapshot while the database and WAL file identities stay unchanged.
#[derive(Debug, Default)]
pub(crate) struct SqliteSnapshotCache(Mutex<Option<CachedSnapshot>>);

impl SqliteSnapshotCache {
    pub(crate) fn read<T>(
        &self,
        root: &Path,
        database: &Path,
        read: impl FnOnce(&Connection) -> Result<T>,
    ) -> Result<T> {
        self.read_using(root, database, false, read)
    }

    /// Reads a stable database and committed WAL prefix. Unchanged precise Unix stamps permit reuse.
    pub(crate) fn read_stamped<T>(
        &self,
        root: &Path,
        database: &Path,
        read: impl FnOnce(&Connection) -> Result<T>,
    ) -> Result<T> {
        self.read_using(root, database, true, read)
    }

    fn read_using<T>(
        &self,
        root: &Path,
        database: &Path,
        stamped: bool,
        read: impl FnOnce(&Connection) -> Result<T>,
    ) -> Result<T> {
        let database = provider_file(root, database)
            .ok_or_else(|| anyhow!("provider database is outside its allowed root"))?;
        let limits = SnapshotLimits::from_environment()?;
        // Unix change time detects same-size rewrites that preserve modification time.
        if !cfg!(unix) && !stamped {
            return read(&sqlite_snapshot_with_limits(root, &database, limits)?.connection);
        }
        let mut cached = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        for _ in 0..3 {
            let stamp = SnapshotStamp::read(root, &database, limits.max_bytes, stamped)?;
            let cacheable = cfg!(unix) && (!stamped || stamp.has_precise_change_times());
            if cacheable
                && let Some(snapshot) = cached.as_ref().filter(|cached| {
                    cached.database == database
                        && cached.stamp == stamp
                        && cached.stamped == stamped
                })
            {
                return read(&snapshot.snapshot.connection);
            }
            *cached = None;
            let live_prefix = if stamped && let Some(wal) = &stamp.wal {
                let path = sidecar(&database, "-wal");
                let prefix = WalPrefix::capture(&path, wal.len)?;
                if prefix.is_none() && optional_regular_file(&path)?.as_ref() != Some(wal) {
                    continue;
                }
                prefix
            } else {
                None
            };
            let snapshot = if let Some(prefix) = &live_prefix {
                let Some(snapshot) = stamp.copy_live_wal(root, &database, limits, prefix)? else {
                    continue;
                };
                snapshot
            } else if cacheable && stamped {
                let Some(snapshot) = stamp.copy_snapshot(root, &database, limits)? else {
                    continue;
                };
                snapshot
            } else {
                sqlite_snapshot_with_limits(root, &database, limits)?
            };
            if stamp != SnapshotStamp::read(root, &database, limits.max_bytes, stamped)? {
                if live_prefix.is_some() {
                    // A later lookup must observe commits appended after this private snapshot.
                    return read(&snapshot.connection);
                }
                continue;
            }
            if !cacheable {
                return read(&snapshot.connection);
            }
            let result = read(&snapshot.connection);
            *cached = Some(CachedSnapshot {
                database,
                stamp,
                stamped,
                snapshot,
            });
            return result;
        }
        Err(anyhow!("provider database changed during snapshot"))
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
/// `SQLite` never opens the provider files, so locks and `-shm` activity stay in the copy. Reads
/// stream through fixed buffers, so memory does not grow with store size. Disk use is bounded by
/// `OMNI_SNAPSHOT_MAX_BYTES`, and the copy must leave a free-space reserve on the temporary volume.
pub(crate) fn sqlite_snapshot(root: &Path, database: &Path) -> Result<SqliteSnapshot> {
    sqlite_snapshot_with_limits(root, database, SnapshotLimits::from_environment()?)
}

/// Applies a provider's tighter snapshot budget while honoring a smaller user-configured limit.
pub(crate) fn sqlite_snapshot_bounded(
    root: &Path,
    database: &Path,
    max_bytes: u64,
) -> Result<SqliteSnapshot> {
    let mut limits = SnapshotLimits::from_environment()?;
    limits.max_bytes = limits.max_bytes.min(max_bytes);
    sqlite_snapshot_with_limits(root, database, limits)
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
        let directory = private_snapshot_directory()?;
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
        if let Some(snapshot) = open_snapshot(directory)? {
            return Ok(snapshot);
        }
    }
    Err(anyhow!("provider database changed during snapshot"))
}

fn private_snapshot_directory() -> io::Result<TempDir> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tempfile::Builder::new()
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir()
    }
    #[cfg(not(unix))]
    tempfile::tempdir()
}

fn open_snapshot(directory: TempDir) -> Result<Option<SqliteSnapshot>> {
    let Ok(connection) = Connection::open_with_flags(
        directory.path().join("snapshot.sqlite"),
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) else {
        return Ok(None);
    };
    let integrity = connection
        .query_row("PRAGMA quick_check(1)", [], |row| row.get::<_, String>(0))
        .unwrap_or_default();
    if integrity != "ok" {
        return Ok(None);
    }
    connection.pragma_update(None, "query_only", "ON")?;
    Ok(Some(SqliteSnapshot {
        connection,
        _directory: directory,
    }))
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

pub(crate) fn sidecar(database: &Path, suffix: &str) -> PathBuf {
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
    let mut source = File::open(source)?;
    let mut target = File::create(target)?;
    let copied = io::copy(&mut (&mut source).take(max_bytes), &mut target)?;
    let mut probe = [0_u8; 1];
    if source.read(&mut probe)? != 0 {
        return Ok(None);
    }
    target.sync_all()?;
    Ok(Some(copied))
}

/// A transcript read could not establish its result within the provider reader's limits.
#[derive(Debug)]
pub struct TranscriptReadLimit(pub(crate) &'static str);

impl std::fmt::Display for TranscriptReadLimit {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.0)
    }
}

impl std::error::Error for TranscriptReadLimit {}

/// A provider transcript ended before a complete record was available.
#[derive(Debug)]
pub struct IncompleteTranscript;

impl std::fmt::Display for IncompleteTranscript {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("provider transcript is incomplete")
    }
}

impl std::error::Error for IncompleteTranscript {}

#[derive(Debug, Default)]
pub(crate) struct JsonLineDamage {
    skipped: Cell<bool>,
    prefix_end: Cell<u64>,
    interior: Cell<usize>,
    last_offset: Cell<Option<u64>>,
    final_record: Cell<bool>,
}

impl JsonLineDamage {
    pub(crate) fn confirm_prefix(&self, end: u64) {
        if self.verified_prefix_end().is_none() {
            self.prefix_end.set(end);
        }
    }

    pub(crate) fn verified_prefix_end(&self) -> Option<u64> {
        (self.count() > 0 || self.skipped.get()).then(|| self.prefix_end.get())
    }

    pub(crate) fn count(&self) -> usize {
        self.interior.get() + usize::from(self.final_record.get())
    }

    pub(crate) fn interior_count(&self) -> usize {
        self.interior.get()
    }

    pub(crate) fn final_record(&self) -> bool {
        self.final_record.get()
    }

    fn record_interior(&self, offset: u64) {
        // The prefix and suffix can visit the same malformed line twice.
        if self
            .last_offset
            .get()
            .is_none_or(|previous| offset > previous)
        {
            self.last_offset.set(Some(offset));
            self.interior.set(self.interior.get() + 1);
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum JsonLineTail<'a> {
    SkipMalformed,
    RequireComplete,
    ReportIncomplete(&'a JsonLineDamage),
    RequireCompleteTail(&'a JsonLineDamage),
}

impl<'a> JsonLineTail<'a> {
    fn record_end(self, reader: &mut (impl Read + Seek), length: u64, budget: u64) -> Result<u64> {
        if matches!(self, Self::SkipMalformed) {
            return Ok(length);
        }
        let mut position = length;
        let mut line_end = length;
        let lower_bound = length.saturating_sub(budget);
        let mut buffer = [0; 8192];
        while position > lower_bound {
            let start = position
                .saturating_sub(buffer.len() as u64)
                .max(lower_bound);
            let chunk = &mut buffer[..usize::try_from(position - start)?];
            reader.seek(SeekFrom::Start(start))?;
            reader.read_exact(chunk)?;
            for (index, &byte) in chunk.iter().enumerate().rev() {
                if !byte.is_ascii_whitespace() {
                    return Ok(line_end);
                }
                if byte == b'\n' {
                    line_end = start + index as u64 + 1;
                }
            }
            position = start;
        }
        if lower_bound > 0 {
            self.incomplete()?;
            return Ok(length);
        }
        Ok(0)
    }

    fn incomplete(self) -> Result<()> {
        match self {
            Self::ReportIncomplete(damage) => damage.final_record.set(true),
            Self::RequireCompleteTail(damage) if damage.interior_count() > 0 => {
                damage.final_record.set(true);
            }
            _ => return Err(IncompleteTranscript.into()),
        }
        Ok(())
    }

    fn parse(self, line: &[u8], offset: u64, at_end: bool) -> Result<Option<Value>> {
        match serde_json::from_slice(line) {
            Ok(value) => Ok(Some(value)),
            Err(_) if self.checked_end(at_end) && !line.iter().all(u8::is_ascii_whitespace) => {
                self.incomplete().map(|()| None)
            }
            Err(_) if !line.iter().all(u8::is_ascii_whitespace) => match self {
                Self::RequireComplete => Err(IncompleteTranscript.into()),
                Self::RequireCompleteTail(damage) | Self::ReportIncomplete(damage) => {
                    damage.record_interior(offset);
                    Ok(None)
                }
                Self::SkipMalformed => Ok(None),
            },
            Err(_) => Ok(None),
        }
    }

    pub(crate) fn damage(self) -> Option<&'a JsonLineDamage> {
        match self {
            Self::RequireCompleteTail(damage) | Self::ReportIncomplete(damage) => Some(damage),
            _ => None,
        }
    }

    fn checked_end(self, at_end: bool) -> bool {
        at_end && !matches!(self, Self::SkipMalformed)
    }

    fn check_skipped_end(
        self,
        reader: &mut (impl Read + Seek),
        at_end: bool,
        bytes: u64,
        budget: u64,
    ) -> Result<bool> {
        if self.checked_end(at_end) {
            if bytes > budget {
                self.incomplete()?;
                return Ok(false);
            }
            let end = reader.stream_position()?;
            reader.seek(SeekFrom::Start(end - bytes))?;
            let valid = json_tail::valid_record(reader.take(bytes))?;
            reader.seek(SeekFrom::Start(end))?;
            if !valid {
                self.incomplete()?;
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn check_final_fragment(self, file: &mut File, end: u64, budget: u64) -> Result<()> {
        let mut position = end.saturating_sub(1);
        let lower = end.saturating_sub(budget);
        let mut buffer = [0; 8192];
        loop {
            let start = position.saturating_sub(buffer.len() as u64).max(lower);
            file.seek(SeekFrom::Start(start))?;
            let chunk = &mut buffer[..usize::try_from(position - start)?];
            file.read_exact(chunk)?;
            if let Some(index) = memchr::memrchr(b'\n', chunk) {
                position = start + index as u64 + 1;
                break;
            }
            position = start;
            if start == lower {
                if lower != 0 {
                    file.seek(SeekFrom::Start(lower - 1))?;
                    let mut previous = [0];
                    file.read_exact(&mut previous)?;
                    if previous[0] != b'\n' {
                        return self.incomplete();
                    }
                }
                break;
            }
        }
        file.seek(SeekFrom::Start(end))?;
        self.check_skipped_end(file, true, end - position, budget)?;
        Ok(())
    }
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
    visit_json_lines_with_tail(path, file_limit, JsonLineTail::SkipMalformed, visit)
}

pub(crate) fn visit_json_lines_with_tail(
    path: &Path,
    file_limit: u64,
    tail: JsonLineTail<'_>,
    visit: impl FnMut(Value) -> Result<()>,
) -> Result<usize> {
    visit_json_lines_with_limits(path, JsonLinesLimits::streamed(file_limit), tail, visit)
}

/// Byte location of one physical line in a bounded JSONL transcript.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct JsonLineLocation {
    pub(crate) offset: u64,
    pub(crate) length: u64,
    /// Whether the buffered bytes include the terminator. An oversized line can expose only its prefix.
    pub(crate) terminated: bool,
}

/// Visits records from an already-open transcript and reports their source locations.
///
/// The caller can seek within the same file handle to reread only selected records after it has
/// collected compact index metadata. The regular file, file-size, record-count, and line-size
/// limits match `visit_json_lines`.
pub(crate) fn visit_json_lines_with_offsets(
    reader: &mut BufReader<File>,
    file_limit: u64,
    mut visit: impl FnMut(JsonLineLocation, Value) -> Result<()>,
) -> Result<usize> {
    visit_json_lines_with_offsets_and_limits(
        reader,
        JsonLinesLimits::streamed(file_limit),
        JsonLineTail::SkipMalformed,
        |loc, record, _| visit(loc, record),
    )
    .map(|skips| skips.oversized)
}

/// Reports nonblank lines and their source positions. The caller bounds the state it retains.
pub(crate) fn visit_streamed_json_records_with_offsets(
    reader: &mut BufReader<File>,
    file_limit: u64,
    mut visit: impl FnMut(JsonLineLocation, JsonlRecord) -> Result<()>,
) -> Result<usize> {
    let limits = JsonLinesLimits {
        records: usize::MAX,
        ..JsonLinesLimits::streamed(file_limit)
    };
    let metadata = reader.get_ref().metadata()?;
    if !metadata.is_file() {
        return Err(anyhow!("provider path is not a file"));
    }
    if metadata.len() > file_limit {
        return Err(TranscriptReadLimit("provider file exceeds safe streaming limit").into());
    }
    reader.rewind()?;
    let mut oversized = 0;
    let consumed = visit_bounded_lines(reader, limits, |location, line| {
        if let Some(record) = line.into_record() {
            visit(location, record)?;
        } else {
            oversized += 1;
        }
        Ok(())
    })?;
    if consumed < metadata.len() {
        return Err(io::Error::new(
            ErrorKind::UnexpectedEof,
            "provider transcript changed during the read",
        )
        .into());
    }
    Ok(oversized)
}

/// Rereads one previously visited record from the same open transcript handle.
pub(crate) fn read_json_line_at(
    reader: &mut BufReader<File>,
    location: JsonLineLocation,
    current_position: &mut u64,
    line: &mut Vec<u8>,
) -> Result<Value> {
    if location.length > MAX_STREAMED_TRANSCRIPT_LINE_SIZE {
        return Err(anyhow!("provider record exceeds safe line limit"));
    }
    let length = usize::try_from(location.length)
        .map_err(|_| anyhow!("provider record length exceeds addressable memory"))?;
    let relative = i64::try_from(i128::from(location.offset) - i128::from(*current_position))
        .map_err(|_| anyhow!("provider record offset exceeds seek range"))?;
    reader.seek_relative(relative)?;
    line.clear();
    line.resize(length, 0);
    reader.read_exact(line)?;
    *current_position = location
        .offset
        .checked_add(location.length)
        .ok_or_else(|| anyhow!("provider file offset exceeds supported range"))?;
    serde_json::from_slice(line).context("provider record changed while rereading transcript")
}

pub(crate) enum JsonLine<'a> {
    Record(Value),
    Unterminated(Value),
    Malformed(&'a [u8]),
    Oversized(&'a [u8], JsonLineLocation),
}

/// A decoded nonblank line. Malformed lines carry no source bytes.
pub(crate) enum JsonlRecord {
    Complete(Value),
    Unterminated(Value),
    Malformed,
}

impl<'a> JsonLine<'a> {
    fn parse(bytes: &'a [u8]) -> Option<Self> {
        match serde_json::from_slice(bytes) {
            Ok(value) if bytes.ends_with(b"\n") => Some(Self::Record(value)),
            Ok(value) => Some(Self::Unterminated(value)),
            Err(_) if bytes.iter().all(u8::is_ascii_whitespace) => None,
            Err(_) => Some(Self::Malformed(bytes)),
        }
    }

    pub(crate) fn into_record(self) -> Option<JsonlRecord> {
        match self {
            Self::Record(value) => Some(JsonlRecord::Complete(value)),
            Self::Unterminated(value) => Some(JsonlRecord::Unterminated(value)),
            Self::Malformed(_) => Some(JsonlRecord::Malformed),
            Self::Oversized(..) => None,
        }
    }
}

/// Folds records from an open file so callers can verify the identity of the source they read.
pub(crate) fn visit_streamed_json_file(
    file: File,
    file_limit: u64,
    tail: JsonLineTail<'_>,
    mut visit: impl FnMut(Value) -> Result<()>,
) -> Result<usize> {
    visit_json_lines_with_offsets_and_limits(
        &mut BufReader::new(file),
        JsonLinesLimits {
            records: usize::MAX,
            ..JsonLinesLimits::streamed(file_limit)
        },
        tail,
        |_, record, _| visit(record),
    )
    .map(|skips| skips.oversized)
}

/// Reads up to the previous searchable prefix and the newest complete records inside the file budget.
pub(crate) fn visit_limited_search_edges(
    mut file: File,
    previous_text_bytes: Option<usize>,
    sample_records: usize,
    tail: JsonLineTail<'_>,
    mut visit: impl FnMut(Value) -> usize,
) -> Result<()> {
    let physical_length = file.metadata()?.len();
    let length = tail.record_end(&mut file, physical_length, MAX_PREVIEW_TAIL_SIZE)?;
    if length == 0 {
        return Ok(());
    }
    file.rewind()?;
    let (prefix_limit, record_limit) = if previous_text_bytes.is_some() {
        (
            MAX_STREAMED_TRANSCRIPT_FILE_SIZE - MAX_PREVIEW_TAIL_SIZE - 1,
            MAX_PROVIDER_RECORDS,
        )
    } else {
        (MAX_PREVIEW_TAIL_SIZE, sample_records)
    };
    let mut prefix = BufReader::new((&mut file).take(prefix_limit.min(length)));
    let mut consumed = 0u64;
    let mut line = Vec::new();
    for _ in 0..record_limit {
        let Some((kind, bytes)) =
            read_bounded_line(&mut prefix, MAX_STREAMED_TRANSCRIPT_LINE_SIZE, &mut line)?
        else {
            break;
        };
        consumed += bytes;
        if (!matches!(kind, BoundedLine::Complete) || !line.ends_with(b"\n"))
            && let Some(damage) = tail.damage()
        {
            damage.skipped.set(true);
        }
        if matches!(kind, BoundedLine::Complete)
            && line.ends_with(b"\n")
            && let Some(record) = tail.parse(&line, consumed, false)?
        {
            let observed = visit(record);
            if previous_text_bytes.is_some_and(|bytes| observed >= bytes) {
                break;
            }
        }
    }

    let start = length.saturating_sub(MAX_PREVIEW_TAIL_SIZE).max(consumed);
    let aligned = start.saturating_sub(1);
    file.seek(SeekFrom::Start(aligned))?;
    let mut suffix = BufReader::new(file.take(length - aligned));
    let mut offset = aligned + suffix.skip_until(b'\n')? as u64;
    if offset > consumed
        && let Some(damage) = tail.damage()
    {
        damage.skipped.set(true);
    }
    if offset == length {
        let mut file = suffix.into_inner().into_inner();
        return tail.check_final_fragment(&mut file, length, MAX_PREVIEW_TAIL_SIZE);
    }
    while let Some((kind, bytes)) =
        read_bounded_line(&mut suffix, MAX_STREAMED_TRANSCRIPT_LINE_SIZE, &mut line)?
    {
        offset += bytes;
        if matches!(kind, BoundedLine::Oversized)
            && let Some(damage) = tail.damage()
        {
            damage.skipped.set(true);
        }
        if matches!(kind, BoundedLine::Complete)
            && let Some(record) = tail.parse(&line, offset, offset == length)?
        {
            visit(record);
        }
    }
    Ok(())
}

/// Visible text for a user turn made only of images, so the turn stays in history.
pub(crate) fn omitted_images_text(count: usize) -> String {
    if count == 1 {
        "[1 image omitted]".to_owned()
    } else {
        format!("[{count} images omitted]")
    }
}

pub(crate) fn visit_json_lines_with_omissions(
    path: &Path,
    file_limit: u64,
    mut visit: impl FnMut(Value, bool) -> Result<()>,
) -> Result<JsonLineSkips> {
    let mut previous = 0;
    let file = File::open(path)?;
    visit_json_lines_with_offsets_and_limits(
        &mut BufReader::new(file),
        JsonLinesLimits::streamed(file_limit),
        JsonLineTail::SkipMalformed,
        |_, record, skips| {
            let gap = skips.total() > previous;
            previous = skips.total();
            visit(record, gap)
        },
    )
}

fn visit_json_lines_with_limits(
    path: &Path,
    limits: JsonLinesLimits,
    tail: JsonLineTail<'_>,
    mut visit: impl FnMut(Value) -> Result<()>,
) -> Result<usize> {
    let file = File::open(path)?;
    visit_json_lines_with_offsets_and_limits(
        &mut BufReader::new(file),
        limits,
        tail,
        |_, record, _| visit(record),
    )
    .map(|skips| skips.oversized)
}

fn visit_json_lines_with_offsets_and_limits(
    reader: &mut BufReader<File>,
    limits: JsonLinesLimits,
    tail: JsonLineTail<'_>,
    mut visit: impl FnMut(JsonLineLocation, Value, JsonLineSkips) -> Result<()>,
) -> Result<JsonLineSkips> {
    let metadata = reader.get_ref().metadata()?;
    if !metadata.is_file() {
        return Err(anyhow!("provider path is not a file"));
    }
    if metadata.len() > limits.file_bytes {
        return Err(TranscriptReadLimit("provider file exceeds safe streaming limit").into());
    }
    let end = tail.record_end(reader, metadata.len(), limits.file_bytes)?;
    reader.seek(SeekFrom::Start(0))?;
    let mut line = Vec::new();
    let mut records = 0_usize;
    let mut omissions = JsonLineSkips::default();
    let mut offset = 0_u64;
    loop {
        if end < metadata.len() && offset == end {
            break;
        }
        if records >= limits.records {
            if !reader.fill_buf()?.is_empty() {
                return Err(TranscriptReadLimit("provider file exceeds safe record limit").into());
            }
            break;
        }
        let line_offset = offset;
        let remaining_file_bytes = limits.file_bytes.saturating_sub(offset);
        let Some((line_kind, read)) = read_bounded_line_with_budget(
            reader,
            limits.line_bytes,
            remaining_file_bytes,
            &mut line,
        )?
        else {
            break;
        };
        records += 1;
        offset = offset
            .checked_add(read)
            .ok_or_else(|| anyhow!("provider file offset exceeds supported range"))?;
        match line_kind {
            BoundedLine::Oversized => {
                if let Some(damage) = tail.damage() {
                    damage.skipped.set(true);
                }
                if tail.check_skipped_end(reader, offset == end, read, limits.file_bytes)? {
                    omissions.observe(WorkspaceGap::Other, true);
                }
            }
            BoundedLine::Complete => {
                if let Some(record) = tail.parse(&line, offset, offset == end)? {
                    visit(
                        JsonLineLocation {
                            offset: line_offset,
                            length: read,
                            terminated: line.ends_with(b"\n"),
                        },
                        record,
                        omissions,
                    )?;
                } else if !line.iter().all(u8::is_ascii_whitespace) {
                    omissions.observe(WorkspaceGap::Other, false);
                }
            }
        }
    }
    if offset < end {
        return Err(io::Error::new(
            ErrorKind::UnexpectedEof,
            "provider transcript changed during the read",
        )
        .into());
    }
    Ok(omissions)
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum WorkspaceGap {
    Other,
    TurnOrRollback,
    PossibleSettings,
    ForeignSettings,
    Unclassified,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct JsonLineSkips {
    pub(crate) oversized: usize,
    pub(crate) malformed: usize,
    unplaced: usize,
    workspace_gaps: usize,
    pub(crate) possible_settings: usize,
    pub(crate) foreign_settings: usize,
    pub(crate) unclassified: usize,
}

impl JsonLineSkips {
    pub(crate) fn total(&self) -> usize {
        self.oversized + self.malformed
    }

    pub(crate) fn workspace_gaps(self) -> usize {
        self.workspace_gaps
    }

    pub(crate) fn combined(self, other: Self) -> Self {
        Self {
            oversized: self.oversized + other.oversized,
            malformed: self.malformed + other.malformed,
            unplaced: self.unplaced + other.unplaced,
            workspace_gaps: self.workspace_gaps + other.workspace_gaps,
            possible_settings: self.possible_settings + other.possible_settings,
            foreign_settings: self.foreign_settings + other.foreign_settings,
            unclassified: self.unclassified + other.unclassified,
        }
    }

    pub(crate) fn observe_unplaced(&mut self, gap: WorkspaceGap) {
        self.observe(gap, false);
        self.unplaced += 1;
    }

    pub(crate) fn observe(&mut self, gap: WorkspaceGap, oversized: bool) {
        self.oversized += usize::from(oversized);
        self.malformed += usize::from(!oversized);
        self.workspace_gaps += usize::from(
            (oversized || gap != WorkspaceGap::Other) && gap != WorkspaceGap::ForeignSettings,
        );
        match gap {
            WorkspaceGap::PossibleSettings => self.possible_settings += 1,
            WorkspaceGap::ForeignSettings => self.foreign_settings += 1,
            WorkspaceGap::Unclassified => self.unclassified += 1,
            WorkspaceGap::TurnOrRollback | WorkspaceGap::Other => {}
        }
    }

    fn parse(
        &mut self,
        line: &[u8],
        classify: &impl Fn(&[u8], bool) -> WorkspaceGap,
    ) -> Option<Value> {
        let record = serde_json::from_slice(line).ok();
        if record.is_none() && !line.iter().all(u8::is_ascii_whitespace) {
            self.observe(classify(line, false), false);
        }
        record
    }
}

/// Reports unreadable records and decoded bytes consumed, including blank lines.
pub(crate) fn visit_json_reader(
    reader: &mut impl BufRead,
    limits: JsonLinesLimits,
    match_malformed: impl Fn(&[u8], bool) -> WorkspaceGap,
    visit: impl FnMut(JsonLineLocation, Value, JsonLineSkips) -> Result<()>,
) -> Result<(JsonLineSkips, u64)> {
    visit_json_reader_with_locations(
        reader,
        limits,
        |_, line, oversized| Ok(match_malformed(line, oversized)),
        visit,
    )
}

/// Supplies the physical source range when an unreadable record needs further inspection.
pub(crate) fn visit_json_reader_with_locations(
    reader: &mut impl BufRead,
    limits: JsonLinesLimits,
    match_malformed: impl Fn(JsonLineLocation, &[u8], bool) -> Result<WorkspaceGap>,
    mut visit: impl FnMut(JsonLineLocation, Value, JsonLineSkips) -> Result<()>,
) -> Result<(JsonLineSkips, u64)> {
    let mut omissions = JsonLineSkips::default();
    let consumed = visit_bounded_lines(reader, limits, |location, line| {
        match line {
            JsonLine::Record(value) | JsonLine::Unterminated(value) => {
                visit(location, value, omissions)?;
            }
            JsonLine::Malformed(bytes) => {
                omissions.observe(match_malformed(location, bytes, false)?, false);
            }
            JsonLine::Oversized(bytes, _) => {
                omissions.observe(match_malformed(location, bytes, true)?, true);
            }
        }
        Ok(())
    })?;
    Ok((omissions, consumed))
}

fn visit_bounded_lines(
    reader: &mut impl BufRead,
    limits: JsonLinesLimits,
    mut visit: impl FnMut(JsonLineLocation, JsonLine<'_>) -> Result<()>,
) -> Result<u64> {
    let mut line = Vec::new();
    let mut records = 0_usize;
    let mut offset = 0_u64;
    loop {
        if records >= limits.records {
            if !reader.fill_buf()?.is_empty() {
                return Err(TranscriptReadLimit("provider file exceeds safe record limit").into());
            }
            return Ok(offset);
        }
        let line_offset = offset;
        let remaining_file_bytes = limits.file_bytes.saturating_sub(offset);
        let Some((line_kind, read)) = read_bounded_line_with_budget(
            reader,
            limits.line_bytes,
            remaining_file_bytes,
            &mut line,
        )?
        else {
            return Ok(offset);
        };
        records += 1;
        offset = offset
            .checked_add(read)
            .ok_or_else(|| anyhow!("provider file offset exceeds supported range"))?;
        let location = JsonLineLocation {
            offset: line_offset,
            length: read,
            terminated: line.ends_with(b"\n"),
        };
        match line_kind {
            BoundedLine::Oversized => visit(location, JsonLine::Oversized(&line, location))?,
            BoundedLine::Complete => {
                if let Some(record) = JsonLine::parse(&line) {
                    visit(location, record)?;
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
    /// Nonblank lines that did not contain valid JSON.
    pub(crate) malformed_records: usize,
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
pub(crate) fn visit_index_json_lines(
    path: &Path,
    mut visit: impl FnMut(Value),
) -> Result<IndexScan> {
    visit_index_json_lines_with_gaps(path, |record| {
        if let Some(record) = record {
            visit(record);
        }
    })
}

/// Reports an unreadable nonblank row as `None` at its position in the file.
pub(crate) fn visit_index_json_lines_with_gaps(
    path: &Path,
    visit: impl FnMut(Option<Value>),
) -> Result<IndexScan> {
    visit_index_json_lines_with_limits(path, MAX_INDEX_FILE_SIZE, MAX_TRANSCRIPT_LINE_SIZE, visit)
}

fn visit_index_json_lines_with_limits(
    path: &Path,
    file_limit: u64,
    line_limit: u64,
    mut visit: impl FnMut(Option<Value>),
) -> Result<IndexScan> {
    let mut file = File::open(path)?;
    let (mut reader, skipped_bytes) = jsonl_tail_reader(&mut file, file_limit)?;
    let mut scan = IndexScan {
        skipped_bytes,
        ..IndexScan::default()
    };
    let mut line = Vec::new();
    while let Some((line_kind, _)) = read_bounded_line(&mut reader, line_limit, &mut line)? {
        match line_kind {
            BoundedLine::Oversized => {
                scan.oversized_records += 1;
                visit(None);
            }
            BoundedLine::Complete => match serde_json::from_slice(&line) {
                Ok(record) => visit(Some(record)),
                Err(_) if !line.iter().all(u8::is_ascii_whitespace) => {
                    scan.malformed_records += 1;
                    visit(None);
                }
                Err(_) => {}
            },
        }
    }
    Ok(scan)
}

pub(crate) enum BoundedLine {
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
    read_bounded_line_with_budget(reader, line_limit, u64::MAX, line)
}

/// Reads one bounded line without consuming bytes beyond the transcript file budget.
pub(crate) fn read_bounded_line_with_budget(
    reader: &mut impl BufRead,
    line_limit: u64,
    file_budget: u64,
    line: &mut Vec<u8>,
) -> io::Result<Option<(BoundedLine, u64)>> {
    line.clear();
    let read = reader
        .by_ref()
        .take(
            line_limit
                .saturating_add(1)
                .min(file_budget.saturating_add(1)),
        )
        .read_until(b'\n', line)? as u64;
    if read == 0 {
        return Ok(None);
    }
    if read > file_budget {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            "provider file exceeds safe streaming limit",
        ));
    }
    if read <= line_limit {
        return Ok(Some((BoundedLine::Complete, read)));
    }
    let rest = if line.last() == Some(&b'\n') {
        0
    } else {
        let remaining = file_budget.saturating_sub(read);
        let rest = reader
            .by_ref()
            .take(remaining.saturating_add(1))
            .skip_until(b'\n')? as u64;
        if rest > remaining {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                "provider file exceeds safe streaming limit",
            ));
        }
        rest
    };
    Ok(Some((BoundedLine::Oversized, read.saturating_add(rest))))
}

pub(crate) fn json_lines_prefix(path: &Path, limit: usize) -> Result<Vec<Value>> {
    let mut preview = JsonLinesPreview::default();
    read_json_lines_prefix(
        &mut File::open(path)?,
        limit,
        u64::MAX,
        MAX_TRANSCRIPT_LINE_SIZE,
        &mut preview,
        &|_, _| WorkspaceGap::Other,
    )?;
    Ok(preview.records)
}

pub(crate) fn json_lines_preview(path: &Path, sample_records: usize) -> Result<Vec<Value>> {
    Ok(json_lines_preview_file_with_suffix(&mut File::open(path)?, sample_records)?.0)
}

/// Visits lines in the bounded suffix and returns its first physical line offset.
pub(crate) fn visit_json_lines_tail(
    path: &Path,
    line_limit: u64,
    mut include: impl FnMut(&[u8]) -> bool,
    mut visit: impl FnMut(u64, JsonLine<'_>),
) -> Result<u64> {
    let mut file = File::open(path)?;
    let (mut reader, mut offset) = jsonl_tail_reader(&mut file, MAX_PREVIEW_TAIL_SIZE)?;
    let first_offset = offset;
    let mut line = Vec::new();
    loop {
        let line_offset = offset;
        line.clear();
        let read = reader.read_until(b'\n', &mut line)?;
        if read == 0 {
            break;
        }
        offset += u64::try_from(read)?;
        if read as u64 > line_limit {
            let location = JsonLineLocation {
                offset: line_offset,
                length: read as u64,
                terminated: line.ends_with(b"\n"),
            };
            visit(line_offset, JsonLine::Oversized(&line, location));
        } else if include(&line)
            && let Some(record) = JsonLine::parse(&line)
        {
            visit(line_offset, record);
        }
    }
    Ok(first_offset)
}

/// Samples an open file and counts consecutive decoded records at EOF.
pub(crate) fn json_lines_preview_file_with_suffix(
    file: &mut File,
    sample_records: usize,
) -> Result<(Vec<Value>, usize)> {
    let preview = read_preview(
        file,
        sample_records,
        u64::MAX,
        MAX_TRANSCRIPT_LINE_SIZE,
        &|_, _| WorkspaceGap::Other,
    )?;
    Ok((preview.records, preview.suffix_len))
}

fn jsonl_tail_reader(file: &mut File, byte_limit: u64) -> Result<(impl BufRead + '_, u64)> {
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(anyhow!("provider path is not a file"));
    }
    let length = metadata.len();
    let start = length.saturating_sub(byte_limit);
    // Include the preceding byte so a newline immediately before the window keeps the first record.
    let mut offset = start.saturating_sub(1);
    file.seek(SeekFrom::Start(offset))?;
    let mut reader = BufReader::new(file.take(length - offset));
    if start > 0 {
        offset += u64::try_from(reader.skip_until(b'\n')?)?;
    }
    Ok((reader, offset))
}

#[derive(Default)]
pub(crate) struct JsonLinesPreview {
    pub(crate) run_starts: Vec<usize>,
    pub(crate) gap_before: std::collections::BTreeSet<usize>,
    pub(crate) records: Vec<Value>,
    pub(crate) omissions: JsonLineSkips,
    pub(crate) sampled: bool,
    /// Number of leading records retained without any unreadable or unsampled row.
    pub(crate) prefix_records: usize,
    suffix_len: usize,
}

impl JsonLinesPreview {
    pub(crate) fn from_records(
        records: Vec<Value>,
        run_starts: Vec<usize>,
        omissions: JsonLineSkips,
        sampled: bool,
        prefix_records: usize,
    ) -> Self {
        Self {
            records,
            gap_before: run_starts.iter().copied().collect(),
            run_starts,
            omissions,
            sampled,
            prefix_records,
            suffix_len: 0,
        }
    }
}

pub(crate) fn json_lines_preview_with_omissions(
    path: &Path,
    sample_records: usize,
    prefix_bytes: u64,
    line_bytes: u64,
    match_malformed: impl Fn(&[u8], bool) -> WorkspaceGap,
) -> Result<JsonLinesPreview> {
    read_preview(
        &mut File::open(path)?,
        sample_records,
        prefix_bytes,
        line_bytes,
        &match_malformed,
    )
}

fn read_preview(
    file: &mut File,
    sample_records: usize,
    prefix_bytes: u64,
    line_bytes: u64,
    match_malformed: &impl Fn(&[u8], bool) -> WorkspaceGap,
) -> Result<JsonLinesPreview> {
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(anyhow!("provider path is not a file"));
    }
    let mut preview = JsonLinesPreview::default();
    let small = metadata.len() <= MAX_PREVIEW_TAIL_SIZE;
    let limit = if small {
        MAX_PROVIDER_RECORDS
    } else {
        sample_records
    };
    let mut prefix = read_json_lines_prefix(
        file,
        limit,
        prefix_bytes,
        line_bytes,
        &mut preview,
        match_malformed,
    )?;
    if prefix.0 == metadata.len() {
        return Ok(preview);
    }
    if small && metadata.len() > prefix_bytes {
        preview.sampled = true;
        preview.suffix_len = 0;
        return Ok(preview);
    }
    if small {
        // A short file can contain more records than the full-preview limit.
        // Use the requested head and tail when the prefix did not reach EOF.
        preview = JsonLinesPreview::default();
        prefix = read_json_lines_prefix(
            file,
            sample_records,
            prefix_bytes,
            line_bytes,
            &mut preview,
            match_malformed,
        )?;
    }
    let tail = read_json_lines_tail(
        file,
        sample_records,
        prefix,
        line_bytes,
        &mut preview,
        match_malformed,
    )?;
    preview
        .records
        .extend(tail.into_iter().map(|(_, record)| record));
    Ok(preview)
}

#[cfg(test)]
fn json_lines_tail_with_offsets(file: &mut File, limit: usize) -> Result<Vec<(u64, Value)>> {
    read_json_lines_tail(
        file,
        limit,
        (0, false),
        MAX_TRANSCRIPT_LINE_SIZE,
        &mut JsonLinesPreview::default(),
        &|_, _| WorkspaceGap::Other,
    )
}

fn read_json_lines_tail(
    file: &mut File,
    limit: usize,
    (prefix_end, trailing_gap): (u64, bool),
    line_bytes: u64,
    preview: &mut JsonLinesPreview,
    match_malformed: &impl Fn(&[u8], bool) -> WorkspaceGap,
) -> Result<Vec<(u64, Value)>> {
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(anyhow!("provider path is not a file"));
    }
    let start = metadata
        .len()
        .saturating_sub(MAX_PREVIEW_TAIL_SIZE)
        .max(prefix_end);
    let partial_first_line = if start > prefix_end {
        preview.sampled = true;
        preview.suffix_len = 0;
        file.seek(SeekFrom::Start(start - 1))?;
        let mut preceding = [0];
        file.read_exact(&mut preceding)?;
        preceding[0] != b'\n'
    } else {
        file.seek(SeekFrom::Start(start))?;
        false
    };
    let mut reader = BufReader::new(file.take(MAX_PREVIEW_TAIL_SIZE));
    let mut offset = start;
    if partial_first_line {
        offset += u64::try_from(reader.skip_until(b'\n')?)?;
    }
    let mut records = VecDeque::with_capacity(limit);
    let mut verified_tail_records = 0;
    let mut tail_truncated = start > prefix_end || trailing_gap;
    let mut last_skips = preview.omissions.total();
    let mut line = Vec::new();
    loop {
        let line_offset = offset;
        line.clear();
        let read = reader.read_until(b'\n', &mut line)?;
        if read == 0 {
            break;
        }
        offset += u64::try_from(read)?;
        if read as u64 > line_bytes {
            preview.suffix_len = 0;
            preview
                .omissions
                .observe(match_malformed(&line, true), true);
            continue;
        }
        if let Some(record) = preview.omissions.parse(&line, match_malformed) {
            preview.suffix_len += 1;
            if limit == 0 {
                preview.suffix_len = 0;
                preview.sampled = true;
                continue;
            }
            if records.len() == limit {
                records.pop_front();
                tail_truncated = true;
                preview.suffix_len = preview.suffix_len.min(limit);
                preview.sampled = true;
            }
            if !preview.sampled && preview.omissions.total() == 0 {
                verified_tail_records += 1;
            }
            let skipped = preview.omissions.total();
            records.push_back((line_offset, record, skipped != last_skips));
            last_skips = skipped;
        } else {
            preview.suffix_len = 0;
        }
    }
    if !preview.sampled {
        preview.prefix_records += verified_tail_records;
    }
    let base = preview.records.len();
    Ok(records
        .into_iter()
        .enumerate()
        .map(|(index, (offset, record, gap))| {
            if gap || (index == 0 && tail_truncated) {
                preview.run_starts.push(base + index);
            }
            if gap || (index == 0 && preview.sampled) {
                preview.gap_before.insert(base + index);
            }
            (offset, record)
        })
        .collect())
}

/// Reads up to `limit` leading lines and reports the end offset and any trailing gap.
fn read_json_lines_prefix(
    file: &mut File,
    limit: usize,
    byte_limit: u64,
    line_bytes: u64,
    preview: &mut JsonLinesPreview,
    match_malformed: &impl Fn(&[u8], bool) -> WorkspaceGap,
) -> Result<(u64, bool)> {
    file.rewind()?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(anyhow!("provider path is not a file"));
    }
    let mut reader = BufReader::new(file.take(byte_limit));
    let mut line = Vec::new();
    let mut consumed = 0_u64;
    let mut last_skips = 0;
    for _ in 0..limit {
        let Some((line_kind, read)) = read_bounded_line(&mut reader, line_bytes, &mut line)? else {
            break;
        };
        let end = consumed + read;
        if end == byte_limit && end < metadata.len() && !line.ends_with(b"\n") {
            // The edge stops inside a record. The tail can still read that record in full.
            break;
        }
        consumed = end;
        let previous = preview.omissions.total();
        match line_kind {
            BoundedLine::Oversized => {
                preview.suffix_len = 0;
                preview
                    .omissions
                    .observe(match_malformed(&line, true), true);
            }
            BoundedLine::Complete => {
                if let Some(record) = preview.omissions.parse(&line, match_malformed) {
                    if preview.omissions.total() == 0 {
                        preview.prefix_records += 1;
                    }
                    let skipped = preview.omissions.total();
                    if skipped != last_skips {
                        preview.run_starts.push(preview.records.len());
                    }
                    last_skips = skipped;
                    preview.records.push(record);
                    preview.suffix_len += 1;
                } else {
                    preview.suffix_len = 0;
                }
            }
        }
        if preview.omissions.total() > previous {
            preview.gap_before.insert(preview.records.len());
        }
    }
    Ok((consumed, last_skips != preview.omissions.total()))
}

/// Collects the physical records when the whole file fits the small-preview budgets.
pub(crate) fn complete_small_json_records(path: &Path) -> Result<Option<Vec<JsonlRecord>>> {
    let file = File::open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(anyhow!("provider path is not a file"));
    }
    let len = metadata.len();
    if len > MAX_PREVIEW_TAIL_SIZE {
        return Ok(None);
    }
    let mut reader = BufReader::new((&file).take(len));
    let mut records = Vec::new();
    let mut consumed = 0;
    let mut line = Vec::new();
    for _ in 0..MAX_PROVIDER_RECORDS {
        let Some((kind, read)) =
            read_bounded_line(&mut reader, MAX_TRANSCRIPT_LINE_SIZE, &mut line)?
        else {
            break;
        };
        if matches!(kind, BoundedLine::Oversized) {
            return Ok(None);
        }
        consumed += read;
        if let Some(record) = JsonLine::parse(&line).and_then(JsonLine::into_record) {
            records.push(record);
        }
    }
    Ok((consumed == len && file.metadata()?.len() == len).then_some(records))
}

#[cfg(test)]
fn json_lines_preview_with_tail(
    path: &Path,
    sample_records: usize,
    tail: JsonLineTail<'_>,
) -> Result<Vec<Value>> {
    Ok(json_lines_preview_with_prefix(path, sample_records, tail)?.0)
}

pub(crate) fn json_lines_preview_with_prefix(
    path: &Path,
    sample_records: usize,
    tail_policy: JsonLineTail<'_>,
) -> Result<(Vec<Value>, usize)> {
    let mut file = File::open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(anyhow!("provider path is not a file"));
    }
    json_lines_preview_at(&mut file, sample_records, tail_policy, metadata.len())
}

/// Reconstructs the sampled windows at the supplied physical end of the transcript.
pub(crate) fn json_lines_preview_at(
    file: &mut File,
    sample_records: usize,
    tail_policy: JsonLineTail<'_>,
    length: u64,
) -> Result<(Vec<Value>, usize)> {
    anyhow::ensure!(
        file.metadata()?.len() >= length,
        "transcript shortened during preview"
    );
    let end = tail_policy.record_end(file, length, MAX_PREVIEW_TAIL_SIZE)?;
    if end <= MAX_PREVIEW_TAIL_SIZE {
        let prefix = read_verified_json_lines_prefix(file, MAX_PROVIDER_RECORDS, tail_policy, end)?;
        if prefix.at_eof {
            return Ok((prefix.records, prefix.verified_records));
        }
    }

    let prefix = read_verified_json_lines_prefix(file, sample_records, tail_policy, end)?;
    if prefix.at_eof {
        return Ok((prefix.records, prefix.verified_records));
    }
    let (tail, verified_end) =
        verified_json_lines_tail_with_offsets(file, sample_records, tail_policy, end, prefix.end)?;
    let overlap = tail.partition_point(|(offset, _)| *offset < prefix.end);
    let appended = tail.into_iter().skip(overlap);
    let mut verified_records = prefix.verified_records;
    let mut records = prefix.records;
    for (offset, record) in appended {
        verified_records += usize::from(prefix.fully_verified && offset < verified_end);
        records.push(record);
    }
    Ok((records, verified_records))
}

fn verified_json_lines_tail_with_offsets(
    file: &mut File,
    limit: usize,
    tail_policy: JsonLineTail<'_>,
    end: u64,
    prefix_end: u64,
) -> Result<(Vec<(u64, Value)>, u64)> {
    let start = end.saturating_sub(MAX_PREVIEW_TAIL_SIZE);
    let mut leading_fragment = start > 0;
    if leading_fragment {
        file.seek(SeekFrom::Start(start - 1))?;
        let mut previous = [0];
        file.read_exact(&mut previous)?;
        leading_fragment = previous[0] != b'\n';
    } else {
        file.seek(SeekFrom::Start(start))?;
    }
    let mut reader = BufReader::new(file.take(end - start));
    let mut offset = start;
    if leading_fragment {
        offset += u64::try_from(reader.skip_until(b'\n')?)?;
        if tail_policy.checked_end(offset == end) {
            let file = reader.into_inner().into_inner();
            tail_policy.check_final_fragment(file, end, MAX_PREVIEW_VALIDATION_BYTES)?;
            return Ok((Vec::new(), 0));
        }
    }
    let mut records = VecDeque::with_capacity(limit);
    let mut verified_end = if offset <= prefix_end {
        end
    } else {
        prefix_end
    };
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
            if tail_policy.checked_end(offset == end) {
                tail_policy.parse(&line, offset, true)?;
            }
            if line_offset >= prefix_end {
                verified_end = verified_end.min(line_offset);
            }
            continue;
        }
        if let Some(record) = tail_policy.parse(&line, offset, offset == end)?
            && limit > 0
        {
            if records.len() == limit
                && let Some((evicted, _)) = records.pop_front()
                && evicted >= prefix_end
            {
                verified_end = verified_end.min(evicted);
            }
            records.push_back((line_offset, record));
        } else if line_offset >= prefix_end && !line.iter().all(u8::is_ascii_whitespace) {
            verified_end = verified_end.min(line_offset);
        }
    }
    Ok((records.into_iter().collect(), verified_end))
}

struct JsonLinesPrefix {
    fully_verified: bool,
    records: Vec<Value>,
    end: u64,
    verified_records: usize,
    at_eof: bool,
}

/// Reads leading lines and tracks the decoded suffix after the last skipped line.
fn read_verified_json_lines_prefix(
    file: &mut File,
    limit: usize,
    tail: JsonLineTail<'_>,
    end: u64,
) -> Result<JsonLinesPrefix> {
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(anyhow!("provider path is not a file"));
    }
    file.rewind()?;
    let mut reader = BufReader::new(file);
    let mut records = Vec::new();
    let mut line = Vec::new();
    let mut consumed = 0_u64;
    let mut verified_records = 0;
    let mut contiguous = true;
    let prefix_budget = if tail.checked_end(true) {
        MAX_PREVIEW_VALIDATION_BYTES
    } else {
        u64::MAX
    };
    for _ in 0..limit {
        if consumed == end || consumed >= prefix_budget {
            break;
        }
        let Some((line_kind, read)) = read_bounded_line(
            &mut reader
                .by_ref()
                .take((prefix_budget - consumed).min(end - consumed)),
            MAX_TRANSCRIPT_LINE_SIZE,
            &mut line,
        )?
        else {
            break;
        };
        consumed += read;
        if consumed == prefix_budget && consumed < end && !line.ends_with(b"\n") {
            consumed -= read;
            break;
        }
        // Previews skip oversized records the way streamed reads do.
        if matches!(line_kind, BoundedLine::Oversized) {
            tail.check_skipped_end(
                &mut reader,
                consumed == end,
                read,
                MAX_PREVIEW_VALIDATION_BYTES,
            )?;
        }
        if matches!(line_kind, BoundedLine::Complete)
            && let Some(record) = tail.parse(&line, consumed, consumed == end)?
        {
            records.push(record);
            verified_records += usize::from(contiguous);
        } else if matches!(line_kind, BoundedLine::Oversized)
            || !line.iter().all(u8::is_ascii_whitespace)
        {
            contiguous = false;
        }
    }
    Ok(JsonLinesPrefix {
        fully_verified: contiguous,
        records,
        end: consumed,
        verified_records,
        at_eof: consumed == end || reader.fill_buf()?.is_empty(),
    })
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

pub(crate) fn encode_uri_component(value: &str) -> String {
    value
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
                char::from(byte).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

pub(crate) fn percent_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let high = *bytes.get(index + 1)?;
            let low = *bytes.get(index + 2)?;
            decoded.push((hex_value(high)? << 4) | hex_value(low)?);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

const fn hex_value(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
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
    preview: Option<EventPreview>,
    rollback_opener: Option<RollbackOpener>,
    removed_mirrors: HashSet<u64>,
    pub(crate) codex_saved_users: Option<bool>,
    pub(crate) codex_saved_assistant: bool,
}

#[derive(Default)]
struct RollbackOpener {
    full_read: bool,
    first_message: Option<(u64, Option<String>)>,
    mirror_openers: Option<BTreeMap<u64, Option<String>>>,
    applied_rollback: bool,
    gap: bool,
    /// A repeated surviving opener cancels an earlier withdrawal of the same title.
    withdrawn: crate::WithdrawnOpeners,
    target: Option<[u8; 32]>,
}

impl RollbackOpener {
    fn active_withdrawals(&self) -> crate::WithdrawnOpeners {
        let mut withdrawn = self.withdrawn.clone();
        if let Some((_, Some(title))) = &self.first_message {
            withdrawn.reissued(title);
        }
        withdrawn
    }
}

#[derive(Default)]
struct EventPreview {
    outline: Vec<PreviewEvent>,
    selection: Option<VecDeque<PreviewEvent>>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) struct PreviewEvent {
    sequence: u64,
    bytes: usize,
    tool: bool,
    unverified_history: bool,
}

impl PreviewEvent {
    fn new(sequence: u64, kind: &EventKind, payload: &Value, source_bytes: usize) -> Self {
        Self {
            sequence,
            bytes: estimated_event_bytes(payload).saturating_add(source_bytes),
            tool: is_tool_event(kind),
            unverified_history: history_unverified(kind, payload),
        }
    }
}

fn select_preview_edges(
    outline: impl DoubleEndedIterator<Item = PreviewEvent> + ExactSizeIterator + Clone,
    limit: usize,
    bytes_per_edge: usize,
) -> (VecDeque<PreviewEvent>, usize) {
    let mut selected = VecDeque::new();
    let add_edge = |events: &mut dyn Iterator<Item = PreviewEvent>| {
        let mut added = Vec::new();
        let mut remaining = bytes_per_edge;
        for event in events {
            if event.bytes > bytes_per_edge {
                continue;
            }
            if added.len() == limit / 2 || event.bytes > remaining {
                break;
            }
            remaining -= event.bytes;
            added.push(event);
        }
        added
    };
    let total = outline.len();
    selected.extend(add_edge(&mut outline.clone()));
    let end = selected.back().map(|event| event.sequence);
    selected.extend(
        add_edge(&mut outline.rev())
            .into_iter()
            .rev()
            .filter(|event| end.is_none_or(|last| event.sequence > last)),
    );
    let omitted = total - selected.len();
    (selected, omitted)
}

fn history_unverified(kind: &EventKind, payload: &Value) -> bool {
    *kind == EventKind::ProviderEvent
        && payload["native_history_unverified"].as_bool() == Some(true)
}

fn preview_prefix_end(
    sequences: impl Iterator<Item = (u64, bool)>,
    selected: &VecDeque<PreviewEvent>,
    end: u64,
) -> u64 {
    let mut prefix_end = end;
    for (index, (sequence, unverified)) in sequences.enumerate() {
        if unverified {
            return 0;
        }
        if prefix_end == end && selected.get(index).map(|event| event.sequence) != Some(sequence) {
            prefix_end = sequence;
        }
    }
    prefix_end
}

impl EventPreview {
    fn record(
        &mut self,
        sequence: u64,
        kind: &EventKind,
        payload: &Value,
        source_bytes: usize,
    ) -> bool {
        let item = PreviewEvent::new(sequence, kind, payload, source_bytes);
        self.outline.push(item);
        let Some(selected) = self.selection.as_mut() else {
            return false;
        };
        if selected
            .front()
            .is_some_and(|next| next.sequence == sequence)
        {
            return selected
                .pop_front()
                .is_some_and(|next| item.bytes <= next.bytes);
        }
        false
    }
}

fn is_tool_event(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::ToolCalled
            | EventKind::ToolCompleted
            | EventKind::ToolFailed
            | EventKind::CommandExecuted
    )
}

fn retain_latest_matching<T>(
    values: &mut Vec<T>,
    limit: usize,
    matches: impl Fn(&T) -> bool,
) -> usize {
    let removed = values
        .iter()
        .filter(|value| matches(value))
        .count()
        .saturating_sub(limit);
    let mut remaining = removed;
    values.retain(|value| {
        if remaining > 0 && matches(value) {
            remaining -= 1;
            false
        } else {
            true
        }
    });
    removed
}

/// Conservative retained-memory estimate for a canonical event and its cloned JSON payload.
pub(crate) fn estimated_event_bytes(payload: &Value) -> usize {
    fn value_bytes(value: &Value) -> usize {
        let own = std::mem::size_of::<Value>();
        let contents = match value {
            Value::String(text) => text.len(),
            Value::Array(values) => values.iter().fold(0_usize, |total, value| {
                total.saturating_add(value_bytes(value))
            }),
            Value::Object(values) => {
                // A small BTreeMap still allocates a whole node. Account for that spare
                // capacity as well as key storage, child values, and per-entry pointers.
                let node =
                    11 * (std::mem::size_of::<String>() + own) + 16 * std::mem::size_of::<usize>();
                values.iter().fold(node, |total, (key, value)| {
                    total
                        .saturating_add(key.len())
                        .saturating_add(
                            std::mem::size_of::<String>() + 4 * std::mem::size_of::<usize>(),
                        )
                        .saturating_add(value_bytes(value))
                })
            }
            _ => 0,
        };
        own.saturating_add(contents)
    }
    // Event metadata includes UUIDs, timestamps, the source ID, and its record type.
    std::mem::size_of::<OmniEvent>()
        .saturating_add(256)
        .saturating_add(value_bytes(payload))
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
            preview: None,
            rollback_opener: None,
            removed_mirrors: HashSet::new(),
            codex_saved_users: None,
            codex_saved_assistant: false,
        }
    }

    pub(crate) fn empty_with_same_plan(&self) -> Self {
        debug_assert_eq!(self.next_sequence, 0);
        let mut builder = Self::new(self.provider, &self.session_id);
        builder.provider_version.clone_from(&self.provider_version);
        builder.thread_id = self.thread_id;
        builder.branch_id = self.branch_id;
        builder.codex_saved_users = self.codex_saved_users;
        builder.codex_saved_assistant = self.codex_saved_assistant;
        if let Some(preview) = &self.preview {
            builder.plan_preview(preview.selection.clone());
        }
        builder.rollback_opener = self.rollback_opener.as_ref().map(|opener| RollbackOpener {
            full_read: opener.full_read,
            target: opener.target,
            gap: opener.gap,
            mirror_openers: opener.mirror_openers.as_ref().map(|_| BTreeMap::new()),
            ..RollbackOpener::default()
        });
        builder
    }

    /// The first pass retains only event sizes and sequence numbers. The second pass materializes the selection.
    pub(crate) fn plan_preview(&mut self, selection: Option<VecDeque<PreviewEvent>>) {
        self.preview = Some(EventPreview {
            selection,
            ..EventPreview::default()
        });
    }

    pub(crate) fn select_preview(
        &mut self,
        limit: usize,
        bytes_per_edge: usize,
    ) -> (VecDeque<PreviewEvent>, usize, u64) {
        self.purge_removed_mirrors();
        let outline = &self
            .preview
            .as_ref()
            .expect("preview planning enabled")
            .outline;
        let (selected, omitted) =
            select_preview_edges(outline.iter().copied(), limit, bytes_per_edge);
        let prefix_end = preview_prefix_end(
            outline
                .iter()
                .map(|event| (event.sequence, event.unverified_history)),
            &selected,
            self.next_sequence,
        );
        (selected, omitted, prefix_end)
    }

    /// Selects the preview after published revisions and rollbacks have updated canonical events.
    pub(crate) fn retain_bounded_preview(
        &mut self,
        limit: usize,
        bytes_per_edge: usize,
    ) -> (usize, u64) {
        self.purge_removed_mirrors();
        let outline = self.events.iter().map(|event| {
            let source_bytes = event.source.native_session_id.len()
                + event
                    .source
                    .provider_version
                    .as_ref()
                    .map_or(0, String::len)
                + event.source.raw_record_type.as_ref().map_or(0, String::len);
            PreviewEvent::new(event.sequence, &event.kind, &event.payload, source_bytes)
        });
        let (selected, omitted) = select_preview_edges(outline, limit, bytes_per_edge);
        let prefix_end = preview_prefix_end(
            self.events.iter().map(|event| {
                (
                    event.sequence,
                    history_unverified(&event.kind, &event.payload),
                )
            }),
            &selected,
            self.next_sequence,
        );
        if omitted == 0 {
            return (0, prefix_end);
        }
        let mut selected = selected.into_iter().peekable();
        self.events.retain(|event| {
            selected
                .next_if(|next| next.sequence == event.sequence)
                .is_some()
        });
        (omitted, prefix_end)
    }

    pub(crate) fn finish_preview(&mut self, expected: usize) -> Result<()> {
        self.purge_removed_mirrors();
        self.preview = None;
        if self.events.len() != expected {
            return Err(anyhow!(
                "provider history changed while reading the preview"
            ));
        }
        Ok(())
    }

    pub(crate) fn set_provider_version(&mut self, provider_version: Option<String>) {
        self.provider_version = provider_version;
    }

    pub(crate) fn checkpoint(&self) -> u64 {
        self.next_sequence
    }

    /// Keep a native position for a message whose public spelling may arrive later.
    pub(crate) fn reserve_sequence(&mut self) -> u64 {
        let sequence = self.next_sequence;
        self.next_sequence += 1;
        sequence
    }

    pub(crate) fn fill_reserved_sequences(&mut self, events: Vec<OmniEvent>) {
        if events.is_empty() {
            return;
        }
        self.events.extend(events.into_iter().map(|mut event| {
            event.event_id = Uuid::new_v5(&self.thread_id, event.sequence.to_string().as_bytes());
            event
        }));
        self.events.sort_unstable_by_key(|event| event.sequence);
    }

    pub(crate) fn events(&self) -> &[OmniEvent] {
        &self.events
    }

    /// Inserts projected events after a surviving source record and keeps source IDs stable.
    pub(crate) fn insert_after(&mut self, index: usize, add: impl FnOnce(&mut Self)) {
        let mut tail = self.events.split_off(index + 1);
        let first_sequence = self.events[index].sequence + 1;
        add(self);
        let inserted = &mut self.events[index + 1..];
        for event in &mut tail {
            event.sequence += inserted.len() as u64;
        }
        for (sequence, event) in (first_sequence..).zip(inserted) {
            event.sequence = sequence;
        }
        self.events.extend(tail);
    }

    pub(crate) fn first_sequence(&self) -> u64 {
        if let Some(preview) = &self.preview {
            return preview
                .outline
                .first()
                .map_or(self.next_sequence, |event| event.sequence);
        }
        self.events
            .first()
            .map_or(self.next_sequence, |event| event.sequence)
    }

    /// Finds a retained event after truncation or pruning has left gaps in the sequence.
    pub(crate) fn event_at_mut(&mut self, sequence: u64) -> Option<&mut OmniEvent> {
        let index = self
            .events
            .binary_search_by_key(&sequence, |event| event.sequence)
            .ok()?;
        self.events.get_mut(index)
    }

    /// Move confirmed early model messages to their first display position after all ID lookups.
    pub(crate) fn order_delayed_messages(&mut self, positions: &HashMap<u64, (u64, u64)>) {
        if positions.is_empty() {
            return;
        }
        self.purge_removed_mirrors();
        let sequences: Vec<_> = self.events.iter().map(|event| event.sequence).collect();
        // An answer from an older window must stay on its side of the compaction boundary.
        for window in self
            .events
            .split_mut(|event| event.kind == EventKind::CompactionCreated)
        {
            window.sort_unstable_by_key(|event| match positions.get(&event.sequence) {
                Some(&(before, ordinal)) => (before, false, ordinal),
                None => (event.sequence, true, 0),
            });
        }
        for (event, sequence) in self.events.iter_mut().zip(sequences) {
            event.sequence = sequence;
        }
    }

    pub(crate) fn drain_events(&mut self) -> std::vec::Drain<'_, OmniEvent> {
        self.purge_removed_mirrors();
        self.events.drain(..)
    }

    pub(crate) fn track_rollback_opener(&mut self, full_read: bool) {
        self.rollback_opener = Some(RollbackOpener {
            full_read,
            ..RollbackOpener::default()
        });
    }

    pub(crate) fn track_saved_opener(&mut self, title: &str) {
        self.rollback_opener = Some(RollbackOpener {
            target: Some(crate::RollbackTitle::fingerprint(title)),
            ..RollbackOpener::default()
        });
    }

    pub(crate) fn tracks_rollback_opener(&self) -> bool {
        self.rollback_opener.is_some()
    }

    pub(crate) fn discard_rollback_opener(&mut self) {
        self.rollback_opener = None;
    }

    pub(crate) fn note_rollback_history_gap(&mut self) {
        if let Some(opener) = &mut self.rollback_opener {
            opener.gap = true;
        }
    }

    pub(crate) fn witnessed_saved_opener_withdrawal(&self, title: &str) -> bool {
        self.rollback_opener.as_ref().is_some_and(|opener| {
            opener
                .active_withdrawals()
                .hashes
                .contains(&crate::RollbackTitle::fingerprint(title))
        })
    }

    pub(crate) fn confirms_saved_local_opener(&self, title: &str) -> bool {
        self.rollback_opener.as_ref().is_some_and(|opener| {
            (!opener.gap
                && opener
                    .first_message
                    .as_ref()
                    .and_then(|(_, title)| title.as_deref())
                    == Some(title))
                || self.witnessed_saved_opener_withdrawal(title)
        })
    }

    pub(crate) fn record_paginated_opener_withdrawal(&mut self, inherited: bool) {
        if let Some((prior, successor)) = omnis_core::codex_withdrawn_search_opener(&self.events) {
            let mut opener = RollbackOpener {
                gap: inherited,
                full_read: true,
                first_message: successor.map(|title| (0, Some(title))),
                ..Default::default()
            };
            opener
                .withdrawn
                .record(crate::RollbackTitle::fingerprint(&prior));
            self.rollback_opener = Some(opener);
        }
    }

    pub(crate) fn withdrew_saved_opener(&self, title: &str) -> bool {
        self.rollback_opener.as_ref().is_some_and(|opener| {
            opener
                .withdrawn
                .hashes
                .contains(&crate::RollbackTitle::fingerprint(title))
        })
    }

    pub(crate) fn rollback_title(&self) -> crate::RollbackTitle {
        match &self.rollback_opener {
            Some(opener) if opener.gap => opener.active_withdrawals().after_gap(),
            Some(opener) if opener.full_read || opener.applied_rollback => {
                crate::RollbackTitle::Known(
                    opener
                        .first_message
                        .as_ref()
                        .and_then(|(_, title)| title.clone()),
                )
            }
            Some(_) => crate::RollbackTitle::UnknownLocal,
            None => crate::RollbackTitle::Unknown,
        }
    }

    pub(crate) fn track_codex_mirror_openers(&mut self, enabled: bool) {
        if let Some(opener) = &mut self.rollback_opener {
            opener.mirror_openers = (enabled && opener.first_message.is_none()).then(BTreeMap::new);
        }
    }

    pub(crate) fn track_codex_provisional_opener(&mut self) {
        if let Some(opener) = &mut self.rollback_opener
            && opener.first_message.is_none()
        {
            opener.mirror_openers.get_or_insert_with(BTreeMap::new);
        }
    }

    pub(crate) fn replace_codex_user_text(
        &mut self,
        sequence: u64,
        text: &str,
        timestamp: Option<DateTime<Utc>>,
    ) {
        let retained = self
            .events
            .binary_search_by_key(&sequence, |event| event.sequence)
            .ok();
        let tracks_opener = self.rollback_opener.as_ref().is_some_and(|opener| {
            !opener.gap
                && (opener.mirror_openers.is_some()
                    || opener
                        .first_message
                        .as_ref()
                        .is_none_or(|(pos, _)| sequence <= *pos))
        });
        if retained.is_none() && !tracks_opener {
            return;
        }
        let event = self.event_at(
            (sequence, None),
            EventKind::MessageUser,
            serde_json::json!({"text":text}),
            timestamp,
            ReplayPolicy::Contextual,
            Some("event_msg".to_owned()),
        );
        if tracks_opener && let Some(opener) = &mut self.rollback_opener {
            let replacement =
                omnis_core::search_title_opener_from_messages(std::slice::from_ref(&event));
            if let Some(candidates) = &mut opener.mirror_openers {
                if let Some((_, title)) = &replacement {
                    candidates.insert(sequence, title.clone());
                } else {
                    candidates.remove(&sequence);
                }
                opener.first_message = candidates
                    .first_key_value()
                    .map(|(&pos, title)| (pos, title.clone()));
            } else {
                opener.first_message = replacement;
            }
        }
        if let Some(index) = retained {
            self.events[index] = event;
        }
    }

    pub(crate) fn remove_codex_mirror(&mut self, sequence: u64) {
        self.removed_mirrors.insert(sequence);
        if let Some(opener) = &mut self.rollback_opener
            && let Some(candidates) = &mut opener.mirror_openers
        {
            candidates.remove(&sequence);
            if opener
                .first_message
                .as_ref()
                .is_some_and(|(pos, _)| *pos == sequence)
            {
                opener.first_message = candidates
                    .first_key_value()
                    .map(|(&pos, title)| (pos, title.clone()));
            }
        }
    }

    fn purge_removed_mirrors(&mut self) {
        if self.removed_mirrors.is_empty() {
            return;
        }
        self.events
            .retain(|event| !self.removed_mirrors.contains(&event.sequence));
        if let Some(preview) = &mut self.preview {
            preview
                .outline
                .retain(|event| !self.removed_mirrors.contains(&event.sequence));
        }
        self.removed_mirrors.clear();
    }

    pub(crate) fn truncate_from(&mut self, checkpoint: u64) {
        self.rollback_events(|sequence| sequence < checkpoint);
    }

    pub(crate) fn rollback_events(&mut self, keep: impl Fn(u64) -> bool) {
        if let Some(opener) = &mut self.rollback_opener
            && !opener.gap
        {
            opener.applied_rollback = true;
            if let Some(candidates) = &mut opener.mirror_openers {
                candidates.retain(|sequence, _| keep(*sequence));
            }
            if let Some((_, Some(title))) = opener
                .first_message
                .take_if(|(sequence, _)| !keep(*sequence))
            {
                let hash = crate::RollbackTitle::fingerprint(&title);
                if opener.target.is_none_or(|target| target == hash) {
                    opener.withdrawn.record(hash);
                }
            }
            if opener.first_message.is_none()
                && let Some(candidates) = &opener.mirror_openers
            {
                opener.first_message = candidates
                    .first_key_value()
                    .map(|(&pos, title)| (pos, title.clone()));
            }
        }
        if let Some(preview) = &mut self.preview {
            preview.outline.retain(|event| keep(event.sequence));
        }
        self.events.retain(|event| keep(event.sequence));
    }

    pub(crate) fn visit_events(&self, visit: impl FnMut(&OmniEvent)) {
        self.events.iter().for_each(visit);
    }

    pub(crate) fn visit_events_mut(&mut self, visit: impl FnMut(&mut OmniEvent)) {
        self.events.iter_mut().for_each(visit);
    }

    pub(crate) fn retain_events(&mut self, keep: impl FnMut(&OmniEvent) -> bool) {
        self.events.retain(keep);
    }

    pub(crate) fn retain_preview_events(&mut self, limit: usize, timestamp: DateTime<Utc>) {
        self.purge_removed_mirrors();
        if self.events.len() <= limit {
            if self.provider == Provider::Codex {
                self.push_codex_preview_prefix(self.checkpoint(), true, Some(timestamp));
            }
            return;
        }
        let omitted = self.events.len() - limit;
        let mut payload =
            serde_json::json!({"omitted_events": omitted, "retained_preview_events": limit});
        if self.provider == Provider::Codex {
            payload["prefix_end_sequence"] = self.events[limit / 2].sequence.into();
        }
        self.events.drain(limit / 2..limit / 2 + omitted);
        self.push(
            EventKind::ProviderEvent,
            payload,
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
        self.push_json_line_omissions(
            JsonLineSkips {
                oversized: omitted_records,
                ..Default::default()
            },
            MAX_STREAMED_TRANSCRIPT_LINE_SIZE,
            None,
            timestamp,
        );
    }

    pub(crate) fn push_json_line_omissions(
        &mut self,
        omissions: JsonLineSkips,
        line_limit: u64,
        prefix_end: Option<u64>,
        timestamp: Option<DateTime<Utc>>,
    ) {
        if omissions.oversized > 0 {
            let mut payload = serde_json::json!({
                "omitted_events": omissions.oversized,
                "omitted_records": omissions.oversized,
                "max_record_bytes": line_limit,
            });
            if let Some(end) = prefix_end {
                payload["prefix_end_sequence"] = end.into();
            }
            self.push(
                EventKind::ProviderEvent,
                payload,
                timestamp,
                ReplayPolicy::HistoricalOnly,
                Some("omnisession.record_size_limit".to_owned()),
                None,
            );
        }
        if omissions.malformed > 0 {
            let mut payload = serde_json::json!({
                "omitted_events": omissions.malformed,
                "malformed_records": omissions.malformed,
            });
            if omissions.unplaced > 0 {
                payload["unplaced_records"] = omissions.unplaced.into();
            }
            if let Some(end) = prefix_end {
                payload["prefix_end_sequence"] = end.into();
            }
            self.push(
                EventKind::ProviderEvent,
                payload,
                timestamp,
                ReplayPolicy::HistoricalOnly,
                Some(
                    if self.provider == Provider::Codex {
                        "omnisession.codex_malformed_records"
                    } else {
                        "omnisession.malformed_records"
                    }
                    .to_owned(),
                ),
                None,
            );
        }
    }

    pub(crate) fn push_codex_preview_prefix(
        &mut self,
        prefix_end_sequence: u64,
        complete: bool,
        timestamp: Option<DateTime<Utc>>,
    ) {
        let mut payload = serde_json::json!({"prefix_end_sequence": prefix_end_sequence});
        if complete {
            payload["prefix_complete"] = true.into();
        }
        self.push(
            EventKind::ProviderEvent,
            payload,
            timestamp,
            ReplayPolicy::HistoricalOnly,
            Some("omnisession.codex_preview_prefix".to_owned()),
            None,
        );
    }

    /// Keeps the workspace decision even when a sampled preview leaves out the omission event.
    pub(crate) fn mark_codex_workspace_unknown(&mut self, timestamp: Option<DateTime<Utc>>) {
        if let Some(event) = self.events.iter_mut().rev().find(|event| {
            matches!(
                event.source.raw_record_type.as_deref(),
                Some("omnisession.codex_malformed_records" | "omnisession.record_size_limit")
            )
        }) {
            event.payload["workspace_unknown"] = true.into();
        } else {
            self.push(
                EventKind::ProviderEvent,
                serde_json::json!({"workspace_unknown":true}),
                timestamp,
                ReplayPolicy::HistoricalOnly,
                Some("omnisession.codex_workspace_unknown".to_owned()),
                None,
            );
        }
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
        let retained = self.preview.as_mut().is_none_or(|preview| {
            let source_bytes = self.session_id.len()
                + self.provider_version.as_ref().map_or(0, String::len)
                + raw_record_type.as_ref().map_or(0, String::len);
            preview.record(sequence, &kind, &payload, source_bytes)
        });
        let observe = self.rollback_opener.as_ref().is_some_and(|opener| {
            !opener.gap
                && (opener.first_message.is_none() || opener.mirror_openers.is_some())
                && matches!(kind, EventKind::MessageUser | EventKind::MessageAssistant)
        });
        if !retained && !observe {
            return;
        }
        let event = self.event_at(
            (sequence, event_id),
            kind,
            payload,
            timestamp,
            replay_policy,
            raw_record_type,
        );
        if observe
            && let Some(opener) = &mut self.rollback_opener
            && let Some((pos, title)) =
                omnis_core::search_title_opener_from_messages(std::slice::from_ref(&event))
        {
            if let Some(candidates) = &mut opener.mirror_openers {
                candidates.insert(pos, title.clone());
                if candidates.len() > super::codex::MAX_CANONICAL_EVENTS {
                    opener.mirror_openers = None;
                }
            }
            if opener.first_message.is_none() {
                opener.first_message = Some((pos, title));
            }
        }
        if retained {
            self.events.push(event);
        }
    }

    fn event_at(
        &self,
        (sequence, event_id): (u64, Option<Uuid>),
        kind: EventKind,
        payload: Value,
        timestamp: Option<DateTime<Utc>>,
        replay_policy: ReplayPolicy,
        raw_record_type: Option<String>,
    ) -> OmniEvent {
        let event_seed = event_id.map_or_else(
            || sequence.to_string(),
            |native_id| format!("{sequence}:{native_id}"),
        );
        OmniEvent {
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
        }
    }

    pub(crate) fn database_step_namespace(&self, index: i64, opaque: bool) -> Uuid {
        let mut key = [0; 9];
        key[..8].copy_from_slice(&index.to_be_bytes());
        key[8] = u8::from(opaque);
        Uuid::new_v5(&self.thread_id, &key)
    }

    pub(crate) fn set_last_database_step_hash(&mut self, hash: Option<Uuid>) {
        if let Some(event) = self.events.last_mut()
            && event.sequence + 1 == self.next_sequence
        {
            event.raw_blob_hash = hash.map(|hash| hash.to_string());
        }
    }

    pub(crate) fn retain_latest_tool_events(&mut self, limit: usize) -> usize {
        if let Some(preview) = &mut self.preview {
            return retain_latest_matching(&mut preview.outline, limit, |event| event.tool);
        }
        retain_latest_matching(&mut self.events, limit, |event| is_tool_event(&event.kind))
    }

    /// Drops the oldest events above `limit` and returns how many were dropped.
    pub(crate) fn retain_latest_events(&mut self, limit: usize) -> usize {
        self.purge_removed_mirrors();
        if let Some(preview) = &mut self.preview {
            let omitted = preview.outline.len().saturating_sub(limit);
            preview.outline.drain(..omitted);
            return omitted;
        }
        let omitted = self.events.len().saturating_sub(limit);
        self.events.drain(..omitted);
        omitted
    }

    pub(crate) fn snapshot(
        mut self,
        session: SessionRef,
        title: Option<String>,
        project_path: Option<PathBuf>,
        git_branch: Option<String>,
        captured_at: DateTime<Utc>,
    ) -> CanonicalSnapshot {
        self.purge_removed_mirrors();
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
        DEFAULT_SQLITE_SNAPSHOT_MAX_BYTES, EventBuilder, IndexScan, JsonLineTail, JsonLinesLimits,
        MAX_TRANSCRIPT_LINE_SIZE, SnapshotLimits, json_lines_prefix, json_lines_preview,
        json_lines_tail_with_offsets, nested_files_with_limit, sample_files_concurrently,
        snapshot_max_bytes, sqlite_snapshot, sqlite_snapshot_with_limits,
        visit_index_json_lines_with_limits, visit_json_lines_with_limits,
    };

    #[test]
    fn over_limit_search_rereads_previous_text_and_keeps_the_latest_turn() {
        use std::io::{Seek, SeekFrom, Write};

        let mut file = tempfile::tempfile().unwrap();
        file.write_all(b"\"first\"\n\"middle\"\n\"unsampled\"\n")
            .unwrap();
        file.seek(SeekFrom::Start(2 * super::MAX_PREVIEW_TAIL_SIZE))
            .unwrap();
        file.write_all(b"\n\"latest\"").unwrap();
        file.rewind().unwrap();
        let mut words = Vec::new();
        let mut bytes = 0;
        super::visit_limited_search_edges(
            file,
            Some(11),
            1,
            JsonLineTail::RequireComplete,
            |value| {
                let word = value.as_str().unwrap().to_owned();
                bytes += word.len();
                words.push(word);
                bytes
            },
        )
        .unwrap();
        assert_eq!(words, ["first", "middle", "latest"]);
    }

    #[test]
    fn limited_search_counts_malformed_lines_inside_its_windows() {
        use std::io::{Seek, SeekFrom, Write};
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(b"{}\n{broken}\n\"").unwrap();
        // Put the malformed records in disjoint sample windows.
        file.seek(SeekFrom::Start(2 * super::MAX_PREVIEW_TAIL_SIZE + 1))
            .unwrap();
        file.write_all(b"\n{also broken}\n\n\"latest\"").unwrap();
        let damage = super::JsonLineDamage::default();
        let mut records = Vec::new();
        super::visit_limited_search_edges(
            file,
            None,
            8,
            JsonLineTail::ReportIncomplete(&damage),
            |v| {
                records.push(v);
                0
            },
        )
        .unwrap();
        assert_eq!(records, [json!({}), json!("latest")]);
        assert_eq!(damage.count(), 2);
        assert!(!damage.final_record());
    }

    #[test]
    fn incomplete_jsonl_tail_preserves_earlier_records_until_repair() {
        let root = tempdir().unwrap();
        let path = root.path().join("transcript.jsonl");
        let preview =
            || super::json_lines_preview_with_tail(&path, 2, JsonLineTail::RequireComplete);
        let padding = format!("\n{}", " \t\n".repeat(4_096));
        for ending in ["", "\r\n", "\n\n", padding.as_str()] {
            std::fs::write(&path, format!("{{}}\n{{\"text\":\"half{ending}")).unwrap();
            assert!(preview().unwrap_err().is::<super::IncompleteTranscript>());
            let stream = super::visit_json_lines_with_tail(
                &path,
                super::MAX_PREVIEW_TAIL_SIZE,
                JsonLineTail::RequireComplete,
                |_| Ok(()),
            );
            assert!(stream.unwrap_err().is::<super::IncompleteTranscript>());
            let edges = super::visit_limited_search_edges(
                std::fs::File::open(&path).unwrap(),
                None,
                1,
                JsonLineTail::RequireComplete,
                |_| 0,
            );
            assert!(edges.unwrap_err().is::<super::IncompleteTranscript>());
            assert_eq!(json_lines_preview(&path, 2).unwrap(), [json!({})]);
        }

        for ending in ["\n", "\r\n", "\n\n"] {
            std::fs::write(&path, format!("{{}}\n{{broken}}{ending}")).unwrap();
            assert!(preview().unwrap_err().is::<super::IncompleteTranscript>());
            let omitted = super::JsonLineDamage::default();
            let values = super::json_lines_preview_with_tail(
                &path,
                2,
                JsonLineTail::ReportIncomplete(&omitted),
            )
            .unwrap();
            assert_eq!(values, [json!({})]);
            assert_eq!(omitted.count(), 1);
        }

        std::fs::write(&path, b"{}\n{discarded\n{\"text\":\"whole\"}").unwrap();
        assert!(preview().unwrap_err().is::<super::IncompleteTranscript>());
        let damage = super::JsonLineDamage::default();
        assert_eq!(
            super::json_lines_preview_with_tail(
                &path,
                2,
                JsonLineTail::RequireCompleteTail(&damage)
            )
            .unwrap(),
            [json!({}), json!({"text":"whole"})]
        );
        assert_eq!(damage.count(), 1);
    }

    #[test]
    fn a_short_read_of_an_observed_transcript_returns_unexpected_eof() {
        let root = tempdir().unwrap();
        let path = root.path().join("transcript.jsonl");
        std::fs::write(&path, b"1\n2\n3\n").unwrap();
        let writer = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        let mut reader = std::io::BufReader::with_capacity(2, std::fs::File::open(path).unwrap());
        let error = super::visit_json_lines_with_offsets_and_limits(
            &mut reader,
            JsonLinesLimits::streamed(64),
            JsonLineTail::RequireComplete,
            |_, _, _| Ok(writer.set_len(2)?),
        )
        .unwrap_err();
        assert_eq!(
            error.downcast_ref::<std::io::Error>().unwrap().kind(),
            std::io::ErrorKind::UnexpectedEof
        );
    }

    #[test]
    fn a_final_fragment_needs_a_known_record_start_within_the_validation_budget() {
        let root = tempdir().unwrap();
        let path = root.path().join("transcript.jsonl");
        let valid = br#"{"content":"a complete response"}"#;
        let mut bytes = b"{}\n".to_vec();
        bytes.extend_from_slice(valid);
        std::fs::write(&path, &bytes).unwrap();
        let mut file = std::fs::File::open(path).unwrap();
        let budget = valid.len() as u64;
        let end = bytes.len() as u64;
        JsonLineTail::RequireComplete
            .check_final_fragment(&mut file, end, budget)
            .unwrap();
        assert!(
            JsonLineTail::RequireComplete
                .check_final_fragment(&mut file, end, budget - 1)
                .unwrap_err()
                .is::<super::IncompleteTranscript>()
        );
    }

    #[test]
    fn valid_unterminated_final_records_do_not_hide_the_search_preview() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("transcript.jsonl");
        let limit = usize::try_from(MAX_TRANSCRIPT_LINE_SIZE).unwrap();
        for size in [limit - 32, limit + 32] {
            let final_record = serde_json::to_string(&"x".repeat(size)).unwrap();
            std::fs::write(&path, format!("42\n{final_record}")).unwrap();
            let records =
                super::json_lines_preview_with_tail(&path, 2, JsonLineTail::RequireComplete)
                    .unwrap();
            assert_eq!(records.first(), Some(&json!(42)));
            assert_eq!(records.len(), if size < limit { 2 } else { 1 });
            std::fs::write(
                &path,
                format!("42\n{}", &final_record[..final_record.len() - 1]),
            )
            .unwrap();
            assert!(
                super::json_lines_preview_with_tail(&path, 2, JsonLineTail::RequireComplete)
                    .unwrap_err()
                    .is::<super::IncompleteTranscript>()
            );
        }
        let window = usize::try_from(super::MAX_PREVIEW_TAIL_SIZE).unwrap();
        let final_record = serde_json::to_string(&"x".repeat(window - 2)).unwrap();
        std::fs::write(&path, format!("42\n43\n44\n{final_record}")).unwrap();
        let records =
            super::json_lines_preview_with_tail(&path, 2, JsonLineTail::RequireComplete).unwrap();
        assert_eq!(records, [json!(42), json!(43)]);
    }

    #[cfg(unix)]
    #[test]
    fn provider_files_resolve_like_provider_file() {
        use std::os::unix::fs::symlink;

        use super::{ProviderFiles, provider_file};

        let temporary = tempdir().expect("temporary directory");
        let root = temporary.path().join("root");
        let outside = temporary.path().join("outside");
        std::fs::create_dir_all(root.join("project")).expect("project directory");
        std::fs::create_dir_all(&outside).expect("outside directory");
        std::fs::write(root.join("project/session.jsonl"), "{}").expect("session file");
        std::fs::write(outside.join("escaped.jsonl"), "{}").expect("outside file");
        symlink(
            root.join("project/session.jsonl"),
            root.join("project/link.jsonl"),
        )
        .expect("file symlink");
        symlink(&outside, root.join("linked-directory")).expect("directory symlink");
        symlink(root.join("project"), root.join("alias")).expect("inner directory symlink");

        let mut files = ProviderFiles::new(&root);
        for candidate in [
            "project/session.jsonl",
            "project/link.jsonl",
            "project/missing.jsonl",
            "project",
            "linked-directory/escaped.jsonl",
            "alias/session.jsonl",
        ] {
            let candidate = root.join(candidate);
            assert_eq!(
                files.resolve(&candidate),
                provider_file(&root, &candidate),
                "{}",
                candidate.display()
            );
        }
        assert!(files.resolve(&root.join("project/session.jsonl")).is_some());
        assert!(
            files
                .resolve(&root.join("linked-directory/escaped.jsonl"))
                .is_none()
        );

        // A directory that is replaced after its first file was resolved is resolved again.
        std::fs::rename(root.join("project"), root.join("moved")).expect("move project");
        symlink(&outside, root.join("project")).expect("replace project with a symlink");
        let escaped = root.join("project/escaped.jsonl");
        assert_eq!(files.resolve(&escaped), None);
        assert_eq!(provider_file(&root, &escaped), None);
    }

    #[cfg(unix)]
    #[test]
    fn walk_skips_files_of_a_directory_replaced_while_it_is_listed() {
        use std::{cell::Cell, os::unix::fs::symlink};

        let temporary = tempdir().expect("temporary directory");
        let root = temporary.path().join("root");
        let outside = temporary.path().join("outside");
        std::fs::create_dir_all(root.join("project")).expect("project directory");
        std::fs::create_dir_all(&outside).expect("outside directory");
        for name in ["a.jsonl", "b.jsonl", "c.jsonl"] {
            std::fs::write(root.join("project").join(name), "{}").expect("session file");
            std::fs::write(outside.join(name), "{}").expect("outside file");
        }

        // The filter runs for each listed file, so it can replace the directory after the first
        // file was admitted.
        let admitted = Cell::new(0);
        let files = nested_files_with_limit(&root, 2, usize::MAX, usize::MAX, &|_, is_dir| {
            if !is_dir {
                if admitted.get() == 1 {
                    std::fs::rename(root.join("project"), root.join("moved"))
                        .expect("move project");
                    symlink(&outside, root.join("project")).expect("replace project");
                }
                admitted.set(admitted.get() + 1);
            }
            true
        });

        assert_eq!(files.len(), 1, "{files:?}");
    }

    #[test]
    fn concurrent_sampling_keeps_file_order() {
        let files = (0..1_000).collect::<Vec<usize>>();
        let sampled = sample_files_concurrently(&files, |slice| {
            slice
                .iter()
                .filter(|file| **file % 3 != 0)
                .copied()
                .collect()
        });
        let expected = files
            .iter()
            .filter(|file| **file % 3 != 0)
            .copied()
            .collect::<Vec<_>>();
        assert_eq!(sampled, expected);
    }

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

    #[cfg(unix)]
    #[test]
    fn stamped_snapshot_reuses_the_copy_and_refreshes_on_a_wal_commit() -> anyhow::Result<()> {
        use std::os::unix::fs::PermissionsExt;

        let (root, db, writer) = wal_store();
        let db = db.canonicalize()?;
        let cache = super::SqliteSnapshotCache::default();
        let read = || {
            cache.read_stamped(root.path(), &db, |conn| {
                let rows: i64 =
                    conn.query_row("SELECT COUNT(*) FROM blobs", [], |row| row.get(0))?;
                let path = Path::new(conn.path().expect("private database path"));
                let directory = path.parent().expect("private copy directory");
                assert_eq!(
                    std::fs::metadata(directory)?.permissions().mode() & 0o077,
                    0
                );
                Ok((rows, path.to_owned()))
            })
        };
        let before = directory_bytes(root.path());
        let first = read()?;
        assert_eq!(first.0, 2);
        let second = read()?;
        if super::SnapshotStamp::read(root.path(), &db, u64::MAX, true)?.has_precise_change_times()
        {
            assert_eq!(first, second);
        }
        assert_eq!(directory_bytes(root.path()), before);

        writer.execute("INSERT INTO blobs (data) VALUES (zeroblob(512))", [])?;
        let written = directory_bytes(root.path());
        let next = read()?;
        assert_eq!(next.0, 3);
        assert_ne!(first.1, next.1);
        assert_eq!(directory_bytes(root.path()), written);
        Ok(())
    }

    #[test]
    fn live_wal_snapshot_accepts_appends_and_rejects_a_restarted_generation() -> anyhow::Result<()>
    {
        let (root, database, writer) = wal_store();
        let db = database.canonicalize()?;
        let wal = super::sidecar(&db, "-wal");
        let limits = SnapshotLimits {
            max_bytes: 1024 * 1024,
            free_space_reserve: 0,
        };
        let observe = || -> anyhow::Result<_> {
            let stamp = super::SnapshotStamp::read(root.path(), &db, limits.max_bytes, true)?;
            let len = stamp.wal.as_ref().expect("fixture WAL").len;
            let prefix = super::WalPrefix::capture(&wal, len)?.expect("fixture WAL prefix");
            Ok((stamp, prefix))
        };
        let (initial, prefix) = observe()?;
        writer.execute("INSERT INTO blobs (data) VALUES (zeroblob(512))", [])?;
        let snapshot = initial
            .copy_live_wal(root.path(), &db, limits, &prefix)?
            .expect("unchanged WAL prefix");
        let rows: i64 = snapshot
            .connection
            .query_row("SELECT COUNT(*) FROM blobs", [], |row| row.get(0))?;
        assert_eq!(rows, 2);

        let restart = || -> anyhow::Result<()> {
            let (busy, frames, copied): (i64, i64, i64) =
                writer.query_row("PRAGMA wal_checkpoint(RESTART)", [], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                })?;
            assert_eq!(busy, 0);
            assert_eq!(frames, copied);
            Ok(())
        };
        restart()?;
        let (checkpointed, prefix) = observe()?;
        let previous = std::fs::read(&wal)?;
        writer.execute("INSERT INTO blobs (data) VALUES (zeroblob(65536))", [])?;
        let restarted = std::fs::read(&wal)?;
        assert!(restarted.len() >= previous.len());
        assert_ne!(previous[16..24], restarted[16..24]);
        assert!(
            checkpointed
                .copy_live_wal(root.path(), &db, limits, &prefix)?
                .is_none()
        );

        restart()?;
        let physical_len = std::fs::metadata(&wal)?.len();
        let insert = "INSERT INTO blobs (data) VALUES (zeroblob(128))";
        writer.execute(insert, [])?;
        let (small, prefix) = observe()?;
        assert!(prefix.len < physical_len);
        let count = "SELECT COUNT(*) FROM blobs";
        let at_capture: i64 = writer.query_row(count, [], |row| row.get(0))?;
        for _ in 0..3 {
            writer.execute(insert, [])?;
        }
        assert_eq!(std::fs::metadata(&wal)?.len(), physical_len);
        assert_eq!(
            writer.query_row(count, [], |row| row.get::<_, i64>(0))?,
            at_capture + 3
        );
        let copy = small
            .copy_live_wal(root.path(), &db, limits, &prefix)?
            .expect("valid commits within the preallocated WAL");
        assert_eq!(
            copy.connection
                .query_row(count, [], |row| row.get::<_, i64>(0))?,
            at_capture
        );
        Ok(())
    }

    #[test]
    fn stamped_registry_recovers_when_the_last_writer_closes_its_wal() -> anyhow::Result<()> {
        let (root, database, writer) = wal_store();
        let db = database.canonicalize()?;
        writer.execute_batch(
            "CREATE TABLE threads(title TEXT); INSERT INTO threads VALUES ('updated title')",
        )?;
        let stamp = super::SnapshotStamp::read(root.path(), &db, u64::MAX, true)?;
        let wal = super::sidecar(&db, "-wal");
        let len = stamp.wal.as_ref().unwrap().len;
        let prefix = super::WalPrefix::capture(&wal, len)?.unwrap();
        drop(writer);
        assert!(!wal.exists());
        assert!(super::WalPrefix::capture(&wal, len)?.is_none());
        let limits = SnapshotLimits::from_environment()?;
        assert!(
            stamp
                .copy_live_wal(root.path(), &db, limits, &prefix)?
                .is_none()
        );
        let title: String =
            super::SqliteSnapshotCache::default().read_stamped(root.path(), &db, |conn| {
                Ok(conn.query_row("SELECT title FROM threads", [], |row| row.get(0))?)
            })?;
        assert_eq!(title, "updated title");
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn stamped_snapshot_rejects_a_changed_database_or_wal() -> anyhow::Result<()> {
        use super::{SnapshotStamp, sidecar};

        let (root, db, writer) = wal_store();
        let db = db.canonicalize()?;
        let limits = SnapshotLimits {
            max_bytes: 256 * 1024,
            free_space_reserve: 0,
        };
        let observe = || SnapshotStamp::read(root.path(), &db, limits.max_bytes, true);
        let prior = observe()?;
        writer.execute("INSERT INTO blobs (data) VALUES (zeroblob(512))", [])?;
        assert!(prior.copy_snapshot(root.path(), &db, limits)?.is_none());

        let with_wal = observe()?;
        drop(writer);
        let closed = observe()?;
        assert!(with_wal.wal.is_some() && closed.wal.is_none());
        assert!(with_wal.copy_snapshot(root.path(), &db, limits)?.is_none());

        let copy = root.path().join("replacement");
        std::fs::copy(&db, &copy)?;
        std::fs::rename(copy, &db)?;
        assert!(closed.copy_snapshot(root.path(), &db, limits)?.is_none());

        let no_wal = observe()?;
        let writer = rusqlite::Connection::open(&db)?;
        writer.execute("INSERT INTO blobs (data) VALUES (zeroblob(512))", [])?;
        assert!(no_wal.copy_snapshot(root.path(), &db, limits)?.is_none());
        drop(writer);
        std::os::unix::fs::symlink(&db, sidecar(&db, "-wal"))?;
        assert!(observe().is_err());
        assert!(super::SqliteSourceStamp::read(root.path(), &db).is_none());
        std::fs::remove_file(sidecar(&db, "-wal"))?;
        std::fs::write(sidecar(&db, "-journal"), b"pending transaction")?;
        assert!(observe().is_err());
        assert!(super::SqliteSourceStamp::read(root.path(), &db).is_none());
        Ok(())
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
            copied <= budget,
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
        let oversized =
            visit_json_lines_with_limits(path, limits, JsonLineTail::SkipMalformed, |_| {
                visited += 1;
                Ok(())
            })?;
        Ok((visited, oversized))
    }

    #[test]
    fn preview_edges_keep_unverified_history_out_of_the_verified_prefix() {
        for limit in [2, 4] {
            for planned in [true, false] {
                let mut builder = EventBuilder::new(Provider::Codex, "uncertain-prefix");
                if planned {
                    builder.plan_preview(None);
                }
                for (kind, payload) in [
                    (EventKind::MessageUser, json!({"text": "first"})),
                    (
                        EventKind::ProviderEvent,
                        json!({"native_history_unverified": true}),
                    ),
                    (EventKind::MessageAssistant, json!({"text": "last"})),
                ] {
                    builder.push(kind, payload, None, ReplayPolicy::Contextual, None, None);
                }
                let result = if planned {
                    let (_, omitted, end) = builder.select_preview(limit, 4096);
                    (omitted, end)
                } else {
                    builder.retain_bounded_preview(limit, 4096)
                };
                assert_eq!(result, (usize::from(limit == 2), 0));
            }
        }
    }

    #[test]
    fn materialized_preview_uses_the_size_of_revised_events() {
        let mut builder = EventBuilder::new(Provider::Codex, "revised-size");
        for text in ["original", "surviving head", "surviving tail"] {
            builder.push(
                EventKind::MessageAssistant,
                json!({"text": text}),
                None,
                ReplayPolicy::Contextual,
                None,
                None,
            );
        }
        builder.event_at_mut(0).unwrap().payload["text"] = "x".repeat(8192).into();
        assert_eq!(builder.retain_bounded_preview(2, 4096), (1, 0));

        let events = &builder.events;
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].payload["text"], "surviving head");
        assert_eq!(events[1].payload["text"], "surviving tail");
    }

    #[test]
    fn physical_records_report_damage_and_keep_offsets_across_blank_lines() {
        use super::{
            JsonlRecord, WorkspaceGap, visit_json_reader, visit_streamed_json_records_with_offsets,
        };

        let (_root, path) = write_fixture(b"1\r\n{x}\n\xff\n \t\r\n2\n3");
        let mut reader = std::io::BufReader::new(std::fs::File::open(&path).unwrap());
        let mut records = Vec::new();
        let (omissions, bytes) = visit_json_reader(
            &mut reader,
            JsonLinesLimits::streamed(64),
            |_, _| WorkspaceGap::Other,
            |location, value, omitted| {
                records.push((
                    location.offset,
                    location.length,
                    location.terminated,
                    value.as_u64(),
                    omitted.malformed,
                ));
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(
            records,
            [
                (0, 3, true, Some(1), 0),
                (13, 2, true, Some(2), 2),
                (15, 1, false, Some(3), 2),
            ]
        );
        assert_eq!((omissions.malformed, bytes), (2, 16));

        let mut records = Vec::new();
        visit_streamed_json_records_with_offsets(&mut reader, 64, |location, record| {
            let (kind, value) = match record {
                JsonlRecord::Complete(v) => ("complete", v.as_u64()),
                JsonlRecord::Unterminated(v) => ("unfinished", v.as_u64()),
                JsonlRecord::Malformed => ("malformed", None),
            };
            records.push((location.offset, location.length, kind, value));
            Ok(())
        })
        .unwrap();
        assert_eq!(
            records,
            [
                (0, 3, "complete", Some(1)),
                (3, 4, "malformed", None),
                (7, 2, "malformed", None),
                (13, 2, "complete", Some(2)),
                (15, 1, "unfinished", Some(3)),
            ]
        );
        assert_eq!(visit_all(&path, LIMITS).unwrap(), (3, 0));
        let small = super::complete_small_json_records(&path).unwrap().unwrap();
        assert!(matches!(small.as_slice(), [
            JsonlRecord::Complete(first), JsonlRecord::Malformed, JsonlRecord::Malformed,
            JsonlRecord::Complete(second), JsonlRecord::Unterminated(last)
        ] if [first.as_u64(), second.as_u64(), last.as_u64()] == [Some(1), Some(2), Some(3)]));
    }

    #[test]
    fn tail_exposes_a_skipped_prefix_even_when_the_first_retained_line_is_blank() {
        use std::io::{Seek, SeekFrom, Write};

        use super::{JsonLine, MAX_PREVIEW_TAIL_SIZE, visit_json_lines_tail};

        let (_root, path) = write_fixture([]);
        let prefix = MAX_PREVIEW_TAIL_SIZE + 5;
        let mut file = std::fs::File::create(&path).unwrap();
        file.seek(SeekFrom::Start(prefix)).unwrap();
        file.write_all(b"\n \n1\n2").unwrap();

        let mut rows = Vec::new();
        let first = visit_json_lines_tail(
            &path,
            32,
            |_| true,
            |offset, line| {
                let (terminated, value) = match line {
                    JsonLine::Record(value) => (true, value),
                    JsonLine::Unterminated(value) => (false, value),
                    _ => panic!("expected a valid value"),
                };
                rows.push((offset, terminated, value.as_u64()));
            },
        )
        .unwrap();

        assert_eq!(first, prefix + 1);
        assert_eq!(
            rows,
            [(prefix + 3, true, Some(1)), (prefix + 5, false, Some(2))]
        );
    }

    fn index_values(path: &Path, file_limit: u64, line_limit: u64) -> (Vec<u64>, IndexScan) {
        let mut values = Vec::new();
        let scan = visit_index_json_lines_with_limits(path, file_limit, line_limit, |record| {
            values.extend(record.and_then(|record| record["n"].as_u64()));
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
    fn windows_path_lookup_never_selects_extensionless_scripts() {
        use std::ffi::OsStr;

        use super::{is_batch_launcher, windows_executable_candidates};

        let npm = Path::new(r"C:\Users\developer\AppData\Roaming\npm");
        let defaults = windows_executable_candidates(npm, "opencode", None);
        assert!(!defaults.iter().any(|path| path.ends_with("opencode")));
        // npm's `opencode.cmd` remains a candidate for validated Node launch preparation.
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
                        malformed_records: 0,
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
                        malformed_records: 0,
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
    fn tail_keeps_complete_records_at_the_byte_limit() {
        let mut content = b"0\n123\n".to_vec();
        let tail_size = usize::try_from(super::MAX_PREVIEW_TAIL_SIZE).unwrap();
        // The final records fill the rest of the window without exceeding the line limit.
        for value in *b"45" {
            content.push(value);
            content.resize(content.len() + tail_size / 2 - 4, b' ');
            content.push(b'\n');
        }
        let (_temporary, path) = write_fixture(&content);
        let read =
            || json_lines_tail_with_offsets(&mut std::fs::File::open(&path).unwrap(), 3).unwrap();
        let read_verified = || {
            let mut file = std::fs::File::open(&path).unwrap();
            let len = file.metadata().unwrap().len();
            super::verified_json_lines_tail_with_offsets(
                &mut file,
                3,
                JsonLineTail::SkipMalformed,
                len,
                0,
            )
            .unwrap()
            .0
        };
        assert_eq!(read()[0], (2, json!(123)));
        assert_eq!(read_verified(), read());

        // Moving the cutoff into the number must discard the entire partial record.
        content.push(b'\n');
        std::fs::write(&path, content).unwrap();
        assert_eq!(read()[0], (6, json!(4)));
        assert_eq!(read_verified(), read());
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

        let records = super::verified_json_lines_tail_with_offsets(
            &mut std::fs::File::open(&path).unwrap(),
            2,
            JsonLineTail::SkipMalformed,
            std::fs::metadata(&path).unwrap().len(),
            0,
        )
        .expect("tail records")
        .0
        .into_iter()
        .map(|(_, record)| record)
        .collect::<Vec<_>>();

        assert_eq!(records[0]["value"], 8);
        assert_eq!(records[1]["value"], 9);
    }

    #[test]
    fn seekable_tail_keeps_a_complete_first_row_and_skips_a_partial_one() {
        let temporary = tempdir().unwrap();
        let path = temporary.path().join("records.jsonl");
        let header = "{\"index\":0}\r\n";
        let first = "{\"index\":1}\r\n";
        let last = "{\"index\":2}";
        let tail_bytes = usize::try_from(super::MAX_PREVIEW_TAIL_SIZE).unwrap();
        let padding = " ".repeat(tail_bytes - first.len() - last.len() - 2);
        let mut contents = format!("{header}{first}{padding}\r\n{last}");
        std::fs::write(&path, &contents).unwrap();
        let records =
            json_lines_tail_with_offsets(&mut std::fs::File::open(&path).unwrap(), 2).unwrap();
        assert_eq!(
            records,
            [
                (header.len() as u64, json!({"index":1})),
                ((contents.len() - last.len()) as u64, json!({"index":2}))
            ]
        );
        assert_eq!(
            json_lines_preview(&path, 2).unwrap(),
            [json!({"index":0}), json!({"index":1}), json!({"index":2})]
        );

        contents.push(' ');
        std::fs::write(&path, &contents).unwrap();
        let records =
            json_lines_tail_with_offsets(&mut std::fs::File::open(&path).unwrap(), 2).unwrap();
        assert_eq!(
            records,
            [((contents.len() - last.len() - 1) as u64, json!({"index":2}))]
        );
    }

    #[test]
    fn sampled_opener_uses_the_contiguous_prefix_across_overlapping_windows() {
        let root = tempdir().unwrap();
        let path = root.path().join("sample.jsonl");
        let padding = json!({"padding": "x".repeat(768 * 1024)}).to_string() + "\n";
        let prefix = padding.repeat(6);
        for (tail, confirmed) in [
            ("\n1\n2\n", 8),
            ("1\n \t\r\n2\n", 8),
            ("1\n2\n\n", 8),
            ("broken\n1\n2\n", 6),
            ("1\nbroken\n2\n", 7),
            ("1\n2\nbroken\n", 8),
        ] {
            std::fs::write(&path, format!("{prefix}{tail}")).unwrap();
            let damage = super::JsonLineDamage::default();
            let (records, verified) = super::json_lines_preview_with_prefix(
                &path,
                6,
                JsonLineTail::ReportIncomplete(&damage),
            )
            .unwrap();
            assert_eq!(&records[6..], &[json!(1), json!(2)]);
            assert_eq!(verified, confirmed, "{tail:?}");
        }
    }

    #[test]
    fn jsonl_preview_tracks_the_consecutive_suffix_across_read_windows() {
        let temporary = tempdir().unwrap();
        let path = temporary.path().join("window.jsonl");
        let padding = "x".repeat(768 * 1024);
        let encode = |id| json!({"id":id,"padding":&padding}).to_string() + "\n";
        let prefix = (0..3).map(encode).collect::<String>();
        let suffix = (3..7).map(encode).collect::<String>();
        let oversized =
            json!({"padding":"x".repeat(usize::try_from(MAX_TRANSCRIPT_LINE_SIZE).unwrap())})
                .to_string()
                + "\n";
        for (gap, limit, ids, expected) in [
            ("", 7, vec![0, 1, 2, 3, 4, 5, 6], 7),
            ("", 4, vec![0, 1, 2, 3, 4, 5, 6], 7),
            ("", 2, vec![0, 1, 5, 6], 2),
            ("invalid\n", 8, vec![0, 1, 2, 3, 4, 5, 6], 4),
            ("invalid\n", 5, vec![0, 1, 2, 3, 4, 5, 6], 4),
            ("invalid\n", 3, vec![0, 1, 2, 4, 5, 6], 3),
            (oversized.as_str(), 8, vec![0, 1, 2, 3, 4, 5, 6], 4),
        ] {
            std::fs::write(&path, format!("{prefix}{gap}{suffix}")).unwrap();
            let (records, count) = super::json_lines_preview_file_with_suffix(
                &mut std::fs::File::open(&path).unwrap(),
                limit,
            )
            .unwrap();
            assert_eq!(
                records
                    .iter()
                    .map(|r| r["id"].as_i64().unwrap())
                    .collect::<Vec<_>>(),
                ids
            );
            assert_eq!(count, expected);
        }
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
    fn small_preview_samples_the_tail_after_the_record_limit() {
        let temporary = tempdir().expect("temporary directory");
        let path = temporary.path().join("many-short-lines.jsonl");
        let mut content = "1\n".to_owned();
        content.push_str(&"0\n".repeat(super::MAX_PROVIDER_RECORDS));
        content.push_str("2\n");
        std::fs::write(&path, content).expect("short JSONL records");

        for (prefix_bytes, expected, suffix_len) in [
            (u64::MAX, vec![json!(1), json!(2)], 1),
            (super::MAX_PREVIEW_TAIL_SIZE, vec![json!(1), json!(2)], 1),
            (2, vec![json!(1)], 0),
        ] {
            let preview = super::json_lines_preview_with_omissions(
                &path,
                1,
                prefix_bytes,
                MAX_TRANSCRIPT_LINE_SIZE,
                |_, _| super::WorkspaceGap::Other,
            )
            .expect("sampled end records");
            assert!(preview.sampled);
            assert_eq!(preview.records, expected);
            assert_eq!(preview.suffix_len, suffix_len);
        }
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

        assert!(super::complete_small_json_records(&path).unwrap().is_none());
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
            let _ = visit_json_lines_with_limits(&path, limits, JsonLineTail::SkipMalformed, |_| Ok(()));
            let scan = visit_index_json_lines_with_limits(&path, file_bytes, line_bytes, |_| {})
                .expect("index scan");
            prop_assert!(scan.skipped_bytes <= bytes.len() as u64);
            let _ = json_lines_prefix(&path, records);
            let _ = json_lines_preview(&path, records);
            let _ = super::verified_json_lines_tail_with_offsets(&mut std::fs::File::open(&path).unwrap(), records, JsonLineTail::SkipMalformed, bytes.len() as u64, 0);
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
            prop_assert_eq!(scan, IndexScan { skipped_bytes: first_kept, ..IndexScan::default() });
        }
    }
}
