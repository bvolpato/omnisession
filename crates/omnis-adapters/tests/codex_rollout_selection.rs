use std::{fmt::Write as _, fs, path::Path};

use anyhow::Result;
use omnis_adapters::{CodexAdapter, NativeResumeSupport, ProviderAdapter};
use omnis_ir::{Provider, SessionRef};
use serde_json::json;

const THREAD: &str = "11111111-1111-4111-8111-111111111111";
const SELECTED: &str = "22222222-2222-4222-8222-222222222222";
const ABANDONED: &str = "33333333-3333-4333-8333-333333333333";

const STAMP: &str = "2026-01-01T00:00:00.000Z";

fn transcript(messages: &[&str]) -> String {
    let metadata = json!({"timestamp":STAMP,"type":"session_meta","ordinal":0,"payload":{
        "id":THREAD,"history_mode":"paginated","cwd":"/workspace","model_provider":"fixture",
        "timestamp":STAMP,"originator":"codex_cli_rs","cli_version":"0.167.1","source":"cli"
    }});
    let mut source = format!("{metadata}\n");
    for (index, text) in messages.iter().enumerate() {
        let turn = format!("019f0000-0000-7000-8000-{index:012}");
        let id = format!("request-{index}");
        let payloads = [
            ("event_msg", json!({"type":"task_started","turn_id":turn})),
            (
                "response_item",
                json!({"type":"message","id":id,"role":"user","content":[{"type":"input_text","text":text}],
                "internal_chat_message_metadata_passthrough":{"turn_id":turn}}),
            ),
            (
                "event_msg",
                json!({"type":"item_completed","thread_id":THREAD,"turn_id":turn,"item":{
                "type":"UserMessage","id":id,"content":[{"type":"text","text":text}]}}),
            ),
            ("event_msg", json!({"type":"task_complete","turn_id":turn})),
        ];
        for (offset, (kind, payload)) in payloads.into_iter().enumerate() {
            let row = json!({"timestamp":STAMP,"ordinal":index * 4 + offset + 1,"type":kind,"payload":payload});
            writeln!(source, "{row}").unwrap();
        }
    }
    source
}

fn rollout(text: &str) -> String {
    transcript(&[text])
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
    assert!(
        adapter
            .list_sessions(None)?
            .iter()
            .all(|row| row.session != session)
    );
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
    assert_unavailable(home.path(), &backup)?;
    let physical = active.join(format!("backup-{THREAD}_{SELECTED}.jsonl"));
    fs::rename(&backup, &physical)?;
    db.execute(
        "UPDATE threads SET rollout_path=?1",
        [physical.to_str().unwrap()],
    )?;
    assert_selected(home.path(), &physical, &physical, "restored conversation")?;
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
    fs::remove_file(&selected)?;
    fs::hard_link(&compressed, &selected)?;
    assert_selected(home.path(), &compressed, &selected, "selected conversation")?;
    fs::remove_file(&selected)?;
    let legacy = rollout("legacy conversation").replace("\"paginated\"", "\"legacy\"");
    fs::write(&selected, &legacy)?;
    let backup = compressed.with_extension("backup");
    fs::rename(&compressed, &backup)?;
    assert_unavailable(home.path(), &selected)?;
    fs::rename(backup, &compressed)?;
    assert_selected(home.path(), &compressed, &selected, "selected conversation")?;
    let database = rusqlite::Connection::open(home.path().join("state_5.sqlite"))?;
    database.execute("UPDATE threads SET history_mode = 'legacy'", [])?;
    let session = SessionRef::new(Provider::Codex, THREAD);
    let adapter = CodexAdapter::with_root(home.path());
    assert_eq!(
        adapter.native_resume_support(&session)?,
        NativeResumeSupport::Direct
    );
    let found = adapter
        .find_native_session(THREAD)?
        .expect("selected session");
    assert!(same_file::is_same_file(
        found.source_path.expect("selected rollout path"),
        &selected
    )?);
    database.execute("UPDATE threads SET history_mode = 'paginated'", [])?;
    let directory = home.path().join("archived_sessions");
    fs::create_dir(&directory)?;
    let archived = directory.join(compressed.file_name().unwrap());
    fs::rename(compressed, &archived)?;
    assert_selected(home.path(), &archived, &abandoned, "selected conversation")?;
    fs::remove_file(selected)?;
    assert_selected(home.path(), &archived, &abandoned, "selected conversation")?;
    database.execute(
        "UPDATE threads SET rollout_path = ?1",
        [archived.to_str().unwrap()],
    )?;
    let restored = active.join(archived.file_name().unwrap());
    fs::rename(&archived, &restored)?;
    assert_selected(home.path(), &restored, &abandoned, "selected conversation")?;
    let wrong_day = home.path().join("sessions/2026/01/02");
    fs::create_dir_all(&wrong_day)?;
    fs::rename(&restored, wrong_day.join(restored.file_name().unwrap()))?;
    assert_unavailable(home.path(), &abandoned)
}

