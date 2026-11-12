use std::{fs, io::Write, path::Path};

use anyhow::Result;
use flate2::{Compression, write::GzEncoder};
use omnis_adapters::{CodexAdapter, CodexWorkspaceProjection, ProviderAdapter};
use omnis_ir::{Provider, SessionRef, SourceRevision};
use serde_json::json;

const ID: &str = "44444444-4444-4444-8444-444444444444";

fn write_rollout(path: &Path, contents: &str) -> Result<SourceRevision> {
    let bytes = match path.extension().unwrap().to_str().unwrap() {
        "gz" => {
            let mut writer = GzEncoder::new(Vec::new(), Compression::fast());
            writer.write_all(contents.as_bytes())?;
            writer.finish()?
        }
        "zst" => zstd::encode_all(contents.as_bytes(), 1)?,
        _ => contents.as_bytes().to_vec(),
    };
    fs::write(path, bytes)?;
    Ok(CodexAdapter::index_file_revision(path).unwrap())
}

#[test]
fn selected_workspace_retains_only_known_directories_across_encodings() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let session = SessionRef::new(Provider::Codex, ID);
    let initial = temp.path().join("creation");
    let next = temp.path().join("next");
    let final_dir = temp.path().join("final");
    let header = json!({"type":"session_meta","payload":{"id":ID,"cwd":initial}});
    let moved = json!({"type":"turn_context","payload":{"cwd":next}});
    let recovered = json!({"type":"turn_context","payload":{"cwd":final_dir}});
    let harmless = r#"{"type":"response_item","payload":{"text":"turn_context"#;
    for extension in ["jsonl", "jsonl.gz", "jsonl.zst"] {
        let path = temp.path().join(format!("rollout-{ID}.{extension}"));
        let mut cases = vec![(harmless.to_owned(), Some(next.as_path()))];
        for torn in [
            r#"{"type":"turn_context","payload":{}}"#,
            r#"{"type":"turn_context","payload":{"cwd":7}}"#,
            r#"{"type":"turn_context","payload":{"cwd":""}}"#,
            r#"{"type":"turn_context","payload":{"cwd":"bad\u0000path"}}"#,
            r#"{"type":"turn_context","payload":"#,
            r#"{"type":"turn_co"#,
            r#"{"type":"t"#,
            r#"{"t"#,
            "{",
        ] {
            cases.push((torn.to_owned(), None));
            cases.push((format!("{torn}\n{recovered}"), Some(final_dir.as_path())));
        }
        for (suffix, effective) in cases {
            let revision = write_rollout(&path, &format!("{header}\n{moved}\n{suffix}\n"))?;
            let (actual, creation) = CodexAdapter::rollout_workspaces(&session, &path, revision)?;
            assert_eq!(actual.as_deref(), effective, "{extension} {suffix}");
            assert_eq!(creation.as_deref(), Some(initial.as_path()));
        }
    }
    Ok(())
}

