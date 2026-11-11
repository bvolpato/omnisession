use super::*;

fn texts(snapshot: &CanonicalSnapshot) -> Vec<&str> {
    snapshot
        .events
        .iter()
        .filter_map(|event| event.payload["text"].as_str())
        .collect()
}

fn archive_and_child(home: &Path) -> anyhow::Result<(PathBuf, PathBuf)> {
    let parent = [header(PARENT, 0, None), turn(1, "user", "archived request")];
    let archive = write(
        home,
        "archived_sessions",
        PARENT,
        PARENT,
        "2026-01-01T00-00-00",
        &parent,
    );
    let child = write(
        home,
        "sessions/2026/01/02",
        CHILD,
        CHILD,
        "2026-01-02T00-00-00",
        &[
            header(CHILD, 2, Some(bookmark(PARENT, &parent))),
            turn(3, "user", "child request"),
        ],
    );
    let db = rusqlite::Connection::open(home.join("state_5.sqlite"))?;
    db.execute_batch(
        "CREATE TABLE threads(id TEXT PRIMARY KEY, rollout_path TEXT, history_mode TEXT)",
    )?;
    db.execute(
        "INSERT INTO threads VALUES (?1, ?2, 'paginated')",
        (CHILD, child.to_str()),
    )?;
    Ok((archive, child))
}

#[test]
fn a_deeper_active_copy_prevents_native_dispatch_to_an_archived_ancestor() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let (archive, _) = archive_and_child(home.path())?;
    let session = SessionRef::new(Provider::Codex, CHILD);
    let support = || CodexAdapter::with_root(home.path()).native_resume_support(&session);
    assert_eq!(support()?, NativeResumeSupport::Since("0.167.0"));
    let deep = home
        .path()
        .join("sessions/backup/export/sessions/2026/01/01");
    fs::create_dir_all(&deep)?;
    let different =
        std::fs::read_to_string(&archive)?.replace("archived request", "differed request");
    fs::write(deep.join(archive.file_name().unwrap()), different)?;
    assert_eq!(support()?, NativeResumeSupport::ImportOnly);
    let snapshot = read(home.path(), false)?;
    assert_eq!(texts(&snapshot), ["archived request", "child request"]);
    Ok(())
}

#[cfg(unix)]
#[test]
fn unreadable_directory_preserves_the_child_and_retries_the_exact_ancestor() -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    let home = tempfile::tempdir()?;
    let (archive, child) = archive_and_child(home.path())?;
    let sibling = home.path().join("sessions/2026/01/03");
    fs::create_dir_all(&sibling)?;
    let hidden = sibling.join(archive.file_name().unwrap());
    fs::rename(&archive, &hidden)?;
    let permissions = fs::metadata(&sibling)?.permissions();
    fs::set_permissions(&sibling, fs::Permissions::from_mode(0o000))?;
    let inaccessible =
        fs::read_dir(&sibling).is_err_and(|e| e.kind() == std::io::ErrorKind::PermissionDenied);
    let session = SessionRef::new(Provider::Codex, CHILD);
    let check = || -> anyhow::Result<()> {
        let adapter = CodexAdapter::with_root(home.path());
        let index = adapter.index_batch(&[CHILD]).unwrap();
        let partial = index(&session, Some(&child), u64::MAX)?;
        assert_eq!(
            partial.source_revision.missing_ancestor,
            Some(PARENT.parse()?)
        );
        for snapshot in [partial.snapshot, adapter.read_session(&session)?] {
            assert_eq!(texts(&snapshot), ["child request"]);
            let reason = snapshot
                .events
                .iter()
                .find_map(|event| event.payload["reason"].as_str())
                .unwrap();
            assert_eq!(
                reason,
                "Codex could not search all directories for a referenced rollout"
            );
        }
        assert_eq!(
            adapter.native_resume_support(&session)?,
            NativeResumeSupport::ImportOnly
        );
        Ok(())
    };
    let result = if inaccessible { check() } else { Ok(()) };
    fs::set_permissions(&sibling, permissions)?;
    result?;
    assert_eq!(
        texts(&read(home.path(), false)?),
        ["archived request", "child request"]
    );
    Ok(())
}
