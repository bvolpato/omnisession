use std::{
    fmt::Write as _,
    fs,
    io::{Read as _, Write as _},
    path::{Path, PathBuf},
};

use omnis_adapters::{CodexAdapter, NativeResumeSupport, ProviderAdapter};
use omnis_ir::{CanonicalSnapshot, Provider, SessionRef};
use serde_json::{Value, json};

const PARENT: &str = "11111111-1111-4111-8111-111111111111";
const CHILD: &str = "22222222-2222-4222-8222-222222222222";
const REVERT: &str = "33333333-3333-4333-8333-333333333333";

#[path = "codex_history_base/partial_inventory.rs"]
mod partial_inventory;
#[path = "codex_history_base/recovered_context.rs"]
mod recovered_context;

fn write(
    home: &Path,
    root: &str,
    thread: &str,
    file_ids: &str,
    stamp: &str,
    lines: &[Value],
) -> PathBuf {
    let directory = home.join(root);
    fs::create_dir_all(&directory).unwrap();
    let path = directory.join(format!("rollout-{stamp}-{file_ids}.jsonl"));
    assert_eq!(lines[0]["payload"]["id"], thread);
    fs::write(&path, encoded(lines)).unwrap();
    path
}

fn encoded(lines: &[Value]) -> String {
    let mut text = String::new();
    for line in lines {
        writeln!(text, "{line}").unwrap();
    }
    text
}

fn header(id: &str, ordinal: u64, base: Option<Value>) -> Value {
    json!({"type":"session_meta","ordinal":ordinal,"payload":{
        "id":id,"cwd":format!("/workspace/{id}"),"history_mode":"paginated","history_base":base.unwrap_or(Value::Null)
    }})
}

fn turn(ordinal: u64, role: &str, text: &str) -> Value {
    json!({"type":"response_item","ordinal":ordinal,"payload":{"type":"message","role":role,
        "content":[{"type":"text","text":text}]}})
}

fn bookmark(id: &str, inherited: &[Value]) -> Value {
    json!({"thread_id":id,"end_ordinal_exclusive":inherited.last().unwrap()["ordinal"].as_u64().unwrap()+1,
        "end_byte_offset":encoded(inherited).len()})
}

fn read(home: &Path, preview: bool) -> anyhow::Result<CanonicalSnapshot> {
    let adapter = CodexAdapter::with_root(home);
    let child = SessionRef::new(Provider::Codex, CHILD);
    if preview {
        adapter.preview_session(&child)
    } else {
        adapter.read_session(&child)
    }
}

#[test]
fn malformed_ancestor_directory_preserves_recoverable_fork_messages() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let day = "sessions/2026/01/01";
    let stamp = "2026-01-01T00-00-00";
    let mut parent = [header(PARENT, 0, None), turn(1, "user", "ancestor request")];
    parent[0]["payload"]["cwd"] = Value::Null;
    write(home.path(), day, PARENT, PARENT, stamp, &parent);
    let leaf = [
        header(CHILD, 2, Some(bookmark(PARENT, &parent))),
        turn(3, "user", "child request"),
    ];
    write(home.path(), day, CHILD, CHILD, stamp, &leaf);

    let adapter = CodexAdapter::with_root(home.path());
    let session = SessionRef::new(Provider::Codex, CHILD);
    assert_eq!(
        adapter.native_resume_support(&session)?,
        NativeResumeSupport::ImportOnly
    );
    let snapshot = adapter.read_session(&session)?;
    let messages: Vec<_> = snapshot
        .events
        .iter()
        .filter_map(|event| event.payload["text"].as_str())
        .collect();
    assert_eq!(messages, ["ancestor request", "child request"]);
    Ok(())
}

#[test]
fn codex_forks_and_reverts_read_only_the_referenced_prefix_of_each_rollout() {
    let home = tempfile::tempdir().unwrap();
    let parent = [
        header(PARENT, 0, None),
        turn(1, "user", "inherited request"),
        turn(2, "assistant", "reply beyond fork"),
    ];
    let middle = [
        header(CHILD, 2, Some(bookmark(PARENT, &parent[..2]))),
        turn(3, "assistant", "retained fork reply"),
        turn(4, "user", "request before revert"),
    ];
    let leaf = [
        header(CHILD, 4, Some(bookmark(CHILD, &middle[..2]))),
        turn(5, "user", "request after revert"),
    ];
    let leaf_path = write(
        home.path(),
        "sessions/2026/01/02",
        CHILD,
        &format!("{CHILD}_{REVERT}"),
        "2026-01-02T00-00-00",
        &leaf,
    );
    // Write the retired rollout last to give it the latest filesystem modification time.
    let retired = write(
        home.path(),
        "sessions/2026/01/01",
        CHILD,
        CHILD,
        "2026-01-01T00-00-00",
        &middle,
    );
    let parent_path = write(
        home.path(),
        "archived_sessions",
        PARENT,
        PARENT,
        "2025-12-01T00-00-00",
        &parent,
    );

    let listed = CodexAdapter::with_root(home.path())
        .list_sessions(None)
        .unwrap();
    assert_eq!(
        listed
            .iter()
            .filter(|entry| entry.session.id == CHILD)
            .count(),
        1
    );
    assert_eq!(
        listed
            .iter()
            .find(|entry| entry.session.id == CHILD)
            .unwrap()
            .source_path
            .as_ref(),
        Some(&leaf_path)
    );
    let bytes = fs::read(&parent_path).unwrap();
    let zst = parent_path.with_extension("jsonl.zst");
    fs::write(&zst, zstd::stream::encode_all(bytes.as_slice(), 1).unwrap()).unwrap();
    fs::remove_file(&parent_path).unwrap();
    let gzip = parent_path.with_extension("jsonl.gz");
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder.write_all(&bytes).unwrap();
    let compressed = encoder.finish().unwrap();
    for gzip_only in [false, true] {
        if gzip_only {
            fs::write(&gzip, &compressed).unwrap();
            fs::remove_file(&zst).unwrap();
        }
        let parent_source = if gzip_only {
            ParentSource::ImportOnly
        } else {
            ParentSource::Native
        };
        assert_active_history(home.path(), &leaf_path, &retired, parent_source);
        if !gzip_only {
            let shadow = home
                .path()
                .join("sessions/2025/12/01")
                .join(parent_path.file_name().unwrap());
            fs::create_dir_all(shadow.parent().unwrap()).unwrap();
            fs::write(&shadow, b"{\n").unwrap();
            assert_active_history(home.path(), &leaf_path, &retired, ParentSource::Unavailable);
            fs::remove_file(shadow).unwrap();
            let backup = zst.with_file_name(format!("backup-{PARENT}.jsonl.zst"));
            fs::rename(&zst, &backup).unwrap();
            assert_active_history(home.path(), &leaf_path, &retired, ParentSource::ImportOnly);
            fs::rename(backup, &zst).unwrap();
        }
    }
    assert_eq!(fs::read(gzip).unwrap(), compressed);
}

#[derive(Clone, Copy)]
enum ParentSource {
    Native,
    ImportOnly,
    Unavailable,
}

fn assert_active_history(home: &Path, leaf: &Path, retired: &Path, parent: ParentSource) {
    let child = SessionRef::new(Provider::Codex, CHILD);
    let adapter = CodexAdapter::with_root(home);
    assert_eq!(
        adapter.native_resume_support(&child).unwrap(),
        match parent {
            ParentSource::Native => NativeResumeSupport::Since("0.167.0"),
            ParentSource::ImportOnly | ParentSource::Unavailable => NativeResumeSupport::ImportOnly,
        }
    );
    // This hint predates the revert and still points to a valid retired rollout.
    let full = adapter
        .index_session(&child, Some(retired), u64::MAX)
        .unwrap();
    assert!(full.source_complete);
    let sampled = adapter
        .index_session(&child, None, fs::metadata(leaf).unwrap().len())
        .unwrap();
    assert!(
        sampled.source_complete,
        "all available inherited records were read to reconstruct the branch"
    );
    for snapshot in [
        read(home, false).unwrap(),
        read(home, true).unwrap(),
        full.snapshot,
        sampled.snapshot,
    ] {
        let text: Vec<_> = snapshot
            .events
            .iter()
            .filter_map(|event| event.payload["text"].as_str())
            .collect();
        let expected = [
            "inherited request",
            "retained fork reply",
            "request after revert",
        ];
        let missing = matches!(parent, ParentSource::Unavailable);
        assert_eq!(text, &expected[usize::from(missing)..]);
        assert_eq!(
            snapshot.events.iter().any(|event| {
                event.source.raw_record_type.as_deref() == Some("omnisession.codex_history_base")
                    && event.payload["native_history_unverified"] == true
            }),
            missing
        );
        assert_eq!(
            snapshot.workspace.root,
            PathBuf::from(format!("/workspace/{CHILD}"))
        );
    }
}

#[test]
fn a_full_read_restores_messages_omitted_from_a_reference_preview() {
    let home = tempfile::tempdir().unwrap();
    let mut parent = vec![header(PARENT, 0, None)];
    parent.extend((1..=1_100).map(|n| turn(n, "user", &format!("request {n}"))));
    write(
        home.path(),
        "sessions",
        PARENT,
        PARENT,
        "2026-01-01T00-00-00",
        &parent,
    );
    write(
        home.path(),
        "sessions",
        CHILD,
        CHILD,
        "2026-01-02T00-00-00",
        &[header(CHILD, 1_101, Some(bookmark(PARENT, &parent)))],
    );
    let adapter = CodexAdapter::with_root(home.path());
    let child = SessionRef::new(Provider::Codex, CHILD);
    for (budget, expected_full) in [(1, false), (u64::MAX, true)] {
        let read = adapter.index_session(&child, None, budget).unwrap();
        assert_eq!(read.source_complete, expected_full);
        assert_eq!(read.source_revision.missing_ancestor, None);
        assert_eq!(
            read.snapshot
                .events
                .iter()
                .any(|ev| ev.payload["text"] == "request 550"),
            expected_full
        );
    }
}

