use std::{
    fs, io,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use omnis_ir::{Provider, SessionRef, SourceRevision};
use uuid::Uuid;

use super::{
    CodexAdapter, SCAN_LIMIT, collect_jsonl, current_rollout, live_rollout_path, rollout_file,
    validate_provider,
};
use crate::{CodexLaunchOverrides, support::SqliteSourceStamp};

pub(super) fn launch_selects_session(
    adapter: &CodexAdapter,
    session: &SessionRef,
    cwd: &Path,
    overrides: &CodexLaunchOverrides,
) -> Result<bool> {
    validate_provider(session, Provider::Codex)?;
    let id = session.id.as_str();
    let home = adapter
        .codex_home
        .as_deref()
        .context("Codex home was not found")?;
    let origin = adapter.current.root(home)?;
    let destination = adapter.current.root_for_launch(home, cwd, overrides)?;
    let selected = adapter.require_rollout(id, None)?;
    if selected.shadowed {
        return Ok(false);
    }
    adapter.read_selected(selected.session, |source, revision| {
        let database = destination.join("state_5.sqlite");
        let stamp = SqliteSourceStamp::read(&destination, &database);
        let agrees =
            || destination_selects(adapter, home, &destination, id, &source.path, revision);
        if !agrees()?
            || adapter.current.root(home)? != origin
            || adapter.current.root_for_launch(home, cwd, overrides)? != destination
        {
            return Ok(false);
        }
        match stamp {
            Some(expected) => {
                Ok(SqliteSourceStamp::read(&destination, &database) == Some(expected))
            }
            None => agrees(),
        }
    })
}

fn destination_selects(
    adapter: &CodexAdapter,
    home: &Path,
    destination: &Path,
    id: &str,
    source: &Path,
    revision: SourceRevision,
) -> Result<bool> {
    if !source.starts_with(home.join("sessions")) || !native_codec(source) {
        return Ok(false);
    }
    let rows = adapter.current.rows_in(destination, [id])?;
    if let Some(row) = rows.get(id).filter(|row| !row.archived_for_launch()) {
        // A native database selection can be verified directly, without listing other threads.
        match row.resolve(home, id, std::iter::empty(), false) {
            Ok(Some(file)) => {
                return Ok(file.native
                    && same_native_copy(&file.path, source)?
                    && CodexAdapter::index_file_revision(source) == Some(revision));
            }
            Ok(None) if row.missing_ordinary_native_path(home, id)? => {}
            Ok(None) => return Ok(false),
            Err(error) if CodexAdapter::is_current_rollout_unavailable(&error) => return Ok(false),
            Err(error) => return Err(error),
        }
    }
    filesystem_selects(home, id, source)
}

fn same_native_copy(candidate: &Path, selected: &Path) -> io::Result<bool> {
    Ok(candidate.extension() == selected.extension()
        && (candidate == selected || same_file::is_same_file(candidate, selected)?))
}

fn native_codec(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext == "jsonl" || ext == "zst")
}

fn native_representation(home: &Path, canonical_home: &Path, path: &Path) -> Option<PathBuf> {
    let plain = path.with_file_name(rollout_file::jsonl_file_name(path)?);
    for candidate in [plain.clone(), plain.with_extension("jsonl.zst")] {
        match fs::symlink_metadata(&candidate) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Ok(metadata)
                if metadata.is_file() && live_rollout_path(home, canonical_home, &candidate) =>
            {
                return Some(candidate);
            }
            _ => return None,
        }
    }
    None
}

fn native_rank(path: &Path) -> Option<(&str, Uuid)> {
    if !native_codec(path) || !current_rollout::canonical_name(path) {
        return None;
    }
    let timestamp = rollout_file::jsonl_file_name(path)?
        .to_str()?
        .strip_prefix("rollout-")?
        .get(..19)?;
    Some((
        timestamp,
        Uuid::try_parse(current_rollout::ids(path)?.1).ok()?,
    ))
}

