use super::*;

#[test]
fn an_unavailable_inherited_prefix_marks_transfer_and_search_incomplete() {
    let temporary = tempfile::tempdir().unwrap();
    let folder = temporary.path().join("sessions");
    fs::create_dir(&folder).unwrap();
    let source = folder.join(format!("rollout-{SESSION_ID}.jsonl"));
    let session = SessionRef::new(Provider::Codex, SESSION_ID);
    let base = json!({
        "thread_id": "11111111-1111-4111-8111-111111111111",
        "end_ordinal_exclusive": 3,
        "end_byte_offset": 512
    });
    let timestamp = "2026-01-05T09:00:00.000Z";
    for history_base in [None, Some(&Value::Null), Some(&base)] {
        let ordinal = history_base
            .and_then(|base| base["end_ordinal_exclusive"].as_u64())
            .unwrap_or(0);
        let mut header = json!({"type":"session_meta","timestamp":timestamp,"ordinal":ordinal,"payload":{
            "id":SESSION_ID,"cwd":"/workspace","history_mode":"paginated"
        }});
        if let Some(history_base) = history_base {
            header["payload"]["history_base"] = history_base.clone();
        }
        let mut body = page_user("turn-1", "item-1", "local child request");
        body["timestamp"] = timestamp.into();
        body["ordinal"] = (ordinal + 1).into();
        let source_text = format!("{header}\n{body}\n");
        fs::write(&source, &source_text).unwrap();
        let adapter = CodexAdapter::with_root(temporary.path());
        let incomplete = history_base.is_some_and(|value| !value.is_null());
        for snapshot in [
            adapter.read_session(&session).unwrap(),
            adapter.preview_session(&session).unwrap(),
        ] {
            let conversation = omnis_core::import_conversation(&snapshot);
            assert_eq!(conversation.messages.len(), 1);
            assert_eq!(conversation.messages[0].text, "local child request");
            assert_eq!(conversation.truncated, incomplete);
            let search = omnis_core::trajectory_search_document(&snapshot);
            assert!(search.text.contains("local child request"));
            assert_eq!(search.source_complete, !incomplete);
            let notice = snapshot.events.iter().find(|event| {
                event.source.raw_record_type.as_deref() == Some("omnisession.codex_history_base")
            });
            assert_eq!(notice.is_some(), incomplete);
            if let Some(notice) = notice {
                assert_eq!(notice.replay_policy, omnis_ir::ReplayPolicy::HistoricalOnly);
                assert_eq!(
                    notice.payload["native_history_unverified"].as_bool(),
                    Some(true)
                );
                assert_eq!(notice.payload["omitted_events"].as_u64(), Some(1));
            }
        }
        assert_eq!(fs::read_to_string(&source).unwrap(), source_text);
    }
}
