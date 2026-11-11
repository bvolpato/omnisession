//! Codex keeps the thread ID when a revert creates another physical rollout.

use std::{
    cmp::Ordering,
    collections::{HashMap, HashSet},
    env, fs, io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, PoisonError},
};

use anyhow::{Context, Result, anyhow, bail};
use rusqlite::OptionalExtension;
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
    sqlite_home: Option<PathBuf>,
    snapshot: SqliteSnapshotCache,
    immutable_paths: Mutex<HashMap<String, PathBuf>>,
}

impl RolloutSelector {
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
        let candidates = Candidates::from_paths(home, paths, id, kind, false)?;
        if kind == IdKind::Rollout {
            return Ok(candidates.filesystem_choice(home));
        }
        let rows = self.current_rows(home, std::iter::once(id))?;
        candidates.resolve(home, id, rows.get(id))
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
        let candidates =
            Candidates::from_paths(home, scan.paths(home, id), id, IdKind::Thread, false)?;
        let mut path = candidates.resolve(home, id, current)?;
        if path.is_none() {
            path = self.find_exact(home, id, IdKind::Thread)?;
        }
        let Some(path) = path else {
            return Ok(None);
        };
        let complete =
            (fresh && scan.notes.is_empty() && !scan.indices(id).is_empty()).then_some(scan);
        let native = Self::native_selects(home, complete, id, &path, current)?;
        Ok(Some((path, native)))
    }

    fn native_selects(
        home: &Path,
        fresh_scan: Option<&CodexFileScan>,
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
        if !same_file::is_same_file(&native, selected)? {
            return Ok(false);
        }
        if let Some(row) = current {
            if row.path.extension().is_some_and(|suffix| suffix == "gz") {
                return Ok(false);
            }
            if let Some(recorded) = native_representation(home, &row.path) {
                return Ok(same_file::is_same_file(recorded, selected)?);
            }
            if row.paginated {
                return Ok(false);
            }
        }
        if !native_filename(selected) {
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
            native_representation(home, path).is_some_and(|candidate| {
                rollout_file::compressed(&candidate) == rollout_file::compressed(selected)
                    && same_file::is_same_file(candidate, selected).unwrap_or(false)
            })
        };
        let active = home.join("sessions");
        if let Some(scan) = fresh_scan {
            return Ok(scan
                .files
                .iter()
                .filter(|path| path.starts_with(&active))
                .all(|path| agrees(path)));
        }
        // Old or bounded discovery can miss a second copy. An unreadable directory
        // also prevents us from confirming which file the provider will open.
        Ok(active_files_agree(active, agrees).unwrap_or(false))
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
            && let Some(thread) = thread_id(path)
            && let Some(valid) = recorded_path(home, path, thread)
        {
            let valid = if valid.file_name() == path.file_name() {
                path.clone()
            } else {
                valid
            };
            return Ok(Some(restored_rollout(home, &valid).unwrap_or(valid)));
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
        visit_rollouts(home, |path| candidates.observe(path, id, kind, false))?;
        candidates.exact_choice(home)
    }

    pub(super) fn available_rollouts(
        &self,
        home: &Path,
        requested: &HashSet<Uuid>,
    ) -> Result<HashSet<Uuid>> {
        let mut matches = HashMap::<Uuid, Option<Candidates>>::new();
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
                    .or_insert_with(|| Some(Candidates::new(home)));
                if let Some(candidates) = entry
                    && candidates
                        .observe(path, rollout, IdKind::Rollout, false)
                        .is_err()
                {
                    // This immutable ID occurs under two different threads.
                    *entry = None;
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
            if let Some(path) = candidates.and_then(|candidates| candidates.best) {
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

    fn database<'a>(&'a self, home: &'a Path) -> Option<(&'a Path, PathBuf)> {
        let root = self.sqlite_home.as_deref().unwrap_or(home);
        provider_file(root, &root.join("state_5.sqlite")).map(|path| (root, path))
    }

    fn current_rows<'a>(
        &self,
        home: &Path,
        ids: impl IntoIterator<Item = &'a str>,
    ) -> Result<HashMap<String, SelectedRollout>> {
        let Some((root, database)) = self.database(home) else {
            return Ok(HashMap::new());
        };
        self.snapshot
            .read(root, &database, |connection| {
                // Older state databases predate paginated history.
                let has_history_mode: bool = connection.query_row(
                    "SELECT EXISTS(SELECT 1 FROM pragma_table_info('threads')
                                   WHERE name = 'history_mode')",
                    [],
                    |row| row.get(0),
                )?;
                let query = if has_history_mode {
                    "SELECT rollout_path, history_mode = 'paginated' FROM threads WHERE id = ?1"
                } else {
                    "SELECT rollout_path, 0 FROM threads WHERE id = ?1"
                };
                let mut statement = connection.prepare(query)?;
                let mut selected = HashMap::new();
                for id in ids {
                    let pointer = statement
                        .query_row([id], |row| {
                            Ok(SelectedRollout {
                                path: PathBuf::from(row.get::<_, String>(0)?),
                                paginated: row.get(1)?,
                            })
                        })
                        .optional()?;
                    if let Some(pointer) = pointer {
                        selected.insert(id.to_owned(), pointer);
                    }
                }
                Ok(selected)
            })
            .context("reading the current Codex rollouts from the state database")
    }
}

