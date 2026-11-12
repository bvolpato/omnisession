//! Codex keeps the original thread ID when a revert creates a new physical rollout.

mod sqlite_home;

use std::{
    collections::HashMap,
    fmt, fs,
    io::{self, BufRead, BufReader, Read},
    path::{Component, Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use omnis_ir::SourceRevision;
use rusqlite::OptionalExtension;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::{
    CodexAdapter, CodexHistoryMode, CodexSession, ObservedRollout, gui_names, live_rollout_path,
    representations, restored_rollout, rollout_file,
};
use crate::support::{MAX_TRANSCRIPT_LINE_SIZE, SqliteSnapshotCache, SqliteSourceStamp};

const COPY_COMPARISON_BYTES: u64 = 64 * 1024 * 1024;

pub(super) type CurrentRolloutStamp = (PathBuf, SqliteSourceStamp);

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
    pinned: Option<PathBuf>,
    sqlite_home: sqlite_home::SqliteHome,
    snapshot: SqliteSnapshotCache,
}

impl CurrentRollouts {
    pub(super) fn pinned(sqlite_home: &Path) -> Self {
        Self {
            pinned: Some(sqlite_home.to_owned()),
            ..Self::from_env()
        }
    }

    pub(super) fn from_env() -> Self {
        Self {
            sqlite_home: sqlite_home::SqliteHome::from_env(),
            ..Self::default()
        }
    }

    pub(super) fn root(&self, home: &Path) -> Result<PathBuf> {
        if let Some(pinned) = &self.pinned {
            return Ok(pinned.clone());
        }
        self.sqlite_home.resolve(home)
    }

    pub(super) fn root_for_launch(
        &self,
        home: &Path,
        cwd: &Path,
        overrides: &crate::CodexLaunchOverrides,
        expand_env_tilde: &dyn Fn() -> Result<bool>,
    ) -> Result<PathBuf> {
        let destination =
            self.sqlite_home
                .resolve_for_launch(home, cwd, overrides, expand_env_tilde)?;
        ensure!(
            self.pinned.as_ref().is_none_or(|root| root == &destination),
            "Codex launch selects a different SQLite home than the native import"
        );
        Ok(destination)
    }

    pub(super) fn stamp(&self, home: &Path) -> Option<CurrentRolloutStamp> {
        let root = self.root(home).ok()?;
        let stamp = SqliteSourceStamp::read(&root, &root.join("state_5.sqlite"))?;
        Some((root, stamp))
    }

    pub(super) fn rows<'a>(
        &self,
        home: &Path,
        ids: impl IntoIterator<Item = &'a str>,
    ) -> Result<HashMap<String, CurrentRollout>> {
        self.rows_in(&self.root(home)?, ids)
    }

    pub(super) fn rows_with_titles<'a>(
        &self,
        home: &Path,
        ids: impl IntoIterator<Item = &'a str>,
        titles: &HashMap<String, String>,
    ) -> Result<HashMap<String, CurrentRollout>> {
        self.read_rows(&self.root(home)?, ids, Some(titles))
    }

    pub(super) fn rows_in<'a>(
        &self,
        root: &Path,
        ids: impl IntoIterator<Item = &'a str>,
    ) -> Result<HashMap<String, CurrentRollout>> {
        self.read_rows(root, ids, None)
    }

    fn read_rows<'a>(
        &self,
        root: &Path,
        ids: impl IntoIterator<Item = &'a str>,
        titles: Option<&HashMap<String, String>>,
    ) -> Result<HashMap<String, CurrentRollout>> {
        let mut ids = ids.into_iter().peekable();
        if ids.peek().is_none() {
            return Ok(HashMap::new());
        }
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
                let txn = conn.unchecked_transaction()?;
                let conn = &txn;
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
                let columns = conn
                    .prepare("SELECT name, pk FROM pragma_table_info('threads')")?
                    .query_map([], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                    })?
                    .collect::<rusqlite::Result<HashMap<_, _>>>()?;
                let mode = if columns.contains_key("history_mode") {
                    "CASE history_mode WHEN 'legacy' THEN 0 WHEN 'paginated' THEN 1 END"
                } else {
                    "0"
                };
                let archived = if columns.contains_key("archived") {
                    "archived IS NOT 0"
                } else {
                    "0"
                };
                let names = gui_names::StateName::projection(&columns);
                let query = format!(
                    "SELECT rollout_path, {mode}, {archived}, {names} FROM threads WHERE id=?1"
                );
                let mut statement = conn.prepare(&query)?;
                let parameter_count = statement.parameter_count();
                let mut selected = HashMap::new();
                for id in ids {
                    if let Some(pointer) = statement
                        .query_row(
                            rusqlite::params_from_iter(
                                [
                                    Some(id),
                                    Some(gui_names::WHITESPACE),
                                    titles.and_then(|names| names.get(id)).map(String::as_str),
                                ]
                                .into_iter()
                                .take(parameter_count),
                            ),
                            |row| {
                                Ok(CurrentRollout {
                                    path: PathBuf::from(row.get::<_, String>(0)?),
                                    paginated: row.get(1)?,
                                    archived: row.get(2)?,
                                    name: gui_names::StateName::from_row(row),
                                })
                            },
                        )
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

#[derive(Debug, PartialEq)]
pub(super) struct CurrentRollout {
    path: PathBuf,
    paginated: Option<bool>,
    archived: bool,
    name: gui_names::StateName,
}

#[derive(Debug)]
pub(super) struct CurrentFile {
    pub(super) session: CodexSession,
    pub(super) path: PathBuf,
    pub(super) revision: SourceRevision,
    pub(super) paginated: bool,
    pub(super) native: bool,
}

impl CurrentFile {
    fn source(&self) -> (&Path, SourceRevision) {
        (&self.path, self.revision)
    }
}

fn same_index_filename(a: &Path, b: &Path) -> bool {
    let a_name = rollout_file::jsonl_file_name(a);
    let b_name = rollout_file::jsonl_file_name(b);
    a_name == b_name
        || (canonical_name(a)
            && canonical_name(b)
            && a_name
                .and_then(Path::to_str)
                .zip(b_name.and_then(Path::to_str))
                .is_some_and(|(a, b)| a.eq_ignore_ascii_case(b)))
}

impl CurrentRollout {
    pub(super) fn observed_name(
        &self,
        indexed: impl FnOnce() -> crate::NativeSessionName,
    ) -> crate::NativeSessionName {
        use crate::NativeSessionName;
        match &self.name {
            gui_names::StateName::Gui(name) => NativeSessionName::Gui(name.clone()),
            gui_names::StateName::Index | gui_names::StateName::CorroboratedIndex => indexed(),
            gui_names::StateName::Guardian(name) => match indexed() {
                named @ NativeSessionName::Named(_) => named,
                _ => NativeSessionName::Gui(Some(name.clone())),
            },
        }
    }

    pub(super) fn corroborates_index(&self) -> bool {
        matches!(self.name, gui_names::StateName::CorroboratedIndex)
    }

    pub(super) fn index_copies_match<'a>(
        &self,
        home: &Path,
        id: &str,
        selected: &Path,
        candidates: impl Iterator<Item = &'a Path>,
    ) -> Result<bool> {
        let root = fs::canonicalize(home)?;
        if !live_rollout_path(home, &root, selected) {
            return Ok(false);
        }
        let Some(original) = self.check(selected, id)? else {
            return Ok(false);
        };
        let mut digest = None;
        let mut remaining = COPY_COMPARISON_BYTES;
        let mut saw_selected = false;
        let mut saw_copy = false;
        for path in candidates {
            if path == selected {
                saw_selected = true;
                continue;
            }
            if !same_index_filename(path, selected) || !live_rollout_path(home, &root, path) {
                return Ok(false);
            }
            let Some(revision) = CodexAdapter::index_file_revision(path) else {
                return Ok(false);
            };
            if !copies_match(
                original.source(),
                (path, revision),
                &mut digest,
                &mut remaining,
            )? {
                return Ok(false);
            }
            saw_copy = true;
        }
        Ok(saw_selected
            && saw_copy
            && CodexAdapter::index_file_revision(selected) == Some(original.revision))
    }

    pub(super) fn archived_for_launch(&self) -> bool {
        self.archived
    }

    pub(super) fn missing_ordinary_native_path(&self, home: &Path, id: &str) -> Result<bool> {
        if self.required()
            || self.generation(id) != Some(id)
            || !canonical_name(&self.path)
            || self.path.extension().is_some_and(|suffix| suffix == "gz")
        {
            return Ok(false);
        }
        let root = fs::canonicalize(home).context(CurrentRolloutUnconfirmed)?;
        let Some(path) = local_recording(home, &root, &self.path)? else {
            return Ok(false);
        };
        if !path.starts_with(home.join("sessions")) {
            return Ok(false);
        }
        let plain = path
            .with_file_name(rollout_file::jsonl_file_name(&path).expect("validated rollout name"));
        Ok([plain.as_path(), &plain.with_extension("jsonl.zst")]
            .iter()
            .all(|file| {
                fs::symlink_metadata(file)
                    .is_err_and(|error| error.kind() == io::ErrorKind::NotFound)
            }))
    }

    pub(super) fn generation(&self, id: &str) -> Option<&str> {
        let (thread, physical) = ids(&self.path)?;
        (thread == id).then_some(physical)
    }

    pub(super) fn ordinary_path_is(&self, path: &Path) -> bool {
        !self.required() && self.path == path
    }

    pub(super) fn required(&self) -> bool {
        self.paginated != Some(false)
            || self.has_legacy_name()
            || ids(&self.path).is_some_and(|(thread, rollout)| thread != rollout)
    }

    pub(super) fn requires_selection(&self, home: &Path) -> bool {
        self.required() || !self.path.starts_with(home)
    }

    pub(super) fn has_legacy_name(&self) -> bool {
        !canonical_name(&self.path)
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
        if selected.is_none() && self.requires_selection(home) {
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
        if self.has_legacy_name() {
            return self.resolve_recorded_path(home, id);
        }
        ensure!(self.generation(id).is_some(), CurrentRolloutUnavailable);
        let name = rollout_file::jsonl_file_name(&self.path).expect("validated rollout name");
        let root = fs::canonicalize(home).context(CurrentRolloutUnconfirmed)?;
        let mut copies = Vec::new();
        let recorded = local_recording(home, &root, &self.path)?;
        let relocated = recorded.is_none();
        if let Some(recorded) = recorded {
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
                    current.native = native
                        && !self.archived
                        && self.paginated == Some(current.paginated)
                        && current.session.history_mode != CodexHistoryMode::Unknown
                        && (!current.paginated || canonical_name(&candidate));
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
        }
        if !complete {
            ensure!(!self.requires_selection(home), CurrentRolloutUnconfirmed);
            return Ok(None);
        }
        for path in paths {
            if rollout_file::jsonl_file_name(path) == Some(name) {
                copies.push(path.to_owned());
            }
        }
        if copies.is_empty() {
            return Ok(None);
        }
        let original = if relocated {
            self.original_recording(id, name)?
        } else {
            None
        };
        let copies = self.agreeing_copies(home, &root, id, copies, original.as_ref());
        if !relocated {
            return copies;
        }
        let selected = copies.context(
            "could not match the local restore to the rollout selected by the Codex state database",
        )?;
        let verified = self.original_recording(id, name)?;
        ensure!(
            original.as_ref().map(|file| (&file.path, file.revision))
                == verified.as_ref().map(|file| (&file.path, file.revision)),
            CurrentRolloutUnconfirmed
        );
        Ok(selected)
    }

    fn original_recording(&self, id: &str, name: &Path) -> Result<Option<CurrentFile>> {
        ensure!(
            self.path.is_absolute() && !omnis_core::may_reach_network(&self.path),
            CurrentRolloutUnconfirmed
        );
        let plain = self.path.with_file_name(name);
        let gzip = self.path.extension().is_some_and(|ext| ext == "gz");
        for path in representations(&plain)
            .filter(|path| path.extension().is_some_and(|ext| ext == "gz") == gzip)
        {
            match fs::symlink_metadata(&path) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Ok(meta) => ensure!(meta.is_file(), CurrentRolloutUnconfirmed),
                Err(error) => return Err(error).context(CurrentRolloutUnconfirmed),
            }
            // Read only a regular Codex rollout under the recorded home's session directories.
            let home = path
                .ancestors()
                .find(|part| part.ends_with("sessions") || part.ends_with("archived_sessions"))
                .and_then(Path::parent)
                .context(CurrentRolloutUnconfirmed)?;
            let root = fs::canonicalize(home).context(CurrentRolloutUnconfirmed)?;
            ensure!(
                live_rollout_path(home, &root, &path),
                CurrentRolloutUnconfirmed
            );
            return self
                .check(&path, id)?
                .map(Some)
                .ok_or_else(|| CurrentRolloutUnavailable.into());
        }
        Ok(None)
    }

    fn agreeing_copies(
        &self,
        home: &Path,
        root: &Path,
        id: &str,
        mut copies: Vec<PathBuf>,
        original: Option<&CurrentFile>,
    ) -> Result<Option<CurrentFile>> {
        let active = home.join("sessions");
        copies.sort_unstable_by(|a, b| {
            super::rollout_priority(b, &active).cmp(&super::rollout_priority(a, &active))
        });
        copies.dedup();
        let mut selected: Option<CurrentFile> = None;
        let mut digest = None;
        let mut bytes = COPY_COMPARISON_BYTES;
        for path in copies {
            if !accessible(home, root, &path)? {
                continue;
            }
            let Some(copy) = self.check(&path, id)? else {
                continue;
            };
            if let Some(prior) = original.or(selected.as_ref())
                && !copies_match(prior.source(), copy.source(), &mut digest, &mut bytes)?
            {
                ensure!(original.is_some(), CurrentRolloutUnavailable);
                continue;
            }
            if original.is_some() {
                return Ok(Some(copy));
            }
            selected.get_or_insert(copy);
        }
        Ok(selected)
    }

    fn resolve_recorded_path(&self, home: &Path, id: &str) -> Result<Option<CurrentFile>> {
        let paginated = self.paginated.context(CurrentRolloutUnavailable)?;
        // A renamed paginated file can be recovered when its suffix identifies the selected generation.
        ensure!(
            !paginated
                || self
                    .generation(id)
                    .is_some_and(|generation| generation != id),
            CurrentRolloutUnavailable
        );
        let Some(path) = self.recorded_representation(home)? else {
            return Ok(None);
        };
        let Some(mut current) = self.check(&path, id)? else {
            return Ok(None);
        };
        if !paginated && current.session.history_mode != CodexHistoryMode::Legacy {
            return Ok(None);
        }
        current.native = !paginated
            && path.extension().is_none_or(|ext| ext != "gz")
            && !self.archived
            && path.starts_with(home.join("sessions"));
        Ok(Some(current))
    }

    fn recorded_representation(&self, home: &Path) -> Result<Option<PathBuf>> {
        let name = rollout_file::jsonl_file_name(&self.path).context(CurrentRolloutUnavailable)?;
        let root = fs::canonicalize(home).context(CurrentRolloutUnconfirmed)?;
        let recorded =
            local_recording(home, &root, &self.path)?.context(CurrentRolloutUnavailable)?;
        let gzip = recorded.extension().is_some_and(|ext| ext == "gz");
        let plain = recorded.with_file_name(name);
        // Codex opens the plain file before its Zstandard sibling. A gzip backup has only its exact path.
        for path in representations(&plain)
            .filter(|path| path.extension().is_some_and(|ext| ext == "gz") == gzip)
        {
            local_recording(home, &root, &path)?;
            if accessible(home, &root, &path)? {
                return Ok(Some(path));
            }
        }
        Ok(None)
    }

    pub(super) fn recorded_file_replaces(&self, home: &Path, prior: &Path, prior_id: Uuid) -> bool {
        let Some(path) = self.recorded_representation(home).ok().flatten() else {
            return false;
        };
        let prior =
            ObservedRollout::read(prior).filter(|source| source.revision.file == Some(prior_id));
        prior
            .and_then(|source| source.physical_identity)
            .zip(ObservedRollout::read(&path).and_then(|source| source.physical_identity))
            .is_some_and(|(prior, selected)| prior != selected)
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
        let paginated =
            CodexHistoryMode::declared(&header["payload"]) == CodexHistoryMode::Paginated;
        let Some(session) = CodexSession::parse_metadata_record(path.to_owned(), &header, observed)
        else {
            return Ok(None);
        };
        Ok(
            (session.id == id && (self.paginated != Some(true) || paginated)).then(|| {
                CurrentFile {
                    session,
                    path: path.to_owned(),
                    revision,
                    paginated,
                    native: false,
                }
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

pub(super) fn canonical_name(path: &Path) -> bool {
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

fn copies_match(
    reference @ (reference_path, _): (&Path, SourceRevision),
    copy @ (copy_path, _): (&Path, SourceRevision),
    reference_digest: &mut Option<[u8; 32]>,
    remaining: &mut u64,
) -> Result<bool> {
    if reference_path.extension() == copy_path.extension()
        && same_file::is_same_file(reference_path, copy_path).context(CurrentRolloutUnconfirmed)?
    {
        return Ok(true);
    }
    if !rollout_file::compressed(reference_path)
        && !rollout_file::compressed(copy_path)
        && fs::metadata(reference_path)
            .context(CurrentRolloutUnconfirmed)?
            .len()
            != fs::metadata(copy_path)
                .context(CurrentRolloutUnconfirmed)?
                .len()
    {
        return Ok(false);
    }
    let expected = match *reference_digest {
        Some(hash) => hash,
        None => *reference_digest.insert(hash_copy(reference, remaining)?),
    };
    match hash_copy(copy, remaining) {
        Ok(hash) => Ok(hash == expected),
        Err(error) if error.is::<CurrentRolloutUnavailable>() => Ok(false),
        Err(error) => Err(error),
    }
}

fn hash_copy((path, revision): (&Path, SourceRevision), remaining: &mut u64) -> Result<[u8; 32]> {
    let encoded = fs::metadata(path).context(CurrentRolloutUnconfirmed)?.len();
    ensure!(encoded <= *remaining, CurrentRolloutUnconfirmed);
    let mut hash = Sha256::new();
    let mut reader = rollout_file::open(path)
        .context(CurrentRolloutUnconfirmed)?
        .take(*remaining + 1);
    let mut buffer = [0; 16 * 1024];
    let mut decoded = 0;
    let result = loop {
        match reader.read(&mut buffer) {
            Ok(0) => break Ok(()),
            Ok(size) => {
                decoded += size as u64;
                hash.update(&buffer[..size]);
            }
            Err(error) => break Err(copy_read_error(path, error)),
        }
    };
    *remaining = remaining
        .checked_sub(encoded.max(decoded))
        .ok_or(CurrentRolloutUnconfirmed)?;
    ensure!(
        Some(revision) == CodexAdapter::index_file_revision(path),
        CurrentRolloutUnconfirmed
    );
    result?;
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
    fn live_registry_reads_both_names_from_one_wal_transaction() -> Result<()> {
        let home = tempfile::tempdir()?;
        let database = home.path().join("state_5.sqlite");
        let writer = rusqlite::Connection::open(&database)?;
        writer.execute_batch("PRAGMA journal_mode=WAL;
            CREATE TABLE threads(id TEXT PRIMARY KEY, rollout_path TEXT, history_mode TEXT, name TEXT)")?;
        for id in [THREAD, NEXT] {
            writer.execute(
                "INSERT INTO threads VALUES (?1, 'before.jsonl', 'paginated', 'before')",
                [id],
            )?;
        }
        writer.execute_batch(
            "CREATE TABLE activity(tick INTEGER); INSERT INTO activity VALUES (0);
            CREATE TABLE padding(bytes BLOB); INSERT INTO padding VALUES (zeroblob(33554432));
            PRAGMA wal_checkpoint(TRUNCATE)",
        )?;
        let selector = CurrentRollouts::default();
        let churn = rusqlite::Connection::open(&database)?;
        churn.execute_batch("PRAGMA synchronous=NORMAL")?;
        let (ready, wait) = std::sync::mpsc::sync_channel(0);
        let (stop, running) = std::sync::mpsc::channel::<()>();
        let worker = std::thread::spawn(move || -> rusqlite::Result<()> {
            churn.execute("UPDATE activity SET tick=1", [])?;
            ready.send(()).unwrap();
            while running.try_recv() == Err(std::sync::mpsc::TryRecvError::Empty) {
                churn.execute("UPDATE activity SET tick=1-tick", [])?;
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            Ok(())
        });
        wait.recv()?;
        let ids = [THREAD, NEXT].into_iter().inspect(|id| {
            if *id == NEXT {
                writer
                    .execute(
                        "UPDATE threads SET rollout_path='after.jsonl', name='after'",
                        [],
                    )
                    .unwrap();
            }
        });
        let before = selector.rows_in(home.path(), ids);
        drop(stop);
        worker.join().unwrap()?;
        let before = before?;
        let after = selector.rows_in(home.path(), [THREAD, NEXT])?;
        for (rows, marker) in [(before, "before"), (after, "after")] {
            for id in [THREAD, NEXT] {
                assert_eq!(rows[id].path, PathBuf::from(format!("{marker}.jsonl")));
                assert_eq!(
                    rows[id]
                        .observed_name(|| crate::NativeSessionName::Unknown)
                        .into_title()
                        .as_deref(),
                    Some(marker)
                );
            }
        }
        drop(writer);
        assert!(!database.with_extension("sqlite-wal").exists());
        assert!(!database.with_extension("sqlite-shm").exists());
        Ok(())
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
        fs::write(
            home.path().join("config.toml"),
            format!("sqlite_home = {}", serde_json::to_string(state.path())?),
        )?;
        let selector = CurrentRollouts::default();
        assert_eq!(
            selector.stamp(home.path()),
            Some((state.path().to_owned(), SqliteSourceStamp::default()))
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
            archived: false,
            name: gui_names::StateName::Index,
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
    fn a_restored_compressed_rollout_must_match_the_readable_sqlite_selection() -> Result<()> {
        let fixture = tempfile::tempdir()?;
        let original = fixture.path().join("origin/sessions/2026/01/01");
        let home = fixture.path().join("restored");
        let archive = home.join("archived_sessions");
        fs::create_dir_all(&archive)?;
        let selected = rollout(&original, None, "opening request")?;
        let prefix = fs::read(&selected)?;
        let copy = archive
            .join(selected.file_name().unwrap())
            .with_extension("jsonl.gz");
        let write_archive = |contents: &[u8]| -> Result<()> {
            let mut gzip = flate2::write::GzEncoder::new(
                fs::File::create(&copy)?,
                flate2::Compression::fast(),
            );
            gzip.write_all(contents)?;
            gzip.finish()?;
            Ok(())
        };
        write_archive(&prefix)?;
        let row = CurrentRollout {
            path: selected.clone(),
            paginated: Some(false),
            archived: false,
            name: gui_names::StateName::Index,
        };
        let read = || row.resolve(&home, THREAD, [copy.as_path()], true);
        assert_eq!(read()?.unwrap().path, copy);

        let tail = serde_json::json!({"type":"event_msg","payload":{"type":"user_message","message":"second request"}});
        fs::OpenOptions::new()
            .append(true)
            .open(&selected)?
            .write_all(format!("{tail}\n").as_bytes())?;
        assert!(read().unwrap_err().is::<CurrentRolloutUnavailable>());
        let active = home.join("sessions/2026/01/01");
        fs::create_dir_all(&active)?;
        let restored = active.join(selected.file_name().unwrap());
        fs::copy(&selected, &restored)?;
        let paths = [copy.as_path(), restored.as_path()];
        let current = row.resolve(&home, THREAD, paths, true)?.unwrap();
        assert_eq!(current.path, restored);
        write_archive(&fs::read(&selected)?)?;
        fs::write(
            &restored,
            fs::read_to_string(&selected)?.replace("second", "absent"),
        )?;
        assert_eq!(row.resolve(&home, THREAD, paths, true)?.unwrap().path, copy);
        fs::remove_file(&restored)?;
        write_archive(&prefix)?;
        let packed = selected.with_extension("jsonl.zst");
        fs::write(
            &packed,
            zstd::stream::encode_all(fs::File::open(&selected)?, 1)?,
        )?;
        fs::remove_file(&selected)?;
        assert!(read().unwrap_err().is::<CurrentRolloutUnavailable>());
        fs::remove_file(packed)?;
        assert_eq!(read()?.unwrap().path, copy);
        Ok(())
    }

    #[test]
    fn a_damaged_gzip_does_not_hide_a_copy_matching_the_readable_original() -> Result<()> {
        let fixture = tempfile::tempdir()?;
        let home = fixture.path().join("restored");
        let selected = rollout(
            &fixture.path().join("origin/sessions"),
            None,
            "final request",
        )?;
        let archive = rollout(&home.join("archived_sessions"), None, "final request")?;
        let active = home.join("sessions");
        fs::create_dir_all(&active)?;
        let damaged = active
            .join(selected.file_name().unwrap())
            .with_extension("jsonl.gz");
        let mut gzip =
            flate2::write::GzEncoder::new(fs::File::create(&damaged)?, flate2::Compression::fast());
        gzip.write_all(&fs::read(&selected)?)?;
        let file = gzip.finish()?;
        file.set_len(file.metadata()?.len() - 4)?;
        drop(file);
        let row = CurrentRollout {
            path: selected,
            paginated: Some(true),
            archived: false,
            name: gui_names::StateName::Index,
        };
        let paths = [damaged.as_path(), archive.as_path()];
        assert_eq!(
            row.resolve(&home, THREAD, paths, true)?.unwrap().path,
            archive
        );
        assert!(
            row.resolve(&home, THREAD, [damaged.as_path()], true)
                .unwrap_err()
                .is::<CurrentRolloutUnavailable>()
        );
        fs::remove_file(&row.path)?;
        assert!(
            row.resolve(&home, THREAD, paths, true)
                .unwrap_err()
                .is::<CurrentRolloutUnavailable>()
        );
        let copy = row.check(&damaged, THREAD)?.unwrap();
        let mut budget = 64 * 1024;
        assert!(
            hash_copy(copy.source(), &mut budget)
                .unwrap_err()
                .is::<CurrentRolloutUnavailable>()
        );
        assert!(budget <= 64 * 1024 - fs::metadata(&damaged)?.len());
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
            archived: false,
            name: gui_names::StateName::Index,
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
            hash_copy(selected.source(), &mut 1)
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