#[test]
fn workspace_settings_belong_to_the_thread_and_survive_turn_rollback() -> Result<()> {
    let root = tempfile::tempdir()?;
    let id = "a4444444-4444-4444-8444-444444444444";
    let creation = root.path().join("creation");
    let saved = root.path().join("saved");
    let header = json!({"type":"session_meta","payload":{"id":id,"cwd":creation,"git":{"branch":"creation"}}});
    let context = json!({"type":"turn_context","payload":{"cwd":creation}});
    let request = json!({"type":"event_msg","payload":{"type":"user_message","message":"request"}});
    let rollback =
        json!({"type":"event_msg","payload":{"type":"thread_rolled_back","num_turns":1}});
    let settings = |owner: &str, cwd: &Path| {
        json!({"type":"event_msg","payload":{
            "type":"thread_settings_applied", "thread_id":owner, "thread_settings":{
                "cwd":cwd, "model":"fixture", "model_provider_id":"synthetic",
                "approval_policy":"never", "approvals_reviewer":"user", "permission_profile":{"type":"disabled"},
                "collaboration_mode":{"mode":"default","settings":{"model":"fixture"}}
            }
        }})
    };
    let owned = settings(id, &saved);
    let alternate_id = id.replace('-', "").to_ascii_uppercase();
    let alternate_owner = settings(&alternate_id, &saved);
    let foreign = settings("55555555-5555-4555-8555-555555555555", &saved);
    let replacement = settings(id, &creation);
    let mut invalid = owned.clone();
    invalid["payload"]["thread_settings"] = json!({"cwd":saved});
    let torn_turn = r#"{"type":"turn_context","payload":"#;
    let torn_settings =
        r#"{"type":"event_msg","payload":{"type":"thread_settings_applied","thread_settings":"#;
    let torn_alternate = format!(
        r#"{{"type":"event_msg","payload":{{"type":"thread_settings_applied","thread_id":"{alternate_id}","thread_settings":"#
    );
    let cases = [
        (
            format!("{context}\n{request}\n{owned}"),
            Some(saved.as_path()),
        ),
        (
            format!("{owned}\n{context}\n{request}\n{rollback}"),
            Some(saved.as_path()),
        ),
        (
            format!("{context}\n{request}\n{owned}\n{replacement}\n{rollback}"),
            Some(creation.as_path()),
        ),
        (
            format!("{owned}\n{foreign}\n{context}"),
            Some(saved.as_path()),
        ),
        (format!("{foreign}\n{context}"), Some(creation.as_path())),
        (
            format!("{alternate_owner}\n{context}"),
            Some(saved.as_path()),
        ),
        (format!("{owned}\n{invalid}\n{context}"), None),
        (format!("{owned}\n{torn_alternate}\n{context}"), None),
        (
            format!("{owned}\n{torn_turn}\n{context}"),
            Some(saved.as_path()),
        ),
        (format!("{owned}\n{torn_settings}\n{context}"), None),
        (
            format!("{torn_settings}\n{owned}\n{context}"),
            Some(saved.as_path()),
        ),
    ];
    let session = SessionRef::new(Provider::Codex, id);
    for ext in ["jsonl", "jsonl.gz", "jsonl.zst"] {
        let home = root.path().join(ext);
        let folder = home.join("sessions");
        fs::create_dir_all(&folder)?;
        let path = folder.join(format!("rollout-{id}.{ext}"));
        let adapter = CodexAdapter::with_root(&home);
        for (rows, expected) in &cases {
            let revision = write_rollout(&path, &format!("{header}\n{rows}\n"))?;
            let (directory, startup) = CodexAdapter::rollout_workspaces(&session, &path, revision)?;
            assert_eq!(directory.as_deref(), *expected, "{ext}: {rows}");
            assert!(startup.is_none(), "{ext}: {rows}");
            let sampled = adapter.index_session(&session, Some(&path), 1)?;
            assert_eq!(
                sampled.workspace_projection,
                CodexWorkspaceProjection::Observed(directory)
            );
            for snapshot in [sampled.snapshot, adapter.read_session(&session)?] {
                assert_eq!(
                    snapshot.workspace.current_dir,
                    expected.unwrap_or(Path::new("")),
                    "{ext}: {rows}"
                );
                assert_eq!(
                    snapshot.workspace.git.branch.as_deref(),
                    (*expected == Some(creation.as_path())).then_some("creation")
                );
            }
        }
    }
    Ok(())
}

