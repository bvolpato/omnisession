//! Codex keeps the thread ID when a revert creates another physical rollout.

mod sqlite_home;

use std::{
    cmp::Ordering,
    collections::{HashMap, HashSet},
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, PoisonError},
};

use anyhow::{Context, Result, anyhow, bail};
use omnis_ir::SourceRevision;
use rusqlite::OptionalExtension;
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::{CodexFileScan, CodexSession, ancestor_paths::AncestorPaths, rollout_file};
use crate::support::{SqliteSnapshotCache, provider_file};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum IdKind {
    Thread,
    Rollout,
}

pub(super) fn ids(path: &Path) -> Option<(&str, &str)> {
    let stem = rollout_file::jsonl_file_name(path)?.file_stem()?.to_str()?;
    let split = stem.len().checked_sub(36)?;
    let rollout = stem.get(split..)?;
    Uuid::try_parse(rollout).ok()?;
    // Legacy rollouts end in the thread ID. A revert appends "_<rollout ID>".
    let thread = stem.get(..split)?.strip_suffix('_').and_then(|prefix| {
        let thread = prefix.get(prefix.len().checked_sub(36)?..)?;
        Uuid::try_parse(thread).ok().map(|_| thread)
    });
    Some((thread.unwrap_or(rollout), rollout))
}

pub(super) fn thread_id(path: &Path) -> Option<&str> {
    ids(path).map(|(thread, _)| thread)
}

pub(super) fn rollout_id(path: &Path) -> Option<&str> {
    ids(path).map(|(_, rollout)| rollout)
}

#[derive(Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(super) enum HistoryMode {
    Legacy,
    Paginated,
}

impl HistoryMode {
    pub(super) fn read(value: &Value) -> Option<Self> {
        Self::deserialize(value).ok()
    }

    pub(super) fn is_paginated(value: &Value) -> bool {
        Self::read(value) == Some(Self::Paginated)
    }
}

fn native_header_mode(header: &Value) -> Option<HistoryMode> {
    let meta = &header["payload"];
    let mode = meta
        .get("history_mode")
        .map_or(Some(HistoryMode::Legacy), HistoryMode::read)?;
    (mode != HistoryMode::Legacy || meta["history_base"].is_null()).then_some(mode)
}

pub(super) fn damaged_paginated_mode(header: &Value) -> bool {
    // Codex writes numeric ordinals only in paginated rollouts.
    let mode = &header["payload"]["history_mode"];
    header["ordinal"].is_u64() && HistoryMode::read(mode).is_none()
}

#[derive(PartialEq, Eq)]
pub(super) enum HeaderIdentity {
    Unique,
    Ambiguous,
}

pub(super) fn header_identity(header: &Value, thread: &str) -> Option<HeaderIdentity> {
    if let Some(payload) = header.get("payload") {
        return (payload["id"].as_str() == Some(thread)).then_some(HeaderIdentity::Unique);
    }
    if header["type"] != "session_meta" || !header["ordinal"].is_u64() {
        return None;
    }
    let mut count = 0;
    let mut matches_thread = false;
    for value in header.as_object()?.values() {
        let Some(id) = value.get("id").and_then(Value::as_str) else {
            continue;
        };
        if HistoryMode::is_paginated(&value["history_mode"]) && Uuid::try_parse(id).is_ok() {
            count += 1;
            matches_thread |= id == thread;
        }
    }
    matches_thread.then_some(if count == 1 {
        HeaderIdentity::Unique
    } else {
        HeaderIdentity::Ambiguous
    })
}

pub(super) fn damaged_header_type(header: &Value) -> bool {
    // A body record's declared type takes precedence over extra metadata-like fields.
    header["ordinal"].is_u64()
        && HistoryMode::is_paginated(&header["payload"]["history_mode"])
        && !matches!(
            header["type"].as_str(),
            Some(
                "session_meta"
                    | "dynamic_tools_updated"
                    | "response_item"
                    | "inter_agent_communication"
                    | "inter_agent_communication_metadata"
                    | "compacted"
                    | "turn_context"
                    | "token_usage_record"
                    | "world_state"
                    | "security_risk_score"
                    | "retained_context"
                    | "event_msg"
                    | "realtime_item"
            )
        )
}

fn timestamp(path: &Path) -> Option<&str> {
    let name = rollout_file::jsonl_file_name(path)?
        .to_str()?
        .strip_prefix("rollout-")?;
    let (date, suffix) = name.split_at_checked(19)?;
    let suffix = suffix.strip_prefix('-')?.strip_suffix(".jsonl")?;
    let (thread, rollout) = suffix.split_once('_').unwrap_or((suffix, suffix));
    Uuid::try_parse(thread).ok()?;
    Uuid::try_parse(rollout).ok()?;
    chrono::NaiveDateTime::parse_from_str(date, "%Y-%m-%dT%H-%M-%S").ok()?;
    Some(date)
}

pub(super) fn canonical_filename(path: &Path) -> bool {
    timestamp(path).is_some()
}

fn native_rank(path: &Path) -> Option<(&str, Uuid)> {
    timestamp(path).zip(rollout_id(path).and_then(|id| Uuid::try_parse(id).ok()))
}

fn native_filename(path: &Path) -> bool {
    rollout_file::representation_priority(path) > 0
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("rollout-"))
}

#[derive(Debug, Default)]
pub(super) struct RolloutSelector {
    sqlite_home: sqlite_home::SqliteHome,
    snapshot: SqliteSnapshotCache,
    immutable_paths: Mutex<HashMap<String, PathBuf>>,
}

#[derive(Debug)]
pub(super) struct Unavailable(String);

impl std::fmt::Display for Unavailable {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.write_str(&self.0)
    }
}

impl std::error::Error for Unavailable {}

impl RolloutSelector {
    pub(super) fn from_env() -> Self {
        Self {
            sqlite_home: sqlite_home::SqliteHome::from_env(),
            ..Self::default()
        }
    }

