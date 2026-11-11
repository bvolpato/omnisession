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
        assert_eq!(visible_text(&snap), ["request", "answer"]);
        let conversation = omnis_core::import_conversation(&snap);
        assert_eq!(conversation.messages.len(), 1);
        assert_eq!(conversation.messages[0].text, "answer");
        assert!(conversation.truncated);
    }
}

#[test]
fn a_trailing_documentary_record_can_reveal_a_missing_display_position() {
    let request = stamped(page_user("turn", "user", "kept request"), 1);
    let usage = |ordinal| {
        stamped(
            event(
                "token_count",
                json!({"rate_limits":{
                    "limit_id":"synthetic","primary":{"used_percent":10.5,"window_minutes":5},
                    "secondary":[20.0,null,null],"plan_type":"future_plan",
                    "credits":{"has_credits":true,"unlimited":false},
                    "individual_limit":{"limit":"100","used":"10","remaining_percent":90,"resets_at":123},
                    "rate_limit_reached_type":"workspace_member_usage_limit_reached"
                }}),
            ),
            ordinal,
        )
    };
    let opaque = |ordinal| stamped(event("future_event", json!({})), ordinal);
    for (tail, expected) in [
        (vec![usage(2)], None),
        (vec![usage(3)], Some((true, false))),
        (
            vec![
                stamped(
                    event(
                        "token_count",
                        json!({"rate_limits":{"primary":{"window_minutes":5}}}),
                    ),
                    2,
                ),
                usage(3),
            ],
            Some((true, false)),
        ),
        (vec![opaque(3)], Some((false, true))),
        (vec![opaque(4), usage(4)], Some((true, false))),
        (
            vec![opaque(4), usage(2), usage(3), usage(4)],
            Some((false, true)),
        ),
    ] {
        for preview in [false, true] {
            let snap = read_with_history_mode(
                std::iter::once(request.clone())
                    .chain(tail.clone())
                    .collect(),
                preview,
                Some("paginated"),
            )
            .unwrap();
            assert_eq!(visible_text(&snap), ["kept request"]);
            let notice = snap.events.iter().find(|e| {
                e.source.raw_record_type.as_deref() == Some("omnisession.codex_rollout_line")
            });
            assert_eq!(
                notice.map(|e| (
                    e.payload["ordinal_gap"] == true,
                    e.payload["possible_ordinal_gap"] == true
                )),
                expected
            );
            assert_eq!(
                omnis_core::trajectory_search_document(&snap).source_complete,
                expected.is_none()
            );
            assert_eq!(
                omnis_core::render_markdown_export(&snap).contains("source omissions"),
                expected.is_some()
            );
        }
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

#[test]
fn a_damaged_plan_cannot_replace_the_visible_proposal_or_claim_its_public_ordinal() {
    let prior = stamped(
        turn_item(
            "turn",
            &json!({"type":"Plan","id":"plan","text":"original proposal"}),
        ),
        1,
    );
    let mut invalid = json!({"type":"Plan","id":"plan"});
    let repaired = stamped(
        json!({"type":"event_msg","payload":["item_completed",SESSION_ID,"turn",["Plan","plan","updated proposal"],null,0]}),
        2,
    );
    for text in [None, Some(Value::Null), Some(7.into())] {
        if let Some(text) = text {
            invalid["text"] = text;
        }
        let broken = stamped(turn_item("turn", &invalid), 2);
        for (tail, expected) in [
            (vec![], "original proposal"),
            (vec![repaired.clone()], "updated proposal"),
        ] {
            for preview in [false, true] {
                let results = read_with_history_mode(
                    [prior.clone(), broken.clone()]
                        .into_iter()
                        .chain(tail.clone())
                        .collect(),
                    preview,
                    Some("paginated"),
                )
                .unwrap();
                assert_eq!(visible_text(&results), [expected]);
                let loss = results
                    .events
                    .iter()
                    .find(|e| {
                        e.source.raw_record_type.as_deref()
                            == Some("omnisession.codex_invalid_message")
                    })
                    .unwrap();
                assert_eq!(loss.payload["omitted_events"], 1);
                assert!(!omnis_core::trajectory_search_document(&results).source_complete);
            }
        }
    }
}

#[test]
fn published_items_can_be_revised_at_an_empty_turn_or_item_id() {
    let plan = |turn, id, text| turn_item(turn, &json!({"type":"Plan","id":id,"text":text}));
    let assistant = |text| {
        let content = json!([{"type":"Text","text":text}]);
        turn_item(
            "turn",
            &json!({"type":"AgentMessage","id":"","content":content}),
        )
    };
    let snap = read_with_history_mode(
        vec![
            plan("turn", "first", "original"),
            plan("turn", "", "other item"),
            plan("", "first", "other turn"),
            plan("", "first", "revised turn"),
            page_user("turn", "", "superseded request"),
            assistant("superseded answer"),
            page_user("turn", "", ""),
            assistant(""),
        ],
        false,
        Some("paginated"),
    )
    .unwrap();
    let conversation = omnis_core::import_conversation(&snap);
    let active: Vec<_> = conversation
        .messages
        .iter()
        .map(|m| m.text.as_str())
        .filter(|text| !text.is_empty())
        .collect();
    assert_eq!(active, ["original", "other item", "revised turn"]);
    assert!(!conversation.truncated);
}

#[test]
fn a_documentary_event_must_decode_to_reserve_its_public_position() {
    let prior = stamped(page_user("turn", "id", "original request"), 1);
    let revised = stamped(page_user("turn", "id", "updated request"), 2);
    for (wire, expected) in [
        (json!({"type":"user_message"}), "updated request"),
        (
            json!({"type":"token_count","rate_limits":17}),
            "updated request",
        ),
        (
            json!({"type":"token_count","rate_limits":{}}),
            "original request",
        ),
        (
            json!(["token_count", null, {"plan_type":{"plus":{}}}]),
            "original request",
        ),
        (
            json!({"type":"token_count","rate_limits":{"primary":{"window_minutes":5}}}),
            "updated request",
        ),
        (
            json!({"type":"token_count","rate_limits":{"secondary":[50,null,"bad reset"]}}),
            "updated request",
        ),
        (
            json!({"type":"token_count","rate_limits":{"credits":{"unlimited":true}}}),
            "updated request",
        ),
        (
            json!({"type":"token_count","rate_limits":{"rate_limit_reached_type":"future_reached"}}),
            "updated request",
        ),
        (
            json!({"type":"task_started","turn_id":"turn","collaboration_mode_kind":{"plan":{}}}),
            "original request",
        ),
        (
            json!({"type":59,"turn_id":"turn","reason":{"interrupted":{}}}),
            "original request",
        ),
        (
            json!({"type":20,"message":"documentary only","images":["image"],"image_order":[{"inline":{}}],"image_details":[{"high":{}}]}),
            "original request",
        ),
        (
            json!({"type":20,"message":"documentary only","image_order":[{"inline":{"unverified":true}}]}),
            "updated request",
        ),
        (
            json!({"type":"task_started","turn_id":"turn","collaboration_mode_kind":"future"}),
            "updated request",
        ),
        (
            json!({"type":"task_started","turn_id":"turn","collaboration_mode_kind":"pair_programming"}),
            "original request",
        ),
        (
            json!({"type":"agent_message","message":null}),
            "updated request",
        ),
        (
            json!({"type":"user_message","message":"documentary only","text_elements":null}),
            "updated request",
        ),
        (
            json!({"type":"agent_message","message":"documentary only","phase":"unknown"}),
            "updated request",
        ),
        (
            json!({"type":"user_message","message":""}),
            "original request",
        ),
        (
            json!(["agent_message", "documentary only"]),
            "original request",
        ),
    ] {
        let row = stamped(json!({"type":"event_msg","payload":wire}), 2);
        for preview in [false, true] {
            let snapshot = read_with_history_mode(
                vec![prior.clone(), row.clone(), revised.clone()],
                preview,
                Some("paginated"),
            )
            .unwrap();
            assert_eq!(visible_text(&snapshot), [expected]);
            let search = omnis_core::trajectory_search_document(&snapshot);
            assert!(!search.source_complete && !search.text.contains("documentary only"));
        }
    }
}

#[test]
fn rate_limit_enums_reserve_the_same_display_position_as_codex() {
    let prior = stamped(page_user("turn", "id", "published"), 1);
    for (limits, accepted) in [
        (json!({"plan_type":"plus"}), true),
        (json!({"plan_type":{"plus":{}}}), true),
        (json!({"plan_type":{"future_plan":null}}), true),
        (json!({"plan_type":{"future_plan":{}}}), true),
        (json!({"plan_type":{"plus":{"unexpected":true}}}), false),
        (
            json!({"rate_limit_reached_type":"rate_limit_reached"}),
            true,
        ),
        (
            json!({"rate_limit_reached_type":{"rate_limit_reached":{}}}),
            true,
        ),
        (
            json!({"rate_limit_reached_type":{"rate_limit_reached":null}}),
            true,
        ),
        (
            json!({"rate_limit_reached_type":{"future_reached":{}}}),
            false,
        ),
        (
            json!({"rate_limit_reached_type":{"rate_limit_reached":[]}}),
            false,
        ),
    ] {
        let quota = stamped(event("token_count", json!({"rate_limits":limits})), 2);
        for (ordinal, expected) in [
            (2, if accepted { "published" } else { "revised" }),
            (3, "revised"),
        ] {
            let revised = stamped(page_user("turn", "id", "revised"), ordinal);
            for preview in [false, true] {
                let snapshot = read_with_history_mode(
                    vec![prior.clone(), quota.clone(), revised.clone()],
                    preview,
                    Some("paginated"),
                )
                .unwrap();
                assert_eq!(
                    visible_text(&snapshot),
                    [expected],
                    "{limits}, ordinal {ordinal}"
                );
                assert_eq!(
                    omnis_core::trajectory_search_document(&snapshot).source_complete,
                    accepted && ordinal == 3,
                    "{limits}, ordinal {ordinal}"
                );
            }
        }
    }
}

#[test]
fn an_undecoded_documentary_ordinal_only_limits_the_display_when_another_record_contests_it() {
    let question = |text, ordinal| stamped(page_user("turn", "id", text), ordinal);
    let opaque = [
        event("future_status", json!({"private":"documentary payload"})),
        turn_item(
            "turn",
            &json!({"type":"FutureItem","id":"opaque","private":"documentary payload"}),
        ),
        json!({"type":"world_state","payload":{"future":"documentary payload"}}),
    ];
    for source in &opaque {
        for (ordinal, expected, contested) in [(2, "published", true), (3, "revision", false)] {
            for preview in [false, true] {
                let prefix = vec![
                    question("published", 1),
                    stamped(source.clone(), 2),
                    question("revision", ordinal),
                ];
                let snapshot =
                    read_with_history_mode(prefix.clone(), preview, Some("paginated")).unwrap();
                assert_eq!(visible_text(&snapshot), [expected]);
                let notice = snapshot.events.iter().find(|e| {
                    e.source.raw_record_type.as_deref() == Some("omnisession.codex_rollout_line")
                });
                assert_eq!(notice.is_some(), contested);
                if let Some(notice) = notice {
                    assert_eq!(notice.payload["contested_ordinal"], true);
                    assert_eq!(notice.payload["omitted_events"], 1);
                    assert!(!notice.payload.to_string().contains("documentary payload"));
                }
                let search = omnis_core::trajectory_search_document(&snapshot);
                assert_eq!(search.source_complete, !contested);
                if contested {
                    assert!(!search.text.contains("revision"));
                    let resolved = read_with_history_mode(
                        prefix
                            .into_iter()
                            .chain([question("confirmed tail", 3)])
                            .collect(),
                        preview,
                        Some("paginated"),
                    )
                    .unwrap();
                    assert_eq!(visible_text(&resolved), ["confirmed tail"]);
                }
            }
        }
    }
}

#[test]
fn positional_rollback_preserves_the_display_and_withdraws_the_same_model_turn() {
    let mut history = paginated_visibility::exchange("first", "first request", "first reply", true);
    history.extend(paginated_visibility::exchange(
        "second",
        "second request",
        "second reply",
        true,
    ));
    for rollback in [
        json!(["thread_rolled_back", 1]),
        json!({"type":"thread_rolled_back","num_turns":1}),
        json!([14, 1]),
        json!({"type":14,"num_turns":1}),
    ] {
        let mut rows = history.clone();
        rows.push(json!({"type":"event_msg","payload":rollback}));
        for preview in [false, true] {
            let snapshot =
                read_with_history_mode(rows.clone(), preview, Some("paginated")).unwrap();
            assert_eq!(
                visible_text(&snapshot),
                [
                    "first request",
                    "first reply",
                    "second request",
                    "second reply"
                ]
            );
            let conversation = omnis_core::import_conversation(&snapshot);
            let active: Vec<_> = conversation
                .messages
                .iter()
                .map(|message| message.text.as_str())
                .collect();
            assert_eq!(active, ["first request", "first reply"]);
            assert!(!omnis_core::render_semantic_handoff(&snapshot).contains("second reply"));
            assert!(
                omnis_core::trajectory_search_document(&snapshot)
                    .text
                    .contains("second reply")
            );
        }
    }
}

#[test]
fn numeric_legacy_messages_and_rollback_respect_the_native_turn_count_range() {
    let prefix = [
        event("user_message", json!({"message":"first request"})),
        event("agent_message", json!({"message":"first reply"})),
        json!({"type":"event_msg","payload":[20,null,"second request",null,[],null,[],[],[],[],null]}),
        json!({"type":"event_msg","payload":{"type":19,"message":"second reply"}}),
    ];
    for (rollback, keep_second) in [
        (json!([14, 1]), false),
        (json!({"type":14,"num_turns":1}), false),
        (json!(["thread_rolled_back", 4_294_967_296_u64]), true),
        (json!({"type":14,"num_turns":4_294_967_296_u64}), true),
    ] {
        let rows: Vec<_> = prefix
            .iter()
            .cloned()
            .chain([json!({"type":"event_msg","payload":rollback})])
            .collect();
        for preview in [false, true] {
            let snapshot = read_with_history_mode(rows.clone(), preview, Some("legacy")).unwrap();
            let text = visible_text(&snapshot);
            assert!(text.contains(&"first request") && text.contains(&"first reply"));
            let search = omnis_core::trajectory_search_document(&snapshot);
            let handoff = omnis_core::render_semantic_handoff(&snapshot);
            if keep_second {
                assert!(!search.source_complete, "damaged rollback: {rollback}");
            }
            for word in ["second request", "second reply"] {
                assert_eq!(text.contains(&word), keep_second, "{rollback}");
                assert_eq!(search.text.contains(word), keep_second);
                assert_eq!(handoff.contains(word), keep_second);
            }
        }
    }
}

#[test]
fn damaged_numeric_legacy_message_cannot_publish_a_revision() {
    let rows = vec![
        event("agent_message", json!({"message":"published"})),
        json!({"type":"event_msg","payload":[19,"invalid revision","unknown_phase"]}),
    ];
    for preview in [false, true] {
        let snapshot = read_with_history_mode(rows.clone(), preview, Some("legacy")).unwrap();
        assert_eq!(visible_text(&snapshot), ["published"]);
        let search = omnis_core::trajectory_search_document(&snapshot);
        assert!(!search.source_complete && !search.text.contains("invalid revision"));
        assert!(!omnis_core::render_semantic_handoff(&snapshot).contains("invalid revision"));
    }
}

#[test]
fn commands_with_an_empty_native_id_keep_the_latest_result_in_each_turn() {
    let named = |output| json!({"type":"CommandExecution","id":"","command":["example"],"cwd":"file:///workspace","parsed_cmd":[],"source":"agent","status":"completed","aggregated_output":output});
    let records = vec![
        turn_item("first", &named("original output")),
        turn_item(
            "first",
            &json!([
                6,
                "",
                null,
                null,
                null,
                ["example"],
                "file:///workspace",
                [],
                "agent",
                null,
                "completed",
                "revised output"
            ]),
        ),
        turn_item("second", &named("second output")),
    ];
    for preview in [false, true] {
        let snapshot = read_with_history_mode(records.clone(), preview, Some("paginated")).unwrap();
        let outputs: Vec<_> = snapshot
            .events
            .iter()
            .filter(|event| event.kind == EventKind::CommandExecuted)
            .map(|event| &event.payload["output"])
            .collect();
        assert_eq!(outputs, ["revised output", "second output"]);
        let search = omnis_core::trajectory_search_document(&snapshot);
        assert!(search.source_complete);
        for text in [search.text, omnis_core::render_semantic_handoff(&snapshot)] {
            assert!(text.contains("revised output") && text.contains("second output"));
            assert!(!text.contains("original output"));
        }
    }
}
