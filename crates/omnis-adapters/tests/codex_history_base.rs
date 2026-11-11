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
        assert_active_history(home.path(), &leaf_path, &retired, gzip_only);
        if !gzip_only {
            let shadow = home
                .path()
                .join("sessions/2025/12/01")
                .join(parent_path.file_name().unwrap());
            fs::create_dir_all(shadow.parent().unwrap()).unwrap();
            fs::write(&shadow, b"{\n").unwrap();
            assert_active_history(home.path(), &leaf_path, &retired, true);
            fs::remove_file(shadow).unwrap();
            let backup = zst.with_file_name(format!("backup-{PARENT}.jsonl.zst"));
            fs::rename(&zst, &backup).unwrap();
            assert_active_history(home.path(), &leaf_path, &retired, true);
            fs::rename(backup, &zst).unwrap();
        }
    }
    assert_eq!(fs::read(gzip).unwrap(), compressed);
}

fn assert_active_history(home: &Path, leaf: &Path, retired: &Path, import_only: bool) {
    let child = SessionRef::new(Provider::Codex, CHILD);
    let adapter = CodexAdapter::with_root(home);
    assert_eq!(
        adapter.native_resume_support(&child).unwrap(),
        if import_only {
            NativeResumeSupport::ImportOnly
        } else {
            NativeResumeSupport::Since("0.167.0")
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
        assert_eq!(
            text,
            [
                "inherited request",
                "retained fork reply",
                "request after revert"
            ]
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
    let retained = [
        "verified intermediate request",
        "first leaf request",
        "first leaf answer",
        "second leaf request",
    ];
    for (damaged, damage_middle) in [
        (&malformed[..], false),
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
    }
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