    pub(super) fn listing<'a, 'id>(
        &'a self,
        home: &'a Path,
        ids: impl IntoIterator<Item = &'id str>,
    ) -> RolloutCatalog<'a> {
        RolloutCatalog {
            selector: self,
            home,
            ids: ids.into_iter().map(str::to_owned).collect(),
            rows: OnceLock::new(),
            fresh_scan: None,
            ancestors: OnceLock::new(),
        }
    }

    pub(super) fn select<'a>(
        &self,
        home: &Path,
        paths: impl IntoIterator<Item = &'a Path>,
        id: &str,
        kind: IdKind,
    ) -> Result<Option<PathBuf>> {
        let candidates = Candidates::from_paths(home, paths, id, kind)?;
        if kind == IdKind::Rollout {
            return Ok(candidates.filesystem_choice(home, kind));
        }
        let rows = self.current_rows(home, std::iter::once(id))?;
        candidates.resolve(home, id, rows.get(id), None)
    }

    pub(super) fn resume_selection(
        &self,
        home: &Path,
        scan: &CodexFileScan,
        id: &str,
        fresh: bool,
    ) -> Result<Option<(PathBuf, bool)>> {
        let rows = self.current_rows(home, [id])?;
        let current = rows.get(id);
        let candidates = Candidates::from_paths(home, scan.paths(home, id), id, IdKind::Thread)?;
        let mut path = candidates.resolve(home, id, current, None)?;
        if path.is_none() {
            path = self.find_exact(home, id, IdKind::Thread)?;
        }
        let Some(path) = path else {
            return Ok(None);
        };
        let fresh = fresh && scan.notes.is_empty() && !scan.indices(id).is_empty();
        let native = Self::native_selects(home, scan, fresh, id, &path, current)?;
        Ok(Some((path, native)))
    }

    fn native_selects(
        home: &Path,
        scan: &CodexFileScan,
        fresh: bool,
        id: &str,
        selected: &Path,
        current: Option<&SelectedRollout>,
    ) -> Result<bool> {
        if is_archived(home, selected) {
            return Ok(false);
        }
        let Some(native) = native_representation(home, selected) else {
            return Ok(false);
        };
        if !same_native_copy(&native, selected)? {
            return Ok(false);
        }
        let Some((_, header)) = CodexSession::selected_header(selected, id)? else {
            return Ok(false);
        };
        let Some(mode) = native_header_mode(&header) else {
            return Ok(false);
        };
        if let Some(row) = current {
            if row.archived
                || row.paginated != Some(mode == HistoryMode::Paginated)
                || (!row.canonical_name() && mode == HistoryMode::Paginated)
            {
                return Ok(false);
            }
            let Some(name) = rollout_file::jsonl_file_name(&row.path) else {
                return Ok(false);
            };
            let plain = row.path.with_file_name(name);
            if row.path.extension().is_some_and(|suffix| suffix == "gz")
                || !managed_recording(home, &plain)
                || (!plain.is_file()
                    && !managed_recording(home, &plain.with_extension("jsonl.zst")))
            {
                return Ok(false);
            }
            if let Some(recorded) = native_representation(home, &row.path) {
                return Ok(same_native_copy(&recorded, selected)?);
            }
            if row.paginated == Some(true)
                || rollout_file::jsonl_file_name(&row.path)
                    == rollout_file::jsonl_file_name(selected)
            {
                return Ok(false);
            }
        }
        if !native_filename(selected) || scan.skipped_directories {
            return Ok(false);
        }
        // Codex keeps the first file at the newest (timestamp, rollout ID). Plain JSONL
        // hides a compressed sibling only in the same directory.
        let selected_rank = native_rank(selected);
        let agrees = |path: &Path| {
            if !native_filename(path)
                || !thread_id(path).is_some_and(|thread| thread.eq_ignore_ascii_case(id))
                || native_rank(path) < selected_rank
            {
                return true;
            }
            native_representation(home, path)
                .is_some_and(|candidate| same_native_copy(&candidate, selected).unwrap_or(false))
        };
        let active = home.join("sessions");
        if fresh {
            return Ok(scan
                .files
                .iter()
                .filter(|path| path.starts_with(&active))
                .all(|path| agrees(path)));
        }
        let mut agree = true;
        let inventory = walk_roots([active], super::SCAN_LIMIT, |path| {
            agree &= agrees(path);
            Ok(())
        })?;
        Ok(agree && inventory.complete())
    }

    /// A filesystem fallback can locate a thread or immutable rollout after the listing fills up.
    pub(super) fn find_exact(
        &self,
        home: &Path,
        id: &str,
        kind: IdKind,
    ) -> Result<Option<PathBuf>> {
        if kind == IdKind::Thread {
            return Self::scan_exact(home, id, kind);
        }
        self.cached_immutable(home, id, || Self::scan_exact(home, id, kind))
    }

    fn cached_immutable(
        &self,
        home: &Path,
        id: &str,
        scan: impl FnOnce() -> Result<Option<PathBuf>>,
    ) -> Result<Option<PathBuf>> {
        // Forks can share an immutable ancestor that is older than the bounded listing.
        // Keep only successful lookups. A missing archive can appear during this process.
        let cached = self
            .immutable_paths
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(id)
            .cloned();
        if let Some(path) = &cached
            && managed_path(home, path).is_some()
            && let Some(valid) = immutable_sibling(home, path)
        {
            let valid = if valid.file_name() == path.file_name() {
                path.clone()
            } else {
                valid
            };
            return Ok(Some(
                restored_rollout(home, &valid, IdKind::Rollout).unwrap_or(valid),
            ));
        }
        let found = scan()?;
        let mut paths = self
            .immutable_paths
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if paths.get(id) == cached.as_ref() {
            if let Some(path) = &found {
                if paths.len() >= super::SCAN_LIMIT {
                    paths.clear();
                }
                paths.insert(id.to_owned(), path.clone());
            } else {
                paths.remove(id);
            }
        }
        Ok(found)
    }

    fn scan_exact(home: &Path, id: &str, kind: IdKind) -> Result<Option<PathBuf>> {
        let mut candidates = Candidates::new(home);
        let inventory = visit_rollouts(home, |path| candidates.observe(path, id, kind, None))?;
        let found = candidates.exact_choice(home, kind)?;
        if found.is_none() {
            inventory.require_access()?;
        }
        Ok(found)
    }

    pub(super) fn available_rollouts(
        &self,
        home: &Path,
        requested: &HashSet<Uuid>,
    ) -> Result<HashSet<Uuid>> {
        let mut matches = HashMap::<Uuid, Option<(Candidates, bool)>>::new();
        if requested.is_empty() {
            return Ok(HashSet::new());
        }
        visit_rollouts(home, |path| {
            let Some(rollout) = rollout_id(path) else {
                return Ok(());
            };
            let Ok(id) = Uuid::try_parse(rollout) else {
                return Ok(());
            };
            if requested.contains(&id) {
                let entry = matches
                    .entry(id)
                    .or_insert_with(|| Some((Candidates::new(home), false)));
                if let Some((candidates, readable)) = entry
                    && let Ok(Some(mode)) = immutable_mode(path)
                {
                    *readable |= mode;
                    if candidates
                        .observe(path, rollout, IdKind::Rollout, Some(mode))
                        .is_err()
                    {
                        // This immutable ID occurs under two different threads.
                        *entry = None;
                    }
                }
            }
            Ok(())
        })?;
        let mut available = HashSet::new();
        let mut cache = self
            .immutable_paths
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        for (id, candidates) in matches {
            if let Some((candidates, true)) = candidates
                && let Some(path) = candidates.best
            {
                if cache.len() < super::SCAN_LIMIT {
                    cache.insert(id.to_string(), path);
                }
                available.insert(id);
            } else {
                cache.remove(&id.to_string());
            }
        }
        Ok(available)
    }

    fn current_rows<'a>(
        &self,
        home: &Path,
        ids: impl IntoIterator<Item = &'a str>,
    ) -> Result<HashMap<String, SelectedRollout>> {
        self.rows_in(&self.sqlite_home.resolve(home)?, ids)
    }

    pub(super) fn unavailable(
        &self,
        home: &Path,
        scan: &CodexFileScan,
        prior: &[(String, Option<SourceRevision>)],
    ) -> Result<HashSet<String>> {
        let rows = self.current_rows(home, prior.iter().map(|(id, _)| id.as_str()))?;
        let catalog = self.listing(home, []);
        let mut unavailable = HashSet::new();
        for (id, revision) in prior {
            let (Some(row), Some(file)) = (rows.get(id), revision.and_then(|source| source.file))
            else {
                continue;
            };
            // Older catalog entries do not record CODEX_HOME. Only retire a file observed in this home.
            let matches = |path: &Path| super::CodexAdapter::index_file_id(path) == Some(file);
            if !scan.paths(home, id).any(matches)
                && matching_sibling(home, &row.path, matches).is_none()
            {
                continue;
            }
            match Candidates::new(home).resolve(home, id, Some(row), Some(&catalog)) {
                Err(error) if error.is::<Unavailable>() => {
                    unavailable.insert(id.clone());
                }
                Ok(Some(selected))
                    if super::CodexAdapter::index_file_id(&selected)
                        .is_some_and(|current| current != file) =>
                {
                    unavailable.insert(id.clone());
                }
                _ => {}
            }
        }
        Ok(unavailable)
    }

    fn rows_in<'a>(
        &self,
        root: &Path,
        ids: impl IntoIterator<Item = &'a str>,
    ) -> Result<HashMap<String, SelectedRollout>> {
        self.read_threads(root, |connection| {
            let (has_mode, has_archived): (bool, bool) = connection.query_row(
                "SELECT max(name='history_mode'), max(name='archived') FROM pragma_table_info('threads')", [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            let mode = if has_mode {
                "CASE history_mode WHEN 'legacy' THEN 0 WHEN 'paginated' THEN 1 END"
            } else {
                "0"
            };
            let archived = if has_archived { "archived IS NOT 0" } else { "0" };
            let query = format!("SELECT rollout_path, {mode}, {archived} FROM threads WHERE id=?1");
            let mut statement = connection.prepare(&query)?;
            let mut selected = HashMap::new();
            for id in ids {
                let pointer = statement.query_row([id], |row| Ok(SelectedRollout {
                    path: PathBuf::from(row.get::<_, String>(0)?),
                    paginated: row.get(1)?,
                    archived: row.get(2)?,
                })).optional()?;
                if let Some(pointer) = pointer {
                    selected.insert(id.to_owned(), pointer);
                }
            }
            Ok(selected)
        })
    }

    fn read_threads<T: Default>(
        &self,
        root: &Path,
        read: impl FnOnce(&rusqlite::Connection) -> Result<T>,
    ) -> Result<T> {
        let database = root.join("state_5.sqlite");
        match fs::symlink_metadata(&database) {
            Ok(meta) => anyhow::ensure!(
                meta.is_file(),
                "Codex state database must be a regular file"
            ),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(T::default()),
            Err(error) => return Err(error).context("locating the Codex state database"),
        }
        self.snapshot
            .read(root, &database, |connection| {
                let kind: Option<String> = connection
                    .query_row(
                        "SELECT type FROM sqlite_schema WHERE name='threads' COLLATE NOCASE",
                        [],
                        |row| row.get(0),
                    )
                    .optional()?;
                let Some(kind) = kind else {
                    return Ok(T::default());
                };
                anyhow::ensure!(kind == "table", "Codex threads must be a table");
                read(connection)
            })
            .context("reading the current Codex rollouts from the state database")
    }

    pub(super) fn custom_ids_for_listing(
        &self,
        home: &Path,
        files: &[PathBuf],
    ) -> Result<Vec<String>> {
        let names: HashSet<_> = files
            .iter()
            .filter(|path| !canonical_filename(path))
            .filter_map(|path| Some(path.with_file_name(rollout_file::jsonl_file_name(path)?)))
            .collect();
        if names.is_empty() {
            return Ok(Vec::new());
        }
        let canonical_home = home.canonicalize()?;
        self.read_threads(&self.sqlite_home.resolve(home)?, |connection| {
            let mut statement = connection.prepare("SELECT id, rollout_path FROM threads WHERE typeof(id)='text' AND typeof(rollout_path)='text'")?;
            let mut rows = statement.query([])?;
            let mut ids = Vec::new();
            while let Some(row) = rows.next()? {
                let recorded = Path::new(row.get_ref(1)?.as_str()?);
                let Ok(relative) = recorded
                    .strip_prefix(home)
                    .or_else(|_| recorded.strip_prefix(&canonical_home))
                else {
                    continue;
                };
                let Some(name) = rollout_file::jsonl_file_name(recorded) else {
                    continue;
                };
                if names.contains(&home.join(relative).with_file_name(name)) {
                    let id = row.get_ref(0)?.as_str()?;
                    if Uuid::parse_str(id).is_ok() {
                        anyhow::ensure!(
                            ids.len() < super::SCAN_LIMIT,
                            "too many Codex state rows refer to the discovered custom rollout files"
                        );
                        ids.push(id.to_owned());
                    }
                }
            }
            Ok(ids)
        })
    }
}

impl super::CodexAdapter {
    /// A different launch directory can select another generation of the same logical thread.
    pub(crate) fn selected_state_agrees_at(
        &self,
        cwd: &Path,
        session: &omnis_ir::SessionRef,
        overrides: &crate::CodexLaunchOverrides,
    ) -> Result<bool> {
        let home = self
            .codex_home
            .as_deref()
            .context("Codex home was not found")?;
        let source = self.selector.sqlite_home.resolve(home)?;
        let destination = self.selector.sqlite_home.resolve_in(home, cwd, overrides)?;
        if source == destination || same_file::is_same_file(&source, &destination).unwrap_or(false)
        {
            return Ok(true);
        }
        let path = self.find_session_path(&session.id)?;
        let rows = self.selector.rows_in(&destination, [&*session.id])?;
        let (scan, cached) = self.session_scan(None);
        let fresh = !cached && scan.notes.is_empty() && !scan.indices(&session.id).is_empty();
        RolloutSelector::native_selects(
            home,
            &scan,
            fresh,
            &session.id,
            &path,
            rows.get(&session.id),
        )
    }
}

struct SelectedRollout {
    path: PathBuf,
    paginated: Option<bool>,
    archived: bool,
}

impl SelectedRollout {
    fn canonical_name(&self) -> bool {
        recorded_file_name(&self.path).is_some_and(canonical_filename)
    }
}

pub(super) struct RolloutCatalog<'a> {
    selector: &'a RolloutSelector,
    home: &'a Path,
    ids: HashSet<String>,
    // Read one SQLite snapshot per listing, including when a restored singleton looks like a legacy thread.
    rows: OnceLock<Result<HashMap<String, SelectedRollout>, String>>,
    pub(super) fresh_scan: Option<Arc<CodexFileScan>>,
    ancestors: OnceLock<Result<AncestorPaths, String>>,
}

