use std::{fs, path::Path};

use anyhow::Result;
use omnis_adapters::{CodexAdapter, NativeResumeSupport, ProviderAdapter};
use omnis_core::{
    build_native_materialization_report_for_snapshot, fidelity_report_for_snapshot,
    write_recovery_handoff,
};
use omnis_ir::{Provider, SessionRef};
use serde_json::{Value, json};

const STAMP: &str = "2026-01-01T00:00:00Z";
const PARENT: &str = "11111111-1111-4111-8111-111111111111";
const CHILD: &str = "22222222-2222-4222-8222-222222222222";
const REVERT: &str = "33333333-3333-4333-8333-333333333333";

fn header(id: &str, ordinal: usize, base: Option<Value>, attached: bool) -> Value {
    json!({"timestamp":STAMP,"type":"session_meta", "ordinal": ordinal, "payload":{
        "id": id, "session_id": PARENT, "history_mode": "paginated", "history_base": base.unwrap_or(Value::Null),
        "timestamp":STAMP,"cwd":"/workspace","originator":"fixture","cli_version":"0.167.1",
        "attached_thread": attached, "forked_from_id": (id == CHILD).then_some(PARENT)
    }})
}

fn message(ordinal: usize, text: &str) -> Value {
    json!({"timestamp":STAMP,"ordinal":ordinal, "type":"response_item","payload":{"type":"message","role":"user",
        "content":[{"type":"input_text","text":text}]}})
}

fn settings(ordinal: usize, cwd: &Path) -> Value {
    json!({"timestamp":STAMP,"ordinal":ordinal, "type":"event_msg", "payload":{"type":"thread_settings_applied","thread_id":CHILD,
        "thread_settings":{"cwd":cwd, "model":"fixture", "model_provider_id":"synthetic",
            "approval_policy":"on-request", "approvals_reviewer":"user",
            "permission_profile":{"type":"managed","file_system":{"type":"restricted","entries":[]},"network":"restricted"},
            "collaboration_mode":{"mode":"default","settings":{"model":"fixture","reasoning_effort":null,"developer_instructions":null}}
        }
    }})
}

fn bookmark(physical: &str, rows: &[Value]) -> Value {
    json!({"thread_id":physical,"end_byte_offset":rows.iter().map(|r| r.to_string().len()+1).sum::<usize>(),
        "end_ordinal_exclusive":rows.last().unwrap()["ordinal"].as_u64().unwrap()+1})
}

fn write(dir: &Path, name: &str, rows: &[Value]) -> Result<()> {
    fs::create_dir_all(dir)?;
    let mut body = rows
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    body.push('\n');
    fs::write(dir.join(format!("rollout-{name}.jsonl")), body)?;
    Ok(())
}

#[test]
fn attached_child_recovers_when_its_parent_was_cut_short() -> Result<()> {
    let home = tempfile::tempdir()?;
    let dir = home.path().join("sessions");
    let parent = [header(PARENT, 0, None, false), message(1, "parent request")];
    let child = [
        header(CHILD, 2, Some(bookmark(PARENT, &parent)), true),
        message(3, "retained child request"),
    ];
    write(&dir, &format!("2026-01-02T00-00-00-{CHILD}"), &child)?;
    let session = SessionRef::new(Provider::Codex, CHILD);
    let name = format!("2026-01-01T00-00-00-{PARENT}");
    let path = dir.join(format!("rollout-{name}.jsonl"));
    for compressed in [false, true] {
        for (rows, expected) in [
            (&parent[..], NativeResumeSupport::Since("0.167.0")),
            (&parent[..1], NativeResumeSupport::ImportOnly),
        ] {
            write(&dir, &name, rows)?;
            if compressed {
                let contents = fs::read(&path)?;
                fs::write(
                    path.with_extension("jsonl.zst"),
                    zstd::stream::encode_all(contents.as_slice(), 1)?,
                )?;
                fs::remove_file(&path)?;
            }
            let adapter = CodexAdapter::with_root(home.path());
            assert_eq!(adapter.native_resume_support(&session)?, expected);
            assert!(
                adapter
                    .read_session(&session)?
                    .events
                    .iter()
                    .any(|event| { event.payload["text"] == "retained child request" })
            );
        }
    }
    Ok(())
}

#[test]
fn paginated_native_resume_requires_a_string_creation_directory() -> Result<()> {
    let home = tempfile::tempdir()?;
    let session = SessionRef::new(Provider::Codex, PARENT);
    let folder = home.path().join("sessions/2026/01/01");
    let name = format!("2026-01-01T00-00-00-{PARENT}");
    for (cwd, expected) in [
        (
            Some(json!(home.path())),
            NativeResumeSupport::Since("0.167.0"),
        ),
        (Some(json!("")), NativeResumeSupport::Since("0.167.0")),
        (None, NativeResumeSupport::ImportOnly),
        (Some(json!(null)), NativeResumeSupport::ImportOnly),
        (
            Some(json!({"path": "/workspace"})),
            NativeResumeSupport::ImportOnly,
        ),
    ] {
        let mut header = header(PARENT, 0, None, false);
        let payload = header["payload"].as_object_mut().unwrap();
        payload.remove("cwd");
        if let Some(cwd) = cwd {
            payload.insert("cwd".into(), cwd);
        }
        write(
            &folder,
            &name,
            &[header, message(1, "available local request")],
        )?;
        let adapter = CodexAdapter::with_root(home.path());
        assert_eq!(adapter.native_resume_support(&session)?, expected);
        assert!(
            adapter
                .read_session(&session)?
                .events
                .iter()
                .any(|event| event.payload["text"] == "available local request")
        );
    }
    Ok(())
}

#[test]
fn attached_revert_keeps_child_turns_and_detached_forks_keep_parent_turns() -> Result<()> {
    for (attached, explicit_revert_cutoff) in [(false, true), (true, true), (true, false)] {
        let home = tempfile::tempdir()?;
        let dir = home.path().join("sessions");
        let parent = [header(PARENT, 0, None, false), message(1, "parent request")];
        write(&dir, &format!("2026-01-01T00-00-00-{PARENT}"), &parent)?;
        let mut fork = header(CHILD, 2, Some(bookmark(PARENT, &parent)), attached);
        if !attached {
            fork["payload"]["forked_from_ordinal_exclusive"] = json!(2);
        }
        let child = [
            fork,
            settings(3, home.path()),
            message(4, "parent generation interrupted"),
            json!({"timestamp":STAMP,"ordinal":5,"type":"event_msg","payload":{"type":"turn_aborted","reason":"interrupted"}}),
            message(6, "child request"),
            message(7, "discarded child request"),
        ];
        write(&dir, &format!("2026-01-02T00-00-00-{CHILD}"), &child)?;
        let mut current = header(CHILD, 7, Some(bookmark(CHILD, &child[..5])), attached);
        if explicit_revert_cutoff {
            current["payload"]["forked_from_ordinal_exclusive"] = json!(2);
        }
        write(
            &dir,
            &format!("2026-01-03T00-00-00-{CHILD}_{REVERT}"),
            &[current, message(8, "new request")],
        )?;
        let adapter = CodexAdapter::with_root(home.path());
        let session = SessionRef::new(Provider::Codex, CHILD);
        for snapshot in [
            adapter.read_session(&session)?,
            adapter.preview_session(&session)?,
            adapter.index_session(&session, None, 65_536)?.snapshot,
        ] {
            let text: Vec<_> = snapshot
                .events
                .iter()
                .filter_map(|e| e.payload["text"].as_str())
                .collect();
            let report = build_native_materialization_report_for_snapshot(
                &snapshot,
                Provider::Claude,
                true,
                false,
                0,
                0,
            );
            assert_eq!(
                report
                    .warnings
                    .iter()
                    .any(|warning| warning.contains("parent context")),
                attached
            );
            assert!(
                fidelity_report_for_snapshot(&snapshot, Provider::Codex, true)
                    .warnings
                    .is_empty()
            );
            assert_eq!(
                omnis_core::needs_recovery_handoff(&snapshot, false),
                attached
            );
            if attached {
                assert_eq!(text, ["child request", "new request"]);
            } else {
                assert_eq!(
                    text,
                    [
                        "parent request",
                        "parent generation interrupted",
                        "child request",
                        "new request"
                    ]
                );
            }
        }
    }
    Ok(())
}

#[test]
fn attached_child_is_fully_visible_even_when_its_parent_model_context_is_missing() -> Result<()> {
    let home = tempfile::tempdir()?;
    let end = 65_535;
    let base = json!({"thread_id":PARENT,"end_byte_offset":8_000_000,"end_ordinal_exclusive":end});
    let mut child = header(CHILD, end, Some(base), true);
    child["payload"]["forked_from_ordinal_exclusive"] = json!(end);
    write(
        &home.path().join("sessions"),
        &format!("2026-01-02T00-00-00-{CHILD}"),
        &[
            child,
            json!({"timestamp":STAMP,"ordinal":end+1,"type":"event_msg","payload":{"type":"task_started","turn_id":"child-turn"}}),
            message(end + 2, "child request"),
            json!({"timestamp":STAMP,"ordinal":end+3,"type":"event_msg","payload":{"type":"turn_aborted","turn_id":"child-turn","reason":"interrupted"}}),
        ],
    )?;
    let adapter = CodexAdapter::with_root(home.path());
    let session = SessionRef::new(Provider::Codex, CHILD);
    let index = adapter.index_session(&session, None, 65_536)?;
    assert!(index.source_complete);
    let mut recovery = Vec::new();
    write_recovery_handoff(&index.snapshot, &mut recovery)?;
    let recovery = String::from_utf8(recovery)?;
    assert!(recovery.contains("This handoff omits that parent context."));
    assert!(recovery.contains("child request"));
    assert!(
        index
            .snapshot
            .events
            .iter()
            .all(|e| e.payload["omitted_events"].is_null())
    );
    assert_eq!(
        adapter.native_resume_support(&session)?,
        NativeResumeSupport::ImportOnly
    );
    Ok(())
}