#[test]
fn a_large_inherited_prefix_keeps_the_surviving_edges_within_the_preview_byte_limit() {
    let temp = tempfile::tempdir().unwrap();
    let mut parent = vec![
        header(PARENT, 0, None),
        turn(1, "user", "first surviving request"),
    ];
    for n in 2..=162 {
        parent.push(turn(
            n,
            "user",
            &format!("turn {n}: {}", "x".repeat(64 * 1024)),
        ));
    }
    parent.push(json!({"type":"turn_context","ordinal":163,"payload":{"cwd":"/inherited/last"}}));
    parent.push(turn(164, "user", "last inherited request"));
    parent.push(json!({"type":"event_msg","ordinal":165,"payload":{"type":"turn_started"}}));
    parent.push(json!({"type":"turn_context","ordinal":166,"payload":{"cwd":"/discarded"}}));
    parent.push(turn(167, "user", "discarded inherited turn"));
    let mark = bookmark(PARENT, &parent);
    parent.push(turn(168, "assistant", "after cutoff"));
    let archive = write(
        temp.path(),
        "archived_sessions",
        PARENT,
        PARENT,
        "2026-01-01T00-00-00",
        &parent,
    );
    let mut gzip = flate2::write::GzEncoder::new(
        fs::File::create(archive.with_extension("jsonl.gz")).unwrap(),
        flate2::Compression::fast(),
    );
    gzip.write_all(&fs::read(&archive).unwrap()).unwrap();
    gzip.finish().unwrap();
    fs::remove_file(archive).unwrap();
    let child = [
        header(CHILD, 168, Some(mark)),
        turn(169, "user", "discarded local turn"),
        json!({"type":"event_msg","ordinal":170,"payload":{"type":"thread_rolled_back","num_turns":2}}),
        turn(171, "assistant", "surviving leaf tail"),
    ];
    write(
        temp.path(),
        "sessions",
        CHILD,
        CHILD,
        "2026-01-02T00-00-00",
        &child,
    );
    let adapter = CodexAdapter::with_root(temp.path());
    let session = SessionRef::new(Provider::Codex, CHILD);
    let read = adapter.index_session(&session, None, 128 * 1024).unwrap();
    assert!(!read.source_complete);
    let messages: Vec<_> = read
        .snapshot
        .events
        .iter()
        .filter_map(|e| e.payload["text"].as_str())
        .collect();
    assert_eq!(messages.first(), Some(&"first surviving request"));
    assert!(omnis_core::session_message_prefix_is_known(
        &read.snapshot,
        read.source_complete
    ));
    assert!(messages.ends_with(&["last inherited request", "surviving leaf tail"]));
    assert!(!messages.contains(&"discarded inherited turn"));
    assert!(!messages.contains(&"discarded local turn"));
    assert!(!messages.contains(&"after cutoff"));
    assert!(messages.iter().map(|m| m.len()).sum::<usize>() <= 8 * 1024 * 1024);
    assert_eq!(read.snapshot.workspace.root, Path::new("/inherited/last"));
}

#[test]
fn damaged_ancestor_preserves_only_the_newer_verified_segments() -> anyhow::Result<()> {
    const GRAND: &str = "44444444-4444-4444-8444-444444444444";
    let home = tempfile::tempdir()?;
    let day = "sessions/2026/01/01";
    let stamp = "2026-01-01T00-00-00";
    let grand = [
        header(GRAND, 0, None),
        turn(1, "user", "grandparent request"),
    ];
    let older = [
        header(PARENT, 2, Some(bookmark(GRAND, &grand))),
        turn(3, "user", "parent request before damage"),
        turn(4, "assistant", "parent reply at damage"),
    ];
    let middle = [
        header(REVERT, 5, Some(bookmark(PARENT, &older))),
        turn(6, "user", "verified intermediate request"),
        json!({"type":"turn_context","ordinal":7,"payload":{"cwd":"/workspace/intermediate"}}),
    ];
    let leaf = [
        header(CHILD, 8, Some(bookmark(REVERT, &middle))),
        turn(9, "user", "first leaf request"),
        turn(10, "assistant", "first leaf answer"),
        turn(11, "user", "second leaf request"),
    ];
    let paths = [
        write(home.path(), day, GRAND, GRAND, stamp, &grand),
        write(home.path(), day, PARENT, PARENT, stamp, &older),
        write(home.path(), day, REVERT, REVERT, stamp, &middle),
        write(home.path(), day, CHILD, CHILD, stamp, &leaf),
    ];
    let text = |snapshot: &CanonicalSnapshot| -> Vec<String> {
        snapshot
            .events
            .iter()
            .filter_map(|e| e.payload["text"].as_str().map(str::to_owned))
            .collect()
    };
    let intact = read(home.path(), false)?;
    assert_eq!(
        text(&intact),
        [
            "grandparent request",
            "parent request before damage",
            "parent reply at damage",
            "verified intermediate request",
            "first leaf request",
            "first leaf answer",
            "second leaf request"
        ]
    );
    let original = encoded(&older).into_bytes();
    let last_row = encoded(&older[..2]).len();
    let mut malformed = original.clone();
    malformed[last_row] = b'!';
    let mut joined_rows = original.clone();
    joined_rows[last_row - 1] = b' ';
    let mut split_row = original.clone();
    let ordinal_key = "\"ordinal\"";
    let ordinal = encoded(&older[2..]).find(ordinal_key).unwrap() + last_row + ordinal_key.len();
    split_row[ordinal] = b'\n';
    split_row[ordinal + 2] = b'\n';
    let retained = [
        "verified intermediate request",
        "first leaf request",
        "first leaf answer",
        "second leaf request",
    ];
    for (damaged, damage_middle) in [
        (&malformed[..], false),
        (&joined_rows[..], false),
        (&split_row[..], false),
        (&original[..last_row], false),
        (&malformed[..], true),
    ] {
        fs::write(&paths[1], damaged)?;
        let expected = if damage_middle {
            let mut bytes = encoded(&middle).into_bytes();
            bytes[encoded(&middle[..2]).len()] = b'!';
            fs::write(&paths[2], bytes)?;
            &retained[1..]
        } else {
            &retained[..]
        };
        assert_partial_messages(home.path(), &paths, expected)?;
        assert_eq!(
            CodexAdapter::with_root(home.path())
                .native_resume_support(&SessionRef::new(Provider::Codex, CHILD))?,
            NativeResumeSupport::ImportOnly
        );
    }
    Ok(())
}

#[test]
fn a_damaged_active_prefix_uses_an_agreeing_complete_archive() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let date = "2026-01-01T00-00-00";
    let day = "sessions/2026/01/01";
    let parent = [
        header(PARENT, 0, None),
        turn(1, "user", "parent one"),
        turn(2, "assistant", "parent two"),
    ];
    let original = write(home.path(), day, PARENT, PARENT, date, &parent);
    let leaf = SessionRef::new(Provider::Codex, CHILD);
    write(
        home.path(),
        day,
        CHILD,
        CHILD,
        date,
        &[
            header(CHILD, 3, Some(bookmark(PARENT, &parent))),
            turn(4, "user", "child"),
        ],
    );
    let adapter = CodexAdapter::with_root(home.path());
    let check = |complete: bool| -> anyhow::Result<()> {
        for snapshot in [
            adapter.read_session(&leaf)?,
            adapter.preview_session(&leaf)?,
            adapter.index_session(&leaf, None, 1)?.snapshot,
        ] {
            let conversation = omnis_core::import_conversation(&snapshot);
            assert_eq!(conversation.truncated, !complete);
            let texts: Vec<_> = conversation
                .messages
                .iter()
                .map(|m| m.text.as_str())
                .collect();
            if complete {
                assert_eq!(texts, ["parent one", "parent two", "child"]);
            } else {
                assert_eq!(texts, ["child"]);
            }
        }
        Ok(())
    };
    let backup = write(
        home.path(),
        "archived_sessions",
        PARENT,
        PARENT,
        date,
        &parent,
    );
    let damaged = original.with_extension("jsonl.zst");
    let mut bytes = zstd::bulk::compress(encoded(&parent[..2]).as_bytes(), 1)?;
    bytes.extend_from_slice(b"invalid frame inside the parent bookmark");
    fs::write(&damaged, &bytes)?;
    fs::remove_file(&original)?;
    check(true)?;
    let conflicting_prefix = encoded(&parent).replace("parent one", "shadow one");
    fs::write(&backup, conflicting_prefix)?;
    check(false)?;
    fs::write(&backup, encoded(&parent))?;
    assert_eq!(
        adapter.native_resume_support(&leaf)?,
        NativeResumeSupport::ImportOnly
    );
    // Two copies can each fit the bookmark but disagree about the conversation.
    let other = encoded(&parent).replace("parent two", "other text");
    let second = backup.with_extension("jsonl.gz");
    let mut stream =
        flate2::write::GzEncoder::new(fs::File::create(&second)?, flate2::Compression::fast());
    stream.write_all(other.as_bytes())?;
    drop(stream.finish()?);
    check(false)?;
    fs::remove_file(&second)?;
    check(true)?;
    assert_eq!(fs::read(&damaged)?, bytes);
    assert_eq!(fs::read_to_string(&backup)?, encoded(&parent));
    Ok(())
}

fn check_parent_copy(
    adapter: &CodexAdapter,
    child: &SessionRef,
    leaf: &Path,
    expected: &[&str],
    partial: bool,
) -> anyhow::Result<()> {
    for snapshot in [
        adapter.read_session(child)?,
        adapter.preview_session(child)?,
        adapter.index_session(child, Some(leaf), 1)?.snapshot,
    ] {
        let conversation = omnis_core::import_conversation(&snapshot);
        assert_eq!(conversation.truncated, partial);
        assert_eq!(CodexAdapter::has_unavailable_ancestry(&snapshot), partial);
        assert_eq!(
            conversation
                .messages
                .iter()
                .map(|message| message.text.as_str())
                .collect::<Vec<_>>(),
            expected
        );
    }
    assert_eq!(
        adapter.native_resume_support(child)?,
        NativeResumeSupport::ImportOnly
    );
    Ok(())
}