impl RolloutCatalog<'_> {
    /// Check exact names once per indexing pass. A later batch can see restored rollouts.
    pub(super) fn find_unlisted_ancestor(&self, id: &str) -> Result<Option<PathBuf>> {
        let mut candidates = Candidates::new(self.home);
        let inventory = self.visit_physical(id, |path| {
            candidates.observe(path, id, IdKind::Rollout, None)
        })?;
        let found = candidates.exact_choice(self.home, IdKind::Rollout)?;
        if found.is_none() {
            inventory.require_access()?;
        }
        Ok(found)
    }

    fn visit_physical(
        &self,
        id: &str,
        mut each: impl FnMut(&Path) -> Result<()>,
    ) -> Result<Inventory> {
        if let Some(scan) = &self.fresh_scan {
            for path in scan.paths(self.home, id) {
                each(path)?;
            }
            return Ok(Inventory::default());
        }
        let paths = self
            .ancestors
            .get_or_init(|| {
                AncestorPaths::scan(self.home, super::SCAN_LIMIT)
                    .map_err(|error| format!("{error:#}"))
            })
            .as_ref()
            .map_err(|message| anyhow!("{message}"))?;
        paths.visit(id, |path| {
            if discovery_path(self.home, path) {
                each(path)?;
            }
            Ok(())
        })
    }

    pub(super) fn select_listed<'a>(
        &self,
        paths: impl IntoIterator<Item = (&'a Path, bool)>,
        id: &str,
    ) -> Result<Option<PathBuf>> {
        self.select(paths.into_iter().map(|(path, mode)| (path, Some(mode))), id)
    }

    pub(super) fn select_for_index<'a>(
        &self,
        paths: impl IntoIterator<Item = &'a Path>,
        id: &str,
    ) -> Result<Option<PathBuf>> {
        self.select(paths.into_iter().map(|path| (path, None)), id)
    }

    fn select<'a>(
        &self,
        paths: impl IntoIterator<Item = (&'a Path, Option<bool>)>,
        id: &str,
    ) -> Result<Option<PathBuf>> {
        if !self.ids.contains(id) {
            return self.selector.select(
                self.home,
                paths.into_iter().map(|(path, _)| path),
                id,
                IdKind::Thread,
            );
        }
        let mut candidates = Candidates::new(self.home);
        for (path, paginated) in paths {
            candidates.observe(path, id, IdKind::Thread, paginated)?;
        }
        let rows = self
            .rows
            .get_or_init(|| {
                self.selector
                    .current_rows(self.home, self.ids.iter().map(String::as_str))
                    .map_err(|error| format!("{error:#}"))
            })
            .as_ref()
            .map_err(|message| anyhow!("{message}"))?;
        let current = rows.get(id);
        if let (Some(best), Some(current)) = (&candidates.best, current)
            && current.canonical_name()
            && current
                .paginated
                .is_some_and(|mode| !mode || candidates.best_paginated)
            && best.parent() == current.path.parent()
            && rollout_file::jsonl_file_name(best).and_then(Path::file_name)
                == rollout_file::jsonl_file_name(&current.path).and_then(Path::file_name)
        {
            // Both discovery and index reads have validated this candidate's header.
            return Ok(candidates.best);
        }
        candidates.resolve(self.home, id, current, Some(self))
    }
}

