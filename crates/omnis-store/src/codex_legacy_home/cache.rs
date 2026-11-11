use std::{
    collections::HashMap,
    fs::File,
    io::Read,
    path::{Component, Path, PathBuf},
};

use chrono::{DateTime, Utc};
use omnis_ir::Provider;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::IndexedSession;

const MAX_DIRECTORY_ENTRIES: usize = 512;
const MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024;
const OLD_SCAN_LIMIT: usize = 10_000;

#[derive(Deserialize)]
struct Cache {
    format: String,
    codex_home: PathBuf,
    rollouts: HashMap<PathBuf, Rollout>,
}

#[derive(Deserialize)]
struct Rollout {
    stamp: Stamp,
    header: Option<Header>,
}

#[derive(Deserialize)]
struct Header {
    id: String,
    project_path: Option<PathBuf>,
    created_at: Option<DateTime<Utc>>,
    is_subagent: bool,
}

#[derive(Deserialize)]
struct Stamp {
    len: u64,
    modified: (i64, u32),
    changed: (i64, u32),
    inode: u64,
    file_id: Option<Uuid>,
}

#[cfg(unix)]
fn identifies_live_file(stamp: &Stamp, path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Ok(stat) = path.symlink_metadata() else {
        return false;
    };
    if !stat.is_file() || stamp.inode == 0 || stat.ino() != stamp.inode {
        return false;
    }
    if let Some(file) = stamp.file_id {
        let name = Uuid::new_v5(&Uuid::NAMESPACE_URL, path.as_os_str().as_encoded_bytes());
        let object = [stat.dev().to_le_bytes(), stat.ino().to_le_bytes()];
        return file == Uuid::new_v5(&name, object.as_flattened());
    }
    stat.len() == stamp.len
        && (stat.mtime(), stat.mtime_nsec()) == (stamp.modified.0, i64::from(stamp.modified.1))
        && (stat.ctime(), stat.ctime_nsec()) == (stamp.changed.0, i64::from(stamp.changed.1))
        && stat.nlink() == 1
}

#[cfg(not(unix))]
fn identifies_live_file(_: &Stamp, _: &Path) -> bool {
    // These cache stamps do not include an inode for verifying a changed path.
    false
}

impl Stamp {
    fn matches(&self, row: &IndexedSession) -> bool {
        if self.modified.1 >= 1_000_000_000 || self.changed.1 >= 1_000_000_000 {
            return false;
        }
        let Some(modified) = DateTime::from_timestamp(self.modified.0, self.modified.1) else {
            return false;
        };
        let Some(updated) = row.updated_at.filter(|_| !row.updated_at_approximate) else {
            return false;
        };
        let same_time = modified.timestamp_millis() == updated.timestamp_millis();
        let Some(source) = row.source_revision else {
            return same_time;
        };
        let (Some(file), Some(contents)) = (self.file_id, source.contents) else {
            return false;
        };
        if source.file != Some(file)
            || file.get_version_num() != 5
            || contents.get_version_num() != 5
            || cfg!(unix) && self.inode == 0
        {
            return false;
        }
        let fields = [
            self.len.to_le_bytes(),
            self.modified.0.to_le_bytes(),
            u64::from(self.modified.1).to_le_bytes(),
            self.changed.0.to_le_bytes(),
            u64::from(self.changed.1).to_le_bytes(),
        ];
        let same_contents = Uuid::new_v5(&file, fields.as_flattened()) == contents;
        if same_time {
            return same_contents;
        }
        // Unix stamps identify the path and inode across appends. Older Windows stamps
        // did not distinguish file IDs from their creation-time fallback.
        cfg!(unix) && !same_contents
    }
}

