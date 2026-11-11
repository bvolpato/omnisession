use std::{fs, path::Path};

use anyhow::Result;
use omnis_adapters::{CodexAdapter, NativeResumeSupport, ProviderAdapter};
use omnis_ir::{Provider, SessionRef};
use serde_json::json;

const THREAD: &str = "11111111-1111-4111-8111-111111111111";
const SELECTED: &str = "22222222-2222-4222-8222-222222222222";
const ABANDONED: &str = "33333333-3333-4333-8333-333333333333";

fn rollout(text: &str) -> String {
    let metadata = json!({"type":"session_meta","ordinal":0,"payload":{
        "id":THREAD,"history_mode":"paginated","cwd":"/workspace","model_provider":"fixture"
    }});
    let message = json!({"type":"response_item","ordinal":1,"payload":{
        "type":"message","role":"user","content":[{"type":"input_text","text":text}]
    }});
    format!("{metadata}\n{message}\n")
}

fn assert_selected(home: &Path, chosen: &Path, hint: &Path, text: &str) -> Result<()> {
    let adapter = CodexAdapter::with_root(home);
    let session = SessionRef::new(Provider::Codex, THREAD);
    let listed = adapter.list_sessions(None)?;
    assert_eq!(listed.len(), 1);
    let found = adapter.find_native_session(THREAD)?.expect("exact session");
    for entry in [&listed[0], &found] {
        assert_eq!(entry.session, session);
        let path = entry.source_path.as_deref().expect("rollout path");
        assert_eq!(path.canonicalize()?, chosen.canonicalize()?);
    }
    assert_eq!(
        adapter.recorded_model_provider(THREAD)?.as_deref(),
        Some("fixture")
    );

    let batch = adapter.index_batch(&[THREAD]).expect("Codex index reader");
    for snapshot in [
        adapter.read_session(&session)?,
        adapter.preview_session(&session)?,
        adapter
            .index_session(&session, Some(hint), u64::MAX)?
            .snapshot,
        batch(&session, Some(hint), u64::MAX)?.snapshot,
    ] {
        let messages: Vec<_> = snapshot
            .events
            .iter()
            .filter_map(|event| event.payload["text"].as_str())
            .collect();
        assert_eq!(messages, [text]);
    }
    assert_eq!(
        adapter.native_resume_support(&session)?,
        NativeResumeSupport::ImportOnly,
    );
    Ok(())
}

fn assert_unavailable(home: &Path, abandoned: &Path) -> Result<()> {
    let adapter = CodexAdapter::with_root(home);
    let session = SessionRef::new(Provider::Codex, THREAD);
    assert!(adapter.list_sessions(None)?.is_empty());
    assert!(adapter.find_native_session(THREAD).ok().flatten().is_none());
    assert!(adapter.read_session(&session).is_err());
    assert!(adapter.preview_session(&session).is_err());
    assert!(
        adapter
            .index_session(&session, Some(abandoned), u64::MAX)
            .is_err()
    );
    let batch = adapter.index_batch(&[THREAD]).unwrap();
    assert!(batch(&session, Some(abandoned), u64::MAX).is_err());
    assert!(matches!(
        adapter.native_resume_support(&session),
        Err(_) | Ok(NativeResumeSupport::ImportOnly)
    ));
    Ok(())
}

#[test]
fn damaged_active_zstd_header_does_not_hide_a_readable_archived_rollout() -> Result<()> {
    let home = tempfile::tempdir()?;
    let active = home.path().join("sessions/2026/01/01");
    let archives = home.path().join("archived_sessions");
    fs::create_dir_all(&active)?;
    fs::create_dir(&archives)?;
    let name = format!("rollout-2026-01-01T00-00-00-{THREAD}.jsonl");
    let broken = active.join(&name).with_extension("jsonl.zst");
    let archive = archives.join(name);
    let frame = zstd::stream::encode_all(rollout("incomplete active file").as_bytes(), 1)?;
    fs::write(&broken, &frame[..4])?;
    fs::write(&archive, rollout("archived conversation"))?;
    assert_selected(home.path(), &archive, &broken, "archived conversation")?;
    // A bad codec sibling cannot hide the valid plain file or prevent index reconciliation.
    let plain = broken.with_extension("");
    fs::write(&plain, rollout("restored conversation"))?;
    let adapter = CodexAdapter::with_root(home.path());
    assert!(
        adapter.discovery_is_complete(),
        "{:?}",
        adapter.discovery_notes()
    );
    assert_eq!(
        adapter.native_resume_support(&SessionRef::new(Provider::Codex, THREAD))?,
        NativeResumeSupport::Since("0.167.0"),
    );
    fs::remove_file(archive)?;
    fs::remove_file(broken)?;
    let backup = active.join(format!("backup-{THREAD}.jsonl"));
    fs::rename(plain, &backup)?;
    assert_selected(home.path(), &backup, &backup, "restored conversation")?;
    let db = rusqlite::Connection::open(home.path().join("state_5.sqlite"))?;
    db.execute(
        "CREATE TABLE threads(id TEXT PRIMARY KEY, rollout_path TEXT, history_mode TEXT)",
        [],
    )?;
    db.execute(
        "INSERT INTO threads VALUES(?1, ?2, 'paginated')",
        [THREAD, backup.to_str().unwrap()],
    )?;
    assert_selected(home.path(), &backup, &backup, "restored conversation")?;
    Ok(())
}

