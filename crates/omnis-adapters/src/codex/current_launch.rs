use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result};
use omnis_ir::{Provider, SessionRef, SourceRevision};

use super::{
    CodexAdapter, CodexFileScan, CodexHistoryMode, SCAN_LIMIT, collect_jsonl, current_rollout,
    live_rollout_path, rollout_file, validate_provider,
};
use crate::{CodexLaunchOverrides, support::SqliteSourceStamp};

pub(super) fn verified_sqlite_home(
    adapter: &CodexAdapter,
    session: &SessionRef,
    cwd: &Path,
    overrides: &CodexLaunchOverrides,
) -> Result<Option<PathBuf>> {
    validate_provider(session, Provider::Codex)?;
    let id = session.id.as_str();
    let home = adapter
        .codex_home
        .as_deref()
        .context("Codex home was not found")?;
    let origin = adapter.current.root(home)?;
    let destination = adapter.current.root_for_launch(home, cwd, overrides)?;
    let had_scan = cached_scan(adapter).is_some();
    let selected = adapter.require_rollout(id, None)?;
    if selected.shadowed {
        return Ok(None);
    }
    // A first lookup can already have walked the full tree during this launch check.
    let fresh = (!had_scan)
        .then(|| cached_scan(adapter))
        .flatten()
        .filter(|scan| scan.active_entries_complete);
    let verified = adapter.read_selected(selected.session, |source, revision| {
        if !source.native_header_is_usable()? {
            return Ok(false);
        }
        let database = destination.join("state_5.sqlite");
        let stamp = SqliteSourceStamp::read(&destination, &database);
        let lookup = || database_selects(adapter, home, &destination, id, &source.path, revision);
        let selected = lookup()?;
        let agrees = selected.map_or_else(
            || {
                filesystem_selects(
                    home,
                    id,
                    &source.path,
                    fresh.as_deref(),
                    source.history_mode == CodexHistoryMode::Paginated,
                )
            },
            Ok,
        )?;
        if !agrees
            || adapter.current.root(home)? != origin
            || adapter.current.root_for_launch(home, cwd, overrides)? != destination
        {
            return Ok(false);
        }
        Ok(match stamp {
            Some(expected) => SqliteSourceStamp::read(&destination, &database) == Some(expected),
            None => lookup()? == selected,
        })
    })?;
    Ok(verified.then_some(destination))
}

fn cached_scan(adapter: &CodexAdapter) -> Option<Arc<CodexFileScan>> {
    adapter
        .session_scan
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

// None means Codex will resolve the thread through its active filenames.
fn database_selects(
    adapter: &CodexAdapter,
    home: &Path,
    destination: &Path,
    id: &str,
    source: &Path,
    revision: SourceRevision,
) -> Result<Option<bool>> {
    if !source.starts_with(home.join("sessions")) || !native_codec(source) {
        return Ok(Some(false));
    }
    let rows = adapter.current.rows_in(destination, [id])?;
    if let Some(row) = rows.get(id).filter(|row| !row.archived_for_launch()) {
        // A native database selection can be verified directly, without listing other threads.
        match row.resolve(home, id, std::iter::empty(), false) {
            Ok(Some(file)) => {
                return Ok(Some(
                    file.native
                        && same_native_copy(&file.path, source)?
                        && CodexAdapter::index_file_revision(source) == Some(revision),
                ));
            }
            Ok(None) if row.missing_ordinary_native_path(home, id)? => {}
            Ok(None) => return Ok(Some(false)),
            Err(error) if CodexAdapter::is_current_rollout_unavailable(&error) => {
                return Ok(Some(false));
            }
            Err(error) => return Err(error),
        }
    }
    Ok(None)
}

pub(super) fn same_native_copy(candidate: &Path, selected: &Path) -> io::Result<bool> {
    Ok(candidate.extension() == selected.extension()
        && (candidate == selected || same_file::is_same_file(candidate, selected)?))
}

fn native_codec(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext == "jsonl" || ext == "zst")
}

