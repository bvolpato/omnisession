//! Codex keeps the original thread ID when a revert creates a new physical rollout.

use std::{
    collections::HashMap,
    env, fmt, fs,
    io::{self, BufRead, BufReader, Read},
    path::{Component, Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use omnis_ir::SourceRevision;
use rusqlite::OptionalExtension;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::{
    CodexAdapter, CodexSession, ObservedRollout, live_rollout_path, representations,
    restored_rollout, rollout_file,
};
pub(super) use crate::support::SqliteSourceStamp as CurrentRolloutStamp;
use crate::support::{MAX_TRANSCRIPT_LINE_SIZE, SqliteSnapshotCache};

pub(super) fn ids(path: &Path) -> Option<(&str, &str)> {
    let stem = rollout_file::jsonl_file_name(path)?.file_stem()?.to_str()?;
    let split = stem.len().checked_sub(36)?;
    let rollout = stem.get(split..)?;
    Uuid::try_parse(rollout).ok()?;
    let thread = stem.get(..split)?.strip_suffix('_').and_then(|prefix| {
        let thread = prefix.get(prefix.len().checked_sub(36)?..)?;
        Uuid::try_parse(thread).ok().map(|_| thread)
    });
    Some((thread.unwrap_or(rollout), rollout))
}

#[derive(Debug, Default)]
pub(super) struct CurrentRollouts {
    sqlite_home: Option<PathBuf>,
    snapshot: SqliteSnapshotCache,
}

impl CurrentRollouts {
    pub(super) fn from_env() -> Self {
        Self {
            sqlite_home: env::var("CODEX_SQLITE_HOME")
                .ok()
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from),
            ..Self::default()
        }
    }

    pub(super) fn stamp(&self, home: &Path) -> Option<CurrentRolloutStamp> {
        let root = self.sqlite_home.as_deref().unwrap_or(home);
        CurrentRolloutStamp::read(root, &root.join("state_5.sqlite"))
    }

    pub(super) fn rows<'a>(
        &self,
        home: &Path,
        ids: impl IntoIterator<Item = &'a str>,
    ) -> Result<HashMap<String, CurrentRollout>> {
        let mut ids = ids.into_iter().peekable();
        if ids.peek().is_none() {
            return Ok(HashMap::new());
        }
        let root = self.sqlite_home.as_deref().unwrap_or(home);
        let db = root.join("state_5.sqlite");
        match fs::symlink_metadata(&db) {
            Ok(meta) => ensure!(
                meta.is_file(),
                "Codex state database must be a regular file"
            ),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(HashMap::new()),
            Err(error) => return Err(error).context("locating the Codex state database"),
        }
        self.snapshot
            .read_stamped(root, &db, |conn| {
                let threads: Option<String> = conn
                    .query_row(
                        "SELECT type FROM sqlite_schema WHERE name='threads' COLLATE NOCASE",
                        [],
                        |row| row.get(0),
                    )
                    .optional()?;
                let Some(kind) = threads else {
                    return Ok(HashMap::new());
                };
                ensure!(kind == "table", "Codex threads must be a table");
                let modes: bool = conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM pragma_table_info('threads') WHERE name='history_mode')",
                    [],
                    |row| row.get(0),
                )?;
                let query = if modes {
                    "SELECT rollout_path, CASE history_mode WHEN 'legacy' THEN 0 WHEN 'paginated' THEN 1 END FROM threads WHERE id=?1"
                } else {
                    "SELECT rollout_path, 0 FROM threads WHERE id=?1"
                };
                let mut statement = conn.prepare(query)?;
                let mut selected = HashMap::new();
                for id in ids {
                    if let Some(pointer) = statement
                        .query_row([id], |row| {
                            Ok(CurrentRollout {
                                path: PathBuf::from(row.get::<_, String>(0)?),
                                paginated: row.get(1)?,
                            })
                        })
                        .optional()?
                    {
                        selected.insert(id.to_owned(), pointer);
                    }
                }
                Ok(selected)
            })
            .context("reading selected Codex rollouts")
    }
}

#[derive(Debug)]
pub(super) struct CurrentRolloutUnavailable;

impl fmt::Display for CurrentRolloutUnavailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the selected Codex rollout is missing, incompatible, or ambiguous")
    }
}