#[test]
fn subagents_and_their_detached_forks_show_only_local_turns() -> Result<()> {
    let home = tempfile::tempdir()?;
    let dir = home.path().join("sessions/2026/01/02");
    let mut meta = header(CHILD, 0, None, false);
    meta["payload"]["subagent_history_start_ordinal"] = json!(4);
    let copied = [
        meta,
        message(1, "copied parent request"),
        message(2, "copied parent reply"),
        message(4, "subagent request"),
    ];
    let name = format!("2026-01-02T00-00-00-{CHILD}");
    write(&dir, &name, &copied)?;
    let path = dir.join(format!("rollout-{name}.jsonl"));
    let intact = fs::read_to_string(&path)?;
    let damaged = intact.replacen("history_mode", "history_mxde", 1);
    fs::remove_file(&path)?;
    let mut fork = header(REVERT, 5, Some(bookmark(CHILD, &copied)), false);
    fork["payload"]["forked_from_id"] = json!(CHILD);
    write(
        &home.path().join("sessions/2026/01/03"),
        &format!("2026-01-03T00-00-00-{REVERT}"),
        &[fork, message(6, "fork request")],
    )?;

    for (mode_damaged, has_archive) in [(false, false), (true, false), (true, true)] {
        let body = if mode_damaged { &damaged } else { &intact };
        fs::write(
            path.with_extension("jsonl.zst"),
            zstd::stream::encode_all(body.as_bytes(), 1)?,
        )?;
        if has_archive {
            write(&home.path().join("archived_sessions"), &name, &copied)?;
        }
        let adapter = CodexAdapter::with_root(home.path());
        let child = SessionRef::new(Provider::Codex, CHILD);
        if !has_archive {
            let native = if mode_damaged {
                NativeResumeSupport::ImportOnly
            } else {
                NativeResumeSupport::Since("0.167.0")
            };
            assert_eq!(adapter.native_resume_support(&child)?, native);
        }
        for id in [CHILD, REVERT] {
            let session = SessionRef::new(Provider::Codex, id);
            let unread_parent = id == REVERT && mode_damaged && !has_archive;
            let expected: &[&str] = if id == CHILD {
                &["subagent request"]
            } else if unread_parent {
                &["fork request"]
            } else {
                &["subagent request", "fork request"]
            };
            for snapshot in [
                adapter.read_session(&session)?,
                adapter.preview_session(&session)?,
                adapter.index_session(&session, None, 1)?.snapshot,
                adapter.index_session(&session, None, u64::MAX)?.snapshot,
            ] {
                let text: Vec<_> = snapshot
                    .events
                    .iter()
                    .filter_map(|event| event.payload["text"].as_str())
                    .collect();
                assert_eq!(
                    text, expected,
                    "{id}, damaged={mode_damaged}, archive={has_archive}"
                );
                assert!(!serde_json::to_string(&snapshot)?.contains("copied parent"));
                assert_eq!(
                    CodexAdapter::has_unavailable_ancestry(&snapshot),
                    unread_parent
                );
                let unverified_mode = id == CHILD && mode_damaged;
                assert_eq!(
                    snapshot.events.iter().any(|event| {
                        event.source.raw_record_type.as_deref()
                            == Some("omnisession.codex_history_mode")
                            && event.payload["native_history_unverified"] == true
                    }),
                    unverified_mode
                );
                assert_eq!(
                    omnis_core::session_message_prefix_is_known(&snapshot, true),
                    !unread_parent && !unverified_mode,
                    "{id}, damaged={mode_damaged}, archive={has_archive}"
                );
                let report = fidelity_report_for_snapshot(&snapshot, Provider::Claude, true);
                assert_eq!(
                    report
                        .warnings
                        .iter()
                        .any(|w| w.contains("omits that copied context")),
                    !unread_parent
                );
            }
        }
    }
    Ok(())
}

#[test]
fn a_subagent_cutoff_survives_a_damaged_header_ordinal() -> Result<()> {
    let home = tempfile::tempdir()?;
    let dir = home.path().join("sessions/2026/01/02");
    let name = format!("2026-01-02T00-00-00-{CHILD}");
    let session = SessionRef::new(Provider::Codex, CHILD);
    let mut meta = header(CHILD, 0, None, false);
    meta["payload"]["subagent_history_start_ordinal"] = json!(4);
    for (ordinal, unplaced) in [
        (None, false),
        (Some(json!("unreadable")), true),
        (Some(json!(99)), false),
    ] {
        let mut rows = vec![
            meta.clone(),
            message(1, "copied parent"),
            message(4, "local request"),
        ];
        let fields = rows[0].as_object_mut().unwrap();
        if let Some(ordinal) = ordinal {
            fields.insert("ordinal".into(), ordinal);
        } else {
            let renamed = fields.remove("ordinal").unwrap();
            fields.insert("ordhnal".into(), renamed);
        }
        if unplaced {
            let mut row = message(5, "unplaced private message");
            row.as_object_mut().unwrap().remove("ordinal");
            rows.push(row);
            rows.push(json!({"type":"turn_context","payload":{"cwd":"/unplaced-directory"}}));
        }
        write(&dir, &name, &rows)?;
        let adapter = CodexAdapter::with_root(home.path());
        assert_eq!(
            adapter.native_resume_support(&session)?,
            NativeResumeSupport::ImportOnly
        );
        let bounded = adapter.index_session(&session, None, 1)?;
        let expected_dir = (!unplaced).then(|| "/workspace".into());
        assert_eq!(
            bounded.workspace_projection,
            omnis_adapters::CodexWorkspaceProjection::Observed(expected_dir)
        );
        for snapshot in [
            adapter.read_session(&session)?,
            adapter.preview_session(&session)?,
            bounded.snapshot,
            adapter.index_session(&session, None, u64::MAX)?.snapshot,
        ] {
            let conversation = omnis_core::import_conversation(&snapshot);
            assert_eq!(conversation.truncated, unplaced);
            assert_eq!(conversation.messages.len(), 1);
            assert_eq!(conversation.messages[0].text, "local request");
            let warnings = fidelity_report_for_snapshot(&snapshot, Provider::Claude, true).warnings;
            assert_eq!(
                warnings
                    .iter()
                    .filter(|warning| warning.contains("no valid ordinal"))
                    .count(),
                usize::from(unplaced)
            );
            assert!(
                !warnings
                    .iter()
                    .any(|warning| warning.contains("invalid JSON"))
            );
            let data = serde_json::to_string(&snapshot)?;
            assert!(!data.contains("copied parent"));
            assert!(!data.contains("unplaced private message"));
        }
    }
    let original = [
        meta,
        message(1, "copied parent"),
        message(4, "local request"),
    ];
    fs::remove_file(dir.join(format!("rollout-{name}.jsonl")))?;
    write(&home.path().join("archived_sessions"), &name, &original)?;
    let mut revert = header(CHILD, 5, Some(bookmark(CHILD, &original)), false);
    revert["payload"]["subagent_history_start_ordinal"] = json!(4);
    write(
        &dir,
        &format!("2026-01-02T00-00-01-{CHILD}_{REVERT}"),
        &[revert, message(6, "after revert")],
    )?;
    let adapter = CodexAdapter::with_root(home.path());
    assert_eq!(
        adapter.native_resume_support(&session)?,
        NativeResumeSupport::Since("0.167.0")
    );
    let snapshot = adapter.read_session(&session)?;
    let conversation = omnis_core::import_conversation(&snapshot);
    assert!(!conversation.truncated);
    assert_eq!(
        conversation
            .messages
            .iter()
            .map(|m| m.text.as_str())
            .collect::<Vec<_>>(),
        ["local request", "after revert"]
    );
    Ok(())
}