fn hex_id(value: &str) -> bool {
    value.len() == 16
        && value
            .bytes()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

fn cache_key(name: &str) -> Option<&str> {
    name.strip_prefix("codex-rollouts-")?
        .strip_suffix(".json")
        .filter(|key| hex_id(key))
}

fn home_digest(home: &Path) -> String {
    hex::encode(&Sha256::digest(home.as_os_str().as_encoded_bytes())[..8])
}

fn schema_v1(format: &str) -> bool {
    let Some((version, parser)) = format.strip_prefix("1-").and_then(|s| s.rsplit_once('-')) else {
        return false;
    };
    // These releases wrote the same header-cache schema.
    let known = matches!(version, "0.9.0" | "0.10.0" | "0.10.1")
        || version
            .strip_prefix("0.8.")
            .is_some_and(|patch| matches!(patch, "52" | "53" | "54" | "55" | "56" | "57" | "58"));
    known && hex_id(parser)
}

fn resolved_local_path(path: &Path) -> Option<PathBuf> {
    if !path.is_absolute() || omnis_core::may_reach_network(path) {
        return None;
    }
    let mut prefix = PathBuf::new();
    let mut components = path.components();
    while let Some(component) = components.next() {
        prefix.push(component.as_os_str());
        if !matches!(component, Component::Normal(_)) {
            continue;
        }
        match prefix.symlink_metadata() {
            Ok(metadata) if metadata.is_symlink() => {
                // Check the target before following it. A dangling link proves no home.
                prefix = omnis_core::resolve_local_path(&prefix)?;
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let parent = prefix.parent()?.canonicalize().ok()?;
                let missing = parent.join(component.as_os_str());
                let suffix = components.as_path();
                return Some(if suffix.as_os_str().is_empty() {
                    missing
                } else {
                    missing.join(suffix)
                });
            }
            Err(_) => return None,
        }
    }
    prefix.canonicalize().ok()
}

fn same_path(left: &Path, right: &Path) -> bool {
    #[cfg(windows)]
    let (left, right) = (dunce::simplified(left), dunce::simplified(right));
    left == right
}

fn session_path(home: &Path, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(home) else {
        return false;
    };
    let mut parts = relative.components();
    let Some(Component::Normal(root)) = parts.next() else {
        return false;
    };
    let in_rollouts = root == "sessions" || root == "archived_sessions";
    let valid_component = |part| matches!(part, Component::Normal(name) if name != "sessions" && name != "archived_sessions");
    if !in_rollouts || !parts.all(valid_component) {
        return false;
    }
    // The header supplies the thread ID. A restored rollout may have any JSONL filename.
    let plain = if path
        .extension()
        .is_some_and(|ext| ext == "gz" || ext == "zst")
    {
        path.file_stem().map(Path::new)
    } else {
        Some(path)
    };
    plain
        .and_then(Path::extension)
        .is_some_and(|ext| ext == "jsonl")
        && !omnis_core::may_reach_network(path)
}

struct HomeEvidence {
    unique_home: Option<String>,
    matches_catalog: bool,
}

impl Cache {
    fn collect(
        &self,
        key: &str,
        rows: &HashMap<&str, &IndexedSession>,
        homes: &mut HashMap<String, HomeEvidence>,
    ) -> Option<()> {
        if !schema_v1(&self.format)
            || !self.codex_home.is_absolute()
            || home_digest(&self.codex_home) != key
            || self.rollouts.len() >= OLD_SCAN_LIMIT
        {
            return None;
        }
        let home = resolved_local_path(&self.codex_home);
        for (path, rollout) in &self.rollouts {
            let Some(header) = &rollout.header else {
                continue;
            };
            let Some(row) = rows.get(header.id.as_str()) else {
                continue;
            };
            if header.is_subagent {
                continue;
            }
            // The workspace migration clears unverified folders. A matching source can
            // still identify its home without restoring that folder.
            let workspace_matches = header.project_path == row.project_path
                || (row.project_path.is_none() && row.source_revision.is_some());
            let matches_catalog = workspace_matches
                && header.created_at.map(|t| t.timestamp_millis())
                    == row.created_at.map(|t| t.timestamp_millis())
                && rollout.stamp.matches(row);
            if !matches_catalog && row.source_revision.is_some() {
                continue;
            }
            let candidate = home
                .as_deref()
                .filter(|home| {
                    if !session_path(&self.codex_home, path) && !session_path(home, path) {
                        return false;
                    }
                    let Some(rollout_path) = resolved_local_path(path) else {
                        return false;
                    };
                    // The saved home may have been a symlink, even if its spelling now names a directory.
                    let expected = path
                        .strip_prefix(&self.codex_home)
                        .map_or_else(|_| path.to_owned(), |suffix| home.join(suffix));
                    same_path(&rollout_path, &expected)
                        && identifies_live_file(&rollout.stamp, &rollout_path)
                })
                .and_then(Path::to_str);
            // An unstamped catalog row may belong to a rollout that has changed since
            // indexing. A matching copy in another home cannot establish ownership.
            let evidence = homes
                .entry(header.id.clone())
                .or_insert_with(|| HomeEvidence {
                    unique_home: candidate.map(str::to_owned),
                    matches_catalog: false,
                });
            if evidence.unique_home.as_deref() != candidate {
                evidence.unique_home = None;
            }
            evidence.matches_catalog |= matches_catalog;
        }
        Some(())
    }
}

pub(super) fn homes<'a>(
    root: &Path,
    rows: impl IntoIterator<Item = &'a IndexedSession>,
) -> HashMap<String, String> {
    let rows = rows
        .into_iter()
        .filter(|row| {
            row.session.provider == Provider::Codex
                && Uuid::try_parse(&row.session.id)
                    .is_ok_and(|id| id.hyphenated().to_string() == row.session.id)
        })
        .map(|row| (row.session.id.as_str(), row))
        .collect::<HashMap<_, _>>();
    let mut homes = HashMap::new();
    let mut remaining = MAX_TOTAL_BYTES;
    if !rows.is_empty()
        && visit_caches(root, |key, file| {
            let metadata = file.metadata().ok()?;
            if !metadata.is_file() || metadata.len() > remaining {
                return None;
            }
            let mut bytes = Vec::new();
            file.take(metadata.len() + 1).read_to_end(&mut bytes).ok()?;
            if u64::try_from(bytes.len()).ok()? != metadata.len() {
                return None;
            }
            remaining -= metadata.len();
            serde_json::from_slice::<Cache>(&bytes)
                .ok()?
                .collect(key, &rows, &mut homes)
        })
        .is_none()
    {
        return HashMap::new();
    }
    homes
        .into_iter()
        .filter(|(_, evidence)| evidence.matches_catalog)
        .filter_map(|(id, evidence)| Some((id, evidence.unique_home?)))
        .collect()
}