#[test]
fn cached_and_uncached_workspace_selection_decode_the_same_rollback() -> Result<()> {
    let root = tempfile::tempdir()?;
    let session = SessionRef::new(Provider::Codex, ID);
    let header =
        json!({"type":"session_meta","payload":{"id":ID,"cwd":root.path().join("original")}});
    let turn = |id: &str, cwd: &str| {
        [
            json!({"type":"event_msg","payload":{"type":"task_started","turn_id":id}}),
            json!({"type":"turn_context","payload":{"turn_id":id,"cwd":root.path().join(cwd)}}),
            json!({"type":"event_msg","payload":{"type":"user_message","message":id}}),
            json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":id}}),
        ]
        .map(|row| row.to_string())
        .join("\n")
    };
    let first = turn("first", "original");
    let second = turn("second", "second");
    for ext in ["jsonl", "jsonl.gz", "jsonl.zst"] {
        let home = root.path().join(ext);
        let folder = home.join("sessions");
        fs::create_dir_all(&folder)?;
        let path = folder.join(format!("rollout-{ID}.{ext}"));
        let adapter = CodexAdapter::with_root(&home);
        for (payload, expected) in [
            (
                json!({"type":"thread_rolled_back","num_turns":1}),
                "original",
            ),
            (json!({"type":14,"num_turns":1}), "original"),
            (json!(["thread_rolled_back", 1]), "original"),
            (json!([14, 1]), "original"),
            (json!([14, 0]), "second"),
            (json!([14, 4_294_967_296_u64]), "second"),
            (json!([14, 1, 2]), "second"),
        ] {
            let rollback = json!({"type":"event_msg","payload":payload});
            let revision =
                write_rollout(&path, &format!("{header}\n{first}\n{second}\n{rollback}\n"))?;
            let (uncached, _) = CodexAdapter::rollout_workspaces(&session, &path, revision)?;
            assert_eq!(
                uncached.as_deref(),
                Some(root.path().join(expected).as_path()),
                "{ext} {payload}"
            );
            let cached = adapter.index_session(&session, Some(&path), 1)?;
            assert_eq!(
                cached.workspace_projection,
                CodexWorkspaceProjection::Observed(uncached)
            );
            assert_eq!(
                adapter.read_session(&session)?.workspace.current_dir,
                root.path().join(expected)
            );
        }
    }
    Ok(())
}

#[test]
fn selected_workspace_checks_the_source_and_uses_surviving_turns() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let session = SessionRef::new(Provider::Codex, ID);
    let path = temp.path().join(format!("rollout-{ID}.jsonl"));
    let creation = temp.path().join("creation");
    let header = json!({"type":"session_meta","payload":{"id":ID,"cwd":creation}});
    let moved = json!({"type":"turn_context","payload":{"cwd":temp.path().join("discarded")}});
    let user = json!({"type":"event_msg","payload":{"type":"user_message","message":"gone"}});
    let rollback =
        json!({"type":"event_msg","payload":{"type":"thread_rolled_back","num_turns":1}});
    let revision = write_rollout(&path, &format!("{header}\n{moved}\n{user}\n{rollback}\n"))?;
    assert_eq!(
        CodexAdapter::rollout_workspaces(&session, &path, revision)?
            .0
            .as_deref(),
        Some(creation.as_path())
    );
    assert!(CodexAdapter::rollout_workspaces(&session, &path, SourceRevision::default()).is_err());
    let wrong_header = json!({"type":"session_meta","payload":{"id":"55555555-5555-4555-8555-555555555555","cwd":creation}});
    let current = write_rollout(&path, &wrong_header.to_string())?;
    assert!(CodexAdapter::rollout_workspaces(&session, &path, revision).is_err());
    assert!(CodexAdapter::rollout_workspaces(&session, &path, current).is_err());
    Ok(())
}