/// Index hints must name a regular file under the same directories and depth as discovery.
pub(super) fn discovery_path(home: &Path, path: &Path) -> bool {
    let Ok(root) = fs::canonicalize(home) else {
        return false;
    };
    let Ok(relative) = path
        .strip_prefix(home)
        .or_else(|_| path.strip_prefix(&root))
    else {
        return false;
    };
    let Ok(tail) = relative
        .strip_prefix("sessions")
        .or_else(|_| relative.strip_prefix("archived_sessions"))
    else {
        return false;
    };
    tail.components().count() <= super::SCAN_DEPTH + 1
        && tail
            .components()
            .all(|part| matches!(part, std::path::Component::Normal(_)))
        && fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_file())
        && fs::canonicalize(path).is_ok_and(|actual| actual == root.join(relative))
}

/// A state pointer or a stale discovery hint can name an obsolete path outside both rollout roots.
pub(super) fn managed_path(home: &Path, path: &Path) -> Option<PathBuf> {
    let path = provider_file(home, path)?;
    let root = home.canonicalize().ok()?;
    ["sessions", "archived_sessions"]
        .iter()
        .any(|name| path.starts_with(root.join(name)))
        .then_some(path)
}

// SQLite can retain absolute paths from another Codex home. Inspect only components
// beneath this home and stop at the first symlink, before looking up its target.
fn managed_recording(home: &Path, path: &Path) -> bool {
    let Ok(root) = home.canonicalize() else {
        return false;
    };
    let Ok(relative) = path
        .strip_prefix(home)
        .or_else(|_| path.strip_prefix(&root))
    else {
        return false;
    };
    if !(relative.starts_with("sessions") || relative.starts_with("archived_sessions"))
        || !relative
            .components()
            .all(|part| matches!(part, std::path::Component::Normal(_)))
    {
        return false;
    }
    let mut parent = root;
    for part in relative.components() {
        parent.push(part);
        match fs::symlink_metadata(&parent) {
            Ok(meta) if meta.is_symlink() => return false,
            Ok(_) => {}
            Err(error) => return error.kind() == io::ErrorKind::NotFound,
        }
    }
    true
}

// Restored state can contain an absolute path written by Codex on the other platform.
fn recorded_file_name(path: &Path) -> Option<&Path> {
    let text = path.to_str()?;
    let windows = text.starts_with(r"\\")
        || text.starts_with("//")
        || matches!(text.as_bytes(), [drive, b':', b'/' | b'\\', ..] if drive.is_ascii_alphabetic());
    let name = if windows {
        text.rsplit(['/', '\\']).next()?
    } else {
        text.rsplit('/').next()?
    };
    (!name.contains('\\')).then(|| Path::new(name))
}

fn relocated_rollout(
    home: &Path,
    recorded: &Path,
    id: &str,
    paginated: bool,
    catalog: Option<&RolloutCatalog<'_>>,
) -> Result<Option<PathBuf>> {
    let Some(recorded) = recorded_file_name(recorded) else {
        return Ok(None);
    };
    if !canonical_filename(recorded) || thread_id(recorded) != Some(id) {
        return Ok(None);
    }
    let name = rollout_file::jsonl_file_name(recorded);
    let mut selected: Option<PathBuf> = None;
    let mut digest = None;
    let mut remaining = 64 * 1024 * 1024;
    let rank = |path: &Path| {
        (
            !is_archived(home, path),
            rollout_file::representation_priority(path),
        )
    };
    let observe = |candidate: &Path| -> Result<()> {
        if rollout_file::jsonl_file_name(candidate) != name
            || !matching_header(candidate, id, paginated)
        {
            return Ok(());
        }
        if let Some(prior) = &selected {
            let same_codec = prior.extension() == candidate.extension();
            if !same_codec || !same_file::is_same_file(prior, candidate)? {
                let expected = match digest {
                    Some(value) => value,
                    None => copy_digest(prior, &mut remaining)?,
                };
                if expected != copy_digest(candidate, &mut remaining)? {
                    bail!(
                        "conflicting local copies of the recorded Codex rollout for thread `{id}`"
                    );
                }
                digest = Some(expected);
            }
            if (rank(prior), prior.as_path()) >= (rank(candidate), candidate) {
                return Ok(());
            }
        }
        selected = Some(candidate.to_owned());
        Ok(())
    };
    visit_immutable_copies(home, rollout_id(recorded).unwrap_or(id), catalog, observe)?;
    Ok(selected)
}

/// Visits managed copies of this physical rollout, including names beyond the discovery limit.
pub(super) fn visit_immutable_copies(
    home: &Path,
    rollout: &str,
    catalog: Option<&RolloutCatalog<'_>>,
    mut visit: impl FnMut(&Path) -> Result<()>,
) -> Result<()> {
    let exact = |path: &Path| {
        if rollout_id(path) == Some(rollout) && (catalog.is_some() || discovery_path(home, path)) {
            visit(path)?;
        }
        Ok(())
    };
    let inventory = if let Some(catalog) = catalog {
        catalog.visit_physical(rollout, exact)
    } else {
        visit_rollouts(home, exact)
    }?;
    inventory.require_access()
}

fn copy_digest(path: &Path, remaining: &mut u64) -> Result<[u8; 32]> {
    let before = super::rollout_cache::ObservedRollout::read(path)
        .and_then(|observed| observed.stamp)
        .context("verifying a relocated Codex rollout")?;
    let encoded = fs::metadata(path)?.len();
    if encoded > *remaining {
        bail!("local copies of the recorded Codex rollout exceed the comparison limit");
    }
    let mut hash = Sha256::new();
    let mut reader = rollout_file::open(path)?.take(*remaining + 1);
    let mut buffer = [0_u8; 16 * 1024];
    let mut count = 0_u64;
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hash.update(&buffer[..read]);
        count += read as u64;
    }
    *remaining = remaining
        .checked_sub(count.max(encoded))
        .context("local copies of the recorded Codex rollout exceed the comparison limit")?;
    if super::rollout_cache::ObservedRollout::read(path).and_then(|observed| observed.stamp)
        != Some(before)
    {
        bail!("a local copy of the recorded Codex rollout changed during comparison");
    }
    Ok(hash.finalize().into())
}

fn matching_header(path: &Path, id: &str, paginated: bool) -> bool {
    matches!(
        CodexSession::parse_metadata_path_result(path),
        Ok(Some(session)) if session.id == id && (!paginated || session.is_paginated)
    )
}