#[cfg(unix)]
fn visit_caches(root: &Path, mut visit: impl FnMut(&str, File) -> Option<()>) -> Option<()> {
    use rustix::fs::{Dir, Mode, OFlags, open, openat};
    let flags = OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW;
    let directory = flags | OFlags::DIRECTORY;
    let root = open(root, directory, Mode::empty()).ok()?;
    let cache = openat(&root, "cache", directory, Mode::empty()).ok()?;
    for (index, item) in Dir::read_from(&cache).ok()?.enumerate() {
        if index >= MAX_DIRECTORY_ENTRIES {
            return None;
        }
        let item = item.ok()?;
        if let Some(key) = item.file_name().to_str().ok().and_then(cache_key) {
            let fd = openat(
                &cache,
                item.file_name(),
                flags | OFlags::NONBLOCK,
                Mode::empty(),
            )
            .ok()?;
            visit(key, File::from(fd))?;
        }
    }
    Some(())
}

#[cfg(windows)]
fn visit_caches(root: &Path, mut visit: impl FnMut(&str, File) -> Option<()>) -> Option<()> {
    use std::{
        fs,
        os::windows::fs::{MetadataExt, OpenOptionsExt},
    };
    // Keep both directory handles open without delete sharing while looking up children.
    let open = |path: &Path| {
        let file = fs::OpenOptions::new()
            .read(true)
            .share_mode(3)
            .custom_flags(0x0200_0000 | 0x0020_0000)
            .open(path)
            .ok()?;
        (file.metadata().ok()?.file_attributes() & 0x400 == 0).then_some(file)
    };
    let _root = open(root)?;
    let cache = root.join("cache");
    let _cache = open(&cache)?;
    for (index, item) in fs::read_dir(cache).ok()?.enumerate() {
        if index >= MAX_DIRECTORY_ENTRIES {
            return None;
        }
        let item = item.ok()?;
        if let Some(key) = item.file_name().to_str().and_then(cache_key) {
            visit(key, open(&item.path())?)?;
        }
    }
    Some(())
}