#[test]
fn a_damaged_current_mode_keeps_every_local_turn_until_a_complete_copy_agrees() -> Result<()> {
    let home = tempfile::tempdir()?;
    let active = home.path().join("sessions/2026/01/01");
    let archived = home.path().join("archived_sessions");
    fs::create_dir_all(&active)?;
    fs::create_dir(&archived)?;
    let name = format!("rollout-2026-01-01T00-00-00-{THREAD}.jsonl");
    let chosen = active.join(&name);
    let copy = archived.join(name);
    let messages = ["opening request", "latest request"];
    let damaged = transcript(&messages).replace("history_mode", "history_mnde");
    fs::write(&chosen, &damaged)?;
    let database = rusqlite::Connection::open(home.path().join("state_5.sqlite"))?;
    database.execute(
        "CREATE TABLE threads(id TEXT PRIMARY KEY, rollout_path TEXT, history_mode TEXT)",
        [],
    )?;
    database.execute(
        "INSERT INTO threads VALUES(?1, ?2, 'paginated')",
        [THREAD, chosen.to_str().unwrap()],
    )?;
    drop(database);
    for backup in [
        None,
        Some(&messages[..1]),
        Some(&["opening request", "other request"][..]),
        Some(&["opening request", "latest request", "extra request"][..]),
        Some(&messages[..]),
    ] {
        if let Some(turns) = backup {
            fs::write(&copy, transcript(turns))?;
        }
        let verified = backup == Some(&messages);
        let adapter = CodexAdapter::with_root(home.path());
        let session = SessionRef::new(Provider::Codex, THREAD);
        let expected = if verified { &copy } else { &chosen };
        let listed = adapter.list_sessions(None)?;
        assert_eq!(listed.len(), 1);
        let selected = listed[0]
            .source_path
            .as_deref()
            .expect("selected rollout path");
        assert!(same_file::is_same_file(selected, expected)?);
        assert_eq!(
            adapter.native_resume_support(&session)?,
            NativeResumeSupport::ImportOnly
        );
        let index = adapter.index_batch(&[THREAD]).unwrap();
        for snapshot in [
            adapter.read_session(&session)?,
            adapter.preview_session(&session)?,
            index(&session, Some(&copy), 0)?.snapshot,
        ] {
            let text: Vec<_> = snapshot
                .events
                .iter()
                .filter_map(|e| e.payload["text"].as_str())
                .collect();
            assert_eq!(text, messages);
            let unverified = snapshot.events.iter().any(|e| {
                e.source.raw_record_type.as_deref() == Some("omnisession.codex_history_mode")
                    && e.payload["native_history_unverified"] == true
            });
            assert_eq!(unverified, !verified);
        }
        assert_eq!(fs::read_to_string(&chosen)?, damaged);
    }

    fs::remove_file(&copy)?;
    let attachment = damaged.replacen(
        r#""history_mnde""#,
        r#""attached_thread":true,"history_mnde""#,
        1,
    );
    fs::write(&chosen, attachment)?;
    let adapter = CodexAdapter::with_root(home.path());
    let session = SessionRef::new(Provider::Codex, THREAD);
    let error = adapter.read_session(&session).unwrap_err();
    assert!(format!("{error:#}").contains("locate the attached rollout's visible history"));
    Ok(())
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
fn a_relocated_paginated_rollout_checks_the_body_of_an_owned_damaged_copy() -> Result<()> {
    let fixture = tempfile::tempdir()?;
    let home = fixture.path();
    let active = home.join("sessions/2026/01/01");
    let archived = home.join("archived_sessions");
    fs::create_dir_all(&active)?;
    fs::create_dir(&archived)?;
    let name = format!("rollout-2026-01-01T00-00-00-{THREAD}_{SELECTED}.jsonl");
    let chosen = active.join(&name);
    let archive = archived.join(&name);
    let database = rusqlite::Connection::open(home.join("state_5.sqlite"))?;
    database.execute(
        "CREATE TABLE threads(id TEXT PRIMARY KEY, rollout_path TEXT, history_mode TEXT)",
        [],
    )?;
    database.execute(
        "INSERT INTO threads VALUES(?1, ?2, 'paginated')",
        [THREAD, &format!(r"C:\Retired\Codex\sessions\{name}")],
    )?;
    let short = rollout("opening request");
    let long = transcript(&["opening request", "latest request"]);
    for (field, changed) in [
        (r#""history_mode":"paginated","#, ""),
        (r#""history_mode":"paginated""#, r#""history_mode":null"#),
        (r#""history_mode""#, r#""history_mnde""#),
        (r#""id""#, r#""hd""#),
        (r#""ordinal""#, r#""ordhnal""#),
    ] {
        let damaged = |source: &str| source.replacen(field, changed, 1);
        for (left, right) in [(&long, &short), (&short, &long)] {
            fs::write(&chosen, damaged(left))?;
            fs::write(&archive, right)?;
            assert_unavailable(home, &archive)?;
        }
        fs::write(&chosen, damaged(&short))?;
        fs::write(&archive, &short)?;
        assert_selected(home, &archive, &chosen, "opening request")?;
        fs::remove_file(&archive)?;
        assert_unavailable(home, &chosen)?;
    }
    fs::write(&chosen, long.replacen(THREAD, ABANDONED, 1))?;
    fs::write(&archive, &short)?;
    assert_selected(home, &archive, &chosen, "opening request")?;
    let damaged = short.replacen("history_mode", "history_mnde", 1);
    fs::write(&chosen, &damaged)?;

    // A matching native sibling cannot validate a third copy with a different tail.
    let sibling = chosen.with_extension("jsonl.zst");
    fs::write(&sibling, zstd::stream::encode_all(short.as_bytes(), 1)?)?;
    fs::write(&archive, &long)?;
    assert_unavailable(home, &sibling)?;
    database.execute(
        "UPDATE threads SET rollout_path = ?1",
        [chosen.to_str().unwrap()],
    )?;
    let adapter = CodexAdapter::with_root(home);
    let session = SessionRef::new(Provider::Codex, THREAD);
    let snapshot = adapter.read_session(&session)?;
    let conversation = omnis_core::import_conversation(&snapshot);
    assert!(conversation.truncated);
    assert_eq!(conversation.messages.len(), 1);
    assert_eq!(conversation.messages[0].text, "opening request");
    assert!(snapshot.events.iter().any(|event| {
        event.source.raw_record_type.as_deref() == Some("omnisession.codex_history_mode")
    }));
    assert_eq!(fs::read_to_string(chosen)?, damaged);
    Ok(())
}

#[test]
fn a_relocated_hard_link_requires_a_corroborated_local_recording() -> Result<()> {
    let fixture = tempfile::tempdir()?;
    let home = fixture.path().join("restored");
    let day = Path::new("sessions/2026/01/01");
    let name = format!("rollout-2026-01-01T00-00-00-{THREAD}_{SELECTED}.jsonl");
    let active = home.join(day).join(&name);
    let original = fixture.path().join("original").join(day).join(&name);
    let archive = home.join("archived_sessions").join(&name);
    for file in [&active, &original, &archive] {
        fs::create_dir_all(file.parent().unwrap())?;
    }
    let history = rollout("corroborated request");
    let db = rusqlite::Connection::open(home.join("state_5.sqlite"))?;
    db.execute(
        "CREATE TABLE threads(id TEXT PRIMARY KEY, rollout_path TEXT, history_mode TEXT)",
        [],
    )?;
    db.execute(
        "INSERT INTO threads VALUES(?1, ?2, 'paginated')",
        [THREAD, original.to_str().unwrap()],
    )?;
    for (field, replacement) in [
        (r#""history_mode":"paginated""#, r#""history_mode":null"#),
        (r#""ordinal""#, r#""ordhnal""#),
        (r#""id""#, r#""hd""#),
    ] {
        let damaged = history.replacen(field, replacement, 1);
        fs::write(&active, &damaged)?;
        fs::hard_link(&active, &original)?;
        assert_unavailable(&home, &active)?;
        fs::write(&archive, rollout("different archive"))?;
        assert_unavailable(&home, &archive)?;
        fs::write(&archive, &history)?;
        assert_selected(&home, &archive, &active, "corroborated request")?;
        fs::remove_file(&original)?;
        fs::write(&original, &damaged)?;
        assert_unavailable(&home, &archive)?;
        fs::remove_file(&archive)?;
        fs::remove_file(&original)?;
    }
    Ok(())
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
    for mode in ["legacy", "paginated"] {
        let db = rusqlite::Connection::open(restored.join("state_5.sqlite"))?;
        db.execute("UPDATE threads SET history_mode = ?1", [mode])?;
        drop(db);
        fs::write(&saved, rollout("different original selection"))?;
        assert_unavailable(&restored, &abandoned)?;
        fs::write(&saved, rollout("restored selection"))?;
        assert_selected(&restored, &selected, &abandoned, "restored selection")?;
    }

    let db = rusqlite::Connection::open(restored.join("state_5.sqlite"))?;
    db.execute(
        "UPDATE threads SET rollout_path = ?1",
        [format!(r"\\server\backups\.codex\archived_sessions\{name}")],
    )?;
    assert_unavailable(&restored, &abandoned)?;
    for pointer in [
        format!(r"C:\Users\restored\.codex\sessions\{name}"),
        format!("/previous/home/.codex/sessions/{name}"),
    ] {
        db.execute("UPDATE threads SET rollout_path = ?1", [pointer])?;
        assert_selected(&restored, &selected, &abandoned, "restored selection")?;
    }
    drop(db);

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
    assert_eq!(fs::read_to_string(&saved)?, rollout("restored selection"));
    Ok(())
}

#[test]
fn switching_the_configured_state_database_selects_its_current_generation() -> Result<()> {
    let fixture = tempfile::tempdir()?;
    let home = fixture.path();
    let session = SessionRef::new(Provider::Codex, THREAD);
    let day = home.join("sessions/2026/01/01");
    fs::create_dir_all(&day)?;
    for (directory, generation, date, text) in [
        ("configured", SELECTED, "01", "configured history"),
        (".", ABANDONED, "02", "default history"),
    ] {
        let name = format!("rollout-2026-01-{date}T00-00-00-{THREAD}_{generation}.jsonl");
        let path = day.join(name);
        fs::write(&path, rollout(text))?;
        let db_dir = home.join(directory);
        fs::create_dir_all(&db_dir)?;
        let db = rusqlite::Connection::open(db_dir.join("state_5.sqlite"))?;
        db.execute_batch(
            "CREATE TABLE threads(id TEXT PRIMARY KEY, rollout_path TEXT, history_mode TEXT)",
        )?;
        db.execute(
            "INSERT INTO threads VALUES(?1, ?2, 'paginated')",
            [THREAD, path.to_str().unwrap()],
        )?;
    }
    let adapter = CodexAdapter::with_root(home);
    let config = home.join("config.toml");
    for (setting, expected) in [
        ("sqlite_home = 'absent/../configured'", "configured history"),
        ("[unrelated]\nsqlite_home = 'configured'", "default history"),
    ] {
        fs::write(&config, setting)?;
        let snapshot = adapter.read_session(&session)?;
        assert!(
            snapshot
                .events
                .iter()
                .any(|event| event.payload["text"] == expected)
        );
    }
    let repo = home.join("project");
    let project_settings = repo.join(".codex");
    let nested = repo.join("nested");
    fs::create_dir_all(&nested)?;
    fs::create_dir_all(&project_settings)?;
    fs::create_dir_all(repo.join(".git"))?;
    fs::write(repo.join(".git/HEAD"), "ref: refs/heads/main")?;
    fs::write(
        project_settings.join("config.toml"),
        "sqlite_home = '../..'",
    )?;
    for (trust, agrees) in [("untrusted", true), ("trusted", false)] {
        fs::write(
            &config,
            format!(
                "sqlite_home = 'configured'\n[projects.{}]\ntrust_level = '{trust}'",
                serde_json::to_string(&repo)?
            ),
        )?;
        assert_eq!(
            adapter
                .codex_continuation_sqlite_home(
                    Some(&session),
                    &nested,
                    &omnis_adapters::CodexLaunchOverrides::default(),
                    &|| Ok(false),
                )?
                .is_some(),
            agrees
        );
    }
    let selected = day.join(format!(
        "rollout-2026-01-01T00-00-00-{THREAD}_{SELECTED}.jsonl"
    ));
    let database = rusqlite::Connection::open(home.join("state_5.sqlite"))?;
    database.execute(
        "UPDATE threads SET rollout_path = ?1",
        [selected.to_str().unwrap()],
    )?;
    assert!(
        adapter
            .codex_continuation_sqlite_home(
                Some(&session),
                &nested,
                &omnis_adapters::CodexLaunchOverrides::default(),
                &|| Ok(false),
            )?
            .is_some()
    );

    fs::write(config, "sqlite_home = 1234")?;
    let error = adapter.read_session(&session).unwrap_err();
    assert!(format!("{error:#}").contains("sqlite_home"));
    Ok(())
}

#[test]
fn a_database_selected_custom_legacy_file_requires_its_recorded_owner() -> Result<()> {
    let fixture = tempfile::tempdir()?;
    let home = fixture.path().canonicalize()?;
    let active = home.join("sessions/2026/01/01");
    fs::create_dir_all(&active)?;
    let manual = active.join("import.jsonl");
    let session = SessionRef::new(Provider::Codex, THREAD);
    let source = rollout("selected legacy text").replace("\"paginated\"", "\"legacy\"");
    fs::write(&manual, &source)?;
    assert!(
        CodexAdapter::with_root(&home)
            .list_sessions(None)?
            .is_empty()
    );

    let db = rusqlite::Connection::open(home.join("state_5.sqlite"))?;
    db.execute_batch(
        "CREATE TABLE threads(id TEXT PRIMARY KEY, rollout_path TEXT, history_mode TEXT)",
    )?;
    db.execute(
        "INSERT INTO threads VALUES(?1,?2,'legacy')",
        [THREAD, manual.to_str().unwrap()],
    )?;
    let adapter = CodexAdapter::with_root(&home);
    assert_eq!(
        adapter.list_sessions(None)?[0].source_path.as_ref(),
        Some(&manual)
    );
    assert_eq!(
        adapter.native_resume_support(&session)?,
        NativeResumeSupport::Direct
    );

    let stale = home
        .join("archived_sessions")
        .join(format!("rollout-2025-01-01T00-00-00-{THREAD}.jsonl"));
    fs::create_dir_all(stale.parent().unwrap())?;
    fs::write(&stale, rollout("retired generation"))?;
    let read = adapter.index_session(&session, Some(&stale), u64::MAX)?;
    assert!(
        read.snapshot
            .events
            .iter()
            .any(|event| event.payload["text"] == "selected legacy text")
    );
    fs::write(&manual, source.replace(THREAD, ABANDONED))?;
    assert_unavailable(&home, &stale)?;
    let prior = CodexAdapter::index_file_revision(&stale).unwrap();
    let adapter = CodexAdapter::with_root(&home);
    assert!(
        adapter
            .unavailable_codex_rollouts(&[(THREAD.to_owned(), Some(prior))])?
            .contains(THREAD)
    );
    let other_home = tempfile::tempdir()?;
    let other = other_home.path().join(stale.file_name().unwrap());
    fs::copy(&stale, &other)?;
    let foreign_source = CodexAdapter::index_file_revision(&other).unwrap();
    assert!(
        adapter
            .unavailable_codex_rollouts(&[(THREAD.to_owned(), Some(foreign_source))])?
            .is_empty()
    );
    let foreign = active.join(format!("rollout-2026-01-01T00-00-00-{ABANDONED}.jsonl"));
    fs::rename(&manual, &foreign)?;
    db.execute(
        "UPDATE threads SET rollout_path=?1 WHERE id=?2",
        [foreign.to_str().unwrap(), THREAD],
    )?;
    assert_unavailable(&home, &stale)?;
    assert_eq!(
        CodexAdapter::with_root(&home).list_sessions(None)?[0]
            .session
            .id,
        ABANDONED
    );
    Ok(())
}

#[test]
fn only_a_supported_flat_header_can_resume_as_legacy() -> Result<()> {
    let fixture = tempfile::tempdir()?;
    let home = fixture.path();
    let active = home.join("sessions/2026/01/01");
    fs::create_dir_all(&active)?;
    let path = active.join(format!("rollout-2026-01-01T00-00-00-{THREAD}.jsonl"));
    let session = SessionRef::new(Provider::Codex, THREAD);
    for (declared, support) in [
        (json!({}), NativeResumeSupport::Direct),
        (
            json!({"history_mode":"future"}),
            NativeResumeSupport::ImportOnly,
        ),
        (
            json!({"history_mode":null}),
            NativeResumeSupport::ImportOnly,
        ),
        (
            json!({"history_mode":"legacy","history_base":{"thread_id":SELECTED,"end_ordinal_exclusive":1,"end_byte_offset":100}}),
            NativeResumeSupport::ImportOnly,
        ),
    ] {
        let mut payload = declared;
        payload["id"] = json!(THREAD);
        fs::write(
            &path,
            format!("{}\n", json!({"type":"session_meta","payload":payload})),
        )?;
        assert_eq!(
            CodexAdapter::with_root(home).native_resume_support(&session)?,
            support
        );
    }
    Ok(())
}