#[test]
fn a_damaged_parent_header_requires_an_archive_matching_its_messages() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let day = "sessions/2026/01/01";
    let stamp = "2026-01-01T00-00-00";
    let parent = [header(PARENT, 0, None), turn(1, "user", "source request")];
    let leaf_rows = [
        header(CHILD, 2, Some(bookmark(PARENT, &parent))),
        turn(3, "user", "child request"),
    ];
    let leaf = write(home.path(), day, CHILD, CHILD, stamp, &leaf_rows);
    let active = write(home.path(), day, PARENT, PARENT, stamp, &parent);
    let archive = write(
        home.path(),
        "archived_sessions",
        PARENT,
        PARENT,
        stamp,
        &parent,
    );
    let adapter = CodexAdapter::with_root(home.path());
    let child = SessionRef::new(Provider::Codex, CHILD);
    let check =
        |expected: &[&str], partial| check_parent_copy(&adapter, &child, &leaf, expected, partial);
    let intact = encoded(&parent);
    let renamed_id = intact.replacen(r#""id":"#, r#""xd":"#, 1);
    for damaged in [
        renamed_id.clone(),
        intact.replacen("history_mode", "history_mnde", 1),
        intact.replacen(r#""type":"session_meta""#, r#""tyqe":"session_meta""#, 1),
        intact.replacen("session_meta", "session_meua", 1),
        intact.replacen(r#""payload":{"#, r#""qayload":{"#, 1),
    ] {
        fs::write(&active, &damaged)?;
        fs::write(&archive, intact.replace("source request", "forged request"))?;
        check(&["child request"], true)?;
        fs::write(&archive, &intact)?;
        check(&["source request", "child request"], false)?;
        assert_eq!(fs::read_to_string(&active)?, damaged);
    }
    let mut ambiguous = parent.clone();
    let meta = ambiguous[0].as_object_mut().unwrap();
    let payload = meta.remove("payload").unwrap();
    meta.insert("first".into(), payload.clone());
    meta.insert("second".into(), payload);
    fs::write(&active, encoded(&ambiguous))?;
    check(&["child request"], true)?;

    fs::remove_file(&archive)?;
    fs::write(&active, &renamed_id)?;
    check(&["child request"], true)?;
    fs::write(&archive, intact.replacen("/workspace/", "/different/", 1))?;
    check(&["child request"], true)?;
    fs::remove_file(&archive)?;
    let mut missing_id = parent.clone();
    missing_id[0]["payload"]
        .as_object_mut()
        .unwrap()
        .remove("id");
    fs::write(&active, encoded(&missing_id))?;
    check(&["child request"], true)?;
    fs::write(&archive, intact.replace("source request", "forged request"))?;
    check(&["child request"], true)?;

    fs::write(&archive, &intact)?;
    check(&["child request"], true)?;
    let owner = format!(r#""id":"{PARENT}""#);
    let foreign = format!(r#""id":"{REVERT}""#);
    let orphan = intact.replacen(&owner, &format!(r#""xd":"{REVERT}""#), 1);
    fs::write(&active, &orphan)?;
    check(&["child request"], true)?;
    let mut uncertain = parent.clone();
    let envelope = uncertain[0].as_object_mut().unwrap();
    envelope.remove("type");
    envelope.remove("ordinal");
    fs::write(&active, encoded(&uncertain))?;
    check(&["child request"], true)?;
    uncertain[0] = json!({});
    fs::write(&active, encoded(&uncertain))?;
    check(&["child request"], true)?;
    uncertain[0] = turn(0, "user", "ordinary body record");
    fs::write(&active, encoded(&uncertain))?;
    check(&["source request", "child request"], false)?;
    fs::write(
        &active,
        intact
            .replacen(&owner, &foreign, 1)
            .replace("source request", "secret request"),
    )?;
    check(&["source request", "child request"], false)?;
    assert_eq!(fs::read_to_string(&leaf)?, encoded(&leaf_rows));
    Ok(())
}

#[test]
fn a_missing_parent_header_ordinal_requires_an_archive_matching_the_readable_rows()
-> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let (root, day) = (home.path(), "sessions/2026/01/01");
    let stamp = "2026-01-01T00-00-00";
    let grand = [header(REVERT, 0, None), turn(1, "user", "grandparent")];
    let mut parent = [
        header(PARENT, 2, Some(bookmark(REVERT, &grand))),
        turn(3, "user", "parent one"),
        turn(4, "assistant", "parent two"),
    ];
    parent[0]["writer_version"] = json!("0.167.1");
    let child = [
        header(CHILD, 5, Some(bookmark(PARENT, &parent))),
        turn(6, "user", "child"),
    ];
    write(root, day, REVERT, REVERT, stamp, &grand);
    let selected = write(root, day, PARENT, PARENT, stamp, &parent);
    let leaf = write(root, day, CHILD, CHILD, stamp, &child);
    let damaged = encoded(&parent).replacen("\"ordinal\"", "\"ordhnal\"", 1);
    fs::write(&selected, &damaged)?;
    let session = SessionRef::new(Provider::Codex, CHILD);
    let check = |expected: &[&str], partial: bool| -> anyhow::Result<()> {
        let adapter = CodexAdapter::with_root(root);
        for snapshot in [
            adapter.read_session(&session)?,
            adapter.preview_session(&session)?,
            adapter.index_session(&session, Some(&leaf), 1)?.snapshot,
        ] {
            let history = omnis_core::import_conversation(&snapshot);
            assert_eq!(history.truncated, partial);
            assert_eq!(
                history
                    .messages
                    .iter()
                    .map(|m| m.text.as_str())
                    .collect::<Vec<_>>(),
                expected
            );
        }
        assert_eq!(
            adapter.native_resume_support(&session)?,
            NativeResumeSupport::ImportOnly
        );
        Ok(())
    };
    check(&["child"], true)?;
    let archive = write(root, "archived_sessions", PARENT, PARENT, stamp, &parent);
    check(&["grandparent", "parent one", "parent two", "child"], false)?;
    fs::write(
        &archive,
        encoded(&parent).replace("parent two", "wrong text"),
    )?;
    check(&["child"], true)?;
    let mut other_header = parent.clone();
    other_header[0]["payload"]["cwd"] = json!(format!("/workspace/{REVERT}"));
    fs::write(&archive, encoded(&other_header))?;
    check(&["child"], true)?;
    let mut other_header = parent.clone();
    other_header[0]["writer_version"] = json!("0.166.0");
    fs::write(&archive, encoded(&other_header))?;
    check(&["child"], true)?;
    assert_eq!(fs::read_to_string(&selected)?, damaged);
    assert_eq!(fs::read_to_string(&leaf)?, encoded(&child));

    fs::write(&archive, encoded(&parent))?;
    fs::write(&selected, damaged.replace("\"ordinal\":4", "\"ordinal\":8"))?;
    let err = read(root, false).unwrap_err();
    assert!(err.to_string().contains("ordinal"), "{err:#}");

    let mut wrong = parent.clone();
    wrong[0]["ordinal"] = json!(9);
    fs::write(&selected, encoded(&wrong))?;
    let err = read(root, false).unwrap_err();
    assert!(
        err.to_string()
            .contains("cutoff precedes the source header"),
        "{err:#}"
    );
    Ok(())
}

fn check_parent_messages(home: &Path, expected: &[&str], incomplete: bool) -> anyhow::Result<()> {
    let adapter = CodexAdapter::with_root(home);
    let session = SessionRef::new(Provider::Codex, CHILD);
    for snapshot in [
        adapter.read_session(&session)?,
        adapter.index_session(&session, None, 1)?.snapshot,
    ] {
        let conversation = omnis_core::import_conversation(&snapshot);
        assert_eq!(conversation.truncated, incomplete);
        assert_eq!(
            conversation
                .messages
                .iter()
                .map(|m| m.text.as_str())
                .collect::<Vec<_>>(),
            expected
        );
        assert_eq!(
            snapshot.events.iter().any(|e| {
                e.source.raw_record_type.as_deref() == Some("omnisession.codex_history_base")
                    && e.payload["omitted_events"].as_u64().is_some_and(|n| n > 0)
            }),
            incomplete
        );
    }
    Ok(())
}

#[test]
fn serde_history_modes_and_tuple_bookmarks_select_the_native_lineage() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let (root, day, stamp) = (home.path(), "sessions/2026/01/01", "2026-01-01T00-00-00");
    let mut parent = [header(PARENT, 0, None), turn(1, "user", "parent")];
    parent[0]["payload"]["history_mode"] = json!({"paginated":null});
    write(root, day, PARENT, PARENT, stamp, &parent);
    let end = encoded(&parent).len();
    let mut child = [
        header(CHILD, 2, Some(json!([PARENT, 2, end]))),
        turn(3, "user", "child"),
    ];
    child[0]["payload"]["history_mode"] = json!({"paginated":null});
    let path = write(root, day, CHILD, CHILD, stamp, &child);
    check_parent_messages(root, &["parent", "child"], false)?;
    child[0]["payload"]["history_mode"] = json!({"legacy":null});
    fs::write(&path, encoded(&child))?;
    check_parent_messages(root, &["child"], false)?;
    child[0]["payload"]["history_mode"] = json!({"paginated":null});
    child[0]["payload"]["history_base"] = json!([PARENT, 2]);
    fs::write(&path, encoded(&child))?;
    check_parent_messages(root, &["child"], true)?;
    Ok(())
}

#[test]
fn a_missing_data_ordinal_requires_agreement_with_the_rows_in_the_parent() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let (root, day, stamp) = (home.path(), "sessions/2026/01/01", "2026-01-01T00-00-00");
    let parent = [
        header(PARENT, 0, None),
        turn(1, "user", "source ask"),
        turn(2, "assistant", "source answer"),
    ];
    let child = [
        header(CHILD, 3, Some(bookmark(PARENT, &parent))),
        turn(4, "user", "child"),
    ];
    let preferred = write(root, day, PARENT, PARENT, stamp, &parent);
    let leaf = write(root, day, CHILD, CHILD, stamp, &child);
    let original = encoded(&parent);
    let key = "\"ordinal\":1,";
    let absent = original.replace(key, &" ".repeat(key.len()));
    let renamed = original.replace(key, "\"ordhnal\":1,");
    fs::write(&preferred, &absent)?;
    check_parent_messages(root, &["child"], true)?;
    let archive = write(root, "archived_sessions", PARENT, PARENT, stamp, &parent);
    for damaged in [&absent, &renamed] {
        fs::write(&archive, &original)?;
        fs::write(&preferred, damaged)?;
        check_parent_messages(root, &["source ask", "source answer", "child"], false)?;
        fs::write(&archive, original.replace("source ask", "shadow ask"))?;
        check_parent_messages(root, &["child"], true)?;
        assert_eq!(fs::read_to_string(&preferred)?, *damaged);
    }
    fs::write(&archive, &original)?;
    fs::write(
        &preferred,
        renamed.replace("\"ordhnal\":1", "\"ordhnal\":9"),
    )?;
    check_parent_messages(root, &["child"], true)?;
    fs::write(&preferred, original.replace(key, "\"ordinal\":9,"))?;
    assert!(
        read(root, false)
            .unwrap_err()
            .to_string()
            .contains("ordinal")
    );
    assert_eq!(fs::read_to_string(&leaf)?, encoded(&child));
    Ok(())
}

#[test]
fn a_missing_subagent_ordinal_can_cover_reserved_positions() -> anyhow::Result<()> {
    for has_next in [false, true] {
        let home = tempfile::tempdir()?;
        let (root, day, stamp) = (home.path(), "sessions/2026/01/01", "2026-01-01T00-00-00");
        let mut meta = header(PARENT, 0, None);
        meta["payload"]["subagent_history_start_ordinal"] = 4.into();
        let mut parent = vec![
            meta,
            turn(1, "user", "copied ask"),
            turn(2, "assistant", "copied answer"),
            turn(4, "user", "local ask"),
        ];
        let mut expected = vec!["local ask"];
        if has_next {
            parent.push(turn(5, "assistant", "local reply"));
            expected.push("local reply");
        }
        expected.push("child");
        let end = if has_next { 6 } else { 5 };
        let mut child = [
            header(CHILD, end, Some(bookmark(PARENT, &parent))),
            turn(end + 1, "user", "child"),
        ];
        let preferred = write(root, day, PARENT, PARENT, stamp, &parent);
        let archive = write(root, "archived_sessions", PARENT, PARENT, stamp, &parent);
        let leaf = write(root, day, CHILD, CHILD, stamp, &child);
        let original = encoded(&parent);
        let key = "\"ordinal\":4,";
        let missing = original.replace(key, &" ".repeat(key.len()));
        fs::write(&preferred, &missing)?;
        check_parent_messages(root, &expected, false)?;
        fs::write(&archive, original.replace("local ask", "other ask"))?;
        check_parent_messages(root, &["child"], true)?;
        fs::remove_file(archive)?;
        check_parent_messages(root, &["child"], true)?;

        // Even the largest legal reservation cannot reach this cutoff.
        child[0]["payload"]["history_base"]["end_ordinal_exclusive"] = (end + 1).into();
        child[0]["ordinal"] = (end + 1).into();
        child[1]["ordinal"] = (end + 2).into();
        fs::write(leaf, encoded(&child))?;
        let error = read(root, false).unwrap_err();
        assert!(error.to_string().contains("cutoffs disagree"), "{error:#}");
        assert_eq!(fs::read_to_string(preferred)?, missing);
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn an_unreadable_parent_file_cannot_authorize_an_archive_with_different_content()
-> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    let home = tempfile::tempdir()?;
    let (root, day, stamp) = (home.path(), "sessions/2026/01/01", "2026-01-01T00-00-00");
    let parent = [header(PARENT, 0, None), turn(1, "user", "source ask")];
    let child = [
        header(CHILD, 2, Some(bookmark(PARENT, &parent))),
        turn(3, "user", "child"),
    ];
    let preferred = write(root, day, PARENT, PARENT, stamp, &parent);
    let leaf = write(root, day, CHILD, CHILD, stamp, &child);
    let archive = write(root, "archived_sessions", PARENT, PARENT, stamp, &parent);
    let contaminated = encoded(&parent).replace("source ask", "shadow ask");
    fs::write(&archive, &contaminated)?;
    let permissions = fs::metadata(&preferred)?.permissions();
    fs::set_permissions(&preferred, fs::Permissions::from_mode(0o000))?;
    let result = match fs::File::open(&preferred) {
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            check_parent_messages(root, &["child"], true)
        }
        Ok(_) => Ok(()), // A privileged process can read a file with mode 000.
        Err(error) => Err(error.into()),
    };
    fs::set_permissions(&preferred, permissions)?;
    result?;
    check_parent_messages(root, &["source ask", "child"], false)?;
    assert_eq!(fs::read_to_string(&preferred)?, encoded(&parent));
    assert_eq!(fs::read_to_string(&leaf)?, encoded(&child));
    assert_eq!(fs::read_to_string(&archive)?, contaminated);
    Ok(())
}

fn assert_partial_messages(
    home: &Path,
    paths: &[PathBuf; 4],
    expected: &[&str],
) -> anyhow::Result<()> {
    let before = paths.each_ref().map(|path| fs::read(path).unwrap());
    let adapter = CodexAdapter::with_root(home);
    let session = SessionRef::new(Provider::Codex, CHILD);
    for snapshot in [
        adapter.read_session(&session)?,
        adapter.preview_session(&session)?,
        adapter
            .index_session(&session, Some(&paths[3]), u64::MAX)?
            .snapshot,
        adapter
            .index_session(&session, Some(&paths[3]), 1)?
            .snapshot,
    ] {
        let text: Vec<_> = snapshot
            .events
            .iter()
            .filter_map(|event| event.payload["text"].as_str())
            .collect();
        assert_eq!(text, expected);
        assert!(snapshot.events.iter().any(|event| {
            event.source.raw_record_type.as_deref() == Some("omnisession.codex_history_base")
                && event.payload["omitted_events"]
                    .as_u64()
                    .is_some_and(|n| n > 0)
                && event.payload["reason"]
                    .as_str()
                    .is_some_and(|s| !s.is_empty())
        }));
        assert!(!omnis_core::trajectory_search_document(&snapshot).source_complete);
        let transfer = omnis_core::import_conversation(&snapshot);
        assert!(transfer.truncated);
        assert_eq!(
            transfer
                .messages
                .iter()
                .map(|m| m.text.as_str())
                .collect::<Vec<_>>(),
            expected
        );
    }
    assert_eq!(paths.each_ref().map(|path| fs::read(path).unwrap()), before);
    Ok(())
}

#[test]
fn codex_keeps_the_local_delta_and_reports_a_missing_rollout() {
    let home = tempfile::tempdir().unwrap();
    let leaf = [
        header(
            CHILD,
            3,
            Some(json!({"thread_id":PARENT,"end_ordinal_exclusive":3,"end_byte_offset":123})),
        ),
        turn(4, "user", "available request"),
    ];
    write(
        home.path(),
        "sessions",
        CHILD,
        CHILD,
        "2026-01-02T00-00-00",
        &leaf,
    );
    let adapter = CodexAdapter::with_root(home.path());
    let child = SessionRef::new(Provider::Codex, CHILD);
    let read = adapter.index_session(&child, None, u64::MAX).unwrap();
    assert!(read.source_complete);
    assert!(!omnis_core::trajectory_search_document(&read.snapshot).source_complete);
    assert!(omnis_core::import_conversation(&read.snapshot).truncated);
    assert!(!omnis_core::session_message_prefix_is_known(
        &read.snapshot,
        read.source_complete
    ));
    assert_eq!(
        read.source_revision.missing_ancestor,
        Some(uuid::Uuid::parse_str(PARENT).unwrap())
    );
    assert_eq!(
        adapter.native_resume_support(&child).unwrap(),
        NativeResumeSupport::ImportOnly
    );
    let snapshot = read.snapshot;
    assert!(
        snapshot
            .events
            .iter()
            .any(|event| event.payload["text"] == "available request")
    );
    assert!(
        snapshot
            .events
            .iter()
            .any(|event| event.payload["omitted_events"] == 1)
    );
}

#[test]
fn a_trailing_oversized_ancestor_record_bounds_the_title_across_the_next_fork() -> anyhow::Result<()>
{
    let home = tempfile::tempdir()?;
    for opener_before_gap in [true, false] {
        let mut parent = vec![header(PARENT, 0, None)];
        if opener_before_gap {
            parent.push(turn(1, "user", "the original request"));
        }
        let end = parent.len() as u64;
        let archive = write(
            home.path(),
            "archived_sessions",
            PARENT,
            PARENT,
            "2026-01-01T00-00-00",
            &parent,
        );
        let mut file = fs::OpenOptions::new().append(true).open(&archive)?;
        write!(file, "{{\"ordinal\":{end},\"padding\":\"")?;
        std::io::copy(&mut std::io::repeat(b'x').take(17 * 1024 * 1024), &mut file)?;
        writeln!(file, "\"}}")?;
        let frozen = json!({"thread_id": PARENT, "end_ordinal_exclusive": end + 1, "end_byte_offset": file.metadata()?.len()});
        let middle = [
            header(REVERT, end + 1, Some(frozen)),
            turn(end + 2, "user", "later inherited request"),
        ];
        write(
            home.path(),
            "sessions",
            REVERT,
            REVERT,
            "2026-01-02T00-00-00",
            &middle,
        );
        write(
            home.path(),
            "sessions",
            CHILD,
            CHILD,
            "2026-01-03T00-00-00",
            &[
                header(CHILD, end + 3, Some(bookmark(REVERT, &middle))),
                turn(end + 4, "user", "child request"),
            ],
        );
        let adapter = CodexAdapter::with_root(home.path());
        let session = SessionRef::new(Provider::Codex, CHILD);
        for budget in [1, u64::MAX] {
            let indexed = adapter.index_session(&session, None, budget)?;
            assert_eq!(
                omnis_core::session_message_prefix_is_known(
                    &indexed.snapshot,
                    indexed.source_complete
                ),
                opener_before_gap
            );
            let first = if opener_before_gap {
                "the original request"
            } else {
                "later inherited request"
            };
            assert_eq!(
                omnis_core::session_search_title(&indexed.snapshot).as_deref(),
                Some(first)
            );
        }
    }
    Ok(())
}

#[test]
fn codex_rejects_cutoffs_that_disagree_with_the_referenced_rollout() {
    let home = tempfile::tempdir().unwrap();
    let parent = [
        header(PARENT, 0, None),
        turn(1, "user", "inherited"),
        turn(2, "assistant", "after fork"),
    ];
    let path = write(
        home.path(),
        "sessions",
        PARENT,
        PARENT,
        "2026-01-01T00-00-00",
        &parent,
    );
    let mut base = bookmark(PARENT, &parent);
    base["end_ordinal_exclusive"] = json!(2);
    write(
        home.path(),
        "sessions",
        CHILD,
        CHILD,
        "2026-01-02T00-00-00",
        &[header(CHILD, 2, Some(base))],
    );
    let error = read(home.path(), false).unwrap_err();
    assert!(error.to_string().contains("cutoffs disagree"), "{error:#}");

    // The saved byte offset includes the opening brace after the last expected ordinal.
    let mut next_byte = bookmark(PARENT, &parent[..2]);
    next_byte["end_byte_offset"] = json!(encoded(&parent[..2]).len() + 1);
    write(
        home.path(),
        "sessions",
        CHILD,
        CHILD,
        "2026-01-02T00-00-00",
        &[header(CHILD, 2, Some(next_byte))],
    );
    let error = read(home.path(), false).unwrap_err();
    assert!(error.to_string().contains("cutoffs disagree"), "{error:#}");

    let mut wrong = parent.clone();
    wrong[2]["ordinal"] = json!(200);
    let mut bytes = encoded(&wrong).into_bytes();
    bytes[encoded(&wrong[..1]).len()] = b'!';
    let mut base = bookmark(PARENT, &parent);
    base["end_byte_offset"] = json!(bytes.len());
    fs::write(path, bytes).unwrap();
    write(
        home.path(),
        "sessions",
        CHILD,
        CHILD,
        "2026-01-02T00-00-00",
        &[header(CHILD, 3, Some(base))],
    );
    let error = read(home.path(), false).unwrap_err();
    assert!(
        error.to_string().contains("source ordinal is inconsistent"),
        "{error:#}"
    );
}

#[test]
fn cyclic_rollouts_keep_the_child_once() {
    let home = tempfile::tempdir().unwrap();
    write(
        home.path(),
        "sessions",
        CHILD,
        CHILD,
        "2026-01-02T00-00-00",
        &[
            header(
                CHILD,
                2,
                Some(json!({"thread_id":CHILD, "end_ordinal_exclusive":2, "end_byte_offset":45})),
            ),
            turn(3, "user", "current request"),
        ],
    );
    let read = CodexAdapter::with_root(home.path())
        .index_session(&SessionRef::new(Provider::Codex, CHILD), None, u64::MAX)
        .unwrap();
    assert!(read.source_complete);
    assert!(read.source_revision.missing_ancestor.is_none());
    let snapshot = read.snapshot;
    assert_eq!(
        snapshot
            .events
            .iter()
            .filter(|event| event.payload["text"] == "current request")
            .count(),
        1
    );
    assert!(snapshot.events.iter().any(|event| {
        event.payload["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("cycle"))
    }));
}

#[cfg(unix)]
#[test]
fn discovery_and_an_index_hint_cannot_follow_a_replaced_storage_root() {
    let home = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let external = write(
        outside.path(),
        "other",
        CHILD,
        CHILD,
        "2026-01-01T00-00-00",
        &[
            header(CHILD, 0, None),
            turn(1, "user", "unrelated external message"),
        ],
    );
    let sessions = home.path().join("sessions");
    std::os::unix::fs::symlink(external.parent().unwrap(), &sessions).unwrap();
    let hint = sessions.join(external.file_name().unwrap());
    let adapter = CodexAdapter::with_root(home.path());
    assert!(adapter.list_sessions(None).unwrap().is_empty());
    assert!(
        adapter
            .index_session(
                &SessionRef::new(Provider::Codex, CHILD),
                Some(&hint),
                u64::MAX
            )
            .is_err()
    );
}

#[test]
fn indexing_a_legacy_duplicate_rechecks_the_selected_path() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let mut meta = header(CHILD, 0, None);
    meta["payload"]
        .as_object_mut()
        .unwrap()
        .remove("history_mode");
    let active = write(
        home.path(),
        "sessions",
        CHILD,
        CHILD,
        "2026-01-01T00-00-00",
        &[meta.clone(), turn(1, "user", "active message")],
    );
    let archived = write(
        home.path(),
        "archived_sessions",
        CHILD,
        CHILD,
        "2026-01-02T00-00-00",
        &[meta, turn(1, "user", "archived message")],
    );
    let database = rusqlite::Connection::open(home.path().join("state_5.sqlite"))?;
    database.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE threads(id TEXT PRIMARY KEY, rollout_path TEXT, history_mode TEXT)")?;
    database.execute(
        "INSERT INTO threads VALUES(?1, ?2, 'legacy')",
        [CHILD, active.to_str().unwrap()],
    )?;
    let adapter = CodexAdapter::with_root(home.path());
    let candidate = adapter.list_sessions(None)?.pop().unwrap();
    assert_eq!(candidate.source_path.as_ref(), Some(&active));
    database.execute(
        "UPDATE threads SET rollout_path=?1",
        [archived.to_str().unwrap()],
    )?;

    let read = adapter.index_session(
        &candidate.session,
        candidate.source_path.as_deref(),
        u64::MAX,
    )?;
    let text: Vec<_> = read
        .snapshot
        .events
        .iter()
        .filter_map(|event| event.payload["text"].as_str())
        .collect();
    assert_eq!(text, ["archived message"]);
    Ok(())
}

#[test]
fn unreadable_compressed_ancestor_header_preserves_the_child_and_retries_a_sibling()
-> anyhow::Result<()> {
    let parent = [
        header(PARENT, 0, None),
        turn(1, "user", "inherited message"),
    ];
    let day = "sessions/2026/01/01";
    let stamp = "2026-01-01T00-00-00";
    let child = SessionRef::new(Provider::Codex, CHILD);
    let original = SessionRef::new(Provider::Codex, PARENT);
    for (extension, prefix) in [
        ("jsonl.gz", [0x1f, 0x8b, 8, 0]),
        ("jsonl.zst", [0x28, 0xb5, 0x2f, 0xfd]),
    ] {
        for attached in [false, true] {
            let home = tempfile::tempdir()?;
            let plain = write(home.path(), day, PARENT, PARENT, stamp, &parent);
            let broken = plain.with_extension(extension);
            fs::write(&broken, prefix)?;
            fs::remove_file(&plain)?;
            let mut meta = header(CHILD, 2, Some(bookmark(PARENT, &parent)));
            meta["payload"]["forked_from_id"] = json!(PARENT);
            meta["payload"]["attached_thread"] = json!(attached);
            let rows = [meta, turn(3, "user", "local message")];
            write(home.path(), day, CHILD, CHILD, stamp, &rows);
            let adapter = CodexAdapter::with_root(home.path());
            assert!(adapter.read_session(&original).is_err());
            assert_eq!(
                adapter.native_resume_support(&child)?,
                NativeResumeSupport::ImportOnly
            );
            let read = adapter.index_session(&child, None, u64::MAX)?;
            assert_eq!(read.source_revision.missing_ancestor.is_some(), !attached);
            for snapshot in [
                read.snapshot,
                adapter.preview_session(&child)?,
                adapter.read_session(&child)?,
            ] {
                let transfer = omnis_core::import_conversation(&snapshot);
                assert_eq!(transfer.truncated, !attached);
                assert_eq!(
                    transfer
                        .messages
                        .iter()
                        .map(|m| m.text.as_str())
                        .collect::<Vec<_>>(),
                    ["local message"]
                );
                assert_eq!(
                    snapshot.events.iter().any(|event| event.payload["reason"]
                        .as_str()
                        .is_some_and(|s| s.contains("compressed rollout header"))),
                    !attached
                );
            }
            fs::write(&plain, encoded(&parent))?;
            let restored = adapter.read_session(&child)?;
            let messages = omnis_core::import_conversation(&restored);
            assert!(!messages.truncated);
            assert_eq!(
                messages
                    .messages
                    .iter()
                    .any(|msg| msg.text == "inherited message"),
                !attached
            );
            assert_eq!(
                adapter.native_resume_support(&child)?,
                NativeResumeSupport::Since("0.167.0")
            );
            assert_eq!(fs::read(&broken)?, prefix);
        }
    }
    Ok(())
}

#[test]
fn a_pre_migration_copy_or_damaged_compressed_prefix_keeps_the_readable_child() -> anyhow::Result<()>
{
    let home = tempfile::tempdir()?;
    let day = "sessions/2026/01/01";
    let stamp = "2026-01-01T00-00-00";
    let parent = [
        header(PARENT, 0, None),
        turn(1, "user", "parent before the gap"),
        turn(2, "assistant", "parent at the gap"),
    ];
    let original = write(home.path(), day, PARENT, PARENT, stamp, &parent);
    let child = SessionRef::new(Provider::Codex, CHILD);
    write(
        home.path(),
        day,
        CHILD,
        CHILD,
        stamp,
        &[
            header(CHILD, 3, Some(bookmark(PARENT, &parent))),
            turn(4, "user", "independent child"),
        ],
    );
    let adapter = CodexAdapter::with_root(home.path());
    let verify = |partial: bool| -> anyhow::Result<()> {
        for snapshot in [
            adapter.read_session(&child)?,
            adapter.preview_session(&child)?,
            adapter.index_session(&child, None, 1)?.snapshot,
        ] {
            let conversation = omnis_core::import_conversation(&snapshot);
            assert_eq!(conversation.truncated, partial);
            let messages: Vec<_> = conversation
                .messages
                .iter()
                .map(|m| m.text.as_str())
                .collect();
            if partial {
                assert_eq!(messages, ["independent child"]);
            } else {
                assert_eq!(
                    messages,
                    [
                        "parent before the gap",
                        "parent at the gap",
                        "independent child"
                    ]
                );
            }
        }
        Ok(())
    };
    verify(false)?;
    let encoded_parent = encoded(&parent);
    let compressed = original.with_extension("jsonl.zst");
    let mut corrupt = zstd::bulk::compress(encoded(&parent[..2]).as_bytes(), 1)?;
    corrupt.extend_from_slice(b"invalid-frame-within-the-fork-bookmark");
    fs::write(&compressed, &corrupt)?;
    fs::remove_file(&original)?;
    verify(true)?;
    assert_eq!(
        adapter.native_resume_support(&child)?,
        NativeResumeSupport::ImportOnly
    );
    assert_eq!(fs::read(&compressed)?, corrupt);
    fs::remove_file(&compressed)?;
    let legacy = encoded(&[
        json!({"type":"session_meta","payload":{"id":PARENT,"cwd":"/workspace"}}),
        json!({"type":"event_msg","payload":{"type":"user_message","message":"legacy-only sentinel"}}),
    ]);
    fs::write(&original, &legacy)?;
    verify(true)?;
    let ancestral_id = uuid::Uuid::try_parse(PARENT)?;
    let wanted = [ancestral_id].into_iter().collect();
    assert!(adapter.available_index_sources(&wanted).is_empty());
    assert_eq!(
        adapter.native_resume_support(&child)?,
        NativeResumeSupport::ImportOnly
    );
    fs::write(
        &compressed,
        zstd::bulk::compress(encoded_parent.as_bytes(), 1)?,
    )?;
    assert!(
        adapter
            .available_index_sources(&wanted)
            .contains(&ancestral_id)
    );
    verify(false)?;
    assert_eq!(
        adapter.native_resume_support(&child)?,
        NativeResumeSupport::ImportOnly
    );
    assert_eq!(fs::read_to_string(&original)?, legacy);
    Ok(())
}

#[test]
fn a_damaged_zstd_suffix_blocks_native_resume_without_invalidating_an_earlier_bookmark()
-> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let day = "sessions/2026/01/01";
    let stamp = "2026-01-01T00-00-00";
    let parent = [
        header(PARENT, 0, None),
        turn(1, "user", "retained request"),
        json!({"type":"event_msg","ordinal":2,"payload":{"type":"token_count","padding":"later bytes ".repeat(131_072)}}),
    ];
    let path = write(home.path(), day, PARENT, PARENT, stamp, &parent);
    let bytes = fs::read(&path)?;
    let frame = zstd::bulk::compress(&bytes, 1)?;
    assert_eq!(
        zstd::zstd_safe::get_frame_content_size(&frame[..18])
            .ok()
            .flatten(),
        Some(u64::try_from(bytes.len())?)
    );
    fs::remove_file(&path)?;
    let child = SessionRef::new(Provider::Codex, CHILD);
    for damaged in [false, true] {
        let saved = if damaged {
            &frame[..frame.len() - 6]
        } else {
            &frame
        };
        fs::write(path.with_extension("jsonl.zst"), saved)?;
        for count in [2, 3] {
            let end = u64::try_from(count)?;
            let rows = [
                header(CHILD, end, Some(bookmark(PARENT, &parent[..count]))),
                turn(end + 1, "user", "child request"),
            ];
            write(home.path(), day, CHILD, CHILD, stamp, &rows);
            let adapter = CodexAdapter::with_root(home.path());
            let support = if damaged {
                NativeResumeSupport::ImportOnly
            } else {
                NativeResumeSupport::Since("0.167.0")
            };
            assert_eq!(adapter.native_resume_support(&child)?, support);
            let read = omnis_core::import_conversation(&adapter.read_session(&child)?);
            assert_eq!(read.truncated, damaged && count == 3);
            assert_eq!(
                read.messages.iter().any(|m| m.text == "retained request"),
                !read.truncated
            );
            assert!(read.messages.iter().any(|m| m.text == "child request"));
        }
    }
    Ok(())
}

#[test]
fn an_attached_ancestor_can_end_before_a_damaged_compressed_frame() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let mut inherited = header(
        PARENT,
        2,
        Some(json!({
            "thread_id":REVERT,"end_ordinal_exclusive":2,"end_byte_offset":512
        })),
    );
    inherited["payload"]["attached_thread"] = json!(true);
    inherited["payload"]["forked_from_id"] = json!(REVERT);
    inherited["payload"]["forked_from_ordinal_exclusive"] = json!(2);
    let inherited = [inherited];
    let child = [
        header(CHILD, 3, Some(bookmark(PARENT, &inherited))),
        turn(4, "user", "local request"),
    ];
    let child = write(
        home.path(),
        "sessions",
        CHILD,
        CHILD,
        "2026-01-02T00-00-00",
        &child,
    );
    let parent = child
        .parent()
        .unwrap()
        .join(format!("rollout-2026-01-01T00-00-00-{PARENT}.jsonl.zst"));
    let mut compressed = zstd::stream::encode_all(encoded(&inherited).as_bytes(), 1)?;
    compressed.extend_from_slice(b"damaged frame after cutoff");
    fs::write(parent, compressed)?;
    for preview in [false, true] {
        let snapshot = read(home.path(), preview)?;
        assert!(
            snapshot
                .events
                .iter()
                .any(|ev| ev.payload["text"] == "local request")
        );
        assert!(!CodexAdapter::has_unavailable_ancestry(&snapshot));
    }
    Ok(())
}

fn detached_from_attached(home: &Path, older_header: bool) -> (PathBuf, Vec<Value>) {
    let day = "sessions/2026/01/01";
    let stamp = "2026-01-01T00-00-00";
    let mut base = json!({"thread_id":REVERT,"end_ordinal_exclusive":2,"end_byte_offset":128});
    if older_header {
        let fields = base.as_object_mut().unwrap();
        let offset = fields.remove("end_byte_offset").unwrap();
        fields.insert("end_byte_offsdt".into(), offset);
    }
    let mut meta = header(PARENT, 2, Some(base));
    meta["payload"]["attached_thread"] = json!(true);
    meta["payload"]["forked_from_id"] = json!(REVERT);
    let source = vec![
        meta,
        json!({"ordinal":3,"type":"event_msg","payload":{"type":"thread_settings_applied","thread_settings":{"cwd":"/fixture/settings"}}}),
        turn(4, "assistant", "interrupted parent"),
        json!({"ordinal":5,"type":"event_msg","payload":{"type":"turn_aborted","reason":"interrupted"}}),
        json!({"ordinal":6,"type":"event_msg","payload":{"type":"task_started","turn_id":"saved-child-turn"}}),
        turn(7, "user", "attached request"),
        turn(8, "assistant", "attached reply"),
    ];
    let path = write(home, day, PARENT, PARENT, stamp, &source);
    write(
        home,
        day,
        CHILD,
        CHILD,
        stamp,
        &[
            header(CHILD, 9, Some(bookmark(PARENT, &source))),
            turn(10, "user", "detached request"),
        ],
    );
    (path, source)
}

#[test]
fn a_split_hidden_settings_row_does_not_consume_two_source_ordinals() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let (path, source) = detached_from_attached(home.path(), false);
    let text = encoded(&source);
    fs::write(
        path,
        text.replacen("fixture/settings", "fixtur\n/settings", 1),
    )?;
    let expected = &["attached request", "attached reply", "detached request"];
    check_parent_messages(home.path(), expected, true)?;
    let archive = write(
        home.path(),
        "archived_sessions",
        PARENT,
        PARENT,
        "2026-01-01T00-00-00",
        &source,
    );
    fs::write(
        &archive,
        text.replace("fixture/settings", "foreign/settings"),
    )?;
    check_parent_messages(home.path(), expected, true)?;
    fs::write(&archive, text)?;
    check_parent_messages(home.path(), expected, false)
}