fn filesystem_selects(home: &Path, id: &str, source: &Path) -> Result<bool> {
    let Some(rank) = native_rank(source) else {
        return Ok(false);
    };
    let canonical_home = fs::canonicalize(home)?;
    if !native_representation(home, &canonical_home, source)
        .as_deref()
        .is_some_and(|native| same_native_copy(native, source).unwrap_or(false))
    {
        return Ok(false);
    }
    let mut files = Vec::new();
    let mut unreadable = 0;
    let mut depth_limited = 0;
    collect_jsonl(
        &home.join("sessions"),
        5,
        &mut files,
        SCAN_LIMIT + 1,
        &mut unreadable,
        &mut depth_limited,
    );
    if unreadable != 0 || depth_limited != 0 || files.len() > SCAN_LIMIT {
        return Ok(false);
    }
    let mut found = false;
    for path in files {
        if !current_rollout::ids(&path).is_some_and(|(thread, _)| thread.eq_ignore_ascii_case(id))
            || native_rank(&path).is_none_or(|other| other < rank)
        {
            continue;
        }
        let Some(candidate) = native_representation(home, &canonical_home, &path) else {
            return Ok(false);
        };
        if !same_native_copy(&candidate, source)? {
            return Ok(false);
        }
        found = true;
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_launch_uses_the_destination_row_and_skips_an_archived_pointer() -> Result<()> {
        const ID: &str = "10000000-0000-4000-a000-000000000001";
        let temp = tempfile::tempdir()?;
        let home = temp.path().canonicalize()?;
        let active = home.join("sessions/2026/01/01");
        fs::create_dir_all(&active)?;
        let older = active.join(format!("rollout-2026-01-01T00-00-00-{ID}.jsonl"));
        let newer = active.join(format!("rollout-2026-01-01T01-00-00-{ID}.jsonl"));
        let header = serde_json::json!({"type": "session_meta", "payload": {"id": ID, "history_mode": "legacy"}});
        for path in [&older, &newer] {
            fs::write(path, format!("{header}\n"))?;
        }
        let set_row = |directory: &Path, selected: &Path, archived: bool| -> Result<()> {
            fs::create_dir_all(directory)?;
            let db = rusqlite::Connection::open(directory.join("state_5.sqlite"))?;
            db.execute_batch("CREATE TABLE IF NOT EXISTS threads(id TEXT PRIMARY KEY, rollout_path TEXT, history_mode TEXT, archived INTEGER)")?;
            db.execute(
                "INSERT OR REPLACE INTO threads VALUES(?1, ?2, 'legacy', ?3)",
                rusqlite::params![ID, selected.to_str().unwrap(), archived],
            )?;
            Ok(())
        };
        set_row(&home, &older, false)?;
        let adapter = CodexAdapter::with_root(&home);
        let source = SessionRef::new(Provider::Codex, ID);
        let selects = |overrides: &CodexLaunchOverrides| {
            launch_selects_session(&adapter, &source, &home, overrides)
        };
        assert!(selects(&CodexLaunchOverrides::default())?);
        let other = home.join("target-state");
        let launch = CodexLaunchOverrides {
            sqlite_home: Some(other.clone()),
            ..Default::default()
        };
        assert!(
            !selects(&launch)?,
            "the default filename lookup opens the newer rollout"
        );
        set_row(&other, &older, false)?;
        assert!(selects(&launch)?);
        set_row(&other, &newer, false)?;
        assert!(!selects(&launch)?);
        set_row(&home, &older, true)?;
        assert!(
            !selects(&CodexLaunchOverrides::default())?,
            "Codex skips the archived row even when both database roots agree"
        );
        set_row(&other, &older, true)?;
        assert!(!selects(&launch)?);
        set_row(&home, &newer, false)?;
        assert!(
            selects(&launch)?,
            "the archived pointer leaves the newer native filename eligible"
        );
        let missing = active.join(format!("rollout-2025-12-01T00-00-00-{ID}.jsonl"));
        set_row(&other, &missing, false)?;
        assert!(
            selects(&launch)?,
            "an absent local legacy pointer falls back to the current native filename"
        );
        let other_home = tempfile::tempdir()?;
        let external = other_home
            .path()
            .canonicalize()?
            .join(missing.file_name().unwrap());
        set_row(&other, &external, false)?;
        assert!(
            !selects(&launch)?,
            "a foreign database pointer cannot authorize a local fallback"
        );
        Ok(())
    }
}
