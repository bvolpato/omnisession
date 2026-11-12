use std::{collections::HashSet, fs, path::Path};

use anyhow::Result;
use omnis_adapters::{CodexAdapter, NativeResumeSupport, ProviderAdapter};
use omnis_ir::{CanonicalSnapshot, EventKind, Provider, SessionRef};
use serde_json::{Value, json};

const THREAD: &str = "11111111-1111-4111-8111-111111111111";
const REPLACEMENT: &str = "22222222-2222-4222-8222-222222222222";
const UNRELATED: &str = "33333333-3333-4333-8333-333333333333";
const LATEST: &str = "44444444-4444-4444-8444-444444444444";

fn header(ordinal: u64, base: Option<&Value>) -> Value {
    json!({"ordinal":ordinal,"type":"session_meta","payload":{
        "id":THREAD,"cwd":"/workspace","history_mode":"paginated","history_base":base
    }})
}

fn rollout(header: &Value, text: &str) -> String {
    let ordinal = header["ordinal"].as_u64().unwrap();
    let raw = json!({"ordinal":ordinal+1,"type":"response_item","payload":{
        "type":"message","role":"assistant","id":"answer",
        "content":[{"type":"output_text","text":text}]
    }});
    let displayed = json!({"ordinal":ordinal+2,"type":"event_msg","payload":{
        "type":"item_completed","thread_id":THREAD,"turn_id":"turn-one",
        "item":{"type":"AgentMessage","id":"answer","content":[{"type":"Text","text":text}]}
    }});
    format!("{header}\n{raw}\n{displayed}\n")
}

fn assert_answer(snapshot: &CanonicalSnapshot, text: &str) {
    let answers: Vec<_> = snapshot
        .events
        .iter()
        .filter(|event| event.kind == EventKind::MessageAssistant)
        .filter_map(|event| event.payload["text"].as_str())
        .collect();
    assert_eq!(answers, [text]);
}

fn select(home: &Path, path: &Path) -> Result<rusqlite::Connection> {
    let db = rusqlite::Connection::open(home.join("state_5.sqlite"))?;
    db.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA wal_autocheckpoint=0;
         CREATE TABLE threads(id TEXT PRIMARY KEY, rollout_path TEXT, history_mode TEXT)",
    )?;
    db.execute(
        "INSERT INTO threads VALUES(?1, ?2, 'paginated')",
        [THREAD, path.to_str().unwrap()],
    )?;
    Ok(db)
}

fn assert_listed_path(adapter: &CodexAdapter, session: &SessionRef, path: &Path) -> Result<()> {
    let listed = adapter.list_sessions(None)?;
    assert_eq!(
        listed.len(),
        1,
        "{}: {}",
        path.display(),
        adapter.discovery_notes().join(", ")
    );
    assert_eq!(&listed[0].session, session);
    assert_eq!(listed[0].source_path.as_deref(), Some(path));
    Ok(())
}