#[test]
fn an_older_attachment_can_verify_its_saved_start_without_an_explicit_fork_cutoff()
-> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let (path, mut source) = detached_from_attached(home.path(), true);
    check_parent_messages(
        home.path(),
        &["attached request", "attached reply", "detached request"],
        true,
    )?;
    source[0]["payload"]["history_base"] = json!([REVERT, 2, null]);
    fs::write(&path, encoded(&source))?;
    write(
        home.path(),
        "sessions/2026/01/01",
        CHILD,
        CHILD,
        "2026-01-01T00-00-00",
        &[
            header(CHILD, 9, Some(bookmark(PARENT, &source))),
            turn(10, "user", "detached request"),
        ],
    );
    check_parent_messages(
        home.path(),
        &["attached request", "attached reply", "detached request"],
        true,
    )?;
    source[0]["payload"]["history_base"][1] = json!(1);
    fs::write(path, encoded(&source))?;
    check_parent_messages(home.path(), &["detached request"], true)
}

#[test]
fn a_split_visible_request_keeps_only_later_verified_turns_without_an_agreeing_archive()
-> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let (path, mut source) = detached_from_attached(home.path(), false);
    source[5]["payload"]["content"][0]["type"] = json!("input_text");
    source.extend([
        json!({"ordinal":9,"type":"event_msg","payload":{"type":"task_started","turn_id":"final-turn","thread_id": PARENT}}),
        turn(10,"user","verified later request"),
        turn(11,"assistant","verified later reply")
    ]);
    write(
        home.path(),
        "sessions",
        CHILD,
        CHILD,
        "2026-01-01T00-00-00",
        &[
            header(CHILD, 12, Some(bookmark(PARENT, &source))),
            turn(13, "user", "detached request"),
        ],
    );
    let archive = home
        .path()
        .join("archived_sessions")
        .join(path.file_name().unwrap());
    fs::create_dir(archive.parent().unwrap())?;
    let gzip = archive.with_extension("jsonl.gz");
    let original = encoded(&source);
    for (key, split) in [
        ("input_text", "input\ntext"),
        ("\"input_text\"", "\"input_text\n"),
    ] {
        fs::write(&path, original.replacen(key, split, 1))?;
        let partial = [
            "verified later request",
            "verified later reply",
            "detached request",
        ];
        check_parent_messages(home.path(), &partial, true)?;
        for compressed in [false, true] {
            for agrees in [false, true] {
                let contents = if agrees {
                    original.clone()
                } else {
                    original.replace("attached request", "unproven request")
                };
                if compressed {
                    let mut writer = flate2::write::GzEncoder::new(
                        fs::File::create(&gzip)?,
                        flate2::Compression::fast(),
                    );
                    writer.write_all(contents.as_bytes())?;
                    writer.finish()?;
                } else {
                    fs::write(&archive, contents)?;
                }
                if agrees {
                    check_parent_messages(
                        home.path(),
                        &[
                            "attached request",
                            "attached reply",
                            "verified later request",
                            "verified later reply",
                            "detached request",
                        ],
                        false,
                    )?;
                } else {
                    check_parent_messages(home.path(), &partial, true)?;
                }
            }
            fs::remove_file(if compressed { &gzip } else { &archive })?;
        }
    }
    source[7]["payload"]["thread_id"] = json!(REVERT);
    fs::write(
        path,
        encoded(&source).replacen("input_text", "input\ntext", 1),
    )?;
    check_parent_messages(home.path(), &["detached request"], true)?;
    Ok(())
}