#[test]
fn a_damaged_selected_revert_recovers_only_from_its_own_physical_rollout() -> Result<()> {
    let home = tempfile::tempdir()?;
    let active = home.path().join("sessions/2026/01/01");
    fs::create_dir_all(&active)?;
    let selected = active.join(format!(
        "rollout-2026-01-01T00-00-00-{THREAD}_{SELECTED}.jsonl"
    ));
    let abandoned = active.join(format!(
        "rollout-2026-01-01T01-00-00-{THREAD}_{ABANDONED}.jsonl"
    ));
    fs::write(&selected, b"{\n")?;
    fs::write(&abandoned, rollout("abandoned conversation"))?;
    let database = rusqlite::Connection::open(home.path().join("state_5.sqlite"))?;
    database.execute(
        "CREATE TABLE threads(id TEXT PRIMARY KEY, rollout_path TEXT, history_mode TEXT)",
        [],
    )?;
    database.execute(
        "INSERT INTO threads VALUES(?1, ?2, 'paginated')",
        [THREAD, selected.to_str().unwrap()],
    )?;
    drop(database);

    assert_unavailable(home.path(), &abandoned)?;

    let compressed = selected.with_extension("jsonl.zst");
    fs::write(
        &compressed,
        zstd::stream::encode_all(rollout("selected conversation").as_bytes(), 1)?,
    )?;
    assert_selected(
        home.path(),
        &compressed,
        &abandoned,
        "selected conversation",
    )?;
    assert_eq!(fs::read(&selected)?, b"{\n");
    let directory = home.path().join("archived_sessions");
    fs::create_dir(&directory)?;
    let archived = directory.join(compressed.file_name().unwrap());
    fs::rename(compressed, &archived)?;
    assert_selected(home.path(), &archived, &abandoned, "selected conversation")?;
    fs::remove_file(selected)?;
    assert_selected(home.path(), &archived, &abandoned, "selected conversation")?;
    fs::remove_file(archived)?;
    assert_unavailable(home.path(), &abandoned)
}

#[test]
fn warm_index_readers_reconcile_fresh_discovery_hints_with_state() -> Result<()> {
    let fixture = tempfile::tempdir()?;
    let home = fixture.path().canonicalize()?.join("codex");
    let archives = home.join("archived_sessions");
    fs::create_dir_all(&archives)?;
    let archive = archives.join(format!("rollout-2026-01-01T00-00-00-{THREAD}.jsonl"));
    fs::write(&archive, rollout("first archive"))?;

    let session = SessionRef::new(Provider::Codex, THREAD);
    let direct = CodexAdapter::with_root(&home);
    let batched = CodexAdapter::with_root(&home);
    for worker in [&direct, &batched] {
        let listed = worker.list_sessions(None)?;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].source_path, Some(archive.canonicalize()?));
    }

    let check = |hint: &Path, expected: &Path, message: &str| -> Result<()> {
        let found = CodexAdapter::with_root(&home)
            .find_native_session(THREAD)?
            .expect("fresh exact lookup");
        assert_eq!(found.source_path, Some(expected.canonicalize()?));
        let revision = CodexAdapter::index_file_revision(expected).expect("selected revision");
        assert!(revision.contents.is_some());
        let read_batch = batched.index_batch(&[THREAD]).expect("Codex index batch");
        for read in [
            direct.index_session(&session, Some(hint), u64::MAX)?,
            read_batch(&session, Some(hint), u64::MAX)?,
        ] {
            let messages: Vec<_> = read
                .snapshot
                .events
                .iter()
                .filter_map(|event| event.payload["text"].as_str())
                .collect();
            assert_eq!(messages, [message]);
            assert_eq!(read.source_revision, revision);
            let selected = read.native_session.expect("verified catalog metadata");
            assert_eq!(selected.source_path, found.source_path);
            assert_eq!(selected.source_revision, Some(revision));
            assert_eq!(selected.project_path, found.project_path);
        }
        Ok(())
    };

    let outside = fixture
        .path()
        .join(format!("rollout-2026-12-31T23-59-59-{THREAD}.jsonl"));
    fs::write(&outside, rollout("unrelated external file"))?;
    check(&outside, &archive, "first archive")?;

    let day = home.join("sessions/2026/01/02");
    fs::create_dir_all(&day)?;
    let active = day.join(format!("rollout-2026-01-02T00-00-00-{THREAD}.jsonl"));
    fs::write(&active, rollout("restored active rollout"))?;
    let hint = CodexAdapter::with_root(&home)
        .list_sessions(None)?
        .into_iter()
        .find(|entry| entry.session == session)
        .and_then(|entry| entry.source_path)
        .expect("discovery supplied a source path");
    check(&hint, &active, "restored active rollout")?;

    let reverted = archives.join(format!(
        "rollout-2026-01-01T10-00-00-{THREAD}_{SELECTED}.jsonl"
    ));
    fs::write(&reverted, rollout("current physical revert"))?;
    let db = rusqlite::Connection::open(home.join("state_5.sqlite"))?;
    db.execute(
        "CREATE TABLE threads(id TEXT PRIMARY KEY, rollout_path TEXT, history_mode TEXT)",
        [],
    )?;
    db.execute(
        "INSERT INTO threads VALUES(?1, ?2, 'paginated')",
        [THREAD, reverted.to_str().unwrap()],
    )?;
    drop(db);
    check(&hint, &reverted, "current physical revert")
}