#[test]
fn legacy_sqlite_path_without_uuid_owns_the_thread_and_fails_closed() -> Result<()> {
    use std::io::Write;

    let fixture = tempfile::tempdir()?;
    let home = fixture.path().canonicalize()?;
    let day = home.join("sessions/2026/01/01");
    let archived = home.join("archived_sessions");
    fs::create_dir_all(&day)?;
    fs::create_dir(&archived)?;
    let source = day.join("rollout-imported.jsonl");
    let stale = archived.join(format!("rollout-2026-01-01T00-00-00-{THREAD}.jsonl.gz"));
    let archived_source = archived.join("rollout-imported.jsonl.gz");
    let session = SessionRef::new(Provider::Codex, THREAD);
    let legacy = |id: &str, answer: &str| {
        let header =
            json!({"type":"session_meta","payload":{"id":id,"cwd":home,"history_mode":"legacy"}});
        let message = json!({"type":"response_item","payload":{"type":"message","role":"assistant",
            "content":[{"type":"output_text","text":answer}]}});
        format!("{header}\n{message}\n")
    };
    let gzip = |text: &str| -> Result<Vec<u8>> {
        let mut writer = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        writer.write_all(text.as_bytes())?;
        Ok(writer.finish()?)
    };
    let kept = legacy(THREAD, "current selected record");
    fs::write(&source, &kept)?;
    let db = select(&home, &source)?;
    db.execute(
        "UPDATE threads SET history_mode='legacy' WHERE id=?1",
        [THREAD],
    )?;
    let point = |path: &Path| {
        db.execute(
            "UPDATE threads SET rollout_path=?1 WHERE id=?2",
            [path.to_str().unwrap(), THREAD],
        )
    };
    let fresh = || CodexAdapter::with_root(&home);
    assert_listed_path(&fresh(), &session, &source)?;
    let alias = day.join(format!("rollout-imported-{UNRELATED}.jsonl"));
    fs::rename(&source, &alias)?;
    point(&alias)?;
    let selected = fresh();
    assert_listed_path(&selected, &session, &alias)?;
    assert_answer(&selected.read_session(&session)?, "current selected record");
    assert!(selected.find_native_session(UNRELATED)?.is_none());
    fs::rename(&alias, &source)?;
    point(&source)?;

    let old = gzip(&legacy(THREAD, "discarded archived record"))?;
    fs::write(&stale, &old)?;
    let adapter = fresh();
    assert_listed_path(&adapter, &session, &source)?;
    let read = adapter.index_session(&session, Some(&stale), u64::MAX)?;
    assert_answer(&read.snapshot, "current selected record");
    assert!(
        read.source_revision
            .matches_file_contents(CodexAdapter::index_file_revision(&source).unwrap())
    );
    assert_answer(&adapter.read_session(&session)?, "current selected record");
    assert_ne!(
        adapter.native_resume_support(&session)?,
        NativeResumeSupport::ImportOnly
    );

    for bad in ["{\n".to_owned(), legacy(UNRELATED, "another thread")] {
        fs::write(&source, bad)?;
        let check = fresh();
        assert!(
            check
                .list_sessions(None)?
                .iter()
                .all(|row| row.session != session)
        );
        assert!(check.read_session(&session).is_err());
        assert!(
            check
                .index_session(&session, Some(&stale), u64::MAX)
                .is_err()
        );
    }
    fs::remove_file(&source)?;
    assert!(fresh().read_session(&session).is_err());
    let missing = day.join(format!("rollout-imported-{THREAD}.jsonl"));
    point(&missing)?;
    assert!(fresh().read_session(&session).is_err());
    fs::write(&archived_source, gzip(&kept)?)?;
    point(&archived_source)?;
    let restored = fresh();
    assert_listed_path(&restored, &session, &archived_source)?;
    assert_answer(&restored.read_session(&session)?, "current selected record");
    assert_eq!(
        restored.native_resume_support(&session)?,
        NativeResumeSupport::ImportOnly
    );
    assert_eq!(fs::read(stale)?, old);
    Ok(())
}

#[test]
fn an_unreadable_renamed_selection_only_retires_a_proven_different_file() -> Result<()> {
    check_unreadable_selection(&format!("rollout-2026-01-01T00-00-00-{THREAD}.jsonl"))?;
    check_unreadable_selection("rollout-original.jsonl")
}

fn check_unreadable_selection(old_name: &str) -> Result<()> {
    let fixture = tempfile::tempdir()?;
    let home = fixture.path().canonicalize()?;
    let logs = home.join("sessions");
    fs::create_dir(&logs)?;
    let old = logs.join(old_name);
    let selected = logs.join("rollout-imported.jsonl");
    let metadata = json!({"type":"session_meta","payload":{"id":THREAD, "history_mode":"legacy"}});
    fs::write(&old, format!("{metadata}\n"))?;
    let prior = CodexAdapter::index_file_revision(&old).unwrap();
    let db = select(&home, &selected)?;
    db.execute(
        "UPDATE threads SET history_mode='legacy' WHERE id=?1",
        [THREAD],
    )?;
    let unavailable = || -> Result<bool> {
        let adapter = CodexAdapter::with_root(&home);
        assert!(
            adapter
                .read_session(&SessionRef::new(Provider::Codex, THREAD))
                .is_err()
        );
        Ok(adapter
            .unavailable_codex_rollouts(&[(THREAD.to_owned(), Some(prior))])?
            .contains(THREAD))
    };

    // An oversized header prevents verification on every platform while metadata stays readable.
    fs::File::create(&selected)?.set_len(3 * 1024 * 1024)?;
    assert!(unavailable()?);
    fs::remove_file(&selected)?;
    fs::hard_link(&old, &selected)?;
    fs::File::create(&selected)?.set_len(3 * 1024 * 1024)?;
    assert!(!unavailable()?);
    fs::remove_file(&old)?;
    assert!(!unavailable()?);
    Ok(())
}