#[test]
fn rollback_between_damaged_rows_and_a_later_turn_removes_recovered_history() -> anyhow::Result<()>
{
    for (count, owner, retained) in [(1, PARENT, false), (0, PARENT, true), (1, REVERT, true)] {
        let home = tempfile::tempdir()?;
        let gap = json!({"type":"event_msg","payload":{"type":"token_count"}});
        let start = |ordinal| json!({"type":"event_msg","ordinal":ordinal,"payload":{"type":"task_started","turn_id":format!("turn-{ordinal}")}});
        let rows = vec![
            header(PARENT, 0, None),
            gap.clone(),
            start(2),
            turn(3, "user", "withdrawn request"),
            turn(4, "assistant", "withdrawn reply"),
            gap,
            json!({"ordinal":6,"type":"event_msg","payload":{"type":"thread_rolled_back","num_turns":count,"thread_id":owner}}),
            start(7),
            turn(8, "user", "kept"),
        ];
        let day = "sessions/2026/01/01";
        let stamp = "2026-01-01T00-00-00";
        write(home.path(), day, PARENT, PARENT, stamp, &rows);
        write(
            home.path(),
            day,
            CHILD,
            CHILD,
            stamp,
            &[
                header(CHILD, 9, Some(bookmark(PARENT, &rows))),
                turn(10, "user", "leaf"),
            ],
        );
        let expected: &[&str] = if retained {
            &["withdrawn request", "withdrawn reply", "kept", "leaf"]
        } else {
            &["kept", "leaf"]
        };
        check_parent_messages(home.path(), expected, true)?;
    }
    Ok(())
}