#[cfg(not(any(unix, windows)))]
fn visit_caches(_: &Path, _: impl FnMut(&str, File) -> Option<()>) -> Option<()> {
    None
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use omnis_ir::SessionRef;
    use serde_json::json;
    use std::fs;

    fn unstamped_row(state: &Path) -> IndexedSession {
        IndexedSession {
            session: SessionRef::new(Provider::Codex, "11111111-1111-4111-8111-111111111111"),
            source_revision: None,
            project_path: Some(state.join("project")),
            title: None,
            git_branch: None,
            created_at: None,
            updated_at: DateTime::from_timestamp(100, 0),
            updated_at_approximate: false,
            event_count: 0,
        }
    }

    #[test]
    fn foreign_cache_ambiguity_and_incomplete_inventory_withhold_the_home() {
        use std::os::unix::fs::MetadataExt;
        use std::time::{Duration, UNIX_EPOCH};

        let temporary = tempfile::tempdir().unwrap();
        let state = temporary.path().canonicalize().unwrap();
        let cache = state.join("cache");
        fs::create_dir(&cache).unwrap();
        let row = unstamped_row(&state);
        let write_cache = |home: &Path, modified: u64| {
            let name = cache.join(format!("codex-rollouts-{}.json", home_digest(home)));
            let make_entry = |path: &Path, timestamp| {
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                let file = File::create(path).unwrap();
                file.set_modified(UNIX_EPOCH + Duration::from_secs(timestamp))
                    .unwrap();
                let stat = file.metadata().unwrap();
                json!({"header":{"id": row.session.id, "project_path": row.project_path, "is_subagent": false},
                    "stamp":{"len":stat.len(), "inode":stat.ino(), "modified":[stat.mtime(),stat.mtime_nsec()],
                    "changed":[stat.ctime(),stat.ctime_nsec()]}})
            };
            let rollout = home.join("sessions/restored.jsonl");
            let archive = home.join("archived_sessions/prior.jsonl");
            let data = json!({"format":"1-0.8.52-0123456789abcdef", "codex_home":home,
                "rollouts":{rollout.to_str().unwrap():make_entry(&rollout, modified),
                    archive.to_str().unwrap():make_entry(&archive, 90)}});
            fs::write(&name, data.to_string()).unwrap();
            name
        };
        let first = state.join("A");
        write_cache(&first, 100);
        assert_eq!(
            homes(&state, [&row]).get(&row.session.id),
            first.to_str().map(str::to_owned).as_ref()
        );
        write_cache(&first, 101);
        assert!(homes(&state, [&row]).is_empty());
        let second = write_cache(&state.join("B"), 100);
        assert!(homes(&state, [&row]).is_empty());
        write_cache(&first, 100);
        assert!(homes(&state, [&row]).is_empty());
        fs::write(second, "{").unwrap();
        assert!(homes(&state, [&row]).is_empty());
    }

    #[test]
    fn a_retargeted_home_link_cannot_lend_its_old_header_to_a_copy() {
        use std::os::unix::fs::{MetadataExt, symlink};
        use std::time::{Duration, UNIX_EPOCH};

        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().canonicalize().unwrap();
        let row = unstamped_row(&state);
        let a = state.join("A");
        let b = state.join("B");
        let links = state.join("links");
        fs::create_dir(&links).unwrap();
        let alias = links.join("alias");
        let cache = state.join("cache");
        fs::create_dir(&cache).unwrap();
        let relative = Path::new("sessions").join(format!("rollout-{}.jsonl", row.session.id));
        for home in [&a, &b] {
            fs::create_dir_all(home.join("sessions")).unwrap();
            let file = File::create(home.join(&relative)).unwrap();
            file.set_modified(UNIX_EPOCH + Duration::from_secs(100))
                .unwrap();
        }
        symlink("../A", &alias).unwrap();
        let save = |home: &Path| {
            let path = home.join(&relative);
            let stat = path.metadata().unwrap();
            let stamp = json!({"len":stat.len(),"inode":stat.ino(),"changed":[stat.ctime(),stat.ctime_nsec()],
                "modified":[stat.mtime(),stat.mtime_nsec()]});
            let header =
                json!({"id":row.session.id,"project_path":row.project_path,"is_subagent":false});
            let data = json!({"format":"1-0.10.1-0123456789abcdef","codex_home":home,
                "rollouts":{path.to_str().unwrap():{"stamp":stamp,"header":header}}});
            let name = cache.join(format!("codex-rollouts-{}.json", home_digest(home)));
            fs::write(&name, data.to_string()).unwrap();
            name
        };
        save(&alias);
        assert_eq!(homes(&state, [&row])[&row.session.id], a.to_str().unwrap());
        fs::remove_file(&alias).unwrap();
        fs::create_dir(&alias).unwrap();
        assert!(homes(&state, [&row]).is_empty());
        fs::remove_dir(&alias).unwrap();
        symlink("../A", &alias).unwrap();
        let physical = save(&a);
        assert_eq!(homes(&state, [&row])[&row.session.id], a.to_str().unwrap());
        fs::remove_file(physical).unwrap();
        File::open(a.join(&relative))
            .unwrap()
            .set_modified(UNIX_EPOCH + Duration::from_secs(101))
            .unwrap();
        save(&alias);
        save(&b);
        assert!(homes(&state, [&row]).is_empty());
        fs::remove_file(&alias).unwrap();
        symlink(&b, &alias).unwrap();
        assert!(homes(&state, [&row]).is_empty());
        fs::remove_file(&alias).unwrap();
        symlink(state.join("gone"), &alias).unwrap();
        assert!(homes(&state, [&row]).is_empty());
    }
}

#[cfg(all(test, windows))]
#[test]
fn ordinary_drive_spelling_resolves_to_the_store_home_key() {
    let directory = tempfile::tempdir().unwrap();
    let canonical = directory.path().canonicalize().unwrap();
    let ordinary = dunce::simplified(&canonical);
    assert!(same_path(
        &resolved_local_path(ordinary).unwrap(),
        &canonical
    ));
}