#[test]
fn a_dual_uuid_rollback_checks_the_title_against_the_logical_thread() -> Result<()> {
    let home = tempfile::tempdir()?;
    let path = home.path().join(format!(
        "rollout-2026-01-01T01-00-00-{THREAD}_{REPLACEMENT}.jsonl"
    ));
    let opener = json!({"ordinal":1, "type":"response_item", "payload":{
        "type":"message","id":"request","role":"user","content":[{"type":"input_text","text":"former opener"}]
    }});
    let display = json!({"ordinal":2, "type":"event_msg","payload":{
        "type":"item_completed","thread_id":THREAD,"turn_id":"turn-one",
        "item":{"type":"UserMessage","id":"request","content":[{"type":"text","text":"former opener"}]}
    }});
    let rollback = json!({"ordinal":3, "type":"event_msg","payload":{"type":"thread_rolled_back","num_turns":1}});
    fs::write(
        &path,
        format!("{}\n{opener}\n{display}\n{rollback}\n", header(0, None)),
    )?;
    let session = SessionRef::new(Provider::Codex, THREAD);
    let revision = CodexAdapter::index_file_revision(&path).unwrap();
    assert!(CodexAdapter::rollout_withdrew_search_title(
        &path,
        &session,
        revision,
        "former opener"
    )?);
    assert!(!CodexAdapter::verify_legacy_local_opener(
        &path,
        &session,
        revision,
        "former opener",
        false
    )?);
    let physical = SessionRef::new(Provider::Codex, REPLACEMENT);
    assert!(
        CodexAdapter::rollout_withdrew_search_title(&path, &physical, revision, "former opener")
            .is_err()
    );
    Ok(())
}

#[test]
fn sqlite_selected_revert_owns_the_public_thread_and_its_index_receipt() -> Result<()> {
    let fixture = tempfile::tempdir()?;
    let home = fixture.path().canonicalize()?;
    let day = home.join("sessions/2026/01/01");
    let archives = home.join("archived_sessions");
    fs::create_dir_all(&day)?;
    fs::create_dir(&archives)?;
    let original = day.join(format!("rollout-2026-01-01T00-00-00-{THREAD}.jsonl"));
    let selected = archives.join(format!(
        "rollout-2026-01-01T01-00-00-{THREAD}_{REPLACEMENT}.jsonl"
    ));
    let first = header(0, None);
    let cutoff = format!("{first}\n").len();
    let base = json!({"thread_id":THREAD,"end_ordinal_exclusive":1,"end_byte_offset":cutoff});
    let discarded = rollout(&first, "discarded answer");
    let kept = rollout(&header(1, Some(&base)), "selected answer");
    fs::write(&original, &discarded)?;
    fs::write(&selected, &kept)?;
    let db = select(&home, &selected)?;

    let adapter = CodexAdapter::with_root(&home);
    let session = SessionRef::new(Provider::Codex, THREAD);
    let listed = adapter.list_sessions(None)?;
    assert_eq!(listed.len(), 1);
    let found = adapter.find_native_session(THREAD)?.unwrap();
    let read = adapter.read_full_index_at(&session, Some(&original))?;
    let revision = omnis_adapters::source_file_revision(&selected).unwrap();
    let receipt = read.native_session.as_ref().unwrap();
    for entry in [&listed[0], &found, receipt] {
        assert_eq!(entry.session, session);
        assert_eq!(entry.source_path.as_ref(), Some(&selected));
        assert!(
            entry
                .source_revision
                .unwrap()
                .matches_file_contents(revision)
        );
    }
    assert!(read.source_revision.matches_file_contents(revision));
    assert_answer(&read.snapshot, "selected answer");
    assert_answer(&adapter.read_session(&session)?, "selected answer");
    assert_answer(&adapter.preview_session(&session)?, "selected answer");
    assert_eq!(
        adapter.native_resume_support(&session)?,
        NativeResumeSupport::ImportOnly
    );
    assert!(
        adapter
            .unavailable_codex_rollouts(&[(THREAD.to_owned(), Some(revision))])?
            .is_empty()
    );
    assert_eq!(fs::read_to_string(&selected)?, kept);

    fs::remove_file(&selected)?;
    let unrelated = day.join(format!("rollout-2026-01-01T02-00-00-{UNRELATED}.jsonl"));
    fs::write(unrelated, b"{\n")?;
    assert!(adapter.list_sessions(None)?.is_empty());
    let fresh = CodexAdapter::with_root(&home);
    assert!(fresh.list_sessions(None)?.is_empty());
    assert_eq!(
        fresh.unavailable_codex_rollouts(&[
            (THREAD.to_owned(), Some(revision)),
            (UNRELATED.to_owned(), None),
        ])?,
        HashSet::from([THREAD.to_owned()])
    );
    assert!(adapter.read_session(&session).is_err());
    assert!(fresh.preview_session(&session).is_err());
    assert!(fresh.find_native_session(THREAD).ok().flatten().is_none());
    assert!(matches!(
        fresh.native_resume_support(&session),
        Err(_) | Ok(NativeResumeSupport::ImportOnly)
    ));
    assert!(
        fresh
            .index_session(&session, Some(&original), u64::MAX)
            .is_err()
    );
    fs::write(&selected, &kept)?;
    assert_listed_path(&adapter, &session, &selected)?;
    let latest = day.join(format!(
        "rollout-2026-01-01T03-00-00-{THREAD}_{LATEST}.jsonl"
    ));
    fs::write(&latest, rollout(&first, "switched answer"))?;
    assert_listed_path(&adapter, &session, &selected)?;
    db.execute(
        "UPDATE threads SET rollout_path=?1 WHERE id=?2",
        [latest.to_str().unwrap(), THREAD],
    )?;
    assert_listed_path(&adapter, &session, &latest)?;
    assert_answer(&adapter.read_session(&session)?, "switched answer");
    let rebound = adapter.index_session(&session, Some(&selected), u64::MAX)?;
    assert_answer(&rebound.snapshot, "switched answer");
    let revision = CodexAdapter::index_file_revision(&latest).unwrap();
    assert!(rebound.source_revision.matches_file_contents(revision));
    assert_eq!(fs::read_to_string(&selected)?, kept);
    assert_eq!(fs::read_to_string(&original)?, discarded);
    Ok(())
}