impl std::error::Error for CurrentRolloutUnavailable {}

#[derive(Debug)]
pub(super) struct CurrentRolloutUnconfirmed;

impl fmt::Display for CurrentRolloutUnconfirmed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("could not verify the selected Codex rollout")
    }
}

impl std::error::Error for CurrentRolloutUnconfirmed {}

#[derive(Debug)]
pub(super) struct CurrentRollout {
    path: PathBuf,
    paginated: Option<bool>,
}

#[derive(Debug)]
pub(super) struct CurrentFile {
    pub(super) session: CodexSession,
    pub(super) path: PathBuf,
    pub(super) revision: SourceRevision,
    pub(super) paginated: bool,
    pub(super) native: bool,
}

impl CurrentRollout {
    pub(super) fn generation(&self, id: &str) -> Option<&str> {
        let (thread, physical) = ids(&self.path)?;
        (thread == id).then_some(physical)
    }

    pub(super) fn ordinary_path_is(&self, path: &Path) -> bool {
        !self.required() && self.path == path
    }

    pub(super) fn required(&self) -> bool {
        self.paginated != Some(false)
            || ids(&self.path).is_some_and(|(thread, rollout)| thread != rollout)
    }

    pub(super) fn resolve<'a>(
        &self,
        home: &Path,
        id: &str,
        paths: impl IntoIterator<Item = &'a Path>,
        complete: bool,
    ) -> Result<Option<CurrentFile>> {
        ensure!(self.paginated.is_some(), CurrentRolloutUnavailable);
        let selected = self.resolve_path(home, id, paths, complete)?;
        if selected.is_none() && self.required() {
            return Err(CurrentRolloutUnavailable.into());
        }
        Ok(selected)
    }

    fn resolve_path<'a>(
        &self,
        home: &Path,
        id: &str,
        paths: impl IntoIterator<Item = &'a Path>,
        complete: bool,
    ) -> Result<Option<CurrentFile>> {
        if self.generation(id).is_none() {
            return Ok(None);
        }
        let name = rollout_file::jsonl_file_name(&self.path).expect("validated rollout name");
        let root = fs::canonicalize(home).context(CurrentRolloutUnconfirmed)?;
        let mut copies = Vec::new();
        if let Some(recorded) = local_recording(home, &root, &self.path)? {
            let plain = recorded.with_file_name(name);
            let native_recording = self.path.extension().is_some_and(|ext| ext != "gz");
            let mut native = native_recording && recorded.starts_with(home.join("sessions"));
            for candidate in representations(&plain) {
                if !accessible(home, &root, &candidate)? {
                    // A symlink or a directory can still shadow a lower native representation.
                    native &= fs::symlink_metadata(&candidate)
                        .is_err_and(|error| error.kind() == io::ErrorKind::NotFound);
                    continue;
                }
                if candidate.extension().is_some_and(|ext| ext != "gz")
                    && native_recording
                    && let Some(mut current) = self.check(&candidate, id)?
                {
                    current.native = native && (!current.paginated || canonical_name(&candidate));
                    return Ok(Some(current));
                }
                native = false;
                copies.push(candidate);
            }
            if recorded.starts_with(home.join("sessions")) {
                copies.extend(representations(&home.join("archived_sessions").join(name)));
            } else if let Some(restored) = restored_rollout(home, &recorded) {
                copies.extend(representations(&restored));
            }
        } else if !canonical_name(&self.path) {
            // A path from another computer can identify a local restore by its exact Codex filename.
            return Ok(None);
        }
        if !complete {
            ensure!(!self.required(), CurrentRolloutUnconfirmed);
            return Ok(None);
        }
        for path in paths {
            if rollout_file::jsonl_file_name(path) == Some(name) {
                copies.push(path.to_owned());
            }
        }
        let active = home.join("sessions");
        copies.sort_unstable_by(|a, b| {
            super::rollout_priority(b, &active).cmp(&super::rollout_priority(a, &active))
        });
        copies.dedup();
        let mut selected: Option<CurrentFile> = None;
        let mut digest = None;
        let mut bytes = 64 * 1024 * 1024;
        for path in copies {
            if !accessible(home, &root, &path)? {
                continue;
            }
            let Some(copy) = self.check(&path, id)? else {
                continue;
            };
            if let Some(prior) = &selected {
                if prior.path.extension() == path.extension()
                    && same_file::is_same_file(&prior.path, &path)
                        .context(CurrentRolloutUnconfirmed)?
                {
                    continue;
                }
                let expected = match digest {
                    Some(hash) => hash,
                    None => hash_copy(prior, &mut bytes)?,
                };
                if hash_copy(&copy, &mut bytes)? != expected {
                    return Err(CurrentRolloutUnavailable.into());
                }
                digest = Some(expected);
            } else {
                selected = Some(copy);
            }
        }
        Ok(selected)
    }

    fn check(&self, path: &Path, id: &str) -> Result<Option<CurrentFile>> {
        let observed = ObservedRollout::read(path).context(CurrentRolloutUnconfirmed)?;
        let revision = observed.revision;
        let header = selected_header(path);
        ensure!(
            CodexAdapter::index_file_revision(path) == Some(revision),
            CurrentRolloutUnconfirmed
        );
        let Some(header) = header? else {
            return Ok(None);
        };
        let paginated = header["payload"]["history_mode"] == "paginated";
        let Some(session) = CodexSession::parse_metadata_record(path.to_owned(), &header, observed)
        else {
            return Ok(None);
        };
        Ok(
            (session.id == id && (!self.required() || paginated)).then(|| CurrentFile {
                session,
                path: path.to_owned(),
                revision,
                paginated,
                native: false,
            }),
        )
    }
}