// An unreadable or damaged parent can still constrain the choice of a backup. The lineage
// reader compares the surviving rows before it can accept a different physical file.
fn immutable_mode(path: &Path) -> Result<Option<bool>> {
    let revision = super::CodexAdapter::index_file_revision(path);
    let records = match rollout_file::header(path) {
        Ok(records) => records,
        Err(error)
            if error
                .downcast_ref::<io::Error>()
                .is_some_and(|e| e.kind() == io::ErrorKind::PermissionDenied) =>
        {
            return Ok(Some(false));
        }
        Err(error) => return Err(error),
    };
    anyhow::ensure!(
        revision.is_some() && super::CodexAdapter::index_file_revision(path) == revision,
        "selected Codex rollout changed during header lookup"
    );
    let Some(header) = records.first() else {
        return Ok(None);
    };
    let Some(thread) = thread_id(path) else {
        return Ok(None);
    };
    if header_identity(header, thread).is_none() {
        return Ok(None);
    }
    if header.get("payload").is_none() || damaged_header_type(header) {
        return Ok(Some(false));
    }
    let paginated = HistoryMode::is_paginated(&header["payload"]["history_mode"]);
    Ok(
        (header["type"] == "session_meta" && (paginated || damaged_paginated_mode(header)))
            .then_some(paginated),
    )
}

fn selected_custom_file(
    home: &Path,
    current: &SelectedRollout,
    id: &str,
    paginated: bool,
) -> Result<PathBuf> {
    let missing = || {
        Unavailable(format!(
            "the selected Codex rollout is missing, incompatible, or ambiguous for thread `{id}`. Restore the file recorded by Codex before continuing."
        ))
    };
    if !managed_recording(home, &current.path)
        || (paginated
            && !ids(&current.path).is_some_and(|(owner, rollout)| owner == id && rollout != id))
    {
        return Err(missing().into());
    }
    let name = rollout_file::jsonl_file_name(&current.path).ok_or_else(missing)?;
    let plain = current.path.with_file_name(name);
    let gzip = current.path.extension().is_some_and(|ext| ext == "gz");
    let candidates = [
        plain.clone(),
        plain.with_extension("jsonl.zst"),
        plain.with_extension("jsonl.gz"),
    ];
    for candidate in candidates
        .iter()
        .filter(|path| path.extension().is_some_and(|ext| ext == "gz") == gzip)
    {
        if !managed_recording(home, candidate) {
            return Err(missing().into());
        }
        match fs::symlink_metadata(candidate) {
            Ok(meta) if meta.is_file() => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error)
                    .context("checking the file selected by the Codex state database");
            }
            Ok(_) => return Err(missing().into()),
        }
        let path = managed_path(home, candidate).ok_or_else(missing)?;
        let selected = CodexSession::selected_header(&path, id).map_err(|error| {
            if error.is::<rollout_file::CompressedHeaderError>() {
                error.context(missing())
            } else {
                error
            }
        })?;
        let (_, header) = selected.ok_or_else(missing)?;
        let mode = if paginated {
            HistoryMode::Paginated
        } else {
            HistoryMode::Legacy
        };
        let payload = &header["payload"];
        let owner = payload
            .get("id")
            .or_else(|| (!paginated).then(|| &payload["session_id"]));
        if owner.and_then(Value::as_str) != Some(id) || native_header_mode(&header) != Some(mode) {
            return Err(missing().into());
        }
        return Ok(path);
    }
    Err(missing().into())
}

fn matching_sibling(
    home: &Path,
    recorded: &Path,
    accepts: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    let plain = recorded.with_file_name(rollout_file::jsonl_file_name(recorded)?);
    ["jsonl", "jsonl.zst", "jsonl.gz"]
        .into_iter()
        .map(|extension| plain.with_extension(extension))
        .filter(|path| managed_recording(home, path))
        .filter_map(|path| managed_path(home, &path))
        .find(|path| accepts(path))
}

fn immutable_sibling(home: &Path, recorded: &Path) -> Option<PathBuf> {
    matching_sibling(home, recorded, |path| {
        immutable_mode(path).ok().flatten().is_some()
    })
}

fn compatible_copy(home: &Path, recorded: &Path, id: &str, paginated: bool) -> Option<PathBuf> {
    if thread_id(recorded)? != id {
        return None;
    }
    matching_sibling(home, recorded, |path| matching_header(path, id, paginated))
}
fn same_native_copy(candidate: &Path, selected: &Path) -> io::Result<bool> {
    Ok(candidate.extension() == selected.extension()
        && same_file::is_same_file(candidate, selected)?)
}

fn native_representation(home: &Path, path: &Path) -> Option<PathBuf> {
    let plain = path.with_file_name(rollout_file::jsonl_file_name(path)?);
    managed_path(home, &plain).or_else(|| managed_path(home, &plain.with_extension("jsonl.zst")))
}

// Archive and unarchive preserve the full basename, including a revert's physical rollout ID.
fn moved_recording(home: &Path, recorded: &Path) -> Option<PathBuf> {
    if is_archived(home, recorded) {
        return restored_basename(home, recorded);
    }
    Some(
        home.join("archived_sessions")
            .join(rollout_file::jsonl_file_name(recorded)?),
    )
}

fn is_archived(home: &Path, path: &Path) -> bool {
    path.starts_with(home.join("archived_sessions"))
        || (!path.starts_with(home)
            && home
                .canonicalize()
                .is_ok_and(|root| path.starts_with(root.join("archived_sessions"))))
}

// Codex restores an archive into the active directory for the date in its original name.
fn restored_basename(home: &Path, archived: &Path) -> Option<PathBuf> {
    let name = rollout_file::jsonl_file_name(archived)?;
    let stamp = name.to_str()?.strip_prefix("rollout-")?.get(..10)?;
    let day = stamp.parse::<chrono::NaiveDate>().ok()?;
    Some(
        home.canonicalize()
            .ok()?
            .join("sessions")
            .join(day.format("%Y/%m/%d").to_string())
            .join(name),
    )
}

fn restored_rollout(home: &Path, archived: &Path, kind: IdKind) -> Option<PathBuf> {
    if !is_archived(home, archived) {
        return None;
    }
    let plain = restored_basename(home, archived)?;
    match kind {
        IdKind::Thread => compatible_copy(home, &plain, thread_id(archived)?, false),
        IdKind::Rollout => immutable_sibling(home, &plain),
    }
}

pub(super) fn provider_can_read_ancestors<'a>(
    home: &Path,
    scan: &CodexFileScan,
    sources: impl Iterator<Item = &'a Path>,
) -> Result<bool> {
    let mut selected = HashMap::new();
    for path in sources {
        // Codex resolves an immutable reference from canonical filenames. It ignores gzip.
        if !native_filename(path) || !canonical_filename(path) {
            return Ok(false);
        }
        let Some(native) = native_representation(home, path) else {
            return Ok(false);
        };
        let archived = is_archived(home, path);
        if !same_native_copy(&native, path)?
            || (archived
                && restored_basename(home, path)
                    .and_then(|name| native_representation(home, &name))
                    .is_some())
        {
            return Ok(false);
        }
        let Some(id) = rollout_id(path) else {
            return Ok(false);
        };
        selected.insert(id, (path, archived));
    }
    let mut readable = true;
    let mut check = |candidate: &Path| {
        let Some((source, archived)) = rollout_id(candidate).and_then(|id| selected.get(id)) else {
            return Ok(());
        };
        if readable
            && native_filename(candidate)
            && canonical_filename(candidate)
            && (*archived || !is_archived(home, candidate))
            && let Some(native) = native_representation(home, candidate)
        {
            // The first active match wins, followed by the first archive. Directory entry order
            // cannot distinguish two different files in the same root.
            readable = same_native_copy(&native, source)?;
        }
        Ok(())
    };
    if !selected.is_empty() && scan.skipped_directories {
        return Ok(false);
    }
    if scan.notes.is_empty() {
        for id in selected.keys() {
            for index in scan.indices(id) {
                check(&scan.files[*index])?;
            }
        }
    } else if !selected.is_empty() {
        let inventory = walk_roots(rollout_roots(home), super::SCAN_LIMIT, check)?;
        if !inventory.complete() {
            return Ok(false);
        }
    }
    Ok(readable)
}