#[test]
fn a_subagent_message_without_an_ordinal_marks_the_visible_gap() -> Result<()> {
    let home = tempfile::tempdir()?;
    let session = SessionRef::new(Provider::Codex, CHILD);
    let mut meta = header(CHILD, 0, None, false);
    meta["payload"]["subagent_history_start_ordinal"] = json!(4);
    for (left, crossing) in [(4, false), (2, true)] {
        for damaged in [false, true] {
            let mut middle = message(5, "middle message");
            if damaged {
                let row = middle.as_object_mut().unwrap();
                let ordinal = row.remove("ordinal").unwrap();
                row.insert("ordhnal".into(), ordinal);
            }
            write(
                &home.path().join("sessions"),
                &format!("2026-01-02T00-00-00-{CHILD}"),
                &[
                    meta.clone(),
                    message(left, "left message"),
                    middle,
                    message(6, "right message"),
                ],
            )?;
            let adapter = CodexAdapter::with_root(home.path());
            for snapshot in [
                adapter.read_session(&session)?,
                adapter.preview_session(&session)?,
                adapter.index_session(&session, None, 1)?.snapshot,
                adapter.index_session(&session, None, u64::MAX)?.snapshot,
            ] {
                let conversation = omnis_core::import_conversation(&snapshot);
                let mut expected = Vec::new();
                if !crossing {
                    expected.push("left message");
                }
                if !damaged {
                    expected.push("middle message");
                }
                expected.push("right message");
                assert_eq!(conversation.truncated, damaged);
                assert_eq!(
                    conversation
                        .messages
                        .iter()
                        .map(|m| m.text.as_str())
                        .collect::<Vec<_>>(),
                    expected
                );
                let markdown = omnis_core::render_markdown_export(&snapshot);
                assert_eq!(markdown.contains("middle message"), !damaged);
                assert_eq!(markdown.contains("left message"), !crossing);
                assert!(markdown.contains("right message"));
                assert_eq!(
                    markdown.matches("no valid ordinal").count(),
                    usize::from(damaged)
                );
            }
        }
    }
    Ok(())
}

#[test]
fn a_subagent_cutoff_does_not_hide_messages_in_legacy_history() -> Result<()> {
    let home = tempfile::tempdir()?;
    let session = SessionRef::new(Provider::Codex, CHILD);
    let mut meta = header(CHILD, 0, None, false);
    meta["payload"]["subagent_history_start_ordinal"] = json!(4);
    meta["payload"]["history_mode"] = json!("legacy");
    let mut rows = [
        meta,
        message(1, "first request"),
        message(4, "last request"),
    ];
    for no_ordinals in [false, true] {
        if no_ordinals {
            rows[0]["payload"]
                .as_object_mut()
                .unwrap()
                .remove("history_mode");
            for row in &mut rows {
                row.as_object_mut().unwrap().remove("ordinal");
            }
        }
        write(
            &home.path().join("sessions"),
            &format!("2026-01-02T00-00-00-{CHILD}"),
            &rows,
        )?;
        let snapshot = CodexAdapter::with_root(home.path()).read_session(&session)?;
        let text: Vec<_> = snapshot
            .events
            .iter()
            .filter_map(|e| e.payload["text"].as_str())
            .collect();
        assert_eq!(text, ["first request", "last request"]);
    }
    Ok(())
}

fn interrupted_fork(dir: &Path) -> Result<Vec<Value>> {
    let parent = [header(PARENT, 0, None, false), message(1, "parent request")];
    write(dir, &format!("2026-01-01T00-00-00-{PARENT}"), &parent)?;
    Ok(vec![
        header(CHILD, 2, Some(bookmark(PARENT, &parent)), true),
        settings(3, dir),
        message(4, "interrupted parent generation"),
        json!({"timestamp":STAMP,"type":"event_msg","ordinal":5,"payload":{"type":"turn_aborted","reason":"interrupted"}}),
        message(6, "child request"),
    ])
}

#[test]
fn an_attached_midturn_abort_hides_the_parent_when_an_ordinal_is_damaged() -> Result<()> {
    let home = tempfile::tempdir()?;
    let dir = home.path().join("sessions");
    let source = interrupted_fork(&dir)?;
    let session = SessionRef::new(Provider::Codex, CHILD);
    for (row, replacement, extra_gap) in [
        (0, Value::Null, false),
        (0, json!(9), false),
        (3, Value::Null, false),
        (3, Value::Null, true),
    ] {
        let mut rows = source.clone();
        rows[row]["ordinal"] = replacement;
        if extra_gap {
            let mut unplaced = message(7, "unplaced child message");
            unplaced.as_object_mut().unwrap().remove("ordinal");
            rows.extend([unplaced, message(8, "later child request")]);
        }
        write(&dir, &format!("2026-01-02T00-00-00-{CHILD}"), &rows)?;
        let adapter = CodexAdapter::with_root(home.path());
        if row == 0 {
            assert_eq!(
                adapter.native_resume_support(&session)?,
                NativeResumeSupport::ImportOnly
            );
        }
        let full_index = adapter.index_session(&session, None, u64::MAX)?;
        assert!(full_index.source_complete);
        for snapshot in [
            adapter.read_session(&session)?,
            adapter.preview_session(&session)?,
            adapter.index_session(&session, None, 1)?.snapshot,
            full_index.snapshot,
        ] {
            let conversation = omnis_core::import_conversation(&snapshot);
            let expected: &[&str] = if extra_gap {
                &["child request", "later child request"]
            } else {
                &["child request"]
            };
            assert_eq!(
                conversation
                    .messages
                    .iter()
                    .map(|m| m.text.as_str())
                    .collect::<Vec<_>>(),
                expected
            );
            assert_eq!(conversation.truncated, extra_gap);
            let export = omnis_core::render_markdown_export(&snapshot);
            assert!(!export.contains("interrupted parent generation"));
            assert!(!export.contains("unplaced child message"));
            assert_eq!(
                export.matches("no valid ordinal").count(),
                usize::from(extra_gap)
            );
        }
    }
    Ok(())
}

#[test]
fn a_detached_fork_obeys_its_attached_sources_bookmark() -> Result<()> {
    let home = tempfile::tempdir()?;
    let dir = home.path().join("sessions");
    let mut source = interrupted_fork(&dir)?;
    source[3].as_object_mut().unwrap().remove("ordinal");
    write(&dir, &format!("2026-01-02T00-00-00-{CHILD}"), &source)?;
    let session = SessionRef::new(Provider::Codex, REVERT);
    for length in [3, 5] {
        let base = bookmark(CHILD, &source[..length]);
        let end = usize::try_from(base["end_ordinal_exclusive"].as_u64().unwrap())?;
        let fork = [
            header(REVERT, end, Some(base), false),
            message(end + 1, "detached request"),
        ];
        write(&dir, &format!("2026-01-03T00-00-00-{REVERT}"), &fork)?;
        let adapter = CodexAdapter::with_root(home.path());
        for snapshot in [
            adapter.read_session(&session)?,
            adapter.index_session(&session, None, 1)?.snapshot,
        ] {
            let conversation = omnis_core::import_conversation(&snapshot);
            assert_eq!(conversation.truncated, length == 3);
            let expected: &[&str] = if length == 3 {
                &["detached request"]
            } else {
                &["child request", "detached request"]
            };
            assert_eq!(
                conversation
                    .messages
                    .iter()
                    .map(|m| m.text.as_str())
                    .collect::<Vec<_>>(),
                expected
            );
            let text = serde_json::to_string(&snapshot)?;
            assert!(!text.contains("interrupted parent generation"));
            assert_eq!(
                text.matches("could not verify which messages were visible")
                    .count(),
                usize::from(length == 3)
            );
        }
    }
    Ok(())
}

#[test]
fn a_damaged_attachment_preamble_keeps_the_confirmed_child_turn() -> Result<()> {
    let home = tempfile::tempdir()?;
    let dir = home.path().join("sessions");
    let session = SessionRef::new(Provider::Codex, CHILD);
    let name = format!("2026-01-02T00-00-00-{CHILD}");
    let mut rows = interrupted_fork(&dir)?;
    rows[1] = message(3, "interrupted parent generation");
    rows[2] = settings(4, &dir);
    for replacement in [Some("{"), Some("{}"), None] {
        if replacement.is_none() {
            rows[0]["payload"]["history_base"] = json!({});
        }
        write(&dir, &name, &rows)?;
        if let Some(replacement) = replacement {
            let path = dir.join(format!("rollout-{name}.jsonl"));
            let body = fs::read_to_string(&path)?.replace(&rows[2].to_string(), replacement);
            fs::write(path, body)?;
        }
        let adapter = CodexAdapter::with_root(home.path());
        for snapshot in [
            adapter.read_session(&session)?,
            adapter.preview_session(&session)?,
        ] {
            let conversation = omnis_core::import_conversation(&snapshot);
            assert!(conversation.truncated);
            assert_eq!(conversation.messages.len(), 1);
            assert_eq!(conversation.messages[0].text, "child request");
            assert_eq!(
                snapshot
                    .events
                    .iter()
                    .filter(|event| event.payload["omitted_events"]
                        .as_u64()
                        .is_some_and(|count| count > 0))
                    .count(),
                1
            );
            assert!(!serde_json::to_string(&snapshot)?.contains("interrupted parent generation"));
        }
    }
    Ok(())
}