fn accessible(home: &Path, root: &Path, path: &Path) -> Result<bool> {
    if live_rollout_path(home, root, path) {
        return Ok(true);
    }
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_file() => {
            fs::canonicalize(path).context(CurrentRolloutUnconfirmed)?;
        }
        Err(e)
            if !matches!(
                e.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
            ) =>
        {
            return Err(e).context(CurrentRolloutUnconfirmed);
        }
        _ => {}
    }
    Ok(false)
}

fn selected_header(path: &Path) -> Result<Option<serde_json::Value>> {
    let reader = rollout_file::open(path).context(CurrentRolloutUnconfirmed)?;
    let mut line = Vec::new();
    if let Err(error) =
        BufReader::new(reader.take(MAX_TRANSCRIPT_LINE_SIZE + 1)).read_until(b'\n', &mut line)
    {
        let error = copy_read_error(path, error);
        if error.is::<CurrentRolloutUnavailable>() {
            return Ok(None);
        }
        return Err(error);
    }
    ensure!(
        u64::try_from(line.len())? <= MAX_TRANSCRIPT_LINE_SIZE,
        CurrentRolloutUnconfirmed
    );
    Ok(serde_json::from_slice(&line).ok())
}

fn copy_read_error(path: &Path, error: io::Error) -> anyhow::Error {
    // Zstandard returns format errors as Other. An unknown error does not rule out the copy.
    let damaged = rollout_file::compressed(path)
        && error.raw_os_error().is_none()
        && (matches!(
            error.kind(),
            io::ErrorKind::InvalidInput | io::ErrorKind::InvalidData | io::ErrorKind::UnexpectedEof
        ) || (path.extension().is_some_and(|ext| ext == "zst")
            && matches!(
                error.to_string().as_str(),
                "Unknown frame descriptor"
                    | "Data corruption detected"
                    | "Restored data doesn't match checksum"
                    | "Header of Literals' block doesn't respect format specification"
            )));
    if damaged {
        anyhow::Error::new(error).context(CurrentRolloutUnavailable)
    } else {
        anyhow::Error::new(error).context(CurrentRolloutUnconfirmed)
    }
}

fn local_recording(home: &Path, root: &Path, path: &Path) -> Result<Option<PathBuf>> {
    let Ok(relative) = path.strip_prefix(home).or_else(|_| path.strip_prefix(root)) else {
        return Ok(None);
    };
    ensure!(
        (relative.starts_with("sessions") || relative.starts_with("archived_sessions"))
            && relative
                .components()
                .all(|part| matches!(part, Component::Normal(_))),
        CurrentRolloutUnavailable
    );
    let mut prefix = root.to_owned();
    for part in relative.components() {
        prefix.push(part);
        match fs::symlink_metadata(&prefix) {
            Ok(meta) => ensure!(!meta.is_symlink(), CurrentRolloutUnavailable),
            Err(error) if error.kind() == io::ErrorKind::NotFound => break,
            Err(error) => return Err(error).context(CurrentRolloutUnconfirmed),
        }
    }
    Ok(Some(home.join(relative)))
}