struct SelectedRollout {
    path: PathBuf,
    paginated: bool,
}

pub(super) struct RolloutCatalog<'a> {
    selector: &'a RolloutSelector,
    home: &'a Path,
    ids: HashSet<String>,
    // A listing consults one snapshot for all ambiguous or paginated threads, including on Windows.
    rows: OnceLock<Result<HashMap<String, SelectedRollout>, String>>,
    pub(super) fresh_scan: Option<Arc<CodexFileScan>>,
    ancestors: OnceLock<Result<AncestorPaths, String>>,
}

impl RolloutCatalog<'_> {
    /// Check exact names once per indexing pass. A later batch can see restored rollouts.
    pub(super) fn find_unlisted_ancestor(&self, id: &str) -> Result<Option<PathBuf>> {
        let mut candidates = Candidates::new(self.home);
        if let Some(scan) = &self.fresh_scan {
            for path in scan.paths(self.home, id) {
                candidates.observe(path, id, IdKind::Rollout, false)?;
            }
        } else {
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
                    candidates.observe(path, id, IdKind::Rollout, false)?;
                }
                Ok(())
            })?;
        }
        candidates.exact_choice(self.home)
    }

    pub(super) fn select_listed<'a>(
        &self,
        paths: impl IntoIterator<Item = &'a Path>,
        id: &str,
        check_state: bool,
    ) -> Result<Option<PathBuf>> {
        self.select(paths, id, check_state, true)
    }

    pub(super) fn select_for_index<'a>(
        &self,
        paths: impl IntoIterator<Item = &'a Path>,
        id: &str,
    ) -> Result<Option<PathBuf>> {
        self.select(paths, id, true, false)
    }

    fn select<'a>(
        &self,
        paths: impl IntoIterator<Item = &'a Path>,
        id: &str,
        check_state: bool,
        validated: bool,
    ) -> Result<Option<PathBuf>> {
        if !self.ids.contains(id) {
            return self.selector.select(self.home, paths, id, IdKind::Thread);
        }
        let candidates = Candidates::from_paths(self.home, paths, id, IdKind::Thread, validated)?;
        if !check_state && !candidates.check_database {
            return Ok(candidates.filesystem_choice(self.home));
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
            && best.parent() == current.path.parent()
            && rollout_file::jsonl_file_name(best).and_then(Path::file_name)
                == rollout_file::jsonl_file_name(&current.path).and_then(Path::file_name)
        {
            // Both discovery and index reads have validated this candidate's header.
            return Ok(candidates.best);
        }
        candidates.resolve(self.home, id, current)
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
    tail.components().count() <= 6
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

fn recorded_path(home: &Path, recorded: &Path, id: &str) -> Option<PathBuf> {
    let plain = recorded.with_file_name(rollout_file::jsonl_file_name(recorded)?.file_name()?);
    if thread_id(&plain)? != id {
        return None;
    }
    for path in [
        plain.clone(),
        plain.with_extension("jsonl.zst"),
        plain.with_extension("jsonl.gz"),
    ] {
        let Some(path) = managed_path(home, &path) else {
            continue;
        };
        if CodexSession::parse_metadata_path_result(&path)
            .ok()
            .flatten()
            .is_some_and(|session| session.id == id)
        {
            return Some(path);
        }
    }
    None
}

fn native_representation(home: &Path, path: &Path) -> Option<PathBuf> {
    let plain = path.with_file_name(rollout_file::jsonl_file_name(path)?);
    managed_path(home, &plain).or_else(|| managed_path(home, &plain.with_extension("jsonl.zst")))
}

// Codex moves every archived rollout into one folder and preserves its full basename.
fn archived_selection(home: &Path, recorded: &Path, id: &str) -> Option<PathBuf> {
    let relative = recorded
        .strip_prefix(home)
        .ok()
        .or_else(|| recorded.strip_prefix(home.canonicalize().ok()?).ok())?;
    if !relative.starts_with("sessions")
        || relative
            .components()
            .any(|part| part == std::path::Component::ParentDir)
    {
        return None;
    }
    let archived = home
        .join("archived_sessions")
        .join(rollout_file::jsonl_file_name(recorded)?);
    recorded_path(home, &archived, id)
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

fn restored_rollout(home: &Path, archived: &Path) -> Option<PathBuf> {
    if !is_archived(home, archived) {
        return None;
    }
    let plain = restored_basename(home, archived)?;
    ["jsonl", "jsonl.zst", "jsonl.gz"]
        .into_iter()
        .map(|ext| plain.with_extension(ext))
        .find(|path| {
            provider_file(home, path).as_ref() == Some(path)
                && matches!(CodexSession::parse_metadata_path_result(path), Ok(Some(_)))
        })
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
        if !same_file::is_same_file(native, path)?
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
            readable = same_file::is_same_file(native, source)?;
        }
        Ok(())
    };
    if scan.notes.is_empty() {
        for id in selected.keys() {
            for index in scan.indices(id) {
                check(&scan.files[*index])?;
            }
        }
    } else if !selected.is_empty() {
        visit_rollouts(home, check)?;
    }
    Ok(readable)
}

struct Candidates {
    active: PathBuf,
    best: Option<PathBuf>,
    check_database: bool,
    read_error: Option<anyhow::Error>,
}

impl Candidates {
    fn new(home: &Path) -> Self {
        Self {
            active: home.join("sessions"),
            best: None,
            check_database: false,
            read_error: None,
        }
    }

    fn from_paths<'a>(
        home: &Path,
        paths: impl IntoIterator<Item = &'a Path>,
        id: &str,
        kind: IdKind,
        validated: bool,
    ) -> Result<Self> {
        let mut candidates = Self::new(home);
        for path in paths {
            candidates.observe(path, id, kind, validated)?;
        }
        Ok(candidates)
    }

    fn resolve(
        self,
        home: &Path,
        id: &str,
        current: Option<&SelectedRollout>,
    ) -> Result<Option<PathBuf>> {
        if let Some(current) = current {
            if let Some(path) = recorded_path(home, &current.path, id) {
                return Ok(Some(path));
            }
            if current.paginated {
                if let Some(path) = archived_selection(home, &current.path, id) {
                    return Ok(Some(path));
                }
                bail!(
                    "the selected rollout for paginated Codex thread `{id}` is missing or invalid"
                );
            }
        }
        Ok(self.filesystem_choice(home))
    }

    fn exact_choice(self, home: &Path) -> Result<Option<PathBuf>> {
        if self.best.is_none()
            && let Some(error) = self.read_error
        {
            return Err(error);
        }
        Ok(self.filesystem_choice(home))
    }

    fn filesystem_choice(self, home: &Path) -> Option<PathBuf> {
        self.best
            .map(|best| restored_rollout(home, &best).unwrap_or(best))
    }

    fn observe(&mut self, path: &Path, id: &str, kind: IdKind, validated: bool) -> Result<()> {
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
        self.check_database |= thread != rollout;
        let mut other_owner = false;
        if let Some(best) = &self.best {
            other_owner = kind == IdKind::Rollout && thread_id(best) != Some(thread);
            self.check_database |=
                best.parent() != path.parent() || rollout_id(best) != Some(rollout);
            if !other_owner && self.compare(path, best) != Ordering::Greater {
                return Ok(());
            }
        }
        if !validated {
            match CodexSession::parse_metadata_path_result(path) {
                Ok(Some(_)) => {}
                Ok(None) => return Ok(()),
                Err(error) => {
                    self.read_error.get_or_insert(error);
                    return Ok(());
                }
            }
        }
        if other_owner {
            bail!("Codex rollout ID `{id}` belongs to more than one thread");
        }
        self.best = Some(path.to_path_buf());
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

fn active_files_agree(root: PathBuf, agrees: impl Fn(&Path) -> bool) -> io::Result<bool> {
    let mut directories = vec![root];
    while let Some(directory) = directories.pop() {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_dir() {
                directories.push(entry.path());
            } else if kind.is_file() && !agrees(&entry.path()) {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

pub(super) fn visit_rollouts(
    home: &Path,
    mut visit: impl FnMut(&Path) -> Result<()>,
) -> Result<()> {
    #[cfg(test)]
    tests::FULL_WALKS.with(|walks| walks.set(walks.get() + 1));
    for directory in ["sessions", "archived_sessions"] {
        visit_files(&home.join(directory), 5, &mut visit)?;
    }
    Ok(())
}

fn visit_files(
    root: &Path,
    depth: usize,
    visit: &mut impl FnMut(&Path) -> Result<()>,
) -> Result<()> {
    match fs::symlink_metadata(root) {
        Ok(metadata) if !metadata.is_dir() => return Ok(()),
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    }
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_file() {
            visit(&entry.path())?;
        } else if kind.is_dir() && depth > 0 {
            visit_files(&entry.path(), depth - 1, visit)?;
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
        let listed = || catalog.select_listed([earlier.as_path(), later.as_path()], THREAD, true);
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
