use super::*;

const TIME: &str = "2026-01-05T09:00:00Z";

fn stamped(mut record: Value, ordinal: u64) -> Value {
    record["ordinal"] = ordinal.into();
    record["timestamp"] = TIME.into();
    record
}

#[test]
fn a_verified_display_requires_the_next_available_native_position() {
    let question = |text| page_user("turn", "id", text);
    let prior = stamped(question("published"), 1);
    let mut explicit_start = stamped(question("revised"), 3);
    explicit_start["timestamp"] = "unparsed".into();
    explicit_start["payload"]["started_at_ms"] = 0.into();
    let mut reuse = stamped(question("discarded"), 2);
    reuse["timestamp"] = Value::Null;
    let mut bad_fallback = stamped(question("discarded"), 2);
    bad_fallback["timestamp"] = "unparsed".into();
    let future_unknown = stamped(
        json!({"type":"future_row","payload":{"text":"private"}}),
        11,
    );
    let missing = |key| {
        let mut record = stamped(question("discarded"), 2);
        record.as_object_mut().unwrap().remove(key);
        record
    };
    let good = stamped(question("revised"), 2);
    for (rows, expected, skipped, gap) in [
        (
            vec![stamped(question("discarded"), 1)],
            "published",
            1,
            false,
        ),
        (
            vec![missing("ordinal"), missing("timestamp")],
            "published",
            2,
            false,
        ),
        (vec![reuse, good.clone()], "revised", 1, false),
        (vec![bad_fallback, good.clone()], "published", 2, false),
        (
            vec![
                stamped(
                    json!({"type":"response_item","payload":{"type":"message","role":"user"}}),
                    2,
                ),
                good.clone(),
            ],
            "revised",
            1,
            false,
        ),
        (
            vec![
                stamped(event("task_started", json!({"turn_id":12})), 2),
                good.clone(),
            ],
            "revised",
            1,
            false,
        ),
        (
            vec![
                stamped(
                    json!({"type":"response_item","payload":{"type":"unknown_model_record"}}),
                    2,
                ),
                good.clone(),
            ],
            "published",
            1,
            false,
        ),
        (vec![future_unknown, good], "revised", 1, false),
        (vec![explicit_start], "revised", 0, true),
    ] {
        for preview in [false, true] {
            let snap = read_with_history_mode(
                std::iter::once(prior.clone()).chain(rows.clone()).collect(),
                preview,
                Some("paginated"),
            )
            .unwrap();
            assert_eq!(visible_text(&snap), [expected]);
            let notice = snap
                .events
                .iter()
                .find(|e| {
                    e.source.raw_record_type.as_deref() == Some("omnisession.codex_rollout_line")
                })
                .unwrap();
            assert_eq!(notice.payload["omitted_events"], skipped);
            assert_eq!(notice.payload["ordinal_gap"], gap);
            let search = omnis_core::trajectory_search_document(&snap);
            assert!(!search.source_complete);
            assert!(!search.text.contains("discarded") && !search.text.contains("private"));
            assert!(omnis_core::import_conversation(&snap).truncated);
        }
    }
}

#[test]
fn an_unpublished_ordinal_still_updates_the_native_model_checkpoint_if_it_decodes() {
    let mut records = paginated_visibility::exchange("turn", "request", "answer", true);
    let window = |kind: &str, window: u64| {
        let item = records
            .iter()
            .find(|r| r["payload"]["role"] == kind)
            .unwrap();
        json!({"type":"compacted","payload":{"message":"","window_number":window,"replacement_history":[item["payload"]]}})
    };
    let first = stamped(window("user", 1), 7);
    let second = stamped(window("assistant", 2), 6);
    let mut invalid = stamped(window("user", 3), 8);
    invalid["timestamp"] = Value::Null;
    records.extend([first, second, invalid]);
    for preview in [false, true] {
        let snap = read_with_history_mode(records.clone(), preview, Some("paginated")).unwrap();
        assert_eq!(published_text(&snap), ["request", "answer"]);
        let conversation = omnis_core::import_conversation(&snap);
        assert_eq!(conversation.messages.len(), 1);
        assert_eq!(conversation.messages[0].text, "answer");
        assert!(conversation.truncated);
    }
}

#[test]
fn an_unterminated_display_stays_pending_and_damaged_middle_lines_do_not_take_its_ordinal() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("sessions");
    fs::create_dir(&dir).unwrap();
    let path = dir.join(format!("rollout-{SESSION_ID}.jsonl"));
    let header = stamped(
        json!({"type":"session_meta","payload":{"id":SESSION_ID,"history_mode":"paginated","cwd":"/workspace"}}),
        0,
    );
    let old = stamped(page_user("turn", "id", "prior"), 1);
    let new = stamped(page_user("turn", "id", "completed"), 2);
    let session = SessionRef::new(Provider::Codex, SESSION_ID);
    for (newline, expected) in [("", "prior"), ("\n", "completed")] {
        let source = format!("{header}\n{old}\n{{invalid\n{new}{newline}");
        fs::write(&path, &source).unwrap();
        let adapter = CodexAdapter::with_root(root.path());
        for snap in [
            adapter.read_session(&session).unwrap(),
            adapter.preview_session(&session).unwrap(),
        ] {
            assert_eq!(visible_text(&snap), [expected]);
            assert!(!omnis_core::trajectory_search_document(&snap).source_complete);
        }
        assert_eq!(fs::read_to_string(&path).unwrap(), source);
    }
}