#[test]
fn unplaced_owned_settings_before_abort_preserve_the_child_and_detached_leaf() -> anyhow::Result<()>
{
    for key in [None, Some("ordinal"), Some("ordhnal")] {
        let home = tempfile::tempdir()?;
        let (path, mut rows) = detached_from_attached(home.path(), false);
        rows[1]["payload"]["thread_id"] = json!(PARENT);
        rows[1].as_object_mut().unwrap().remove("ordinal");
        if let Some(key) = key {
            rows[1][key] = if key == "ordinal" {
                Value::Null
            } else {
                json!(3)
            };
        }
        rows[4] = turn(6, "user", "after abort");
        fs::write(path, encoded(&rows))?;
        write(
            home.path(),
            "sessions/2026/01/01",
            CHILD,
            CHILD,
            "2026-01-01T00-00-00",
            &[
                header(CHILD, 9, Some(bookmark(PARENT, &rows))),
                turn(10, "user", "detached request"),
            ],
        );
        check_parent_messages(
            home.path(),
            &[
                "after abort",
                "attached request",
                "attached reply",
                "detached request",
            ],
            true,
        )?;
        let adapter = CodexAdapter::with_root(home.path());
        let attached = adapter.read_session(&SessionRef::new(Provider::Codex, PARENT))?;
        let conversation = omnis_core::import_conversation(&attached);
        assert_eq!(
            conversation
                .messages
                .iter()
                .map(|message| message.text.as_str())
                .collect::<Vec<_>>(),
            ["after abort", "attached request", "attached reply"]
        );
        for snapshot in [attached, read(home.path(), false)?] {
            assert_ne!(
                snapshot.workspace.current_dir,
                Path::new("/fixture/settings")
            );
        }
    }
    Ok(())
}