#[test]
fn rollback_restores_only_workspaces_established_after_the_latest_damaged_record() -> Result<()> {
    let root = tempfile::tempdir()?;
    let session = SessionRef::new(Provider::Codex, ID);
    let header =
        json!({"type":"session_meta","payload":{"id":ID,"cwd":root.path().join("creation")}});
    let damaged = r#"{"type":"turn_context","payload":{"cwd":"torn"#;
    let started = json!({"type":"event_msg","payload":{"type":"task_started"}});
    let reply = json!({"type":"event_msg","payload":{"type":"agent_message","message":"reply"}});
    let rollback =
        json!({"type":"event_msg","payload":{"type":"thread_rolled_back","num_turns":1}});
    let turn = |directory: &str| {
        let context = json!({"type":"turn_context","payload":{"cwd":root.path().join(directory)}});
        let user =
            json!({"type":"event_msg","payload":{"type":"user_message","message":directory}});
        format!("{started}\n{context}\n{user}\n{reply}")
    };
    let history = format!(
        "{header}\n{}\n{damaged}\n{}\n{}",
        turn("creation"),
        turn("B"),
        turn("C")
    );
    let cases = [
        (rollback.to_string(), Some("B")),
        (format!("{rollback}\n{rollback}"), None),
        (format!("{rollback}\n{rollback}\n{}", turn("D")), Some("D")),
        (format!("{damaged}\n{}\n{rollback}", turn("D")), None),
    ];
    for extension in ["jsonl", "jsonl.gz", "jsonl.zst"] {
        let home = root.path().join(extension);
        let sessions = home.join("sessions");
        fs::create_dir_all(&sessions)?;
        let path = sessions.join(format!("rollout-{ID}.{extension}"));
        let adapter = CodexAdapter::with_root(&home);
        for (suffix, expected) in &cases {
            let revision = write_rollout(&path, &format!("{history}\n{suffix}\n"))?;
            let expected = expected.map(|name| root.path().join(name));
            let (workspace, _) = CodexAdapter::rollout_workspaces(&session, &path, revision)?;
            assert_eq!(workspace, expected, "{extension}, {suffix}");
            let preview = adapter.index_session(&session, Some(&path), 1)?;
            assert_eq!(
                preview.workspace_projection,
                CodexWorkspaceProjection::Observed(workspace),
                "{extension}, {suffix}"
            );
            for snapshot in [preview.snapshot, adapter.read_session(&session)?] {
                assert_eq!(
                    snapshot.workspace.current_dir,
                    expected.as_deref().unwrap_or_else(|| Path::new(""))
                );
                assert_eq!(
                    snapshot
                        .events
                        .iter()
                        .any(|e| e.payload["workspace_unknown"] == true),
                    expected.is_none()
                );
                assert!(omnis_core::import_conversation(&snapshot).truncated);
            }
        }
    }
    Ok(())
}

