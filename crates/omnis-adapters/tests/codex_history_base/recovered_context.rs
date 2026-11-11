use super::*;

fn fork_through(home: &Path, middle: &[Value]) -> anyhow::Result<(CodexAdapter, SessionRef)> {
    let base = [
        header(PARENT, 0, None),
        json!({"ordinal":1,"type":"turn_context","payload":{"cwd":home.join("older")}}),
        turn(2, "user", "grandparent request"),
    ];
    let save =
        |owner, body: &[Value]| write(home, "sessions", owner, owner, "2026-01-01T00-00-00", body);
    save(PARENT, &base);
    let mut rows = vec![header(CHILD, 3, Some(bookmark(PARENT, &base)))];
    rows.extend_from_slice(middle);
    let path = save(CHILD, &rows);
    save(
        REVERT,
        &[
            header(
                REVERT,
                middle.last().unwrap()["ordinal"].as_u64().unwrap() + 1,
                Some(bookmark(CHILD, &rows)),
            ),
            turn(
                middle.last().unwrap()["ordinal"].as_u64().unwrap() + 2,
                "user",
                "leaf request",
            ),
        ],
    );
    let body = fs::read_to_string(&path)?;
    fs::write(path, body.replace("\"ordinal\":5", "\"ordinaX\":5"))?;
    Ok((
        CodexAdapter::with_root(home),
        SessionRef::new(Provider::Codex, REVERT),
    ))
}

#[test]
fn recovered_context_updates_the_project_and_rollback_restores_the_old_project()
-> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let older = home.path().join("older");
    let newer = home.path().join("newer");
    fs::create_dir(&older)?;
    fs::create_dir(&newer)?;
    for rolled_back in [false, true] {
        let mut rows = vec![
            json!({"ordinal":4,"type":"event_msg","payload":{"type":"task_started","turn_id":"middle"}}),
            json!({"ordinal":5,"type":"turn_context","payload":{"turn_id":"middle","cwd":newer}}),
            turn(6, "user", "middle request"),
            turn(7, "assistant", "middle reply"),
        ];
        if rolled_back {
            rows.push(json!({"ordinal":8,"type":"event_msg","payload":{"thread_id":CHILD,"type":"thread_rolled_back","num_turns":1}}));
        }
        let (adapter, session) = fork_through(home.path(), &rows)?;
        let project = if rolled_back { &older } else { &newer };
        assert!(
            adapter
                .project_search_candidates(project)?
                .0
                .iter()
                .any(|s| s.session == session)
        );
        for snapshot in [
            adapter.read_session(&session)?,
            adapter.preview_session(&session)?,
            adapter.index_session(&session, None, 1)?.snapshot,
        ] {
            assert_eq!(&snapshot.workspace.current_dir, project);
        }
    }
    Ok(())
}

#[test]
fn an_unverified_directory_clears_the_preceding_workspace() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let old = home.path().join("old-middle");
    let (adapter, session) = fork_through(
        home.path(),
        &[
            json!({"ordinal":4,"type":"turn_context","payload":{"cwd":old}}),
            json!({"ordinal":5,"type":"turn_context","payload":{"cwd":42}}),
            turn(6, "user", "after uncertain directory"),
        ],
    )?;
    let read = adapter.index_session(&session, None, u64::MAX)?;
    assert_eq!(
        read.workspace_projection,
        omnis_adapters::CodexWorkspaceProjection::Observed(None)
    );
    assert_ne!(read.snapshot.workspace.current_dir, old);
    Ok(())
}

#[test]
fn recognized_nonconversation_records_keep_the_verified_grandparent() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let receipt = |owner, content: Value| {
        json!({"ordinal":5,"type":"event_msg","payload":{
        "type":"item_completed","thread_id":owner,"turn_id":"middle","item":{
            "type":"UserMessage","id":"request","content":content}}})
    };
    for (gap, retains) in [
        (turn(5, "developer", "developer instructions"), true),
        (
            receipt(CHILD, json!([{"type":"text","text":"middle request"}])),
            true,
        ),
        (
            receipt(PARENT, json!([{"type":"text","text":"middle request"}])),
            false,
        ),
        (receipt(CHILD, json!(false)), false),
    ] {
        let mut request = turn(4, "user", "middle request");
        request["payload"]["id"] = json!("request");
        let (adapter, session) = fork_through(
            home.path(),
            &[request, gap, turn(6, "assistant", "middle reply")],
        )?;
        let snapshot = adapter.read_session(&session)?;
        let conversation = omnis_core::import_conversation(&snapshot);
        assert_eq!(
            conversation
                .messages
                .iter()
                .any(|m| m.text == "grandparent request"),
            retains
        );
        assert!(
            !conversation
                .messages
                .iter()
                .any(|m| m.text == "developer instructions")
        );
        assert!(
            conversation
                .messages
                .iter()
                .any(|m| m.text == "leaf request")
        );
    }
    Ok(())
}