#[test]
fn rollback_counts_numbered_requests_after_an_unplaced_turn_start() -> anyhow::Result<()> {
    for (count, expected) in [
        (1, &["A", "A reply", "leaf"][..]),
        (2, &["leaf"][..]),
        (0, &["A", "A reply", "B", "B reply", "leaf"][..]),
    ] {
        let home = tempfile::tempdir()?;
        let start = |id| json!({"type":"event_msg","payload":{"type":"task_started","turn_id":id}});
        let mut a = start("A");
        a["ordinal"] = json!(2);
        let rows = [
            header(PARENT, 0, None),
            json!({"type":"event_msg","payload":{"type":"token_count"}}),
            a,
            turn(3, "user", "A"),
            turn(4, "assistant", "A reply"),
            start("B"),
            turn(6, "user", "B"),
            turn(7, "assistant", "B reply"),
            json!({"ordinal":8,"type":"event_msg","payload":{"type":"task_complete","turn_id":"B"}}),
            json!({"ordinal":9,"type":"event_msg","payload":{"type":"thread_rolled_back","num_turns":count}}),
        ];
        let day = "sessions/2026/01/01";
        let stamp = "2026-01-01T00-00-00";
        write(home.path(), day, PARENT, PARENT, stamp, &rows);
        write(
            home.path(),
            day,
            CHILD,
            CHILD,
            stamp,
            &[
                header(CHILD, 10, Some(bookmark(PARENT, &rows))),
                turn(11, "user", "leaf"),
            ],
        );
        check_parent_messages(home.path(), expected, true)?;
    }
    Ok(())
}

#[test]
fn anchored_control_runs_preserve_complete_turns_and_apply_withdrawals_in_order()
-> anyhow::Result<()> {
    for (count, expected) in [
        (0, &["A", "B", "C", "leaf"][..]),
        (1, &["A", "C", "leaf"][..]),
        (2, &["C", "leaf"][..]),
    ] {
        let home = tempfile::tempdir()?;
        let start = |n| json!({"ordinal":n,"type":"event_msg","payload":{"type":"task_started","thread_id":PARENT,"turn_id":format!("turn-{n}")}});
        let done = |n, t| json!({"ordinal":n,"type":"event_msg","payload":{"type":"task_complete","turn_id":format!("turn-{t}")}});
        let token = |n| json!({"ordinal":n,"type":"event_msg","payload":{"type":"token_count"}});
        let rows = [
            header(PARENT, 0, None),
            start(1),
            turn(2, "user", "A"),
            done(3, 1),
            start(4),
            token(5),
            turn(6, "user", "B"),
            done(7, 4),
            json!({"ordinal":8,"type":"event_msg","payload":{"type":"thread_rolled_back","thread_id":PARENT,"num_turns":count}}),
            token(9),
            start(10),
            turn(11, "user", "C"),
            done(12, 10),
        ];
        let parent = write(
            home.path(),
            "sessions",
            PARENT,
            PARENT,
            "2026-01-01T00-00-00",
            &rows,
        );
        write(
            home.path(),
            "sessions",
            CHILD,
            CHILD,
            "2026-01-01T00-00-00",
            &[
                header(CHILD, 13, Some(bookmark(PARENT, &rows))),
                turn(14, "user", "leaf"),
            ],
        );
        for indices in [&[4][..], &[8], &[4, 8], &[4, 5], &[8, 9], &[4, 5, 8, 9]] {
            let mut damaged = encoded(&rows);
            for index in indices {
                damaged = damaged.replace(
                    &format!("\"ordinal\":{index},"),
                    &format!("\"ordinaX\":{index},"),
                );
            }
            fs::write(&parent, damaged)?;
            check_parent_messages(home.path(), expected, true)?;
        }
        let malformed = encoded(&rows)
            .replace("\"ordinal\":2,", "!ordinal\":2,")
            .replace("\"ordinal\":4,", "\"ordinaX\":4,");
        fs::write(&parent, malformed)?;
        let later: &[&str] = if count == 0 {
            &["B", "C", "leaf"]
        } else {
            &["C", "leaf"]
        };
        check_parent_messages(home.path(), later, true)?;
    }
    Ok(())
}

#[test]
fn partial_replies_do_not_hide_numbered_requests_or_change_earlier_rollbacks() -> anyhow::Result<()>
{
    let day = "sessions";
    let stamp = "2026-01-01T00-00-00";
    for (count, owner) in [(0, PARENT), (1, PARENT), (2, PARENT), (1, REVERT)] {
        let home = tempfile::tempdir()?;
        let start = |n| json!({"ordinal":n,"type":"event_msg","payload":{"type":"task_started","turn_id":format!("t{n}")}});
        let done = |n, t| json!({"ordinal":n,"type":"event_msg","payload":{"type":"task_complete","turn_id":format!("t{t}")}});
        let rows = [
            header(PARENT, 0, None),
            start(1),
            turn(2, "user", "A"),
            turn(3, "assistant", "A reply"),
            done(4, 1),
            start(5),
            turn(6, "user", "B"),
            turn(7, "assistant", "B reply"),
            done(8, 5),
            json!({"ordinal":9,"type":"event_msg","payload":{"type":"thread_rolled_back","thread_id":owner,"num_turns":count}}),
            start(10),
            turn(11, "user", "C"),
            turn(12, "assistant", "C reply"),
            done(13, 10),
        ];
        let parent = write(home.path(), day, PARENT, PARENT, stamp, &rows);
        write(
            home.path(),
            day,
            CHILD,
            CHILD,
            stamp,
            &[
                header(CHILD, 14, Some(bookmark(PARENT, &rows))),
                turn(15, "user", "leaf"),
            ],
        );
        for gaps in [&[12][..], &[7], &[4, 7], &[7, 8, 9], &[4, 7, 8, 9, 12]] {
            let mut damaged = encoded(&rows);
            for n in gaps {
                damaged =
                    damaged.replace(&format!("\"ordinal\":{n},"), &format!("\"ordinaX\":{n},"));
            }
            fs::write(&parent, damaged)?;
            let effective = if owner == PARENT { count } else { 0 };
            let mut expected = Vec::new();
            if effective < 2 {
                expected.extend(["A", "A reply"]);
            }
            if effective == 0 {
                expected.extend(["B", "B reply"]);
            }
            expected.extend(["C", "C reply", "leaf"]);
            check_parent_messages(home.path(), &expected, true)?;
        }
    }
    Ok(())
}

#[test]
fn an_unfinished_turn_with_an_unplaced_request_still_consumes_its_rollback_slot()
-> anyhow::Result<()> {
    for leaf_rollback in [false, true] {
        for (start_owner, turn_id) in [(PARENT, "B"), (REVERT, "B"), (PARENT, "")] {
            for count in [0, 1, 2] {
                let home = tempfile::tempdir()?;
                let mut rows = vec![
                    header(PARENT, 0, None),
                    json!({"ordinal":1,"type":"event_msg","payload":{"type":"task_started","turn_id":"A"}}),
                    turn(2, "user", "A"),
                    json!({"ordinal":3,"type":"event_msg","payload":{"type":"task_complete","turn_id":"A"}}),
                    json!({"ordinal":4,"type":"event_msg","payload":{"type":"task_started","turn_id":turn_id,"thread_id":start_owner}}),
                    turn(5, "user", "unplaced B"),
                ];
                let rollback = |ordinal| json!({"ordinal":ordinal,"type":"event_msg","payload":{"type":"thread_rolled_back","num_turns":count}});
                if !leaf_rollback {
                    rows.push(rollback(6));
                }
                let parent = write(
                    home.path(),
                    "sessions",
                    PARENT,
                    PARENT,
                    "2026-01-01T00-00-00",
                    &rows,
                );
                let mut leaf = vec![header(
                    CHILD,
                    rows.len() as u64,
                    Some(bookmark(PARENT, &rows)),
                )];
                if leaf_rollback {
                    leaf.push(rollback(7));
                }
                leaf.push(turn(8, "user", "leaf"));
                write(
                    home.path(),
                    "sessions",
                    CHILD,
                    CHILD,
                    "2026-01-01T00-00-00",
                    &leaf,
                );
                let damaged = encoded(&rows).replace("\"ordinal\":5,", "\"ordinaX\":5,");
                fs::write(parent, damaged)?;
                let keep_a =
                    count == 0 || (count == 1 && start_owner == PARENT && !turn_id.is_empty());
                let expected: &[&str] = if keep_a { &["A", "leaf"] } else { &["leaf"] };
                check_parent_messages(home.path(), expected, true)?;
            }
        }
    }
    Ok(())
}