#[test]
fn a_missing_prefix_requires_a_surviving_local_directory_for_native_routing() -> Result<()> {
    let temp = tempfile::tempdir()?;
    fs::create_dir(temp.path().join("sessions"))?;
    let session = SessionRef::new(Provider::Codex, ID);
    let path = temp.path().join(format!("sessions/rollout-{ID}.jsonl"));
    let adapter = CodexAdapter::with_root(temp.path());
    let creation = temp.path().join("creation");
    let next = temp.path().join("next");
    let inherited = json!({"type":"session_meta","payload":{
        "id":ID,"cwd":creation,"history_mode":"paginated",
        "history_base":{"thread_id":"55555555-5555-4555-8555-555555555555",
        "end_ordinal_exclusive":2, "end_byte_offset":512}
    }});
    let moved = json!({"type":"turn_context","payload":{"turn_id":"first","cwd":next}});
    let later = json!({"type":"turn_context","payload":{"turn_id":"second","cwd":temp.path().join("later")}});
    let started =
        |id: &str| json!({"type":"event_msg","payload":{"type":"task_started","turn_id":id}});
    let first = started("first");
    let second = started("second");
    let request = |turn: &str| {
        let raw = json!({"type":"response_item","payload":{"type":"message","role":"user",
            "internal_chat_message_metadata_passthrough":{"turn_id":turn},"id":turn,
            "content":[{"type":"input_text","text":turn}]}});
        let display = json!({"type":"event_msg","payload":{"type":"item_completed",
            "thread_id":ID,"turn_id":turn,
            "item":{"type":"UserMessage","id":turn,"content":[{"type":"text","text":turn}]}}});
        format!("{raw}\n{display}")
    };
    let user = request("first");
    let another = request("second");
    let rollback =
        json!({"type":"event_msg","payload":{"type":"thread_rolled_back","num_turns":1}});
    let write = |header: &serde_json::Value, tail: &str| {
        let mut document = String::new();
        for (index, mut row) in std::iter::once(header.clone())
            .chain(tail.lines().map(|line| serde_json::from_str(line).unwrap()))
            .enumerate()
        {
            row["ordinal"] = (2 + index).into();
            row["timestamp"] = "2026-01-01T00:00:00.000Z".into();
            document.push_str(&row.to_string());
            document.push('\n');
        }
        write_rollout(&path, &document)
    };
    for (tail, effective) in [
        (String::new(), None),
        (moved.to_string(), Some(next.as_path())),
        (format!("{moved}\n{user}\n{rollback}"), None),
        (
            format!("{moved}\n{user}\n{rollback}\n{moved}"),
            Some(next.as_path()),
        ),
        (
            format!("{first}\n{moved}\n{user}\n{second}\n{later}\n{another}\n{rollback}"),
            Some(next.as_path()),
        ),
    ] {
        let inherited_revision = write(&inherited, &tail)?;
        let (effective_dir, startup) =
            CodexAdapter::rollout_workspaces(&session, &path, inherited_revision)?;
        assert_eq!(effective_dir.as_deref(), effective, "{tail}");
        assert_eq!(startup.as_deref(), Some(creation.as_path()));
        let read = adapter.index_session(&session, Some(&path), u64::MAX)?;
        assert_eq!(
            read.workspace_projection,
            CodexWorkspaceProjection::Observed(effective_dir),
            "{tail}"
        );
        if effective.is_none() {
            assert_eq!(read.snapshot.workspace.current_dir, creation);
        }
    }
    let mut subagent = inherited;
    subagent["payload"]["agent_role"] = json!("explorer");
    for (tail, directory) in [(String::new(), None), (moved.to_string(), Some(next))] {
        write(&subagent, &tail)?;
        let read = adapter.index_session(&session, Some(&path), u64::MAX)?;
        assert!(read.native_session.is_none());
        assert_eq!(
            read.workspace_projection,
            CodexWorkspaceProjection::Observed(directory)
        );
    }
    Ok(())
}

#[test]
fn routing_and_project_search_use_only_visible_subagent_directories() -> Result<()> {
    let root = tempfile::tempdir()?;
    let sessions = root.path().join("sessions/2026/01/02");
    fs::create_dir_all(&sessions)?;
    let path = sessions.join(format!("rollout-2026-01-02T00-00-00-{ID}.jsonl"));
    let session = SessionRef::new(Provider::Codex, ID);
    let creation = root.path().join("creation");
    let copied = root.path().join("copied");
    let local = root.path().join("local");
    for directory in [&creation, &copied, &local] {
        fs::create_dir(directory)?;
    }
    let settings = json!({"timestamp":"2026-01-01T00:00:00Z","ordinal":1,"type":"event_msg","payload":{
        "type":"thread_settings_applied","thread_id":ID,"thread_settings":{
            "cwd":copied,"model":"fixture","model_provider_id":"synthetic","approval_policy":"never",
            "approvals_reviewer":"user","permission_profile":{"type":"disabled"},
            "collaboration_mode":{"mode":"default","settings":{"model":"fixture"}}
        }
    }});
    let turn = |ordinal: u64, cwd: &Path| json!({"timestamp":"2026-01-01T00:00:00Z","ordinal":ordinal,"type":"turn_context","payload":{"cwd":cwd}});
    let moved = turn(4, &local);
    let user = json!({"timestamp":"2026-01-01T00:00:00Z","ordinal":5,"type":"event_msg","payload":{"type":"user_message","message":"local request"}});
    let rollback = json!({"timestamp":"2026-01-01T00:00:00Z","ordinal":6,"type":"event_msg","payload":{"type":"thread_rolled_back","num_turns":1}});
    for mode in ["history_mode", "history_mxde"] {
        let header = json!({"timestamp":"2026-01-01T00:00:00Z","ordinal":0,"type":"session_meta","payload":{
            "id":ID,"cwd":creation,(mode):"paginated","subagent_history_start_ordinal":4
        }});
        for (tail, expected) in [
            (String::new(), &creation),
            (moved.to_string(), &local),
            (format!("{moved}\n{user}\n{rollback}"), &creation),
        ] {
            let revision = write_rollout(
                &path,
                &format!("{header}\n{settings}\n{}\n{tail}\n", turn(2, &copied)),
            )?;
            let (effective, startup) = CodexAdapter::rollout_workspaces(&session, &path, revision)?;
            assert_eq!(effective.as_ref(), Some(expected), "{mode} {tail}");
            assert!(startup.is_none());
            let adapter = CodexAdapter::with_root(root.path());
            assert!(adapter.project_search_candidates(&copied)?.0.is_empty());
            let (found, _) = adapter.project_search_candidates(&local)?;
            assert_eq!(
                found.iter().any(|row| row.session == session),
                expected == &local
            );
        }
    }
    Ok(())
}