#[test]
fn incomplete_restore_hides_only_a_cache_from_a_proven_different_generation() -> Result<()> {
    let fixture = tempfile::tempdir()?;
    let home = fixture.path().canonicalize()?;
    let day = home.join("sessions/2026/01/01");
    let archives = home.join("archived_sessions");
    fs::create_dir_all(&day)?;
    fs::create_dir(&archives)?;
    let session = SessionRef::new(Provider::Codex, THREAD);
    let original = day.join(format!("rollout-2026-01-01T00-00-00-{THREAD}.jsonl"));
    fs::write(&original, rollout(&header(0, None), "obsolete generation"))?;
    let db = select(&home, &original)?;
    let old_read = CodexAdapter::with_root(&home).index_session(&session, None, u64::MAX)?;
    assert_answer(&old_read.snapshot, "obsolete generation");

    let filename = format!("rollout-2026-01-01T01-00-00-{THREAD}_{REPLACEMENT}.jsonl");
    let pointer = day.join(&filename);
    let archive = archives.join(&filename);
    fs::write(&archive, rollout(&header(0, None), "current generation"))?;
    db.execute(
        "UPDATE threads SET rollout_path=?1 WHERE id=?2",
        [pointer.to_str().unwrap(), THREAD],
    )?;
    let current_read = CodexAdapter::with_root(&home).index_session(&session, None, u64::MAX)?;
    assert_answer(&current_read.snapshot, "current generation");
    assert_eq!(
        current_read.native_session.unwrap().source_path.as_deref(),
        Some(archive.as_path())
    );
    assert_ne!(
        old_read.source_revision.file,
        current_read.source_revision.file
    );

    fs::create_dir_all(home.join("sessions/nested/1/2/3/4/5/6"))?;
    let original_home = tempfile::tempdir()?;
    let old_home = original_home.path().canonicalize()?.join("sessions");
    fs::create_dir_all(&old_home)?;
    let relocated = old_home.join(&filename);
    fs::copy(&archive, &relocated)?;
    for selected in [&pointer, &relocated] {
        db.execute(
            "UPDATE threads SET rollout_path=?1 WHERE id=?2",
            [selected.to_str().unwrap(), THREAD],
        )?;
        let limited = CodexAdapter::with_root(&home);
        assert!(!limited.discovery_is_complete());
        assert!(limited.read_session(&session).is_err());
        assert_eq!(
            limited.unavailable_codex_rollouts(&[(
                THREAD.to_owned(),
                Some(old_read.source_revision)
            )])?,
            HashSet::from([THREAD.to_owned()])
        );
        assert!(
            limited
                .unavailable_codex_rollouts(&[(
                    THREAD.to_owned(),
                    Some(current_read.source_revision)
                )])?
                .is_empty()
        );
    }
    Ok(())
}
