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