#[test]
fn an_omitted_context_or_request_keeps_numbered_evidence_and_distinct_rollback_slots()
-> anyhow::Result<()> {
    for mirrored in [false, true] {
        for count in [0, 1, 2] {
            let home = tempfile::tempdir()?;
            let start = |ordinal, id| json!({"ordinal":ordinal,"type":"event_msg","payload":{"type":"task_started","turn_id":id}});
            let mut rows = vec![
                header(PARENT, 0, None),
                start(1, "A"),
                turn(2, "user", "A"),
                json!({"ordinal":3,"type":"event_msg","payload":{"type":"task_complete","turn_id":"A"}}),
                start(4, "B"),
                json!({"ordinal":5,"type":"turn_context","payload":{"turn_id":"B","cwd":"/fixture/unplaced"}}),
                turn(6, "user", "B first"),
                turn(7, "user", "B second"),
            ];
            if mirrored {
                rows.push(json!({"ordinal":8,"type":"event_msg","payload":{"type":"user_message","message":"B second"}}));
            }
            let n = rows.len() as u64;
            rows.push(turn(n, "assistant", "numbered reply"));
            let day = "sessions";
            let stamp = "2026-01-01T00-00-00";
            let parent = write(home.path(), day, PARENT, PARENT, stamp, &rows);
            let end = n + 1;
            write(
                home.path(),
                day,
                CHILD,
                CHILD,
                stamp,
                &[
                    header(CHILD, end, Some(bookmark(PARENT, &rows))),
                    json!({"ordinal":end+1,"type":"event_msg","payload":{"type":"thread_rolled_back","num_turns":count}}),
                    turn(end + 2, "user", "leaf"),
                ],
            );
            fs::write(
                parent,
                encoded(&rows)
                    .replace("\"ordinal\":5,", "\"ordinaX\":5,")
                    .replace("\"ordinal\":7,", "\"ordinaX\":7,"),
            )?;
            let mut expected = vec!["A"];
            if count < 2 {
                expected.push("B first");
            }
            if count == 0 {
                if mirrored {
                    expected.push("B second");
                }
                expected.push("numbered reply");
            }
            expected.push("leaf");
            check_parent_messages(home.path(), &expected, true)?;
        }
    }
    Ok(())
}

#[test]
fn equal_display_text_cannot_corroborate_conflicting_native_images_or_ids() -> anyhow::Result<()> {
    let day = "sessions";
    let stamp = "2026-01-01T00-00-00";
    for (event_id, image, same) in [
        ("request", "synthetic-image-A", true),
        ("other", "synthetic-image-A", false),
        ("request", "synthetic-image-B", false),
    ] {
        let home = tempfile::tempdir()?;
        let start = |n, id| json!({"ordinal":n,"type":"event_msg","payload":{"type":"task_started","turn_id":id}});
        for completed in [false, true] {
            let mut rows = vec![
                header(PARENT, 0, None),
                start(1, "A"),
                turn(2, "user", "A"),
                start(3, "B"),
                json!({"ordinal":4,"type":"response_item","payload":{"type":"message","role":"user","id":"request","content":[{"type":"input_text","text":"image request"},{"type":"input_image","image_url":"synthetic-image-A"}]}}),
                json!({"ordinal":5,"type":"event_msg","payload":{"type":"user_message","message":"image request","item_id":event_id,"images":[image]}}),
                turn(6, "assistant", "reply"),
            ];
            if completed {
                rows.push(json!({"ordinal":7,"type":"event_msg","payload":{"type":"task_complete","turn_id":"B"}}));
            }
            let end = rows.len() as u64;
            for damaged in [false, true] {
                let parent = write(home.path(), day, PARENT, PARENT, stamp, &rows);
                if damaged {
                    fs::write(
                        parent,
                        encoded(&rows).replace("\"ordinal\":4,", "\"ordinaX\":4,"),
                    )?;
                }
                for count in [0, 1, 2] {
                    write(
                        home.path(),
                        day,
                        CHILD,
                        CHILD,
                        stamp,
                        &[
                            header(CHILD, end, Some(bookmark(PARENT, &rows))),
                            json!({"ordinal":end+1,"type":"event_msg","payload":{"type":"thread_rolled_back","num_turns":count}}),
                            turn(end + 2, "user", "leaf"),
                        ],
                    );
                    let first_visible = !same && (!damaged || completed);
                    let mut expected = Vec::new();
                    if (!same && (!completed || damaged)) || count < 2 {
                        expected.push("A");
                    }
                    if first_visible && (count == 0 || ((!completed || damaged) && count < 2)) {
                        expected.push("image request");
                    }
                    if count == 0 {
                        expected.extend(["image request", "reply"]);
                    }
                    expected.push("leaf");
                    check_parent_messages(home.path(), &expected, damaged)?;
                    let preview = read(home.path(), true)?;
                    let conversation = omnis_core::import_conversation(&preview);
                    assert_eq!(
                        conversation
                            .messages
                            .iter()
                            .map(|m| m.text.as_str())
                            .collect::<Vec<_>>(),
                        expected
                    );
                }
            }
        }
    }
    Ok(())
}

#[test]
fn a_withheld_reply_separates_requests_with_identical_text() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let start = |ordinal, id| json!({"ordinal":ordinal,"type":"event_msg","payload":{"type":"task_started","turn_id":id}});
    let rows = [
        header(PARENT, 0, None),
        start(1, "A"),
        turn(2, "user", "earlier"),
        start(3, "B"),
        turn(4, "user", "continue"),
        turn(5, "assistant", "withheld"),
        json!({"ordinal":6,"type":"event_msg","payload":{"type":"user_message","message":"continue"}}),
    ];
    let stamp = "2026-01-01T00-00-00";
    let parent = write(home.path(), "sessions", PARENT, PARENT, stamp, &rows);
    fs::write(
        parent,
        encoded(&rows).replace("\"ordinal\":5,", "\"ordinaX\":5,"),
    )?;
    for (count, expected) in [
        (0, vec!["earlier", "continue", "continue", "leaf"]),
        (1, vec!["earlier", "continue", "leaf"]),
        (2, vec!["earlier", "leaf"]),
    ] {
        write(
            home.path(),
            "sessions",
            CHILD,
            CHILD,
            stamp,
            &[
                header(CHILD, 7, Some(bookmark(PARENT, &rows))),
                json!({"ordinal":8,"type":"event_msg","payload":{"type":"thread_rolled_back","num_turns":count}}),
                turn(9, "user", "leaf"),
            ],
        );
        check_parent_messages(home.path(), &expected, true)?;
    }
    Ok(())
}

#[test]
fn typed_media_mirrors_are_consumed_once_in_either_wire_order() -> anyhow::Result<()> {
    for (raw_media, event_fields) in [
        (
            json!({"type":"input_image","file_id":"synthetic-file","detail":"high"}),
            json!({"file_ids":["synthetic-file"],"image_order":["file"]}),
        ),
        (
            json!({"type":"input_audio","file_id":"synthetic-file"}),
            json!({"audio_references":[{"file_id":"synthetic-file"}],"audio_order":["file"]}),
        ),
        (
            json!({"type":"input_image","image_url":"test-url","detail":"low"}),
            json!({"images":["test-url"]}),
        ),
    ] {
        for (order, requests) in [
            ("EE", 2usize),
            ("RE", 1),
            ("ER", 1),
            ("REE", 2),
            ("ERR", 2),
            ("RREE", 2),
            ("EERR", 2),
            ("RERE", 2),
        ] {
            let home = tempfile::tempdir()?;
            let rows = media_turn_rows(order, &raw_media, &event_fields);
            let n = rows.last().unwrap()["ordinal"].as_u64().unwrap();
            let parent = write(
                home.path(),
                "sessions",
                PARENT,
                PARENT,
                "2026-01-01T00-00-00",
                &rows,
            );
            for damaged in [false, true] {
                let body = encoded(&rows);
                fs::write(
                    &parent,
                    if damaged {
                        body.replace("\"ordinal\":3,", "\"ordinaX\":3,")
                    } else {
                        body
                    },
                )?;
                for count in [0, 1, 2] {
                    write(
                        home.path(),
                        "sessions",
                        CHILD,
                        CHILD,
                        "2026-01-01T00-00-00",
                        &[
                            header(CHILD, n + 1, Some(bookmark(PARENT, &rows))),
                            json!({"ordinal":n+2,"type":"event_msg","payload":{"type":"thread_rolled_back","num_turns":count}}),
                            turn(n + 3, "user", "leaf"),
                        ],
                    );
                    let mut expected = Vec::new();
                    let checkpoints: usize = if order == "ERR" && damaged { 2 } else { 1 };
                    if count <= checkpoints {
                        expected.push("older");
                    }
                    let retained = if count == 0 {
                        requests
                    } else {
                        checkpoints.saturating_sub(count)
                    };
                    expected.extend(std::iter::repeat_n("media request", retained));
                    expected.push("leaf");
                    check_parent_messages(home.path(), &expected, damaged)?;
                    let snapshot = read(home.path(), true)?;
                    let view = omnis_core::import_conversation(&snapshot);
                    assert_eq!(
                        view.messages
                            .iter()
                            .map(|m| m.text.as_str())
                            .collect::<Vec<_>>(),
                        expected
                    );
                }
            }
        }
    }
    Ok(())
}

fn media_turn_rows(order: &str, raw_media: &Value, event_fields: &Value) -> Vec<Value> {
    let mut rows = vec![
        header(PARENT, 0, None),
        turn(1, "user", "older"),
        json!({"ordinal":2,"type":"event_msg","payload":{"type":"task_started","turn_id":"B"}}),
    ];
    let mut submission = 0;
    for kind in order.chars() {
        let n = rows.len() as u64;
        let mut record = if kind == 'R' {
            let mut r = turn(n, "user", "media request");
            r["payload"]["content"]
                .as_array_mut()
                .unwrap()
                .push(raw_media.clone());
            r
        } else {
            json!({"ordinal":n,"type":"event_msg","payload":{"type":"user_message","message":"media request"}})
        };
        if kind == 'E' {
            if order != "EE" {
                submission += 1;
                record["payload"]["client_id"] = json!(format!("submission-{submission}"));
            }
            record["payload"]
                .as_object_mut()
                .unwrap()
                .extend(event_fields.as_object().unwrap().clone());
        }
        rows.push(record);
    }
    let n = rows.len() as u64;
    rows.push(
        json!({"ordinal":n,"type":"event_msg","payload":{"type":"task_complete","turn_id":"B"}}),
    );
    rows
}
