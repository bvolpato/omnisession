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
                if !unread_parent {
                    assert!(omnis_core::session_message_prefix_is_known(&snapshot, true));
                    let report = fidelity_report_for_snapshot(&snapshot, Provider::Claude, true);
                    assert!(
                        report
                            .warnings
                            .iter()
                            .any(|w| w.contains("omits that copied context"))
                    );
                }
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