pub(super) fn native_representation(
    home: &Path,
    canonical_home: &Path,
    path: &Path,
) -> Option<PathBuf> {
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

fn legacy_search_matches(relative: &Path, id: &str) -> bool {
    let name = relative.file_name().and_then(|name| name.to_str());
    if !native_codec(relative) || !name.is_some_and(|name| name.starts_with("rollout-")) {
        return false;
    }
    // Codex 0.137 searches the relative path for the UUID before it tries canonical filenames.
    let mut bytes = relative.as_os_str().as_encoded_bytes().iter();
    id.bytes()
        .all(|wanted| bytes.any(|byte| byte.eq_ignore_ascii_case(&wanted)))
}

pub(super) fn source_filename_selects(
    adapter: &CodexAdapter,
    id: &str,
    source: &Path,
    paginated: bool,
) -> Result<bool> {
    let home = adapter
        .codex_home
        .as_deref()
        .context("Codex home was not found")?;
    let (scan, cached) = adapter.session_scan(None);
    // An ordinary fork can copy the selected history when the source inventory already shows a conflict.
    let known = (!paginated || !cached)
        && scan.active_entries_complete
        && scan.files.iter().any(|path| path == source);
    filesystem_selects(home, id, source, known.then_some(&scan), paginated)
}

// Paginated history requires a Codex version that ranks active files by timestamp and rollout ID.
fn native_rank<'a>(path: &'a Path, id: &str) -> Option<(&'a str, uuid::Uuid)> {
    if !native_codec(path) {
        return None;
    }
    let (thread, rollout) = current_rollout::ids(path)?;
    if !thread.eq_ignore_ascii_case(id) {
        return None;
    }
    Some((
        current_rollout::timestamp(path)?,
        uuid::Uuid::try_parse(rollout).ok()?,
    ))
}