#[test]
fn damaged_attachment_preambles_keep_verified_child_turns_in_detached_forks() -> Result<()> {
    let home = tempfile::tempdir()?;
    let dir = home.path().join("sessions");
    for damage in [
        "settings tag",
        "settings body",
        "missing base",
        "damaged base",
        "abort ordinal",
    ] {
        let mut child = interrupted_fork(&dir)?;
        child[0]["payload"]["forked_from_ordinal_exclusive"] = json!(2);
        match damage {
            "settings tag" => child[1]["payload"]["type"] = json!("thread_settings_applief"),
            "settings body" => {
                let payload = child[1]["payload"].as_object_mut().unwrap();
                let settings = payload.remove("thread_settings").unwrap();
                payload.insert("thread_settingt".into(), settings);
            }
            "missing base" => {
                child[0]["payload"]
                    .as_object_mut()
                    .unwrap()
                    .remove("history_base");
            }
            "damaged base" => {
                child[0]["payload"]["history_base"]
                    .as_object_mut()
                    .unwrap()
                    .remove("end_byte_offset");
                child[3]["payload"] = json!({"type":"warning", "message":"settings warning"});
            }
            "abort ordinal" => child[3]["ordinal"] = json!(9),
            _ => unreachable!(),
        }
        child.truncate(4);
        child.push(json!({"timestamp":STAMP,"ordinal":6,"type":"event_msg","payload":{"type":"task_started","turn_id":"child-turn"}}));
        child.push(message(7, "confirmed child request"));
        let mut reply = message(8, "confirmed child reply");
        reply["payload"]["role"] = json!("assistant");
        reply["payload"]["content"][0]["type"] = json!("output_text");
        child.push(reply);
        write(&dir, &format!("2026-01-02T00-00-00-{CHILD}"), &child)?;
        let detached = [
            header(REVERT, 9, Some(bookmark(CHILD, &child)), false),
            message(10, "detached request"),
        ];
        write(&dir, &format!("2026-01-03T00-00-00-{REVERT}"), &detached)?;
        let adapter = CodexAdapter::with_root(home.path());
        for id in [CHILD, REVERT] {
            let session = SessionRef::new(Provider::Codex, id);
            for snapshot in [
                adapter.read_session(&session)?,
                adapter.preview_session(&session)?,
                adapter.index_session(&session, None, 1)?.snapshot,
            ] {
                let conversation = omnis_core::import_conversation(&snapshot);
                assert!(conversation.truncated, "{id} {damage}");
                let messages: Vec<_> = conversation
                    .messages
                    .iter()
                    .map(|message| message.text.as_str())
                    .collect();
                let mut expected = vec!["confirmed child request", "confirmed child reply"];
                if id == REVERT {
                    expected.push("detached request");
                }
                assert_eq!(messages, expected, "{id} {damage}");
                let mut handoff = Vec::new();
                write_recovery_handoff(&snapshot, &mut handoff)?;
                assert!(!String::from_utf8(handoff)?.contains("interrupted parent generation"));
            }
        }
    }
    Ok(())
}

#[test]
fn an_attached_source_ending_before_its_abort_withholds_the_unplaced_response() -> Result<()> {
    let home = tempfile::tempdir()?;
    let dir = home.path().join("sessions");
    let source = interrupted_fork(&dir)?;
    write(&dir, &format!("2026-01-02T00-00-00-{CHILD}"), &source[..3])?;
    let detached = [
        header(REVERT, 7, Some(bookmark(CHILD, &source)), false),
        message(8, "detached request"),
    ];
    write(&dir, &format!("2026-01-03T00-00-00-{REVERT}"), &detached)?;
    let adapter = CodexAdapter::with_root(home.path());
    for id in [CHILD, REVERT] {
        let snapshot = adapter.read_session(&SessionRef::new(Provider::Codex, id))?;
        let conversation = omnis_core::import_conversation(&snapshot);
        assert!(conversation.truncated);
        assert!(
            !omnis_core::render_markdown_export(&snapshot)
                .contains("interrupted parent generation")
        );
        assert_eq!(conversation.messages.len(), usize::from(id == REVERT));
    }
    Ok(())
}

#[test]
fn a_detached_fork_reports_omissions_in_its_replacement_archive() -> Result<()> {
    let home = tempfile::tempdir()?;
    let dir = home.path().join("sessions");
    let archive = home.path().join("archived_sessions");
    let name = format!("2026-01-02T00-00-00-{CHILD}");
    let child = interrupted_fork(&dir)?;
    write(&dir, &name, &child[..1])?;
    let session = SessionRef::new(Provider::Codex, REVERT);
    for damaged in [false, true] {
        let mut archived = child.clone();
        if damaged {
            archived[1]["payload"]["type"] = json!("thread_settings_applief");
        }
        write(&archive, &name, &archived)?;
        let detached = [
            header(REVERT, 7, Some(bookmark(CHILD, &archived)), false),
            message(8, "detached request"),
        ];
        write(&dir, &format!("2026-01-03T00-00-00-{REVERT}"), &detached)?;
        let snapshot = CodexAdapter::with_root(home.path()).read_session(&session)?;
        let conversation = omnis_core::import_conversation(&snapshot);
        assert_eq!(conversation.truncated, damaged);
        assert_eq!(
            conversation
                .messages
                .iter()
                .map(|m| m.text.as_str())
                .collect::<Vec<_>>(),
            ["child request", "detached request"]
        );
    }
    Ok(())
}

#[test]
fn valid_tool_catalog_preserves_the_attached_workspace() -> Result<()> {
    let home = tempfile::tempdir()?;
    let dir = home.path().join("sessions");
    let cwd = home.path().join("child");
    fs::create_dir(&cwd)?;
    let mut child = interrupted_fork(&dir)?;
    child.truncate(1);
    child.extend([
        settings(3, &cwd),
        json!({"timestamp":STAMP,"type":"dynamic_tools_updated","ordinal":4,"payload":{"thread_id":CHILD,"tools":[]}}),
        json!({"timestamp":STAMP,"type":"event_msg","ordinal":5,"payload":{"type":"task_started","turn_id":"child-turn"}}),
        message(6, "recorded request"),
    ]);
    write(&dir, &format!("2026-01-02T00-00-00-{CHILD}"), &child)?;
    let snapshot = CodexAdapter::with_root(home.path())
        .read_session(&SessionRef::new(Provider::Codex, CHILD))?;
    assert_eq!(snapshot.workspace.current_dir, cwd);
    assert!(!omnis_core::import_conversation(&snapshot).truncated);
    Ok(())
}