struct Candidates {
    active: PathBuf,
    best: Option<PathBuf>,
    best_paginated: bool,
    read_error: Option<anyhow::Error>,
}

impl Candidates {
    fn new(home: &Path) -> Self {
        Self {
            active: home.join("sessions"),
            best: None,
            best_paginated: false,
            read_error: None,
        }
    }

    fn from_paths<'a>(
        home: &Path,
        paths: impl IntoIterator<Item = &'a Path>,
        id: &str,
        kind: IdKind,
    ) -> Result<Self> {
        let mut candidates = Self::new(home);
        for path in paths {
            candidates.observe(path, id, kind, None)?;
        }
        Ok(candidates)
    }

    fn resolve(
        self,
        home: &Path,
        id: &str,
        current: Option<&SelectedRollout>,
        catalog: Option<&RolloutCatalog<'_>>,
    ) -> Result<Option<PathBuf>> {
        if let Some(current) = current {
            let paginated = current
                .paginated
                .ok_or_else(|| Unavailable("the selected Codex rollout is missing, incompatible, or ambiguous because the state row declares an unsupported history mode".to_owned()))?;
            if !current.canonical_name() {
                return selected_custom_file(home, current, id, paginated).map(Some);
            }
            anyhow::ensure!(
                recorded_file_name(&current.path).and_then(thread_id) == Some(id),
                Unavailable("the selected Codex rollout is missing, incompatible, or ambiguous because the state row names a different thread".to_owned())
            );
            let managed = managed_recording(home, &current.path);
            let path = if managed {
                compatible_copy(home, &current.path, id, paginated).or_else(|| {
                    let moved = moved_recording(home, &current.path)?;
                    compatible_copy(home, &moved, id, paginated)
                })
            } else {
                relocated_rollout(home, &current.path, id, paginated, catalog)?
            };
            if let Some(path) = path {
                return Ok(Some(path));
            }
            if paginated || ids(&current.path).is_some_and(|(thread, rollout)| thread != rollout) {
                return Err(Unavailable(format!(
                    "the selected rollout for paginated Codex thread `{id}` is missing or invalid"
                ))
                .into());
            }
        }
        Ok(self.filesystem_choice(home, IdKind::Thread))
    }

    fn exact_choice(self, home: &Path, kind: IdKind) -> Result<Option<PathBuf>> {
        if self.best.is_none()
            && let Some(error) = self.read_error
        {
            return Err(error);
        }
        Ok(self.filesystem_choice(home, kind))
    }

    fn filesystem_choice(self, home: &Path, kind: IdKind) -> Option<PathBuf> {
        self.best
            .map(|best| restored_rollout(home, &best, kind).unwrap_or(best))
    }

    fn observe(
        &mut self,
        path: &Path,
        id: &str,
        kind: IdKind,
        paginated: Option<bool>,
    ) -> Result<()> {
        let Some((thread, rollout)) = ids(path) else {
            return Ok(());
        };
        let key = match kind {
            IdKind::Thread => thread,
            IdKind::Rollout => rollout,
        };
        if key != id {
            return Ok(());
        }
        let mut other_owner = false;
        if let Some(best) = &self.best {
            other_owner = kind == IdKind::Rollout && thread_id(best) != Some(thread);
            if !other_owner && self.compare(path, best) != Ordering::Greater {
                return Ok(());
            }
        }
        let paginated = if let Some(mode) = paginated {
            mode
        } else {
            let mode = match kind {
                IdKind::Thread => CodexSession::parse_metadata_path_result(path)
                    .map(|session| session.map(|session| session.is_paginated)),
                IdKind::Rollout => immutable_mode(path),
            };
            match mode {
                Ok(Some(mode)) => mode,
                Ok(None) => return Ok(()),
                Err(error) => {
                    self.read_error.get_or_insert(error);
                    return Ok(());
                }
            }
        };
        if other_owner {
            bail!("Codex rollout ID `{id}` belongs to more than one thread");
        }
        self.best = Some(path.to_path_buf());
        self.best_paginated = paginated;
        Ok(())
    }

    fn compare(&self, a: &Path, b: &Path) -> Ordering {
        a.starts_with(&self.active)
            .cmp(&b.starts_with(&self.active))
            // Codex cannot open an external gzip copy. A revert still follows its rollout timestamp.
            .then_with(|| {
                let native_or_reverted = |path| {
                    rollout_file::representation_priority(path) > 0
                        || thread_id(path) != rollout_id(path)
                };
                native_or_reverted(a).cmp(&native_or_reverted(b))
            })
            .then_with(|| timestamp(a).cmp(&timestamp(b)))
            .then_with(|| rollout_id(a).cmp(&rollout_id(b)))
            .then_with(|| {
                rollout_file::representation_priority(a)
                    .cmp(&rollout_file::representation_priority(b))
            })
            .then_with(|| a.cmp(b))
    }
}

#[derive(Clone, Copy, Default)]
pub(super) struct Inventory {
    inaccessible: bool,
    limited: bool,
}

impl Inventory {
    fn complete(self) -> bool {
        !self.inaccessible && !self.limited
    }

    fn require_access(self) -> Result<()> {
        if self.inaccessible {
            return Err(InaccessibleInventory.into());
        }
        Ok(())
    }

    fn accessible<T>(&mut self, result: io::Result<T>) -> io::Result<Option<T>> {
        match result {
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                self.inaccessible = true;
                Ok(None)
            }
            result => result.map(Some),
        }
    }
}

#[derive(Debug)]
pub(super) struct InaccessibleInventory;

impl std::fmt::Display for InaccessibleInventory {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.write_str("Codex could not search all source rollout directories")
    }
}

impl std::error::Error for InaccessibleInventory {}

fn rollout_roots(home: &Path) -> [PathBuf; 2] {
    [home.join("sessions"), home.join("archived_sessions")]
}

pub(super) fn visit_rollouts(
    home: &Path,
    visit: impl FnMut(&Path) -> Result<()>,
) -> Result<Inventory> {
    #[cfg(test)]
    tests::FULL_WALKS.with(|walks| walks.set(walks.get() + 1));
    walk_roots(rollout_roots(home), usize::MAX, visit)
}

fn walk_roots(
    roots: impl IntoIterator<Item = PathBuf>,
    mut remaining: usize,
    mut visit: impl FnMut(&Path) -> Result<()>,
) -> Result<Inventory> {
    let mut inventory = Inventory::default();
    for directory in roots {
        visit_files(
            &directory,
            super::SCAN_DEPTH,
            &mut remaining,
            &mut inventory,
            &mut visit,
        )?;
    }
    Ok(inventory)
}

