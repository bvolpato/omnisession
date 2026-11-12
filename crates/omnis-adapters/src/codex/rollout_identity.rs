//! Codex keeps the thread ID when a revert creates another physical rollout.

use std::{
    collections::{HashMap, HashSet},
    fs, io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, PoisonError},
};

use anyhow::{Result, anyhow, bail};
use serde::Deserialize;
use serde_json::Value;
use uuid::Uuid;

use super::{
    CodexFileScan, CodexSession,
    ancestor_paths::AncestorPaths,
    current_launch::{native_representation, same_native_copy},
    current_rollout::{canonical_name, ids, timestamp},
    rollout_file,
};
use crate::support::provider_file;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum IdKind {
    Thread,
    Rollout,
}

pub(super) fn thread_id(path: &Path) -> Option<&str> {
    ids(path).map(|(thread, _)| thread)
}

pub(super) fn rollout_id(path: &Path) -> Option<&str> {
    ids(path).map(|(_, rollout)| rollout)
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
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

pub(super) fn damaged_paginated_mode(header: &Value) -> bool {
    // Codex writes numeric ordinals only in paginated rollouts.
    let mode = &header["payload"]["history_mode"];
    header["ordinal"].is_u64() && HistoryMode::read(mode).is_none()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum HeaderEvidence {
    Native(HistoryMode),
    Incomplete,
    Foreign,
    Other,
}

impl HeaderEvidence {
    /// An incomplete header can rule out a backup. It cannot establish ownership or native readiness.
    pub(super) fn read(header: &Value, thread: &str) -> Self {
        if is_body_record(header) {
            return Self::Other;
        }
        if header["type"] != "session_meta" && !damaged_header_type(header) {
            return Self::Incomplete;
        }
        let payload = &header["payload"];
        let mode = super::CodexHistoryMode::declared(payload);
        let legacy = mode == super::CodexHistoryMode::Legacy
            && payload["history_base"].is_null()
            && !damaged_paginated_mode(header);
        let owner = payload
            .get("id")
            .or_else(|| payload.get("session_id").filter(|_| legacy));
        match owner
            .and_then(Value::as_str)
            .filter(|id| Uuid::try_parse(id).is_ok())
        {
            Some(id) if id == thread => {}
            Some(_) => return Self::Foreign,
            _ => return Self::Incomplete,
        }
        if header["type"] != "session_meta" {
            return Self::Incomplete;
        }
        if mode == super::CodexHistoryMode::Paginated
            && super::history_base::has_valid_start(header)
        {
            Self::Native(HistoryMode::Paginated)
        } else if legacy {
            Self::Native(HistoryMode::Legacy)
        } else {
            Self::Incomplete
        }
    }
}

pub(super) fn damaged_header_type(header: &Value) -> bool {
    header["ordinal"].is_u64()
        && HistoryMode::is_paginated(&header["payload"]["history_mode"])
        && header["type"] != "session_meta"
        && !is_body_record(header)
}

fn is_body_record(header: &Value) -> bool {
    // A body record's declared type takes precedence over extra metadata-like fields.
    matches!(
        header["type"].as_str(),
        Some(
            "dynamic_tools_updated"
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

fn native_filename(path: &Path) -> bool {
    rollout_file::representation_priority(path) > 0
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("rollout-"))
}

#[derive(Debug, Default)]
pub(super) struct RolloutSelector {
    immutable_paths: Mutex<HashMap<String, PathBuf>>,
}

impl RolloutSelector {
    pub(super) fn select<'a>(
        home: &Path,
        paths: impl IntoIterator<Item = &'a Path>,
        id: &str,
        kind: IdKind,
    ) -> Result<Option<PathBuf>> {
        let candidates = Candidates::from_paths(home, paths, id, kind)?;
        Ok(candidates.filesystem_choice(home, kind))
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
        let inventory = visit_rollouts(home, |path| candidates.observe(path, id, kind, false))?;
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
                        .observe(path, rollout, IdKind::Rollout, true)
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
}

pub(super) struct RolloutCatalog<'a> {
    home: &'a Path,
    pub(super) fresh_scan: Option<Arc<CodexFileScan>>,
    ancestors: OnceLock<Result<AncestorPaths, String>>,
}

impl<'a> RolloutCatalog<'a> {
    pub(super) fn new(home: &'a Path) -> Self {
        Self {
            home,
            fresh_scan: None,
            ancestors: OnceLock::new(),
        }
    }

    pub(super) fn inventory_revision(&self, id: Uuid) -> Uuid {
        let mut copies = Vec::new();
        let rollout = id.to_string();
        let inventory = self.visit_physical(&rollout, |path| {
            if rollout_id(path) != Some(rollout.as_str()) {
                return Ok(());
            }
            let revision = super::CodexAdapter::index_file_revision(path)
                .and_then(|r| r.contents)
                .ok_or_else(|| anyhow!("unavailable Codex ancestor revision"))?;
            // The contents revision includes the canonical path, physical identity and modification stamp.
            copies.push(revision);
            Ok(())
        });
        let Ok(inventory) = inventory else {
            return Uuid::nil();
        };
        copies.sort_unstable();
        let mut bytes: Vec<_> = copies.into_iter().flat_map(Uuid::into_bytes).collect();
        if !inventory.complete() {
            bytes.extend([
                u8::from(inventory.limited),
                u8::from(inventory.inaccessible),
            ]);
        }
        Uuid::new_v5(&id, &bytes)
    }

    /// Check exact names once per indexing pass. A later batch can see restored rollouts.
    pub(super) fn find_unlisted_ancestor(&self, id: &str) -> Result<Option<PathBuf>> {
        let mut candidates = Candidates::new(self.home);
        let inventory = self.visit_physical(id, |path| {
            candidates.observe(path, id, IdKind::Rollout, false)
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
            for path in scan.ancestor_paths(self.home, id) {
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
        && fs::canonicalize(path).is_ok_and(|actual| {
            omnis_core::codex_rollout_path_in_home(&root, relative).as_deref() == Some(&actual)
        })
}

/// A state pointer or a stale discovery hint can name an obsolete path outside both rollout roots.
pub(super) fn managed_path(home: &Path, path: &Path) -> Option<PathBuf> {
    let path = provider_file(home, path)?;
    let root = home.canonicalize().ok()?;
    ["sessions", "archived_sessions"]
        .iter()
        .filter_map(|name| omnis_core::codex_rollout_path_in_home(&root, Path::new(name)))
        .any(|directory| path.starts_with(directory))
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

fn matching_header(path: &Path, id: &str) -> bool {
    matches!(CodexSession::parse_observed_metadata(path), Ok(Some(session)) if session.id == id)
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
        return Ok(Some(false));
    };
    let Some(thread) = thread_id(path) else {
        return Ok(None);
    };
    Ok(match HeaderEvidence::read(header, thread) {
        HeaderEvidence::Native(HistoryMode::Paginated) => Some(true),
        HeaderEvidence::Incomplete => Some(false),
        HeaderEvidence::Native(HistoryMode::Legacy)
        | HeaderEvidence::Foreign
        | HeaderEvidence::Other => None,
    })
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

fn compatible_copy(home: &Path, recorded: &Path, id: &str) -> Option<PathBuf> {
    if thread_id(recorded)? != id {
        return None;
    }
    matching_sibling(home, recorded, |path| matching_header(path, id))
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
        IdKind::Thread => compatible_copy(home, &plain, thread_id(archived)?),
        IdKind::Rollout => immutable_sibling(home, &plain),
    }
}

pub(super) fn provider_can_read_ancestors<'a>(
    home: &Path,
    scan: &CodexFileScan,
    sources: impl Iterator<Item = &'a Path>,
) -> Result<bool> {
    let root = fs::canonicalize(home)?;
    let mut selected = HashMap::new();
    for path in sources {
        // Codex resolves an immutable reference from canonical filenames. It ignores gzip.
        if !native_filename(path) || !canonical_name(path) {
            return Ok(false);
        }
        let Some(native) = native_representation(home, &root, path) else {
            return Ok(false);
        };
        let archived = is_archived(home, path);
        if !same_native_copy(&native, path)?
            || (archived
                && restored_basename(home, path)
                    .and_then(|name| native_representation(home, &root, &name))
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
            && canonical_name(candidate)
            && (*archived || !is_archived(home, candidate))
        {
            // The first active match wins, followed by the first archive. Directory entry order
            // cannot distinguish two different files in the same root.
            readable = match native_representation(home, &root, candidate) {
                Some(native) => same_native_copy(&native, source)?,
                None => false,
            };
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
    read_error: Option<anyhow::Error>,
}

impl Candidates {
    fn new(home: &Path) -> Self {
        Self {
            active: home.join("sessions"),
            best: None,
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
            candidates.observe(path, id, kind, false)?;
        }
        Ok(candidates)
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

    fn observe(&mut self, path: &Path, id: &str, kind: IdKind, header_checked: bool) -> Result<()> {
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
            if !other_owner
                && rollout_priority(path, &self.active) <= rollout_priority(best, &self.active)
            {
                return Ok(());
            }
        }
        if !header_checked {
            let accepted = match kind {
                IdKind::Thread => CodexSession::parse_observed_metadata(path)
                    .map(|session| session.is_some_and(|session| session.id == thread)),
                IdKind::Rollout => immutable_mode(path).map(|mode| mode.is_some()),
            };
            match accepted {
                Ok(true) => {}
                Ok(false) => return Ok(()),
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
}

pub(super) fn rollout_priority<'a>(
    path: &'a Path,
    active: &Path,
) -> (bool, bool, Option<&'a str>, Option<&'a str>, u8, &'a Path) {
    let representation = rollout_file::representation_priority(path);
    (
        path.starts_with(active),
        // Codex cannot open external gzip copies. A revert still follows its rollout timestamp.
        representation > 0 || thread_id(path) != rollout_id(path),
        timestamp(path),
        rollout_id(path),
        representation,
        path,
    )
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
    use anyhow::Context;

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
                "ordinal": 0,
                "payload": { "id": THREAD, "history_mode": "paginated" }
            })
            .to_string(),
        )?;
        Ok(path)
    }

    #[test]
    fn native_fallback_checks_all_files_with_the_selected_native_rank() -> Result<()> {
        let name = format!("rollout-2026-01-01T00-00-00-{THREAD}_{FIRST}.jsonl");
        let home = tempfile::tempdir()?;
        let first = home.path().join("sessions/2026/01/01");
        let second = home.path().join("sessions/2026/01/02");
        fs::create_dir_all(&first)?;
        fs::create_dir_all(&second)?;
        let plain = first.join(&name);
        let compressed = second.join(&name).with_extension("jsonl.zst");
        let header = serde_json::json!({"type":"session_meta","payload":{"id":THREAD,"history_mode":"paginated"}});
        fs::write(&plain, header.to_string())?;
        let select = || {
            super::super::current_launch::filesystem_selects(
                home.path(),
                THREAD,
                &plain,
                None,
                true,
            )
        };
        assert!(select()?);
        let frame = zstd::stream::encode_all(fs::File::open(&plain)?, 1)?;
        fs::write(plain.with_extension("jsonl.zst"), &frame)?;
        assert!(select()?);
        fs::write(&compressed, frame)?;
        assert!(!select()?);
        let alias = second.join(&name);
        fs::hard_link(&plain, &alias)?;
        assert!(select()?);
        fs::remove_file(&alias)?;
        let db = rusqlite::Connection::open(home.path().join("state_5.sqlite"))?;
        db.execute_batch(
            "CREATE TABLE threads(id TEXT PRIMARY KEY, rollout_path TEXT, history_mode TEXT)",
        )?;
        db.execute(
            "INSERT INTO threads VALUES(?1, ?2, 'paginated')",
            (THREAD, compressed.to_str()),
        )?;
        drop(db);
        let selected = super::super::current_rollout::CurrentRollouts::default()
            .rows(home.path(), [THREAD])?[THREAD]
            .resolve(home.path(), THREAD, [&*plain, &*compressed], true)?
            .context("selected rollout")?;
        assert!(selected.native);
        assert!(same_file::is_same_file(selected.path, compressed)?);
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
        let batch = RolloutCatalog::new(home.path());
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
        let next_batch = RolloutCatalog::new(home.path());
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
        for limit in [0, 1] {
            let batch = RolloutCatalog::new(home.path());
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
        let batch = RolloutCatalog::new(home.path());
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
            RolloutCatalog::new(home.path())
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
        let active_project = home.path().join("active");
        let archived_project = home.path().join("archived");
        for (path, project) in [(&active, &active_project), (&archive, &archived_project)] {
            fs::create_dir(project)?;
            fs::write(
                path,
                serde_json::json!({"type":"session_meta", "payload":{
                    "id": THREAD, "cwd": project
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
            Some(archived_project.as_path())
        );
        let session = SessionRef::new(Provider::Codex, THREAD);
        let batch = adapter.index_batch(&[THREAD]).unwrap();
        let read = || batch(&session, Some(&active), u64::MAX);
        assert_eq!(
            read()?.snapshot.workspace.current_dir,
            archived_project.as_path()
        );
        database.execute(
            "UPDATE threads SET rollout_path = ?1",
            [active.to_str().unwrap()],
        )?;
        assert_eq!(
            read()?.snapshot.workspace.current_dir,
            active_project.as_path()
        );
        let refreshed =
            adapter.index_batch(&[THREAD]).unwrap()(&session, Some(&archive), u64::MAX)?;
        assert_eq!(
            refreshed.snapshot.workspace.current_dir,
            active_project.as_path()
        );
        assert_eq!(
            adapter.read_session(&session)?.workspace.current_dir,
            active_project.as_path()
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

        let selector = super::super::current_rollout::CurrentRollouts::default();
        let snapshot = selector.rows(home.path(), [THREAD])?;
        let paths = [earlier.as_path(), later.as_path()];
        let listed = || {
            snapshot[THREAD]
                .resolve(home.path(), THREAD, paths, true)?
                .context("missing listed rollout")
        };
        let select = || {
            selector.rows(home.path(), [THREAD])?[THREAD]
                .resolve(home.path(), THREAD, paths, true)?
                .context("missing selected rollout")
        };
        assert!(same_file::is_same_file(select()?.path, &earlier)?);
        assert!(same_file::is_same_file(listed()?.path, &earlier)?);
        writer.execute(
            "UPDATE threads SET rollout_path = ?1",
            [later.to_str().unwrap()],
        )?;
        assert!(same_file::is_same_file(select()?.path, &later)?);
        // The rest of a listing observes the same SQLite snapshot.
        assert!(same_file::is_same_file(listed()?.path, &earlier)?);
        let compressed = later.with_extension("jsonl.zst");
        fs::write(
            &compressed,
            zstd::stream::encode_all(fs::File::open(&later)?, 1)?,
        )?;
        fs::remove_file(&later)?;
        assert!(same_file::is_same_file(select()?.path, &compressed)?);
        fs::remove_file(compressed)?;
        assert!(
            select()
                .unwrap_err()
                .is::<super::super::current_rollout::CurrentRolloutUnavailable>()
        );
        Ok(())
    }
}