fn canonical_name(path: &Path) -> bool {
    let Some((thread, rollout)) = ids(path) else {
        return false;
    };
    let Some((stamp, suffix)) = rollout_file::jsonl_file_name(path)
        .and_then(Path::to_str)
        .and_then(|name| name.strip_prefix("rollout-"))
        .and_then(|name| name.split_at_checked(19))
    else {
        return false;
    };
    let Some(suffix) = suffix
        .strip_prefix('-')
        .and_then(|name| name.strip_suffix(".jsonl"))
    else {
        return false;
    };
    chrono::NaiveDateTime::parse_from_str(stamp, "%Y-%m-%dT%H-%M-%S").is_ok()
        && (suffix == thread || suffix.split_once('_') == Some((thread, rollout)))
}

fn hash_copy(copy: &CurrentFile, remaining: &mut u64) -> Result<[u8; 32]> {
    let encoded = fs::metadata(&copy.path)
        .context(CurrentRolloutUnconfirmed)?
        .len();
    ensure!(encoded <= *remaining, CurrentRolloutUnconfirmed);
    let mut hash = Sha256::new();
    let mut reader = rollout_file::open(&copy.path)
        .context(CurrentRolloutUnconfirmed)?
        .take(*remaining + 1);
    let mut buffer = [0; 16 * 1024];
    let mut decoded = 0;
    loop {
        let size = reader
            .read(&mut buffer)
            .map_err(|error| copy_read_error(&copy.path, error))?;
        if size == 0 {
            break;
        }
        hash.update(&buffer[..size]);
        decoded += size as u64;
    }
    *remaining = remaining
        .checked_sub(encoded.max(decoded))
        .ok_or(CurrentRolloutUnconfirmed)?;
    ensure!(
        Some(copy.revision) == CodexAdapter::index_file_revision(&copy.path),
        CurrentRolloutUnconfirmed
    );
    Ok(hash.finalize().into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const THREAD: &str = "10000000-0000-4000-a000-000000000001";
    const NEXT: &str = "20000000-0000-4000-a000-000000000002";

    fn rollout(directory: &Path, physical: Option<&str>, text: &str) -> Result<PathBuf> {
        fs::create_dir_all(directory)?;
        let suffix = physical.map_or_else(String::new, |id| format!("_{id}"));
        let path = directory.join(format!(
            "rollout-2026-01-01T00-00-00-{THREAD}{suffix}.jsonl"
        ));
        let meta = serde_json::json!({"type":"session_meta","payload":{"id":THREAD,"history_mode":"paginated"}});
        let item = serde_json::json!({"type":"event_msg","payload":{"type":"user_message","message":text}});
        fs::write(&path, format!("{meta}\n{item}\n"))?;
        Ok(path)
    }

    #[test]
    fn a_new_sqlite_selection_in_the_wal_supersedes_the_original_rollout() -> Result<()> {
        let home = tempfile::tempdir()?;
        let state = tempfile::tempdir()?;
        let active = home.path().join("sessions/2026/01/01");
        let old = rollout(&active, None, "discarded")?;
        let next = rollout(&active, Some(NEXT), "current")?;
        let db = state.path().join("state_5.sqlite");
        let wal = db.with_extension("sqlite-wal");
        let selector = CurrentRollouts {
            sqlite_home: Some(state.path().to_owned()),
            ..Default::default()
        };
        assert_eq!(
            selector.stamp(home.path()),
            Some(CurrentRolloutStamp::default())
        );
        let conn = rusqlite::Connection::open(&db)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE settings(name TEXT)")?;
        assert!(selector.rows(home.path(), [THREAD])?.is_empty());
        conn.execute_batch(
            "CREATE TABLE threads(id TEXT PRIMARY KEY, rollout_path TEXT, history_mode TEXT)",
        )?;
        conn.execute(
            "INSERT INTO threads VALUES (?1, ?2, 'paginated')",
            [THREAD, old.to_str().unwrap()],
        )?;
        let paths = [old.as_path(), next.as_path()];
        let read = || -> Result<CurrentFile> {
            selector.rows(home.path(), [THREAD])?[THREAD]
                .resolve(home.path(), THREAD, paths, true)?
                .context("selected rollout")
        };
        let before = selector.stamp(home.path());
        assert_eq!(read()?.path, old);
        conn.execute(
            "UPDATE threads SET rollout_path=?1 WHERE id=?2",
            [next.to_str().unwrap(), THREAD],
        )?;
        conn.execute(
            "INSERT INTO threads VALUES (?1, ?2, 'unsupported')",
            [NEXT, next.to_str().unwrap()],
        )?;
        let different_modes = selector.rows(home.path(), [NEXT, THREAD])?;
        assert_eq!(different_modes[THREAD].generation(THREAD), Some(NEXT));
        assert!(
            different_modes[NEXT]
                .resolve(home.path(), NEXT, paths, true)
                .unwrap_err()
                .is::<CurrentRolloutUnavailable>()
        );
        if let (Some(before), Some(after)) = (before, selector.stamp(home.path())) {
            assert_ne!(before, after);
        }
        let source = (fs::read(&db)?, fs::read(&wal)?);
        let current = read()?;
        assert_eq!(current.path, next);
        assert_eq!(current.session.id, THREAD);
        assert_eq!(current.session.observed_revision, Some(current.revision));
        assert!(current.native && current.paginated);
        assert_eq!(ids(&current.path), Some((THREAD, NEXT)));
        let renamed = active.join(format!("restored-{THREAD}_{NEXT}.jsonl"));
        fs::rename(&next, &renamed)?;
        let renamed_row = CurrentRollout {
            path: renamed,
            paginated: Some(true),
        };
        let verified = renamed_row.resolve(home.path(), THREAD, [], true)?.unwrap();
        assert_eq!(verified.session.id, THREAD);
        assert!(!verified.native);
        let error = read().expect_err("a missing selected generation must hide the original");
        assert!(error.is::<CurrentRolloutUnavailable>(), "{error:#}");
        assert_eq!((fs::read(db)?, fs::read(wal)?), source);
        Ok(())
    }

    #[test]
    fn restoring_a_selected_generation_requires_agreeing_local_copies() -> Result<()> {
        let home = tempfile::tempdir()?;
        let archive = home.path().join("archived_sessions");
        let path = rollout(&archive, Some(NEXT), "current")?;
        let active = home.path().join("sessions/2026/01/01");
        let row = CurrentRollout {
            path: active.join(path.file_name().unwrap()),
            paginated: Some(true),
        };
        let packed = path.with_extension("jsonl.gz");
        let source = fs::read(&path)?;
        let mut out =
            flate2::write::GzEncoder::new(fs::File::create(&packed)?, flate2::Compression::fast());
        out.write_all(&source)?;
        drop(out.finish()?);
        fs::remove_file(&path)?;
        let selected = row
            .resolve(home.path(), THREAD, [packed.as_path()], true)?
            .unwrap();
        assert_eq!(selected.path, packed);
        assert!(!selected.native);
        assert!(
            hash_copy(&selected, &mut 1)
                .unwrap_err()
                .is::<CurrentRolloutUnconfirmed>()
        );
        let copy = rollout(&archive.join("backup"), Some(NEXT), "conflict")?;
        let paths = [packed.as_path(), copy.as_path()];
        assert!(
            row.resolve(home.path(), THREAD, paths, true)
                .unwrap_err()
                .is::<CurrentRolloutUnavailable>()
        );
        fs::write(&copy, source)?;
        assert!(row.resolve(home.path(), THREAD, paths, true)?.is_some());
        assert!(
            row.resolve(home.path(), THREAD, paths, false)
                .unwrap_err()
                .is::<CurrentRolloutUnconfirmed>()
        );
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&archive, home.path().join("sessions"))?;
            assert!(
                row.resolve(home.path(), THREAD, paths, true)
                    .unwrap_err()
                    .is::<CurrentRolloutUnavailable>()
            );
            fs::remove_file(home.path().join("sessions"))?;
        }
        fs::write(&packed, b"invalid gzip frame")?;
        fs::remove_file(&copy)?;
        assert!(
            row.resolve(home.path(), THREAD, paths, true)
                .unwrap_err()
                .is::<CurrentRolloutUnavailable>()
        );
        Ok(())
    }
}