fn visit_files(
    root: &Path,
    depth: usize,
    remaining: &mut usize,
    inventory: &mut Inventory,
    visit: &mut impl FnMut(&Path) -> Result<()>,
) -> Result<()> {
    let metadata = match fs::symlink_metadata(root) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        metadata => inventory.accessible(metadata)?,
    };
    if !metadata.is_some_and(|metadata| metadata.is_dir()) {
        inventory.limited = true;
        return Ok(());
    }
    let Some(entries) = inventory.accessible(fs::read_dir(root))? else {
        return Ok(());
    };
    for entry in entries {
        if *remaining == 0 {
            inventory.limited = true;
            break;
        }
        *remaining -= 1;
        let Some(entry) = inventory.accessible(entry)? else {
            continue;
        };
        let Some(kind) = inventory.accessible(entry.file_type())? else {
            continue;
        };
        if kind.is_file() {
            visit(&entry.path())?;
        } else if kind.is_dir() {
            if depth == 0 {
                inventory.limited = true;
            } else {
                visit_files(&entry.path(), depth - 1, remaining, inventory, visit)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    thread_local! {
        pub(super) static FULL_WALKS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    const THREAD: &str = "10000000-0000-4000-a000-000000000001";
    const FIRST: &str = "20000000-0000-4000-a000-000000000002";
    const SECOND: &str = "20000000-0000-4000-a000-000000000003";

    fn rollout(dir: &Path, stamp: &str, suffix: &str) -> Result<PathBuf> {
        fs::create_dir_all(dir)?;
        let path = dir.join(format!("rollout-{stamp}-{THREAD}{suffix}.jsonl"));
        fs::write(
            &path,
            serde_json::json!({
                "type": "session_meta",
                "payload": { "id": THREAD, "history_mode": "paginated" }
            })
            .to_string(),
        )?;
        Ok(path)
    }

    #[test]
    fn native_fallback_checks_all_files_with_the_selected_native_rank() -> Result<()> {
        for name in [
            format!("rollout-2026-01-01T00-00-00-{THREAD}_{FIRST}.jsonl"),
            format!("rollout-{THREAD}.jsonl"),
        ] {
            let home = tempfile::tempdir()?;
            let first = home.path().join("sessions/2026/01/01");
            let second = home.path().join("sessions/2026/01/02");
            fs::create_dir_all(&first)?;
            fs::create_dir_all(&second)?;
            let plain = first.join(&name);
            let compressed = second.join(&name).with_extension("jsonl.zst");
            let header = serde_json::json!({"type":"session_meta","payload":{"id":THREAD}});
            fs::write(&plain, header.to_string())?;
            let select = |paths: &[&Path]| -> Result<(PathBuf, bool)> {
                let scan = CodexFileScan {
                    files: paths.iter().map(|p| p.to_path_buf()).collect(),
                    ..Default::default()
                };
                RolloutSelector::default()
                    .resume_selection(home.path(), &scan, THREAD, false)?
                    .context("selected rollout")
            };
            assert!(select(&[&plain])?.1);
            let frame = zstd::stream::encode_all(fs::File::open(&plain)?, 1)?;
            fs::write(plain.with_extension("jsonl.zst"), &frame)?;
            assert!(select(&[&plain])?.1);
            fs::write(&compressed, frame)?;
            // Discovery can predate the second copy or stop before its directory.
            assert!(!select(&[&plain])?.1);
            assert!(!select(&[&plain, &compressed])?.1);
            for limit in [1, 10] {
                let scan = super::super::scan_session_files(home.path(), limit);
                assert!(
                    !RolloutSelector::default()
                        .resume_selection(home.path(), &scan, THREAD, true)?
                        .unwrap()
                        .1
                );
            }

            let alias = second.join(&name);
            fs::hard_link(&plain, &alias)?;
            assert!(select(&[&plain, &alias, &compressed])?.1);
            fs::remove_file(&alias)?;
            let db = rusqlite::Connection::open(home.path().join("state_5.sqlite"))?;
            db.execute_batch("CREATE TABLE threads(id TEXT PRIMARY KEY, rollout_path TEXT)")?;
            db.execute(
                "INSERT INTO threads VALUES(?1, ?2)",
                (THREAD, compressed.to_str()),
            )?;
            drop(db);
            let (selected, native) = select(&[&plain, &compressed])?;
            assert!(native);
            assert!(same_file::is_same_file(selected, compressed)?);
        }
        Ok(())
    }

    #[test]
    fn finds_active_reverts_and_the_exact_archived_ancestor() -> Result<()> {
        let home = tempfile::tempdir()?;
        let day = home.path().join("sessions/2026/01/01");
        let older = rollout(&day, "2026-01-01T00-00-00", &format!("_{FIRST}"))?;
        let newest = rollout(&day, "2026-01-01T00-00-00", &format!("_{SECOND}"))?;
        let archived = rollout(
            &home.path().join("archived_sessions"),
            "2026-01-02T00-00-00",
            "",
        )?;
        let selector = RolloutSelector::default();
        assert_eq!(
            selector.find_exact(home.path(), THREAD, IdKind::Thread)?,
            Some(newest)
        );
        assert_eq!(
            selector.find_exact(home.path(), THREAD, IdKind::Rollout)?,
            Some(archived)
        );
        assert_eq!(
            selector.find_exact(home.path(), FIRST, IdKind::Rollout)?,
            Some(older)
        );
        let requested = [
            THREAD,
            FIRST,
            SECOND,
            "30000000-0000-4000-a000-000000000004",
        ]
        .map(Uuid::try_parse)
        .into_iter()
        .collect::<Result<HashSet<_>, _>>()?;
        let available = selector.available_rollouts(home.path(), &requested)?;
        assert_eq!(
            available,
            [THREAD, FIRST, SECOND]
                .map(|id| Uuid::try_parse(id).unwrap())
                .into()
        );
        Ok(())
    }

    #[test]
    fn concurrent_forks_with_distinct_unlisted_parents_share_one_filename_walk() -> Result<()> {
        let home = tempfile::tempdir()?;
        let directory = home.path().join("archived_sessions");
        let known = rollout(&directory, "2026-01-01T00-00-00", &format!("_{FIRST}"))?;
        let selector = RolloutSelector::default();
        let batch = selector.listing(home.path(), [THREAD]);
        std::thread::scope(|scope| -> Result<()> {
            let readers: Vec<_> = (0..12)
                .map(|_| {
                    scope.spawn(|| -> Result<_> {
                        let missing = Uuid::new_v4().to_string();
                        assert!(batch.find_unlisted_ancestor(&missing)?.is_none());
                        assert_eq!(batch.find_unlisted_ancestor(FIRST)?.as_ref(), Some(&known));
                        Ok(FULL_WALKS.with(std::cell::Cell::get))
                    })
                })
                .collect();
            let mut walks = 0;
            for reader in readers {
                walks += reader.join().unwrap()?;
            }
            assert_eq!(walks, 1);
            Ok(())
        })?;
        let restored = rollout(&directory, "2026-01-02T00-00-00", &format!("_{SECOND}"))?;
        assert!(batch.find_unlisted_ancestor(SECOND)?.is_none());
        let next_batch = selector.listing(home.path(), [THREAD]);
        assert_eq!(next_batch.find_unlisted_ancestor(SECOND)?, Some(restored));
        Ok(())
    }

    #[test]
    fn the_batch_still_resolves_an_older_ancestor_when_its_filename_index_spills() -> Result<()> {
        let home = tempfile::tempdir()?;
        let archive = home.path().join("archived_sessions");
        #[cfg(unix)]
        let archive = {
            use std::os::unix::ffi::OsStringExt;
            fs::create_dir_all(&archive)?;
            let non_utf8 = archive.join(std::ffi::OsString::from_vec(vec![0xff]));
            // Some Unix filesystems require valid UTF-8 names.
            if fs::create_dir(&non_utf8).is_ok() {
                non_utf8
            } else {
                archive
            }
        };
        let archived = rollout(&archive, "2026-01-01T00-00-00", &format!("_{FIRST}"))?;
        let active = rollout(&home.path().join("sessions"), "2026-01-02T00-00-00", "")?;
        let selector = RolloutSelector::default();
        for limit in [0, 1] {
            let batch = selector.listing(home.path(), [THREAD]);
            assert!(
                batch
                    .ancestors
                    .set(Ok(AncestorPaths::scan(home.path(), limit)?))
                    .is_ok()
            );
            assert!(batch.find_unlisted_ancestor(SECOND)?.is_none());
            assert_eq!(
                batch.find_unlisted_ancestor(THREAD)?.as_ref(),
                Some(&active)
            );
            assert_eq!(
                batch.find_unlisted_ancestor(FIRST)?.as_ref(),
                Some(&archived)
            );
        }
        Ok(())
    }

    #[test]
    fn reuses_an_immutable_ancestor_until_it_moves_and_retries_missing_ancestors() -> Result<()> {
        let home = tempfile::tempdir()?;
        let selector = RolloutSelector::default();
        let scans = std::cell::Cell::new(0_usize);
        let lookup = || {
            selector.cached_immutable(home.path(), FIRST, || {
                assert!(selector.immutable_paths.try_lock().is_ok());
                scans.set(scans.get() + 1);
                RolloutSelector::scan_exact(home.path(), FIRST, IdKind::Rollout)
            })
        };
        let batch = selector.listing(home.path(), [THREAD]);
        assert_eq!(lookup()?, None);
        assert_eq!(batch.find_unlisted_ancestor(FIRST)?, None);
        let active = rollout(
            &home.path().join("sessions"),
            "2026-01-01T00-00-00",
            &format!("_{FIRST}"),
        )?;
        assert_eq!(lookup()?.as_ref(), Some(&active));
        assert_eq!(batch.find_unlisted_ancestor(FIRST)?, None);
        assert_eq!(
            selector
                .listing(home.path(), [THREAD])
                .find_unlisted_ancestor(FIRST)?
                .as_ref(),
            Some(&active)
        );
        assert_eq!(lookup()?.as_ref(), Some(&active));
        assert_eq!(scans.get(), 2);
        let archive = home.path().join("archived_sessions");
        fs::create_dir(&archive)?;
        let moved = archive.join(active.file_name().unwrap());
        fs::rename(&active, &moved)?;
        assert_eq!(lookup()?, Some(moved));
        assert_eq!(scans.get(), 3);
        Ok(())
    }

    #[test]
    fn a_bounded_listing_and_its_index_batch_follow_an_unlisted_legacy_selection() -> Result<()> {
        use crate::{CodexAdapter, ProviderAdapter};
        use omnis_ir::{Provider, SessionRef};

        let home = tempfile::tempdir()?;
        let active = rollout(&home.path().join("sessions"), "2026-01-01T00-00-00", "")?;
        let archive = rollout(
            &home.path().join("archived_sessions"),
            "2026-01-02T00-00-00",
            "",
        )?;
        for (path, label) in [(&active, "active"), (&archive, "archived")] {
            fs::write(
                path,
                serde_json::json!({"type":"session_meta", "payload":{
                    "id": THREAD, "cwd": label
                }})
                .to_string(),
            )?;
        }
        let database = rusqlite::Connection::open(home.path().join("state_5.sqlite"))?;
        database.execute_batch(
            "PRAGMA journal_mode=WAL; CREATE TABLE threads(id TEXT PRIMARY KEY, rollout_path TEXT)",
        )?;
        database.execute(
            "INSERT INTO threads VALUES (?1, ?2)",
            [THREAD, archive.to_str().unwrap()],
        )?;

        let adapter = CodexAdapter::with_root(home.path());
        *adapter.session_scan.lock().unwrap() = Some(std::sync::Arc::new(
            super::super::scan_session_files(home.path(), 1),
        ));
        let candidate = adapter.list_sessions(None)?.pop().unwrap();
        assert_eq!(
            candidate.project_path.as_deref(),
            Some(Path::new("archived"))
        );
        let session = SessionRef::new(Provider::Codex, THREAD);
        let batch = adapter.index_batch(&[THREAD]).unwrap();
        let read = || batch(&session, Some(&active), u64::MAX);
        assert_eq!(
            read()?.snapshot.workspace.current_dir,
            Path::new("archived")
        );
        database.execute(
            "UPDATE threads SET rollout_path = ?1",
            [active.to_str().unwrap()],
        )?;
        assert_eq!(
            read()?.snapshot.workspace.current_dir,
            Path::new("archived")
        );
        let refreshed =
            adapter.index_batch(&[THREAD]).unwrap()(&session, Some(&archive), u64::MAX)?;
        assert_eq!(
            refreshed.snapshot.workspace.current_dir,
            Path::new("active")
        );
        assert_eq!(
            adapter.read_session(&session)?.workspace.current_dir,
            Path::new("active")
        );
        Ok(())
    }

    #[test]
    fn an_immutable_lookup_rechecks_a_cached_path_and_uses_only_matching_headers() -> Result<()> {
        let home = tempfile::tempdir()?;
        let active = rollout(
            &home.path().join("sessions/2026/01/01"),
            "2026-01-01T00-00-00",
            &format!("_{FIRST}"),
        )?;
        let archive = rollout(
            &home.path().join("archived_sessions"),
            "2026-01-01T00-00-00",
            &format!("_{FIRST}"),
        )?;
        let selector = RolloutSelector::default();
        let locate = || selector.find_exact(home.path(), FIRST, IdKind::Rollout);
        assert_eq!(locate()?.as_deref(), Some(active.as_path()));
        fs::write(
            &active,
            serde_json::json!({"type":"session_meta","payload":{"id":SECOND}}).to_string(),
        )?;
        assert_eq!(locate()?.as_deref(), Some(archive.as_path()));
        fs::remove_file(archive)?;
        assert_eq!(locate()?, None);
        assert!(
            selector
                .available_rollouts(home.path(), &[Uuid::try_parse(FIRST)?].into())?
                .is_empty()
        );
        Ok(())
    }

    #[test]
    fn paginated_catalog_selection_survives_revert_candidates_and_missing_current_file()
    -> Result<()> {
        let home = tempfile::tempdir()?;
        let earlier = rollout(&home.path().join("sessions"), "2026-01-01T00-00-00", "")?;
        let later = rollout(
            &home.path().join("sessions"),
            "2026-01-02T00-00-00",
            &format!("_{SECOND}"),
        )?;
        let database = home.path().join("state_5.sqlite");
        let writer = rusqlite::Connection::open(&database)?;
        writer.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE threads(id TEXT PRIMARY KEY, rollout_path TEXT, history_mode TEXT);",
        )?;
        writer.execute(
            "INSERT INTO threads VALUES (?1, ?2, 'paginated')",
            [THREAD, earlier.to_str().unwrap()],
        )?;

        let selector = RolloutSelector::default();
        let catalog = selector.listing(home.path(), [THREAD]);
        let listed =
            || catalog.select_listed([(earlier.as_path(), true), (later.as_path(), true)], THREAD);
        let select = || {
            selector.select(
                home.path(),
                [earlier.as_path(), later.as_path()],
                THREAD,
                IdKind::Thread,
            )
        };
        assert_eq!(select()?, Some(earlier.canonicalize()?));
        assert_eq!(listed()?, Some(earlier.canonicalize()?));
        writer.execute(
            "UPDATE threads SET rollout_path = ?1",
            [later.to_str().unwrap()],
        )?;
        assert_eq!(select()?, Some(later.canonicalize()?));
        // The rest of a listing observes the same SQLite snapshot.
        assert_eq!(listed()?, Some(earlier.canonicalize()?));
        let compressed = later.with_extension("jsonl.zst");
        fs::write(
            &compressed,
            zstd::stream::encode_all(fs::File::open(&later)?, 1)?,
        )?;
        fs::remove_file(later)?;
        assert_eq!(
            selector.select(home.path(), [earlier.as_path()], THREAD, IdKind::Thread)?,
            Some(compressed.canonicalize()?)
        );
        fs::remove_file(compressed)?;
        assert!(
            selector
                .select(home.path(), [earlier.as_path()], THREAD, IdKind::Thread)
                .unwrap_err()
                .to_string()
                .contains("missing or invalid")
        );
        Ok(())
    }
}