#[test]
fn a_moved_state_pointer_selects_only_agreeing_copies_in_this_home() -> Result<()> {
    let fixture = tempfile::tempdir()?;
    let original = fixture.path().join("original");
    let restored = fixture.path().join("restored");
    let day = Path::new("sessions/2026/01/01");
    let name = format!("rollout-2026-01-01T00-00-00-{THREAD}_{SELECTED}.jsonl");
    let saved = original.join(day).join(&name);
    fs::create_dir_all(saved.parent().unwrap())?;
    fs::write(&saved, rollout("restored selection"))?;
    let create = rusqlite::Connection::open(original.join("state_5.sqlite"))?;
    create.execute_batch(
        "CREATE TABLE threads(id TEXT PRIMARY KEY, rollout_path TEXT, history_mode TEXT)",
    )?;
    create.execute(
        "INSERT INTO threads VALUES(?1, ?2, 'paginated')",
        [THREAD, saved.to_str().unwrap()],
    )?;
    drop(create);
    fs::rename(&original, &restored)?;
    let selected = restored.join(day).join(&name);
    let abandoned = selected.with_file_name(format!(
        "rollout-2026-01-02T00-00-00-{THREAD}_{ABANDONED}.jsonl"
    ));
    fs::write(&abandoned, rollout("abandoned revert"))?;
    assert_selected(&restored, &selected, &abandoned, "restored selection")?;

    fs::create_dir_all(saved.parent().unwrap())?;
    fs::write(&saved, rollout("different original selection"))?;
    for mode in ["legacy", "paginated"] {
        let db = rusqlite::Connection::open(restored.join("state_5.sqlite"))?;
        db.execute("UPDATE threads SET history_mode = ?1", [mode])?;
        drop(db);
        assert_selected(&restored, &selected, &abandoned, "restored selection")?;
    }

    let archived = restored
        .join("archived_sessions")
        .join(name)
        .with_extension("jsonl.zst");
    fs::create_dir_all(archived.parent().unwrap())?;
    fs::write(
        &archived,
        zstd::stream::encode_all(rollout("restored selection").as_bytes(), 1)?,
    )?;
    assert_selected(&restored, &selected, &abandoned, "restored selection")?;
    fs::write(&selected, rollout("conflicting local copy"))?;
    assert_unavailable(&restored, &abandoned)?;

    let sibling = selected.with_extension("jsonl.zst");
    fs::rename(&archived, &sibling)?;
    fs::write(
        &selected,
        rollout("pre-migration copy").replace("\"paginated\"", "\"legacy\""),
    )?;
    assert_selected(&restored, &sibling, &abandoned, "restored selection")?;
    fs::remove_file(sibling)?;
    assert_unavailable(&restored, &abandoned)?;
    fs::write(&selected, rollout("restored selection"))?;

    let database = rusqlite::Connection::open(restored.join("state_5.sqlite"))?;
    database.execute(
        "UPDATE threads SET rollout_path = ?1",
        [selected.to_str().unwrap()],
    )?;
    let session = SessionRef::new(Provider::Codex, THREAD);
    assert_eq!(
        CodexAdapter::with_root(&restored).native_resume_support(&session)?,
        NativeResumeSupport::Since("0.167.0")
    );
    fs::remove_file(&selected)?;
    fs::remove_file(&abandoned)?;
    let first_name = format!("rollout-2026-01-01T00-00-00-{THREAD}.jsonl");
    let single_legacy = restored.join(day).join(&first_name);
    fs::write(
        &single_legacy,
        rollout("pre-migration singleton").replace("\"paginated\"", "\"legacy\""),
    )?;
    let foreign_first = original.join(day).join(first_name);
    database.execute(
        "UPDATE threads SET rollout_path = ?1",
        [foreign_first.to_str().unwrap()],
    )?;
    assert_unavailable(&restored, &single_legacy)?;
    assert_eq!(
        fs::read_to_string(&saved)?,
        rollout("different original selection")
    );
    Ok(())
}
