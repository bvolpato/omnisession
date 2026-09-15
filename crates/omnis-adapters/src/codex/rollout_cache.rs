//! Codex rollout headers persisted across listings.
//!
//! Opening thousands of rollouts dominates Codex listing, while `stat` is cheap. A cached header is
//! reused only while the rollout's size, timestamps, and file identity are unchanged, so appended,
//! rewritten, or replaced rollouts are parsed again.

use std::{
    collections::HashMap,
    fs,
    io::{self, ErrorKind, Read, Write},
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::support::provider_root;

/// Files written by another cache format or build are ignored and replaced.
const CACHE_FORMAT: &str = concat!("1-", env!("CARGO_PKG_VERSION"));
/// Largest cache file read. The 10,000-rollout scan limit keeps real caches far smaller.
const MAX_CACHE_FILE_SIZE: u64 = 64 * 1024 * 1024;

/// Returns the default cache file for one Codex home under `OMNISESSION_HOME`.
pub(super) fn default_path(codex_home: &Path) -> Option<PathBuf> {
    let state_root = provider_root("OMNISESSION_HOME", &[".omnisession"])?;
    let digest = Sha256::digest(codex_home.as_os_str().as_encoded_bytes());
    Some(
        state_root
            .join("cache")
            .join(format!("codex-rollouts-{}.json", hex::encode(&digest[..8]))),
    )
}

/// Stat fields that change when a rollout is written or replaced.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct RolloutStamp {
    len: u64,
    modified: (i64, u32),
    /// Inode change time on Unix, creation time elsewhere.
    changed: (i64, i64),
    inode: u64,
}

impl RolloutStamp {
    pub(super) fn new(metadata: &fs::Metadata) -> Option<Self> {
        let modified = metadata.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
        #[cfg(unix)]
        let (changed, inode) = {
            use std::os::unix::fs::MetadataExt;
            ((metadata.ctime(), metadata.ctime_nsec()), metadata.ino())
        };
        #[cfg(not(unix))]
        let (changed, inode) = {
            let created = metadata.created().ok()?.duration_since(UNIX_EPOCH).ok()?;
            (
                (
                    i64::try_from(created.as_secs()).ok()?,
                    i64::from(created.subsec_nanos()),
                ),
                0,
            )
        };
        Some(Self {
            len: metadata.len(),
            modified: (
                i64::try_from(modified.as_secs()).ok()?,
                modified.subsec_nanos(),
            ),
            changed,
            inode,
        })
    }
}

/// Session metadata from a rollout's `session_meta` record.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct RolloutHeader {
    pub(super) id: String,
    pub(super) project_path: Option<PathBuf>,
    pub(super) git_branch: Option<String>,
    pub(super) cli_version: Option<String>,
    pub(super) is_subagent: bool,
    pub(super) created_at: Option<DateTime<Utc>>,
}

#[derive(Deserialize, Serialize)]
pub(super) struct CachedRollout {
    stamp: RolloutStamp,
    /// `None` records a rollout without user session metadata.
    pub(super) header: Option<RolloutHeader>,
}

#[derive(Deserialize, Serialize)]
struct CacheFile {
    format: String,
    codex_home: String,
    rollouts: HashMap<String, CachedRollout>,
}

/// Headers loaded from one cache file, plus the entries this listing confirmed or parsed.
pub(super) struct RolloutCache {
    path: PathBuf,
    codex_home: String,
    previous: HashMap<String, CachedRollout>,
    current: HashMap<String, CachedRollout>,
    parsed: bool,
}

impl RolloutCache {
    /// Loads cached headers for `codex_home`. A missing, unreadable, or stale file starts empty.
    pub(super) fn load(path: &Path, codex_home: &Path) -> Option<Self> {
        let codex_home = codex_home.to_str()?.to_owned();
        let previous = read_cache_file(path)
            .filter(|file| file.format == CACHE_FORMAT && file.codex_home == codex_home)
            .map(|file| file.rollouts)
            .unwrap_or_default();
        Some(Self {
            path: path.to_path_buf(),
            codex_home,
            previous,
            current: HashMap::new(),
            parsed: false,
        })
    }

    /// Returns the cached entry while the rollout's stamp is unchanged.
    pub(super) fn get(&mut self, rollout: &Path, stamp: RolloutStamp) -> Option<&CachedRollout> {
        let key = rollout.to_str()?;
        let cached = self
            .previous
            .remove(key)
            .filter(|cached| cached.stamp == stamp)?;
        Some(&*self.current.entry(key.to_owned()).or_insert(cached))
    }

    pub(super) fn insert(
        &mut self,
        rollout: &Path,
        stamp: RolloutStamp,
        header: Option<RolloutHeader>,
    ) {
        if let Some(key) = rollout.to_str() {
            self.current
                .insert(key.to_owned(), CachedRollout { stamp, header });
            self.parsed = true;
        }
    }

    /// Persists this listing's entries when a rollout was parsed or disappeared.
    ///
    /// The cache only saves time, so write failures are ignored.
    pub(super) fn save(self) {
        if !self.parsed && self.previous.is_empty() {
            return;
        }
        let _ = write_cache_file(
            &self.path,
            &CacheFile {
                format: CACHE_FORMAT.to_owned(),
                codex_home: self.codex_home,
                rollouts: self.current,
            },
        );
    }
}

fn read_cache_file(path: &Path) -> Option<CacheFile> {
    let mut bytes = Vec::new();
    fs::File::open(path)
        .ok()?
        .take(MAX_CACHE_FILE_SIZE + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if u64::try_from(bytes.len()).ok()? > MAX_CACHE_FILE_SIZE {
        return None;
    }
    serde_json::from_slice(&bytes).ok()
}

fn write_cache_file(path: &Path, cache: &CacheFile) -> io::Result<()> {
    let directory = path
        .parent()
        .ok_or_else(|| io::Error::from(ErrorKind::InvalidInput))?;
    // The store refuses a symlinked state root, so the cache does too.
    if let Some(state_root) = directory.parent()
        && fs::symlink_metadata(state_root).is_ok_and(|metadata| metadata.file_type().is_symlink())
    {
        return Err(io::Error::from(ErrorKind::InvalidInput));
    }
    create_private_directory(directory)?;
    let mut staged = tempfile::NamedTempFile::new_in(directory)?;
    staged.write_all(&serde_json::to_vec(cache)?)?;
    staged.persist(path).map_err(|error| error.error)?;
    Ok(())
}

#[cfg(unix)]
fn create_private_directory(directory: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;

    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(directory)
}

#[cfg(not(unix))]
fn create_private_directory(directory: &Path) -> io::Result<()> {
    fs::create_dir_all(directory)
}