#[test]
fn bounded_index_keeps_the_workspace_outside_the_retained_edges() -> Result<()> {
    let creation = std::env::temp_dir().join("creation");
    let continuation = std::env::temp_dir().join("continuation");
    let header = json!({"type":"session_meta","payload":{"id":ID,"cwd":creation,"git":{"branch":"creation-branch"}}});
    let message =
        |text: &str| json!({"type":"event_msg","payload":{"type":"agent_message","message":text}});
    let padding = format!(
        "{}\n",
        json!({"type":"progress","padding":"p".repeat(64 * 1024)})
    )
    .repeat(75);
    let moved = json!({"type":"turn_context","payload":{"cwd":continuation}})
        .to_string()
        .replace("turn_context", r"turn_\u0063ontext");
    let medium_progress = json!({"type":"progress","padding":"p".repeat(3 * 1024 * 1024)});
    let mut tail = format!(
        "{}\r\n{medium_progress}\n",
        message("first complete tail row")
    );
    let last = message("surviving reply").to_string();
    tail.extend(std::iter::repeat_n(
        ' ',
        4 * 1024 * 1024 - tail.len() - last.len() - 1,
    ));
    tail.push('\n');
    tail.push_str(&last);
    let contents = format!(
        "{header}\n{}\n{medium_progress}\n{}\n{padding}{moved}\n{}\n{padding}{tail}",
        message("thread_rolled_back inside a message"),
        message(&"q".repeat(4 * 1024 * 1024)),
        message("hidden between the retained edges"),
    );
    for extension in ["jsonl", "jsonl.gz", "jsonl.zst"] {
        let home = tempfile::tempdir()?;
        fs::create_dir(home.path().join("sessions"))?;
        let path = home
            .path()
            .join(format!("sessions/rollout-{ID}.{extension}"));
        let revision = write_rollout(&path, &contents)?;
        let read = CodexAdapter::with_root(home.path()).index_session(
            &SessionRef::new(Provider::Codex, ID),
            Some(&path),
            64 * 1024,
        )?;
        assert_eq!(read.source_revision, revision);
        assert!(!read.source_complete);
        assert_eq!(
            read.workspace_projection,
            CodexWorkspaceProjection::Observed(Some(continuation.clone()))
        );
        assert_eq!(read.snapshot.workspace.current_dir, continuation);
        assert!(read.snapshot.workspace.git.branch.is_none());
        let visible: Vec<_> = read
            .snapshot
            .events
            .iter()
            .filter_map(|event| event.payload["text"].as_str())
            .collect();
        assert!(visible.iter().all(|text| text.len() < 100));
        assert_eq!(
            visible,
            [
                "thread_rolled_back inside a message",
                "first complete tail row",
                "surviving reply"
            ],
            "{extension}"
        );
    }
    Ok(())
}