pub(super) fn filesystem_selects(
    home: &Path,
    id: &str,
    source: &Path,
    fresh: Option<&CodexFileScan>,
    paginated: bool,
) -> Result<bool> {
    let Some(rank) = native_rank(source, id) else {
        return Ok(false);
    };
    let canonical_home = fs::canonicalize(home)?;
    if !native_representation(home, &canonical_home, source)
        .as_deref()
        .is_some_and(|native| same_native_copy(native, source).unwrap_or(false))
    {
        return Ok(false);
    }
    let active = home.join("sessions");
    let mut walked = Vec::new();
    let mut walked_links = Vec::new();
    let (files, links) = if let Some(scan) = fresh {
        (&scan.files, &scan.active_links)
    } else {
        let mut unreadable = 0;
        let mut depth_limited = 0;
        collect_jsonl(
            &active,
            5,
            &mut walked,
            SCAN_LIMIT + 1,
            &mut unreadable,
            &mut depth_limited,
            (!paginated).then_some(&mut walked_links),
        );
        if unreadable != 0
            || depth_limited != 0
            || walked.len() > SCAN_LIMIT
            || walked_links.len() > SCAN_LIMIT
        {
            return Ok(false);
        }
        (&walked, &walked_links)
    };
    let name = rollout_file::jsonl_file_name(source);
    let matches_id = |path: &Path| {
        path.strip_prefix(&active)
            .is_ok_and(|relative| legacy_search_matches(relative, id))
    };
    let different_recording = |path: &Path| {
        path.parent() != source.parent() || rollout_file::jsonl_file_name(path) != name
    };
    if !paginated {
        // Codex 0.137 follows directory links during filename search. Their contents are outside this inventory.
        for link in links {
            match fs::metadata(link) {
                Ok(target)
                    if target.is_dir()
                        || (target.is_file() && matches_id(link) && different_recording(link)) =>
                {
                    return Ok(false);
                }
                Err(error)
                    if !matches!(
                        error.kind(),
                        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                    ) =>
                {
                    return Ok(false);
                }
                _ => {}
            }
        }
    }
    let mut found = false;
    for path in files {
        let agrees = if paginated {
            if !path.starts_with(&active) || native_rank(path, id).is_none_or(|other| other < rank)
            {
                continue;
            }
            if let Some(candidate) = native_representation(home, &canonical_home, path) {
                same_native_copy(&candidate, source)?
            } else {
                false
            }
        } else {
            if !matches_id(path) {
                continue;
            }
            !different_recording(path)
        };
        if !agrees {
            if fs::symlink_metadata(path)
                .is_err_and(|error| error.kind() == io::ErrorKind::NotFound)
            {
                continue;
            }
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
            verified_sqlite_home(&adapter, &source, &home, overrides).map(|root| root.is_some())
        };
        assert!(selects(&CodexLaunchOverrides::default())?);
        let other = home.join("target-state");
        let launch = CodexLaunchOverrides {
            sqlite_home: Some(other.clone()),
            ..Default::default()
        };
        assert!(
            !selects(&launch)?,
            "the destination has two possible active recordings"
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
            !selects(&launch)?,
            "the archived row cannot settle an ambiguous filename lookup"
        );
        fs::remove_file(&older)?;
        assert!(
            selects(&launch)?,
            "the remaining active recording is unambiguous"
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

    #[test]
    fn filename_fallback_requires_one_active_recording_across_dates_and_codecs() -> Result<()> {
        use crate::{NativeResumeSupport, ProviderAdapter};

        const ID: &str = "00000000-8c81-4a30-b4fa-42a6c628648f";
        let temp = tempfile::tempdir()?;
        let home = temp.path().canonicalize()?;
        let older = home
            .join("sessions/2025/12/31")
            .join(format!("rollout-2025-12-31T23-00-00-{ID}.jsonl"));
        let plain = home
            .join("sessions/2026/01/01")
            .join(format!("rollout-2026-01-01T00-00-00-{ID}.jsonl"));
        let compressed = plain.with_extension("jsonl.zst");
        let archived = home
            .join("archived_sessions")
            .join(older.file_name().unwrap());
        for path in [&older, &plain, &archived] {
            fs::create_dir_all(path.parent().unwrap())?;
        }
        let transcript = format!("{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{ID}\"}}}}\n");
        fs::write(
            &compressed,
            zstd::stream::encode_all(transcript.as_bytes(), 1)?,
        )?;
        fs::write(&archived, &transcript)?;
        let selects = |path| filesystem_selects(&home, ID, path, None, false);
        let support = || {
            CodexAdapter::with_root(&home)
                .native_resume_support(&SessionRef::new(Provider::Codex, ID))
        };
        assert!(selects(&compressed)?);
        assert_eq!(support()?, NativeResumeSupport::Since("0.137.0"));
        fs::write(&older, &transcript)?;
        assert!(!selects(&compressed)?);
        assert!(!selects(&older)?);
        assert_eq!(support()?, NativeResumeSupport::ImportOnly);
        let legacy = older.with_file_name(format!("rollout-{ID}.jsonl"));
        fs::rename(&older, &legacy)?;
        assert!(!selects(&compressed)?);
        fs::remove_file(legacy)?;
        fs::write(&plain, &transcript)?;
        assert!(selects(&plain)?);
        assert!(!selects(&compressed)?);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn filename_fallback_checks_directory_and_file_links_in_fresh_and_cached_scans() -> Result<()> {
        use std::os::unix::fs::symlink;

        const ID: &str = "37cf0001-1111-4111-8111-111111111111";
        let temp = tempfile::tempdir()?;
        let home = temp.path().canonicalize()?;
        let active = home.join("sessions");
        let day = active.join("2026/01/02");
        let hidden = home.join("backup");
        fs::create_dir_all(&day)?;
        fs::create_dir(&hidden)?;
        let source = day.join(format!("rollout-2026-01-02T00-00-00-{ID}.jsonl"));
        let older = hidden.join(format!("rollout-2025-01-01T00-00-00-{ID}.jsonl"));
        let header = format!("{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{ID}\"}}}}\n");
        fs::write(&source, &header)?;
        fs::write(&older, header)?;
        let check = |expected| -> Result<()> {
            let scan = super::super::scan_session_files(&home, SCAN_LIMIT);
            for inventory in [Some(&scan), None] {
                assert_eq!(
                    filesystem_selects(&home, ID, &source, inventory, false)?,
                    expected
                );
                assert!(filesystem_selects(&home, ID, &source, inventory, true)?);
            }
            Ok(())
        };
        let directory_link = active.join("old.bak");
        symlink(&hidden, &directory_link)?;
        check(false)?;
        fs::remove_file(directory_link)?;
        let file_link = active.join(source.file_name().unwrap());
        symlink(&older, &file_link)?;
        check(false)?;
        fs::remove_file(file_link)?;
        symlink(older, active.join("notes.txt"))?;
        check(true)
    }
}
