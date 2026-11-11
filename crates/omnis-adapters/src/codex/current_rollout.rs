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
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::{
    CodexAdapter, CodexHistoryMode, CodexSession, ObservedRollout, live_rollout_path,
    representations, restored_rollout, rollout_file,
    rollout_identity::{self, HeaderEvidence, HistoryMode},
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
    sqlite_home: sqlite_home::SqliteHome,
    snapshot: SqliteSnapshotCache,
}

impl CurrentRollouts {
    pub(super) fn from_env() -> Self {
        Self {
            sqlite_home: sqlite_home::SqliteHome::from_env(),
            ..Self::default()
        }
    }

    pub(super) fn root(&self, home: &Path) -> Result<PathBuf> {
        self.sqlite_home.resolve(home)
    }

    pub(super) fn root_for_launch(
        &self,
        home: &Path,
        cwd: &Path,
        overrides: &crate::CodexLaunchOverrides,
        expand_env_tilde: &dyn Fn() -> Result<bool>,
    ) -> Result<PathBuf> {
        self.sqlite_home
            .resolve_for_launch(home, cwd, overrides, expand_env_tilde)
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

    pub(super) fn rows_in<'a>(
        &self,
        root: &Path,
        ids: impl IntoIterator<Item = &'a str>,
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
                let (has_mode, has_archived): (bool, bool) = conn.query_row(
                    "SELECT max(name='history_mode'), max(name='archived') FROM pragma_table_info('threads')",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?;
                let mode = if has_mode {
                    "CASE history_mode WHEN 'legacy' THEN 0 WHEN 'paginated' THEN 1 END"
                } else {
                    "0"
                };
                let archived = if has_archived { "archived IS NOT 0" } else { "0" };
                let query = format!("SELECT rollout_path, {mode}, {archived} FROM threads WHERE id=?1");
                let mut statement = conn.prepare(&query)?;
                let mut selected = HashMap::new();
                for id in ids {
                    if let Some(pointer) = statement
                        .query_row([id], |row| {
                            Ok(CurrentRollout {
                                path: PathBuf::from(row.get::<_, String>(0)?),
                                paginated: row.get(1)?,
                                archived: row.get(2)?,
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
    archived: bool,
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
    pub(super) fn archived_for_launch(&self) -> bool {
        self.archived
    }

    pub(super) fn recorded_file_is(&self, home: &Path, file: Uuid) -> bool {
        let Some(name) = rollout_file::jsonl_file_name(&self.path) else {
            return false;
        };
        let Ok(root) = fs::canonicalize(home) else {
            return false;
        };
        let Ok(Some(recorded)) = local_recording(home, &root, &self.path) else {
            return false;
        };
        representations(&recorded.with_file_name(name)).any(|path| {
            live_rollout_path(home, &root, &path)
                && CodexAdapter::index_file_id(&path) == Some(file)
        })
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
        let (thread, physical) = ids(recorded_file_name(&self.path)?)?;
        (thread == id).then_some(physical)
    }

    pub(super) fn ordinary_path_is(&self, path: &Path) -> bool {
        !self.required() && self.path == path
    }

    pub(super) fn required(&self) -> bool {
        self.paginated != Some(false)
            || self.has_legacy_name()
            || recorded_file_name(&self.path)
                .and_then(ids)
                .is_some_and(|(thread, rollout)| thread != rollout)
    }

    pub(super) fn requires_selection(&self, home: &Path) -> bool {
        self.required() || self.outside_home(home)
    }

    pub(super) fn outside_home(&self, home: &Path) -> bool {
        !self.path.starts_with(home)
    }

    pub(super) fn has_legacy_name(&self) -> bool {
        !recorded_file_name(&self.path).is_some_and(canonical_name)
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
        let name = recorded_file_name(&self.path)
            .and_then(rollout_file::jsonl_file_name)
            .context(CurrentRolloutUnavailable)?;
        let root = fs::canonicalize(home).context(CurrentRolloutUnconfirmed)?;
        let mut copies = Vec::new();
        let local = local_recording(home, &root, &self.path)?;
        let mut checked_gzip = None;
        if let Some(recorded) = &local {
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
                    && let Some(mut current) = self.check_recorded(home, &root, &candidate, id)?
                {
                    current.native = native
                        && current.path == candidate
                        && !self.archived
                        && self.paginated == Some(current.paginated)
                        && current.session.history_mode != CodexHistoryMode::Unknown
                        && (!current.paginated || canonical_name(&candidate));
                    return Ok(Some(current));
                }
                if !native_recording && candidate == *recorded {
                    let current = self.check_recorded(home, &root, recorded, id)?;
                    if self.paginated == Some(true)
                        && current
                            .as_ref()
                            .is_some_and(|file| file.path != candidate || !file.paginated)
                    {
                        return Ok(current);
                    }
                    checked_gzip = Some((recorded.clone(), current));
                }
                native = false;
                copies.push(candidate);
            }
            if recorded.starts_with(home.join("sessions")) {
                copies.extend(representations(&home.join("archived_sessions").join(name)));
            } else if let Some(restored) = restored_rollout(home, recorded) {
                copies.extend(representations(&restored));
            }
        }
        if !complete {
            ensure!(!self.requires_selection(home), CurrentRolloutUnconfirmed);
            return Ok(None);
        }
        // A local recording can move between its active and archived locations. Other copies
        // can corroborate that recovery but cannot identify an arbitrary new location.
        let expected = local.map(|_| copies.clone());
        for path in paths {
            if rollout_file::jsonl_file_name(path) == Some(name) {
                copies.push(path.to_owned());
            }
        }
        if copies.is_empty() {
            return Ok(None);
        }
        let relocated = expected.is_none();
        let original = if relocated {
            self.original_recording(id, name)?
        } else {
            None
        };
        let mut agreement =
            self.agreeing_copies(home, &root, id, copies, expected.as_deref(), checked_gzip)?;
        let original_state = original
            .as_ref()
            .map(|file| (file.path.clone(), file.revision));
        if let Some(original) = original
            && agreement.selected.is_some()
        {
            agreement.consider_original(original, self.paginated == Some(true))?;
        }
        let copies = agreement.finish(id);
        if !relocated {
            return copies;
        }
        let selected = copies.context(
            "could not match the local restore to the rollout selected by the Codex state database",
        )?;
        let verified = self.original_recording(id, name)?;
        ensure!(
            original_state == verified.map(|file| (file.path, file.revision)),
            CurrentRolloutUnconfirmed
        );
        Ok(selected)
    }

    fn original_recording(&self, id: &str, name: &Path) -> Result<Option<CurrentFile>> {
        ensure!(
            !omnis_core::may_reach_network(&self.path),
            CurrentRolloutUnconfirmed
        );
        if !self.path.is_absolute() {
            // A restored database can point to a drive on another operating system.
            let foreign_absolute = self.path.to_str().is_some_and(|path| {
                matches!(path.as_bytes(), [drive, b':', b'/' | b'\\', ..] if drive.is_ascii_alphabetic())
                    || path.starts_with('/')
            });
            ensure!(foreign_absolute, CurrentRolloutUnconfirmed);
            return Ok(None);
        }
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
            let header = FileHeader::read(&path)?.context(CurrentRolloutUnavailable)?;
            let file = header.file(&path, id).context(CurrentRolloutUnavailable)?;
            ensure!(
                self.paginated != Some(true)
                    || file.paginated
                    || (header.observed.has_file_identity && header.owns_paginated_rows(id)),
                CurrentRolloutUnavailable
            );
            return Ok(Some(file));
        }
        Ok(None)
    }

    fn agreeing_copies(
        &self,
        home: &Path,
        root: &Path,
        id: &str,
        mut copies: Vec<PathBuf>,
        expected: Option<&[PathBuf]>,
        mut checked_gzip: Option<(PathBuf, Option<CurrentFile>)>,
    ) -> Result<CopyAgreement> {
        let active = home.join("sessions");
        copies.sort_unstable_by(|a, b| {
            let known = |path: &PathBuf| expected.is_none_or(|paths| paths.contains(path));
            known(b).cmp(&known(a)).then_with(|| {
                super::rollout_priority(b, &active).cmp(&super::rollout_priority(a, &active))
            })
        });
        copies.dedup();
        let mut agreement = CopyAgreement::new();
        for path in copies {
            if agreement.selected.is_none() && expected.is_some_and(|paths| !paths.contains(&path))
            {
                break;
            }
            if !accessible(home, root, &path)? {
                continue;
            }
            if checked_gzip
                .as_ref()
                .is_some_and(|(checked, _)| checked == &path)
            {
                if let Some(copy) = checked_gzip.take().and_then(|(_, file)| file) {
                    agreement.consider_complete(copy)?;
                }
            } else {
                agreement.observe(self, &path, id)?;
            }
        }
        Ok(agreement)
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
            if !accessible(home, &root, &path)? {
                ensure!(
                    fs::symlink_metadata(&path)
                        .is_err_and(|error| error.kind() == io::ErrorKind::NotFound),
                    CurrentRolloutUnavailable
                );
                continue;
            }
            let Some(header) = FileHeader::read(&path)? else {
                return Ok(None);
            };
            if paginated
                && HeaderEvidence::read(&header.value, id)
                    == HeaderEvidence::Native(rollout_identity::HistoryMode::Legacy)
            {
                continue;
            }
            let Some(mut current) = self.recorded_header(home, &root, &path, id, &header)? else {
                return Ok(None);
            };
            if !paginated && current.session.history_mode != CodexHistoryMode::Legacy {
                return Ok(None);
            }
            current.native =
                !paginated && !gzip && !self.archived && path.starts_with(home.join("sessions"));
            return Ok(Some(current));
        }
        Ok(None)
    }

    fn check_recorded(
        &self,
        home: &Path,
        root: &Path,
        path: &Path,
        id: &str,
    ) -> Result<Option<CurrentFile>> {
        let Some(first) = FileHeader::read(path)? else {
            return Ok(None);
        };
        self.recorded_header(home, root, path, id, &first)
    }

    fn recorded_header(
        &self,
        home: &Path,
        root: &Path,
        path: &Path,
        id: &str,
        first: &FileHeader,
    ) -> Result<Option<CurrentFile>> {
        let file = first.file(path, id);
        if self.paginated != Some(true) {
            return Ok(file);
        }
        match HeaderEvidence::read(&first.value, id) {
            HeaderEvidence::Native(HistoryMode::Paginated) => return Ok(file),
            HeaderEvidence::Incomplete => {}
            _ => return Ok(None),
        }
        let name = rollout_file::jsonl_file_name(path).context(CurrentRolloutUnavailable)?;
        let counterpart = if path.starts_with(home.join("sessions")) {
            Some(home.join("archived_sessions").join(name))
        } else {
            restored_rollout(home, path)
        };
        let verify = || -> Result<Option<CurrentFile>> {
            let mut agreement = CopyAgreement::new();
            agreement
                .damaged
                .push((path.to_owned(), first.observed.revision));
            for plain in [Some(path.with_file_name(name)), counterpart]
                .into_iter()
                .flatten()
            {
                for candidate in representations(&plain) {
                    if candidate != path && accessible(home, root, &candidate)? {
                        agreement.observe(self, &candidate, id)?;
                    }
                }
            }
            agreement.finish(id)
        };
        let verified = verify();
        first.confirm(path)?;
        match verified {
            Ok(Some(copy)) => Ok(Some(copy)),
            _ if first.owns_paginated_rows(id) => Ok(file),
            Err(error) => Err(error),
            Ok(None) => Err(CurrentRolloutUnavailable.into()),
        }
    }
}

struct CopyAgreement {
    selected: Option<CurrentFile>,
    selected_hash: Option<CopyDigests>,
    damaged: Vec<(PathBuf, SourceRevision)>,
    remaining: u64,
}

impl CopyAgreement {
    fn new() -> Self {
        Self {
            selected: None,
            selected_hash: None,
            damaged: Vec::new(),
            remaining: COPY_COMPARISON_BYTES,
        }
    }

    fn observe(&mut self, row: &CurrentRollout, path: &Path, id: &str) -> Result<()> {
        let Some(header) = FileHeader::read_bounded(path, &mut self.remaining)? else {
            return Ok(());
        };
        if row.paginated == Some(true) {
            match HeaderEvidence::read(&header.value, id) {
                HeaderEvidence::Native(HistoryMode::Paginated) => {}
                HeaderEvidence::Incomplete if header.recognizable_paginated_header() => {
                    self.damaged
                        .push((path.to_owned(), header.observed.revision));
                    return Ok(());
                }
                _ => return Ok(()),
            }
        }
        if let Some(copy) = header.file(path, id) {
            self.consider_complete(copy)?;
        }
        Ok(())
    }

    fn consider_original(&mut self, original: CurrentFile, paginated: bool) -> Result<()> {
        if !paginated || original.paginated {
            return self.consider_complete(original);
        }
        for (path, revision) in &self.damaged {
            if path.extension() == original.path.extension()
                && same_file::is_same_file(path, &original.path)
                    .context(CurrentRolloutUnconfirmed)?
            {
                ensure!(
                    CodexAdapter::index_file_revision(path) == Some(*revision)
                        && CodexAdapter::index_file_revision(&original.path)
                            == Some(original.revision),
                    CurrentRolloutUnconfirmed
                );
                return Ok(());
            }
        }
        Err(CurrentRolloutUnavailable.into())
    }

    fn consider_complete(&mut self, copy: CurrentFile) -> Result<()> {
        let Some(selected) = &self.selected else {
            self.selected = Some(copy);
            return Ok(());
        };
        if selected.path.extension() == copy.path.extension()
            && same_file::is_same_file(&selected.path, &copy.path)
                .context(CurrentRolloutUnconfirmed)?
        {
            return Ok(());
        }
        if !rollout_file::compressed(&selected.path) && !rollout_file::compressed(&copy.path) {
            ensure!(
                fs::metadata(&selected.path)
                    .context(CurrentRolloutUnconfirmed)?
                    .len()
                    == fs::metadata(&copy.path)
                        .context(CurrentRolloutUnconfirmed)?
                        .len(),
                CurrentRolloutUnavailable
            );
        }
        let expected = match self.selected_hash {
            Some(hash) => hash,
            None => hash_copy(selected, &mut self.remaining)?,
        };
        ensure!(
            hash_copy(&copy, &mut self.remaining)?.wire == expected.wire,
            CurrentRolloutUnavailable
        );
        self.selected_hash = Some(expected);
        Ok(())
    }

    fn finish(mut self, id: &str) -> Result<Option<CurrentFile>> {
        if let Some(selected) = &self.selected
            && !self.damaged.is_empty()
        {
            let header =
                FileHeader::read_revision(&selected.path, selected.revision, &mut self.remaining)?;
            let expected = match self.selected_hash {
                Some(hash) => hash,
                None => hash_copy(selected, &mut self.remaining)?,
            };
            for (path, revision) in self.damaged {
                let observed = FileHeader::read_revision(&path, revision, &mut self.remaining)?;
                ensure!(
                    header.value["payload"]["id"].as_str() == Some(id)
                        && super::history_base::matching_header(observed.value, &header.value)
                        && copy_digests(&path, revision, &mut self.remaining)?.body
                            == expected.body,
                    CurrentRolloutUnavailable
                );
            }
            header.confirm(&selected.path)?;
        }
        Ok(self.selected)
    }
}

struct FileHeader {
    value: Value,
    observed: ObservedRollout,
}

impl FileHeader {
    fn read(path: &Path) -> Result<Option<Self>> {
        let mut limit = MAX_TRANSCRIPT_LINE_SIZE;
        Self::read_bounded(path, &mut limit)
    }

    fn read_revision(path: &Path, revision: SourceRevision, remaining: &mut u64) -> Result<Self> {
        Self::read_bounded(path, remaining)?
            .filter(|header| header.observed.revision == revision)
            .context(CurrentRolloutUnconfirmed)
    }

    fn owns_paginated_rows(&self, id: &str) -> bool {
        self.value["type"] == "session_meta"
            && self.value["payload"]["id"].as_str() == Some(id)
            && self.recognizable_paginated_header()
    }

    fn recognizable_paginated_header(&self) -> bool {
        if self.value["type"] == "session_meta" {
            HistoryMode::is_paginated(&self.value["payload"]["history_mode"])
                || rollout_identity::damaged_paginated_mode(&self.value)
        } else {
            rollout_identity::damaged_header_type(&self.value)
        }
    }

    fn read_bounded(path: &Path, remaining: &mut u64) -> Result<Option<Self>> {
        let observed = ObservedRollout::read(path).context(CurrentRolloutUnconfirmed)?;
        let header = selected_header(path, remaining);
        ensure!(
            CodexAdapter::index_file_revision(path) == Some(observed.revision),
            CurrentRolloutUnconfirmed
        );
        Ok(header?.map(|value| Self { value, observed }))
    }

    fn confirm(&self, path: &Path) -> Result<()> {
        ensure!(
            CodexAdapter::index_file_revision(path) == Some(self.observed.revision),
            CurrentRolloutUnconfirmed
        );
        Ok(())
    }

    fn file(&self, path: &Path, id: &str) -> Option<CurrentFile> {
        let paginated =
            CodexHistoryMode::declared(&self.value["payload"]) == CodexHistoryMode::Paginated;
        if paginated && self.value["payload"]["id"].as_str() != Some(id) {
            return None;
        }
        let session =
            CodexSession::parse_metadata_record(path.to_owned(), &self.value, self.observed)?;
        (session.id == id).then(|| CurrentFile {
            session,
            path: path.to_owned(),
            revision: self.observed.revision,
            paginated,
            native: false,
        })
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

fn selected_header(path: &Path, remaining: &mut u64) -> Result<Option<serde_json::Value>> {
    let limit = MAX_TRANSCRIPT_LINE_SIZE.min(*remaining);
    let reader = rollout_file::open(path).context(CurrentRolloutUnconfirmed)?;
    let mut line = Vec::new();
    let result = BufReader::new(reader.take(limit + 1)).read_until(b'\n', &mut line);
    let bytes = u64::try_from(line.len())?;
    *remaining = remaining.saturating_sub(bytes);
    ensure!(bytes <= limit, CurrentRolloutUnconfirmed);
    if let Err(error) = result {
        let error = copy_read_error(path, error);
        if error.is::<CurrentRolloutUnavailable>() {
            return Ok(None);
        }
        return Err(error);
    }
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

// Codex can restore a Windows state database on Unix or a Unix database on Windows.
fn recorded_file_name(path: &Path) -> Option<&Path> {
    if let Some(text) = path.to_str() {
        let windows = text.starts_with(r"\\")
            || text.starts_with("//")
            || matches!(text.as_bytes(), [drive, b':', b'/' | b'\\', ..] if drive.is_ascii_alphabetic());
        if windows {
            return text.rsplit(['/', '\\']).next().map(Path::new);
        }
    }
    path.file_name().map(Path::new)
}

pub(super) fn canonical_name(path: &Path) -> bool {
    timestamp(path).is_some()
}

pub(super) fn timestamp(path: &Path) -> Option<&str> {
    let (thread, rollout) = ids(path)?;
    let (stamp, suffix) = rollout_file::jsonl_file_name(path)
        .and_then(Path::to_str)
        .and_then(|name| name.strip_prefix("rollout-"))
        .and_then(|name| name.split_at_checked(19))?;
    let suffix = suffix
        .strip_prefix('-')
        .and_then(|name| name.strip_suffix(".jsonl"))?;
    chrono::NaiveDateTime::parse_from_str(stamp, "%Y-%m-%dT%H-%M-%S").ok()?;
    (suffix == thread || suffix.split_once('_') == Some((thread, rollout))).then_some(stamp)
}

#[derive(Clone, Copy, Debug)]
struct CopyDigests {
    wire: [u8; 32],
    body: [u8; 32],
}

fn hash_copy(copy: &CurrentFile, remaining: &mut u64) -> Result<CopyDigests> {
    copy_digests(&copy.path, copy.revision, remaining)
}

fn copy_digests(path: &Path, revision: SourceRevision, remaining: &mut u64) -> Result<CopyDigests> {
    let encoded = fs::metadata(path).context(CurrentRolloutUnconfirmed)?.len();
    ensure!(encoded <= *remaining, CurrentRolloutUnconfirmed);
    let mut wire = Sha256::new();
    let mut body = Sha256::new();
    let mut in_body = false;
    let mut reader = rollout_file::open(path)
        .context(CurrentRolloutUnconfirmed)?
        .take(*remaining + 1);
    let mut buffer = [0; 16 * 1024];
    let result = (|| -> io::Result<()> {
        loop {
            let size = reader.read(&mut buffer)?;
            if size == 0 {
                return Ok(());
            }
            let part = &buffer[..size];
            wire.update(part);
            if in_body {
                body.update(part);
            } else if let Some(newline) = memchr::memchr(b'\n', part) {
                body.update(&part[newline + 1..]);
                in_body = true;
            }
        }
    })();
    let decoded = *remaining + 1 - reader.limit();
    let fits = decoded <= *remaining;
    *remaining = remaining.saturating_sub(encoded.max(decoded));
    result.map_err(|error| copy_read_error(path, error))?;
    ensure!(fits, CurrentRolloutUnconfirmed);
    ensure!(
        Some(revision) == CodexAdapter::index_file_revision(path),
        CurrentRolloutUnconfirmed
    );
    Ok(CopyDigests {
        wire: wire.finalize().into(),
        body: body.finalize().into(),
    })
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
        let meta = serde_json::json!({"type":"session_meta","ordinal":0,"payload":{"id":THREAD,"history_mode":"paginated"}});
        let item = serde_json::json!({"type":"event_msg","ordinal":1,"payload":{"type":"user_message","message":text}});
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
        let mut gzip =
            flate2::write::GzEncoder::new(fs::File::create(&copy)?, flate2::Compression::fast());
        gzip.write_all(&prefix)?;
        gzip.finish()?;
        let row = CurrentRollout {
            path: selected.clone(),
            paginated: Some(false),
            archived: false,
        };
        let read = || row.resolve(&home, THREAD, [copy.as_path()], true);
        assert_eq!(read()?.unwrap().path, copy);

        let tail = serde_json::json!({"type":"event_msg","payload":{"type":"user_message","message":"second request"}});
        fs::OpenOptions::new()
            .append(true)
            .open(&selected)?
            .write_all(format!("{tail}\n").as_bytes())?;
        assert!(read().unwrap_err().is::<CurrentRolloutUnavailable>());
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
    fn restoring_a_selected_generation_requires_agreeing_local_copies() -> Result<()> {
        let home = tempfile::tempdir()?;
        let archive = home.path().join("archived_sessions");
        let path = rollout(&archive, Some(NEXT), "current")?;
        let active = home.path().join("sessions/2026/01/01");
        let row = CurrentRollout {
            path: active.join(path.file_name().unwrap()),
            paginated: Some(true),
            archived: false,
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