fn add_receipt_media(rows: &mut [Value], diff: &str) {
    if diff.contains("image") || diff == "wrong detail" {
        rows[4]["payload"]["content"].as_array_mut().unwrap().push(
            json!({"type":"input_image","image_url":"data:image/png;base64,Yw==","detail":"high"}),
        );
        rows[5]["payload"]["item"]["content"]
            .as_array_mut()
            .unwrap()
            .push(json!({"type":"image","image_url":"data:image/png;base64,Yw=="}));
    }
    if matches!(
        diff,
        "matching wrapped image" | "matching literal image" | "wrong literal image"
    ) {
        let open = "<image name=[Iris]>";
        let parts = rows[4]["payload"]["content"].as_array_mut().unwrap();
        parts.insert(1, json!({"type":"input_text","text":open}));
        parts.push(json!({"type":"input_text","text":"</image>"}));
        if diff.ends_with("literal image") {
            let saved = rows[5]["payload"]["item"]["content"]
                .as_array_mut()
                .unwrap();
            let saved_open = if diff == "wrong literal image" {
                "<image>"
            } else {
                open
            };
            saved.insert(1, json!({"type":"text","text":saved_open}));
            saved.push(json!({"type":"text","text":"</image>"}));
        }
    } else if diff == "matching uploaded audio" {
        rows[4]["payload"]["content"]
            .as_array_mut()
            .unwrap()
            .push(json!({"type":"input_audio","file_id":"audio-file"}));
        rows[5]["payload"]["item"]["content"]
            .as_array_mut()
            .unwrap()
            .push(json!({"type":"audio","file_id":"audio-file"}));
    }
    if diff.ends_with("local audio") || diff.ends_with("literal audio") {
        let path = "/synthetic/voice memo.wav";
        let parts = rows[4]["payload"]["content"].as_array_mut().unwrap();
        parts.extend([
            json!({"type":"input_text","text":format!(r#"<audio name=[Audio #1] path="{path}">"#)}),
            json!({"type":"input_audio","audio_url":"data:audio/wav;base64,UklGRg=="}),
            json!({"type":"input_text","text":"</audio>"}),
        ]);
        if diff.ends_with("literal audio") {
            let url = if diff == "wrong literal audio" {
                "different"
            } else {
                "data:audio/wav;base64,UklGRg=="
            };
            let opening = parts[1]["text"].clone();
            rows[5]["payload"]["item"]["content"]
                .as_array_mut()
                .unwrap()
                .extend([
                    json!({"type":"text","text":opening}),
                    json!({"type":"audio","audio_url":url}),
                    json!({"type":"text","text":"</audio>"}),
                ]);
            return;
        }
        if diff == "incomplete local audio" {
            parts.pop();
        }
        let saved = if diff == "wrong local audio" {
            "/synthetic/other.wav"
        } else {
            path
        };
        rows[5]["payload"]["item"]["content"]
            .as_array_mut()
            .unwrap()
            .push(json!({"type":"local_audio","path":saved}));
    }
}

fn change_receipt_evidence(rows: &mut [Value], diff: &str) {
    let receipt = &mut rows[5]["payload"];
    match diff {
        "matching after foreign start" => {
            rows[1] = json!({"type":"event_msg","ordinal":3,"payload":{"type":"task_started","thread_id":PARENT,"turn_id":"parent-turn"}});
        }
        "matching after foreign outer start" => {
            rows[1] = json!({"type":"event_msg","ordinal":3,"thread_id":PARENT,"payload":{"type":"task_started","turn_id":"parent-turn"}});
        }
        "conflicting receipt owner" => rows[5]["thread_id"] = json!(PARENT),
        "conflicting receipt turn" => rows[5]["turn_id"] = json!("other-turn"),
        "wrong image" => {
            receipt["item"]["content"][1]["image_url"] = json!("data:image/png;base64,YQ==");
        }
        "wrong detail" => receipt["item"]["content"][1]["detail"] = json!("low"),
        "ambiguous image" => receipt["item"]["content"][1]["file_id"] = json!("another-file"),
        "wrong thread" => receipt["thread_id"] = json!(PARENT),
        "missing thread" => {
            receipt.as_object_mut().unwrap().remove("thread_id");
        }
        "wrong turn" => receipt["turn_id"] = json!("different-turn"),
        "wrong id" => receipt["item"]["id"] = json!("different-item"),
        "wrong text" => receipt["item"]["content"][0]["text"] = json!("different content"),
        _ => {}
    }
}

#[test]
fn an_attached_request_is_recovered_only_from_its_child_receipt() -> Result<()> {
    let home = tempfile::tempdir()?;
    let dir = home.path().join("sessions");
    let mut child = interrupted_fork(&dir)?;
    child.truncate(3); // Includes an unverified parent response.
    child.push(json!({"timestamp":STAMP,"type":"event_msg","ordinal":5,"payload":{"type":"task_startec","turn_id":"child-turn"}}));
    let mut opening = message(6, "confirmed child opening");
    opening["payload"]["id"] = json!("opening");
    child.push(opening);
    child.push(json!({"timestamp":STAMP,"type":"event_msg","ordinal":7,"payload":{"type":"item_completed","thread_id":CHILD,"turn_id":"child-turn","item":{"type":"UserMessage","id":"opening","content":[{"type":"text","text":"confirmed child opening"}]}}}));
    for diff in [
        "matching",
        "matching after foreign start",
        "matching after foreign outer start",
        "conflicting receipt owner",
        "conflicting receipt turn",
        "matching image",
        "matching wrapped image",
        "matching literal image",
        "wrong literal image",
        "matching uploaded audio",
        "matching local audio",
        "matching literal audio",
        "wrong literal audio",
        "wrong local audio",
        "incomplete local audio",
        "wrong image",
        "wrong detail",
        "ambiguous image",
        "wrong thread",
        "missing thread",
        "wrong turn",
        "wrong id",
        "wrong text",
    ] {
        let mut rows = child.clone();
        add_receipt_media(&mut rows, diff);
        change_receipt_evidence(&mut rows, diff);
        if diff.starts_with("matching") || diff.starts_with("conflicting receipt") {
            rows.push(message(8, "later child request"));
            rows.push(json!({"timestamp":STAMP,"type":"event_msg","ordinal":9,"payload":{"type":"user_message","message":"later child request"}}));
        } else {
            rows.push(json!({"timestamp":STAMP,"type":"event_msg","ordinal":8,"payload":{"type":"task_started","turn_id":"further-child-turn"}}));
            rows.push(message(9, "later child request"));
        }
        write(&dir, &format!("2026-01-02T00-00-00-{CHILD}"), &rows)?;
        let detached = [
            header(REVERT, 10, Some(bookmark(CHILD, &rows)), false),
            message(11, "detached request"),
        ];
        write(&dir, &format!("2026-01-03T00-00-00-{REVERT}"), &detached)?;
        let adapter = CodexAdapter::with_root(home.path());
        for id in [CHILD, REVERT] {
            let snapshot = adapter.read_session(&SessionRef::new(Provider::Codex, id))?;
            let conversation = omnis_core::import_conversation(&snapshot);
            let messages: Vec<_> = conversation
                .messages
                .iter()
                .map(|m| m.text.as_str())
                .collect();
            assert_eq!(
                messages
                    .iter()
                    .any(|m| m.starts_with("confirmed child opening")),
                diff.starts_with("matching"),
                "{id} {diff}"
            );
            assert_eq!(
                messages.contains(&"confirmed child opening\n<image name=[Iris]>\n</image>"),
                diff == "matching literal image",
                "{id} {diff}"
            );
            let literal = "confirmed child opening\n<audio name=[Audio #1] path=\"/synthetic/voice memo.wav\">\n</audio>";
            assert_eq!(
                messages.contains(&literal),
                diff == "matching literal audio",
                "{id} {diff}"
            );
            assert!(
                !messages.contains(&"interrupted parent generation"),
                "{id} {diff}"
            );
            assert!(messages.contains(&"later child request"), "{id} {diff}");
        }
    }
    Ok(())
}

#[test]
fn an_inherited_abort_preserves_only_the_childs_owned_workspace() -> Result<()> {
    let home = tempfile::tempdir()?;
    let dir = home.path().join("sessions");
    let child_dir = home.path().join("child");
    fs::create_dir(&child_dir)?;
    for (owner, expected) in [
        (CHILD, child_dir.as_path()),
        (PARENT, Path::new("/workspace")),
    ] {
        let mut rows = interrupted_fork(&dir)?;
        rows[1] = settings(3, &child_dir);
        rows[1]["payload"]["thread_id"] = json!(owner);
        write(&dir, &format!("2026-01-02T00-00-00-{CHILD}"), &rows)?;
        let adapter = CodexAdapter::with_root(home.path());
        let session = SessionRef::new(Provider::Codex, CHILD);
        for snapshot in [
            adapter.read_session(&session)?,
            adapter.preview_session(&session)?,
            adapter.index_session(&session, None, u64::MAX)?.snapshot,
        ] {
            assert_eq!(snapshot.workspace.current_dir, expected);
            assert!(
                !omnis_core::render_markdown_export(&snapshot)
                    .contains("interrupted parent generation")
            );
        }
    }
    Ok(())
}

#[test]
fn a_detached_fork_recovers_damaged_records_only_from_an_agreeing_copy() -> Result<()> {
    let home = tempfile::tempdir()?;
    let dir = home.path().join("sessions");
    let archive = home.path().join("archived_sessions");
    let parent_name = format!("2026-01-01T00-00-00-{PARENT}");
    let mut reply = message(2, "parent response");
    reply["payload"]["role"] = json!("assistant");
    reply["payload"]["content"][0]["type"] = json!("output_text");
    let parent = [
        header(PARENT, 0, None, false),
        message(1, "parent request"),
        reply,
        json!({"type":"event_msg","ordinal":3,"payload":{"type":"task_complete","turn_id":"one"}}),
    ];
    let mut base = bookmark(PARENT, &parent);
    base["end_byte_offset"] = json!(base["end_byte_offset"].as_u64().unwrap() + 6);
    write(
        &dir,
        &format!("2026-01-02T00-00-00-{REVERT}"),
        &[
            header(REVERT, 4, Some(base), false),
            message(5, "detached request"),
        ],
    )?;
    let path = dir.join(format!("rollout-{parent_name}.jsonl"));
    let copy = archive.join(path.file_name().unwrap());
    write(&dir, &parent_name, &parent)?;
    let original = fs::read_to_string(&path)?
        .replace("\"ordinal\":0,", "\"ordinal\":0   ,")
        .replace("\"ordinal\":1,", "\"ordinal\":1   ,");
    fs::create_dir_all(&archive)?;
    let mut cases = vec![("joined", ""), ("split and truncated", "")];
    for invalid in ["null", "\"\"  ", "true", "[]  ", "{}  ", "-1  ", "1.5 "] {
        cases.extend([("request ordinal", invalid), ("header ordinal", invalid)]);
    }
    for (damage, invalid) in cases {
        let mut damaged = original.as_bytes().to_vec();
        if damage == "joined" {
            let delimiter = original.match_indices('\n').nth(1).unwrap().0;
            damaged[delimiter] = b' ';
        } else if damage == "split and truncated" {
            let split = std::str::from_utf8(&damaged)?.find("input_text").unwrap() + "input".len();
            damaged[split] = b'\n';
            damaged.truncate(damaged.len() - parent[3].to_string().len() - 1);
        } else {
            let field = if damage == "header ordinal" {
                "\"ordinal\":0"
            } else {
                "\"ordinal\":1"
            };
            let start = std::str::from_utf8(&damaged)?.find(field).unwrap() + "\"ordinal\":".len();
            damaged[start..start + 4].copy_from_slice(invalid.as_bytes());
        }
        fs::write(&path, damaged)?;
        for agrees in [true, false] {
            fs::write(&copy, &original)?;
            if !agrees {
                fs::write(
                    &copy,
                    fs::read_to_string(&copy)?.replace("parent response", "forged response"),
                )?;
            }
            let adapter = CodexAdapter::with_root(home.path());
            let snapshot = adapter.read_session(&SessionRef::new(Provider::Codex, REVERT))?;
            let conversation = omnis_core::import_conversation(&snapshot);
            let texts: Vec<_> = conversation
                .messages
                .iter()
                .map(|msg| msg.text.as_str())
                .collect();
            assert!(texts.contains(&"detached request"));
            assert!(!texts.contains(&"forged response"));
            assert_eq!(
                texts.contains(&"parent request"),
                agrees,
                "{damage} {invalid}"
            );
            assert_eq!(texts.contains(&"parent response"), agrees);
        }
    }
    Ok(())
}

#[test]
fn a_nested_fork_retains_ancestors_across_a_placed_metadata_gap() -> Result<()> {
    let home = tempfile::tempdir()?;
    let dir = home.path().join("sessions");
    let mut reply = message(2, "grandparent reply");
    reply["payload"]["role"] = json!("assistant");
    reply["payload"]["content"][0]["type"] = json!("output_text");
    let grandparent = [
        header(PARENT, 0, None, false),
        message(1, "grandparent request"),
        reply,
    ];
    write(&dir, &format!("2026-01-01T00-00-00-{PARENT}"), &grandparent)?;
    for (event, expected) in [
        (
            json!({"type":"token_count"}),
            vec![
                "grandparent request",
                "grandparent reply",
                "middle request",
                "leaf request",
            ],
        ),
        (
            json!({"type":"thread_rolled_back","num_turns":1}),
            vec!["middle request", "leaf request"],
        ),
        (
            json!({"type":"unknown_control"}),
            vec!["middle request", "leaf request"],
        ),
    ] {
        let middle = [
            header(CHILD, 3, Some(bookmark(PARENT, &grandparent)), false),
            json!({"type":"event_msg","payload":event}),
            json!({"type":"event_msg","ordinal":5,"payload":{"type":"task_started","turn_id":"middle"}}),
            message(6, "middle request"),
        ];
        write(&dir, &format!("2026-01-02T00-00-00-{CHILD}"), &middle)?;
        write(
            &dir,
            &format!("2026-01-03T00-00-00-{REVERT}"),
            &[
                header(REVERT, 7, Some(bookmark(CHILD, &middle)), false),
                message(8, "leaf request"),
            ],
        )?;
        let snapshot = CodexAdapter::with_root(home.path())
            .read_session(&SessionRef::new(Provider::Codex, REVERT))?;
        let conversation = omnis_core::import_conversation(&snapshot);
        let texts: Vec<_> = conversation
            .messages
            .iter()
            .map(|msg| msg.text.as_str())
            .collect();
        assert_eq!(texts, expected, "{event}");
    }
    Ok(())
}

#[test]
fn an_unfinished_middle_reply_can_be_withheld_without_hiding_its_ancestors() -> Result<()> {
    let home = tempfile::tempdir()?;
    let dir = home.path().join("sessions");
    let grandparent = [
        header(PARENT, 0, None, false),
        message(1, "grandparent request"),
    ];
    write(&dir, &format!("2026-01-01T00-00-00-{PARENT}"), &grandparent)?;
    let reply = |owner| {
        json!({"ordinal":5,"type":"event_msg","payload":{
            "type":"agent_message","thread_id":owner,"message":"unfinished reply"
        }})
    };
    for (body, retain_grandparent) in [
        (reply(CHILD), true),
        (reply(PARENT), false),
        (
            json!({"ordinal":5,"type":"turn_context","payload":{"cwd":"/synthetic"}}),
            true,
        ),
        (
            json!({"ordinal":5,"type":"response_item","payload":{"type":"function_call_output","call_id":"fixture","output":"unused"}}),
            true,
        ),
    ] {
        let mut middle = [
            header(CHILD, 2, Some(bookmark(PARENT, &grandparent)), false),
            json!({"ordinal":3,"type":"event_msg","payload":{"type":"task_started","turn_id":"middle"}}),
            message(4, "middle request"),
            body,
        ];
        let boundary = bookmark(CHILD, &middle);
        let reply = middle[3].as_object_mut().unwrap();
        let ordinal = reply.remove("ordinal").unwrap();
        reply.insert("ordinaX".into(), ordinal);
        write(&dir, &format!("2026-01-02T00-00-00-{CHILD}"), &middle)?;
        write(
            &dir,
            &format!("2026-01-03T00-00-00-{REVERT}"),
            &[
                header(REVERT, 6, Some(boundary), false),
                message(7, "leaf request"),
            ],
        )?;
        let snapshot = CodexAdapter::with_root(home.path())
            .read_session(&SessionRef::new(Provider::Codex, REVERT))?;
        let conversation = omnis_core::import_conversation(&snapshot);
        let messages: Vec<_> = conversation
            .messages
            .iter()
            .map(|m| m.text.as_str())
            .collect();
        if retain_grandparent {
            assert_eq!(
                messages,
                ["grandparent request", "middle request", "leaf request"]
            );
        } else {
            assert_eq!(messages, ["middle request", "leaf request"]);
        }
    }
    Ok(())
}

#[test]
fn a_subagent_cutoff_keeps_proven_local_messages_after_an_ordinal_gap() -> Result<()> {
    let home = tempfile::tempdir()?;
    let dir = home.path().join("sessions");
    for marker in [false, true] {
        for (cutoff, anchor, copied_rows, foreign_anchor) in [
            (3, 4, false, false),
            (3, 4, false, true),
            (3, 4, true, false),
            (5, 4, true, false),
            (5, 5, true, false),
            (5, 6, true, false),
            (2, 4, true, false),
        ] {
            let mut meta = header(PARENT, 0, None, false);
            meta["payload"]["subagent_history_start_ordinal"] = json!(cutoff);
            let mut copied = message(3, "unplaced request");
            copied.as_object_mut().unwrap().remove("ordinal");
            let mut rows = vec![
                meta,
                message(1, "copied request"),
                json!({"type":"event_msg","ordinal":2,"payload":{"type":"token_count"}}),
                copied,
                json!({"type":"event_msg","ordinal":anchor,"payload":{"type":"token_count"}}),
            ];
            if foreign_anchor {
                rows.last_mut().unwrap()["payload"]["thread_id"] = json!(REVERT);
            }
            if !copied_rows {
                rows.drain(1..3);
            }
            let immediate_reply = !copied_rows && !foreign_anchor && !marker;
            if immediate_reply {
                rows.pop();
            }
            if marker {
                rows.push(json!({"type":"event_msg","ordinal":anchor+1,"payload":{"type":"task_started","turn_id":"local"}}));
            }
            let ordinal = anchor + usize::from(!immediate_reply) + usize::from(marker);
            let mut reply = message(ordinal, "local reply");
            reply["payload"]["role"] = json!("assistant");
            reply["payload"]["content"][0]["type"] = json!("output_text");
            rows.push(reply);
            rows.push(message(ordinal + 1, "local request"));
            write(&dir, &format!("2026-01-01T00-00-00-{PARENT}"), &rows)?;
            write(
                &dir,
                &format!("2026-01-02T00-00-00-{REVERT}"),
                &[
                    header(REVERT, ordinal + 2, Some(bookmark(PARENT, &rows)), false),
                    message(ordinal + 3, "leaf request"),
                ],
            )?;
            let adapter = CodexAdapter::with_root(home.path());
            if anchor == 4 && cutoff == 3 {
                let direct = adapter.read_session(&SessionRef::new(Provider::Codex, PARENT))?;
                let direct = omnis_core::import_conversation(&direct);
                assert_eq!(
                    direct
                        .messages
                        .iter()
                        .map(|m| m.text.as_str())
                        .collect::<Vec<_>>(),
                    ["local reply", "local request"]
                );
            }
            let snapshot = adapter.read_session(&SessionRef::new(Provider::Codex, REVERT))?;
            let conversation = omnis_core::import_conversation(&snapshot);
            let texts: Vec<_> = conversation
                .messages
                .iter()
                .map(|m| m.text.as_str())
                .collect();
            let expected = vec!["local reply", "local request", "leaf request"];
            assert_eq!(
                texts, expected,
                "marker={marker}, cutoff={cutoff}, anchor={anchor}"
            );
        }
    }
    Ok(())
}

fn subagent_rows_with_gap(missing: usize, gap: &str) -> Vec<Value> {
    let mut meta = header(PARENT, 0, None, false);
    meta["payload"]["subagent_history_start_ordinal"] = json!(3);
    let mut rows = vec![meta];
    for _ in 0..missing {
        let mut request = message(0, "unplaced");
        request.as_object_mut().unwrap().remove("ordinal");
        rows.push(request);
    }
    match gap {
        "abort" => {
            rows[1] = json!({"type":"event_msg","payload":{"type":"turn_aborted","reason":"interrupted","turn_id":"local"}});
        }
        "foreign abort" => {
            rows[1] = json!({"type":"event_msg","payload":{"type":"turn_aborted","reason":"interrupted","thread_id":REVERT}});
        }
        "zero rollback" => {
            rows[1] =
                json!({"type":"event_msg","payload":{"type":"thread_rolled_back","num_turns":0}});
        }
        "invalid rollback" => {
            rows[1] =
                json!({"type":"event_msg","payload":{"type":"thread_rolled_back","num_turns":"0"}});
        }
        "assistant" => rows[1]["payload"]["role"] = json!("assistant"),
        "token" => rows[1] = json!({"type":"event_msg","payload":{"type":"token_count"}}),
        "context" => rows[1] = json!({"type":"turn_context","payload":{"turn_id":"local"}}),
        "completion" => {
            rows[1] = json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":"local","thread_id":PARENT}});
        }
        "foreign token" => {
            rows[1] =
                json!({"type":"event_msg","payload":{"type":"token_count","thread_id":REVERT}});
        }
        "opaque" => rows[1] = json!({"type":"event_msg","payload":{"type":"unknown_control"}}),
        "foreign" => rows[1]["payload"]["thread_id"] = json!(REVERT),
        "rollback" => {
            rows[1] =
                json!({"type":"event_msg","payload":{"type":"thread_rolled_back","num_turns":1}});
        }
        "malformed" => rows[1] = json!({"type":"damaged_marker"}),
        _ => (),
    }
    rows
}

#[test]
fn a_fork_keeps_numbered_subagent_messages_after_recognized_unplaced_records() -> Result<()> {
    let home = tempfile::tempdir()?;
    let dir = home.path().join("sessions");
    for (missing, gap) in [
        (2, "requests"),
        (4, "requests"),
        (1, "assistant"),
        (1, "token"),
        (1, "abort"),
        (1, "zero rollback"),
        (1, "invalid rollback"),
        (1, "foreign abort"),
        (2, "context"),
        (2, "completion"),
        (2, "foreign token"),
        (2, "opaque"),
        (2, "foreign"),
        (2, "rollback"),
        (2, "malformed"),
    ] {
        let mut rows = subagent_rows_with_gap(missing, gap);
        let preserves_suffix = matches!(
            gap,
            "requests"
                | "assistant"
                | "token"
                | "context"
                | "completion"
                | "abort"
                | "zero rollback"
        );
        let first = 3 + missing;
        for (ordinal, text, role) in [
            (first, "first reply", "assistant"),
            (first + 1, "next request", "user"),
            (first + 2, "next reply", "assistant"),
        ] {
            let mut item = message(ordinal, text);
            item["payload"]["role"] = json!(role);
            rows.push(item);
        }
        write(&dir, &format!("2026-01-01T00-00-00-{PARENT}"), &rows)?;
        write(
            &dir,
            &format!("2026-01-02T00-00-00-{REVERT}"),
            &[
                header(REVERT, first + 3, Some(bookmark(PARENT, &rows)), false),
                message(first + 4, "leaf"),
            ],
        )?;
        if gap == "malformed" {
            let path = dir.join(format!("rollout-2026-01-01T00-00-00-{PARENT}.jsonl"));
            fs::write(
                &path,
                fs::read_to_string(&path)?.replace("\"damaged_marker\"", "!damaged_marker\""),
            )?;
        }
        let adapter = CodexAdapter::with_root(home.path());
        for id in [PARENT, REVERT] {
            if id == PARENT && !preserves_suffix {
                continue;
            }
            let snapshot = adapter.read_session(&SessionRef::new(Provider::Codex, id))?;
            let conversation = omnis_core::import_conversation(&snapshot);
            let texts: Vec<_> = conversation
                .messages
                .iter()
                .map(|message| message.text.as_str())
                .collect();
            let mut expected = if preserves_suffix {
                vec!["first reply", "next request", "next reply"]
            } else {
                vec![]
            };
            if id == REVERT {
                expected.push("leaf");
            }
            assert_eq!(texts, expected, "missing={missing}, gap={gap}, id={id}");
        }
    }
    Ok(())
}

#[test]
fn a_suppressed_raw_request_cannot_replace_a_receipt_confirmed_child_message() -> Result<()> {
    let home = tempfile::tempdir()?;
    let dir = home.path().join("sessions");
    let receipt = |n, id: &str| json!({"ordinal":n,"type":"event_msg","payload":{"type":"item_completed","thread_id":CHILD,"turn_id":"child-turn","item":{"type":"UserMessage","id":id,"content":[{"type":"text","text":id}]}}});
    let raw = |n, id: &str| {
        let mut r = message(n, id);
        r["payload"]["id"] = json!(id);
        r
    };
    let saved = |n, id: &str| json!({"ordinal":n,"type":"event_msg","payload":{"type":"user_message","item_id":id,"message":id}});
    for multipart in [false, true] {
        let mut rows = interrupted_fork(&dir)?;
        rows.truncate(3);
        let mut b = raw(10, "Bluebell");
        if multipart {
            b["payload"]["content"] =
                json!([{"type":"input_text","text":"Blue"},{"type":"input_text","text":"bell"}]);
        }
        rows.extend([json!({"type":"event_msg","ordinal":5,"payload":{"type":"task_startec","turn_id":"child-turn"}}),raw(6,"opening"),receipt(7,"opening"),raw(8,"Apricot"),saved(9,"Apricot"),b,raw(11,"Cedar"),receipt(12,"Cedar"),saved(13,"Bluebell")]);
        write(&dir, &format!("2026-01-02T00-00-00-{CHILD}"), &rows)?;
        write(
            &dir,
            &format!("2026-01-03T00-00-00-{REVERT}"),
            &[
                header(REVERT, 14, Some(bookmark(CHILD, &rows)), false),
                message(15, "leaf"),
            ],
        )?;
        let adapter = CodexAdapter::with_root(home.path());
        for id in [CHILD, REVERT] {
            let session = SessionRef::new(Provider::Codex, id);
            for snapshot in [
                adapter.read_session(&session)?,
                adapter.preview_session(&session)?,
            ] {
                let conversation = omnis_core::import_conversation(&snapshot);
                let texts: Vec<_> = conversation
                    .messages
                    .iter()
                    .map(|m| m.text.as_str())
                    .collect();
                let mut expected = vec!["opening", "Apricot", "Cedar", "Bluebell"];
                if id == REVERT {
                    expected.push("leaf");
                }
                assert_eq!(texts, expected);
            }
        }
    }
    Ok(())
}

#[test]
fn conflicting_attachment_markers_do_not_expose_an_interrupted_parent() -> Result<()> {
    let home = tempfile::tempdir()?;
    let dir = home.path().join("sessions");
    for kind in [
        "item_started",
        "item_completed",
        "task_started",
        "turn_started",
        "turn_aborted",
    ] {
        let mut rows = interrupted_fork(&dir)?;
        rows.truncate(2);
        if kind == "item_started" || kind == "turn_aborted" {
            rows[0]["payload"]["history_base"] = json!({});
        }
        rows.push(json!({"type":"event_msg","ordinal":4,"thread_id":PARENT,"payload":{"type":kind,"thread_id":CHILD,"turn_id":"foreign","reason":"interrupted"}}));
        let mut copied = message(5, "interrupted parent content");
        copied["thread_id"] = json!(PARENT);
        rows.push(copied);
        let mut abort = json!({"type":"event_msg","ordinal":6,"payload":{"type":"turn_aborted","thread_id":PARENT,"reason":"interrupted"}});
        if matches!(kind, "item_started" | "task_started") {
            abort["payload"]
                .as_object_mut()
                .unwrap()
                .remove("thread_id");
        }
        rows.extend([
            abort,
            message(7,"after the parent abort"),
            json!({"type":"event_msg","ordinal":8,"payload":{"type":"task_started","thread_id":CHILD,"turn_id":"local"}}),
            message(9,"verified child request"),
        ]);
        write(&dir, &format!("2026-01-02T00-00-00-{CHILD}"), &rows)?;
        write(
            &dir,
            &format!("2026-01-03T00-00-00-{REVERT}"),
            &[
                header(REVERT, 10, Some(bookmark(CHILD, &rows)), false),
                message(11, "detached request"),
            ],
        )?;
        let adapter = CodexAdapter::with_root(home.path());
        for id in [CHILD, REVERT] {
            let snapshot = adapter.read_session(&SessionRef::new(Provider::Codex, id))?;
            let conversation = omnis_core::import_conversation(&snapshot);
            let texts: Vec<_> = conversation
                .messages
                .iter()
                .map(|m| m.text.as_str())
                .collect();
            let mut expected = if id == REVERT && rows[0]["payload"]["history_base"] == json!({}) {
                vec![]
            } else {
                vec!["after the parent abort", "verified child request"]
            };
            if id == REVERT {
                expected.push("detached request");
            }
            assert_eq!(texts, expected, "{kind} {id}");
        }
    }
    Ok(())
}

#[test]
fn paired_attachment_requests_survive_metadata_until_an_inherited_abort() -> Result<()> {
    let home = tempfile::tempdir()?;
    let dir = home.path().join("sessions");
    for ending in ["metadata", "task_started", "turn_aborted"] {
        let mut rows = interrupted_fork(&dir)?;
        rows.truncate(3);
        rows.push(json!({"type":"event_msg","ordinal":5,"payload":{"type":"damaged_marker"}}));
        for (ordinal, text) in [(6, "first pair"), (8, "second pair"), (10, "foreign pair")] {
            let mut raw = message(ordinal, text);
            if ordinal == 10 {
                raw["thread_id"] = json!(PARENT);
            }
            rows.push(raw);
            rows.push(json!({"type":"event_msg","ordinal":ordinal+1,"payload":{"type":"user_message","message":text}}));
        }
        rows.push(json!({"type":"event_msg","ordinal":12,"payload":{"type":"token_count"}}));
        if ending != "metadata" {
            rows.push(json!({"type":"event_msg","ordinal":13,"payload":{"type":ending,"turn_id":"next","reason":"interrupted"}}));
        }
        let end = rows.len() + 2;
        write(&dir, &format!("2026-01-02T00-00-00-{CHILD}"), &rows)?;
        write(
            &dir,
            &format!("2026-01-03T00-00-00-{REVERT}"),
            &[
                header(REVERT, end, Some(bookmark(CHILD, &rows)), false),
                message(end + 1, "detached request"),
            ],
        )?;
        let adapter = CodexAdapter::with_root(home.path());
        for id in [CHILD, REVERT] {
            let snapshot = adapter.read_session(&SessionRef::new(Provider::Codex, id))?;
            let conversation = omnis_core::import_conversation(&snapshot);
            let texts: Vec<_> = conversation
                .messages
                .iter()
                .map(|m| m.text.as_str())
                .collect();
            for text in ["first pair", "second pair"] {
                assert_eq!(
                    texts.contains(&text),
                    ending != "turn_aborted",
                    "{id} {ending}: {texts:?}"
                );
            }
            assert!(
                !texts.contains(&"foreign pair")
                    && !texts.contains(&"interrupted parent generation"),
                "{id} {ending}: {texts:?}"
            );
        }
    }
    Ok(())
}

#[test]
fn attached_saved_pairs_require_consistent_turn_ownership() -> Result<()> {
    let home = tempfile::tempdir()?;
    let dir = home.path().join("sessions");
    for case in [
        "child",
        "copied",
        "copied item",
        "child abort",
        "several before abort",
        "foreign abort",
        "wrong turn abort",
        "closed parent",
        "wrong close",
        "root start",
        "foreign owner",
        "turn mismatch",
        "item mismatch",
    ] {
        let rows = scoped_pair_rows(&dir, case)?;
        let end = rows.len() + 2;
        write(&dir, &format!("2026-01-02T00-00-00-{CHILD}"), &rows)?;
        write(
            &dir,
            &format!("2026-01-03T00-00-00-{REVERT}"),
            &[
                header(REVERT, end, Some(bookmark(CHILD, &rows)), false),
                message(end + 1, "detached request"),
            ],
        )?;
        for id in [CHILD, REVERT] {
            let snapshot = CodexAdapter::with_root(home.path())
                .read_session(&SessionRef::new(Provider::Codex, id))?;
            let conversation = omnis_core::import_conversation(&snapshot);
            if matches!(case, "child" | "child abort") {
                assert!(!conversation.truncated, "{id} {case}");
            }
            if case == "several before abort" {
                assert!(conversation.truncated);
                assert!(
                    conversation
                        .messages
                        .iter()
                        .any(|m| m.text == "earlier child")
                );
                assert!(
                    !conversation
                        .messages
                        .iter()
                        .any(|m| m.text == "unplaced parent")
                );
            }
            let messages = conversation.messages;
            assert_eq!(
                messages
                    .iter()
                    .filter(|m| m.text == "paired request")
                    .count(),
                usize::from(matches!(
                    case,
                    "child"
                        | "child abort"
                        | "several before abort"
                        | "closed parent"
                        | "root start"
                )),
                "{id} {case}"
            );
        }
    }
    Ok(())
}

fn scoped_pair_rows(dir: &Path, case: &str) -> Result<Vec<Value>> {
    let mut rows = interrupted_fork(dir)?;
    rows.truncate(2);
    if matches!(
        case,
        "copied" | "copied item" | "closed parent" | "wrong close" | "root start"
    ) {
        let kind = if case == "copied item" {
            "item_started"
        } else {
            "task_started"
        };
        rows.push(json!({"type":"event_msg","ordinal":4,"payload":{"type":kind,"turn_id":"scope","thread_id":PARENT}}));
        if matches!(case, "closed parent" | "wrong close") {
            let turn = if case == "closed parent" {
                "scope"
            } else {
                "another"
            };
            rows.push(json!({"type":"event_msg","ordinal":5,"payload":{"type":"task_complete","turn_id":turn}}));
        }
        if case == "root start" {
            rows.push(json!({"type":"event_msg","ordinal":5,"thread_id":CHILD,"payload":{"type":"task_started","turn_id":"child-turn"}}));
        }
    }
    if case == "several before abort" {
        rows.push(json!({"type":"event_msg","ordinal":4,"payload":{"type":"damaged_marker"}}));
        for (ordinal, text, owned) in [(5, "unplaced parent", false), (7, "earlier child", true)] {
            let mut raw = message(ordinal, text);
            let mut saved = json!({"type":"event_msg","ordinal":ordinal+1,"payload":{"type":"user_message","message":text}});
            if owned {
                for row in [&mut raw, &mut saved] {
                    row["payload"]["thread_id"] = json!(CHILD);
                    row["payload"]["turn_id"] = json!("earlier");
                }
            }
            rows.extend([raw, saved]);
        }
    }
    let ordinal = rows.len() + 2;
    let mut raw = message(ordinal, "paired request");
    raw["payload"]["id"] = json!("pair");
    raw["payload"]["turn_id"] = json!("scope");
    let mut saved = json!({"type":"event_msg","ordinal":ordinal+1,"payload":{"type":"user_message","message":"paired request","item_id":"pair","turn_id":"scope"}});
    if !case.starts_with("copied") {
        raw["payload"]["thread_id"] = json!(CHILD);
        saved["payload"]["thread_id"] = json!(CHILD);
    }
    match case {
        "closed parent" | "wrong close" => {
            raw["payload"].as_object_mut().unwrap().remove("turn_id");
            saved["payload"].as_object_mut().unwrap().remove("turn_id");
        }
        "root start" => {
            raw["payload"]["role"] = json!("assistant");
            raw["payload"]["content"] = json!([{"type":"output_text","text":"paired request"}]);
            saved["payload"]["type"] = json!("agent_message");
            raw["payload"]["turn_id"] = json!("child-turn");
            saved["payload"]["turn_id"] = json!("child-turn");
        }
        "foreign owner" => saved["payload"]["thread_id"] = json!(PARENT),
        "turn mismatch" => saved["payload"]["turn_id"] = json!("another"),
        "item mismatch" => saved["payload"]["item_id"] = json!("another"),
        _ => {}
    }
    rows.extend([raw, saved]);
    if case.ends_with("abort") {
        let owner = if case == "foreign abort" {
            PARENT
        } else {
            CHILD
        };
        let turn = if case == "wrong turn abort" {
            "another"
        } else {
            "scope"
        };
        rows.push(json!({"type":"event_msg","ordinal":ordinal+2,"payload":{"type":"turn_aborted","turn_id":turn,"thread_id":owner,"reason":"interrupted"}}));
    } else if case.starts_with("copied") {
        let kind = if case == "copied item" {
            "item_completed"
        } else {
            "task_complete"
        };
        rows.push(json!({"type":"event_msg","ordinal":ordinal+2,"payload":{"type":kind,"turn_id":"scope","thread_id":PARENT}}));
    } else {
        rows.push(json!({"type":"event_msg","ordinal":ordinal+2,"payload":{"type":"item_completed","thread_id":CHILD,"turn_id":"scope","item":{"type":"UserMessage","id":"pair","content":[{"type":"text","text":"paired request"}]}}}));
    }
    Ok(rows)
}

#[test]
fn damaged_attachment_boundaries_use_the_surviving_item_and_turn_evidence() -> Result<()> {
    let home = tempfile::tempdir()?;
    let dir = home.path().join("sessions");
    for case in [
        "next parent item",
        "null turns",
        "pending receipt",
        "wrong receipt",
        "child reply",
        "missing abort turn",
    ] {
        let base = match case {
            "next parent item" => "copied item",
            "child reply" | "missing abort turn" => "child abort",
            _ => "child",
        };
        let mut rows = scoped_pair_rows(&dir, base)?;
        match case {
            "next parent item" => {
                rows[2]["payload"]["item"] = json!({"id":"previous"});
                let mut complete = rows[2].clone();
                complete["payload"]["type"] = json!("item_completed");
                rows.insert(3, complete);
            }
            "pending receipt" | "wrong receipt" | "null turns" => {
                for row in &mut rows[2..4] {
                    row["payload"]["turn_id"] = Value::Null;
                }
                if case != "null turns" {
                    rows.insert(2,json!({"type":"event_msg","payload":{"type":"item_started","thread_id":PARENT,"turn_id":"parent","item":{"id":"parent item"}}}));
                    if case == "wrong receipt" {
                        rows[5]["payload"]["item"]["id"] = json!("different");
                    }
                }
            }
            "child reply" => {
                rows.insert(4,json!({"type":"response_item","payload":{"type":"message","role":"assistant","thread_id":CHILD,"turn_id":"scope","id":"reply","content":[{"type":"output_text","text":"paired answer"}]}}));
                rows.insert(5,json!({"type":"event_msg","payload":{"type":"agent_message","thread_id":CHILD,"turn_id":"scope","item_id":"reply","message":"paired answer"}}));
            }
            _ => {
                rows[4]["payload"]
                    .as_object_mut()
                    .unwrap()
                    .remove("turn_id");
            }
        }
        for (index, row) in rows.iter_mut().enumerate() {
            row["ordinal"] = json!(index + 2);
        }
        let end = rows.len() + 2;
        write(&dir, &format!("2026-01-02T00-00-00-{CHILD}"), &rows)?;
        write(
            &dir,
            &format!("2026-01-03T00-00-00-{REVERT}"),
            &[
                header(REVERT, end, Some(bookmark(CHILD, &rows)), false),
                message(end + 1, "detached request"),
            ],
        )?;
        for id in [CHILD, REVERT] {
            let snapshot = CodexAdapter::with_root(home.path())
                .read_session(&SessionRef::new(Provider::Codex, id))?;
            let conversation = omnis_core::import_conversation(&snapshot);
            let contains = |text| conversation.messages.iter().any(|m| m.text == text);
            assert_eq!(
                contains("paired request"),
                matches!(case, "null turns" | "pending receipt" | "child reply"),
                "{id} {case}"
            );
            assert_eq!(
                contains("paired answer"),
                case == "child reply",
                "{id} {case}"
            );
            if case == "missing abort turn" {
                assert!(conversation.truncated, "{id} {case}");
            }
        }
    }
    Ok(())
}
