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
    let header = stamped(
        json!({"type":"session_meta","payload":{"id":SESSION_ID,"history_mode":"paginated","cwd":"/workspace"}}),
        0,
    );
    let old = stamped(page_user("turn", "id", "prior"), 1);
    let new = stamped(page_user("turn", "id", "completed"), 2);
    let session = SessionRef::new(Provider::Codex, SESSION_ID);
    for compression in [None, Some("gz"), Some("zst")] {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("sessions");
        fs::create_dir(&dir).unwrap();
        let path = dir.join(match compression {
            Some(codec) => format!("rollout-{SESSION_ID}.jsonl.{codec}"),
            None => format!("rollout-{SESSION_ID}.jsonl"),
        });
        for (newline, expected) in [("", "prior"), ("\n", "completed")] {
            let source = format!("{header}\n{old}\n{{invalid\n{new}{newline}");
            let bytes = match compression {
                Some(codec) => encode_rollout(source.as_bytes(), codec),
                None => source.into_bytes(),
            };
            fs::write(&path, &bytes).unwrap();
            let adapter = CodexAdapter::with_root(root.path());
            for snap in [
                adapter.read_session(&session).unwrap(),
                adapter.preview_session(&session).unwrap(),
            ] {
                assert_eq!(visible_text(&snap), [expected], "{compression:?}");
                assert!(!omnis_core::trajectory_search_document(&snap).source_complete);
            }
            assert_eq!(fs::read(&path).unwrap(), bytes);
        }
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
    assert_eq!(active, ["original", "revised turn"]);
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

#[test]
fn paginated_revisions_share_the_item_id_across_display_kinds() {
    for id in ["", "shared"] {
        let command = turn_item(
            "turn",
            &json!({"type":"CommandExecution","id":id,"command":["example"],"cwd":"file:///workspace","parsed_cmd":[],"source":"agent","status":"completed","aggregated_output":"verified command"}),
        );
        let mut rows = vec![page_user("turn", id, "superseded request"), command];
        for (current, old) in [
            ("verified command", "superseded request"),
            ("current request", "verified command"),
        ] {
            for preview in [false, true] {
                let snapshot =
                    read_with_history_mode(rows.clone(), preview, Some("paginated")).unwrap();
                let search = omnis_core::trajectory_search_document(&snapshot);
                assert!(search.source_complete);
                for text in [search.text, omnis_core::render_semantic_handoff(&snapshot)] {
                    assert!(text.contains(current) && !text.contains(old), "{text}");
                }
            }
            rows.push(page_user("turn", id, "current request"));
        }
    }
}

#[test]
fn a_context_compaction_revises_the_display_without_retaining_an_old_objective() {
    for item in [
        json!({"type":"ContextCompaction","id":"shared"}),
        json!([18, "shared"]),
    ] {
        let mut rows = vec![
            page_user("turn", "older", "still active objective"),
            page_user("turn", "shared", "superseded objective"),
            turn_item("turn", &item),
            page_user("turn", "later", "later request"),
        ];
        for restored in [false, true] {
            for preview in [false, true] {
                let snapshot =
                    read_with_history_mode(rows.clone(), preview, Some("paginated")).unwrap();
                let search = omnis_core::trajectory_search_document(&snapshot);
                assert!(
                    search.source_complete && !search.text.contains("superseded objective"),
                    "{item}"
                );
                assert!(
                    !omnis_core::render_semantic_handoff(&snapshot)
                        .contains("superseded objective")
                );
                let expected = if restored {
                    vec![
                        "still active objective",
                        "restored position",
                        "later request",
                    ]
                } else {
                    vec!["still active objective", "later request"]
                };
                assert_eq!(visible_text(&snapshot), expected);
                assert_eq!(omnis_core::import_trajectory(&snapshot).tool_events, 0);
            }
            rows.push(page_user("turn", "shared", "restored position"));
        }
    }
}

#[test]
fn private_completed_items_replace_a_display_without_exporting_their_contents() {
    for item in [
        json!({"type":"Reasoning","id":"shared","summary_text":["hidden fixture"],"raw_content":["hidden fixture"],"status":"hidden fixture"}),
        json!([5, "shared", ["hidden fixture"]]),
        json!([2, "shared", [["hidden fixture", "run"]]]),
        json!([
            9,
            null,
            "custom",
            "shared",
            "started",
            "00000000-0000-4000-8000-000000000000",
            "/root/fixture"
        ]),
        json!([14, "shared", ["custom", "hidden fixture"], "hidden fixture"]),
    ] {
        let rows = vec![
            page_user("turn", "active", "actual objective"),
            page_user("turn", "shared", "obsolete request"),
            turn_item("turn", &item),
        ];
        for preview in [false, true] {
            let snapshot =
                read_with_history_mode(rows.clone(), preview, Some("paginated")).unwrap();
            let search = omnis_core::trajectory_search_document(&snapshot);
            assert!(search.source_complete, "{item}");
            assert_eq!(visible_text(&snapshot), ["actual objective"], "{item}");
            for text in [search.text, omnis_core::render_semantic_handoff(&snapshot)] {
                assert!(
                    text.contains("actual objective")
                        && !text.contains("obsolete request")
                        && !text.contains("hidden fixture"),
                    "{item}"
                );
            }
            assert!(
                snapshot
                    .events
                    .iter()
                    .all(|e| !e.payload.to_string().contains("hidden fixture"))
            );
        }
    }
}

#[test]
fn malformed_reasoning_preserves_the_command_failure_and_leaves_its_ordinal_open() {
    let mut rows = vec![
        event("task_started", json!({"turn_id":"turn"})),
        page_user("turn", "user", "objective"),
        json!({"type":"response_item","payload":{"type":"function_call_output","call_id":"shared","output":"physical result"}}),
        turn_item(
            "turn",
            &json!({"type":"CommandExecution","id":"shared","command":["fixture"],"cwd":"file:///fixture","parsed_cmd":[],"source":"agent","status":"failed","exit_code":1}),
        ),
        turn_item(
            "turn",
            &json!({"type":"Reasoning","id":"shared","summary_text":false,"raw_content":false}),
        ),
    ];
    for repaired in [false, true] {
        for preview in [false, true] {
            let snapshot =
                read_with_history_mode(rows.clone(), preview, Some("paginated")).unwrap();
            assert!(snapshot.events.iter().any(
                |e| e.kind == EventKind::ToolFailed && e.payload["output"] == "physical result"
            ));
            let search = omnis_core::trajectory_search_document(&snapshot);
            assert!(!search.source_complete);
            assert_eq!(search.text.contains("repair accepted"), repaired);
        }
        let mut repair = page_user("turn", "repair", "repair accepted");
        repair["ordinal"] = json!(5);
        repair["timestamp"] = json!("2026-01-05T09:00:00.000Z");
        rows.push(repair);
    }
}

#[test]
fn a_completed_review_replaces_the_old_reply_with_its_published_findings() {
    let expected = "fixture explanation\n\nReview comment:\n\n- fixture title — /fixture/file.rs:8-9\n  body one\n  body two";
    let finding = json!([
        "fixture title",
        "body one\nbody two",
        0.9,
        1,
        ["/fixture/file.rs", [8, 9]]
    ]);
    for (output, review) in [
        (
            json!([
                [finding],
                "hidden correctness",
                "  fixture explanation  ",
                0.5
            ]),
            expected,
        ),
        (Value::Null, "Reviewer failed to output a response."),
    ] {
        let rows = vec![
            page_user("turn", "user", "review request"),
            turn_item(
                "turn",
                &json!({"type":"AgentMessage","id":"r","content":[{"type":"Text","text":"old reply"}]}),
            ),
            turn_item("turn", &json!([15, "r", output])),
        ];
        for preview in [false, true] {
            let snapshot =
                read_with_history_mode(rows.clone(), preview, Some("paginated")).unwrap();
            assert_eq!(visible_text(&snapshot), ["review request", review]);
            let search = omnis_core::trajectory_search_document(&snapshot);
            assert!(search.source_complete);
            for text in [search.text, omnis_core::render_semantic_handoff(&snapshot)] {
                for part in review.lines().filter(|line| !line.is_empty()) {
                    assert!(text.contains(part.trim()), "{text}");
                }
                assert!(!text.contains("old reply") && !text.contains("hidden correctness"));
            }
        }
    }
}

fn completed_lookup(status: &str) -> Value {
    turn_item(
        "turn",
        &json!({
            "type":"DynamicToolCall","id":"shared","tool":"lookup","arguments":{},
            "status":status,"success":status=="completed","content_items":[{"type":"inputText","text":"first receipt"}]
        }),
    )
}

fn physical_call(name: &str, arguments: &Value) -> Value {
    json!({"type":"response_item","payload":{
        "type":"function_call","name":name,"arguments":arguments.to_string(),"call_id":"shared"}} )
}

fn physical_result(text: &str) -> Value {
    json!({"type":"response_item","payload":{
        "type":"function_call_output","call_id":"shared","output":text}})
}

fn command_display(name: &str, status: &str, text: &str) -> Value {
    turn_item(
        "turn",
        &json!({
            "type":"CommandExecution","id":"shared","command":[name],"cwd":"file:///workspace",
            "source":"agent","parsed_cmd":[],"status":status,"aggregated_output":text
        }),
    )
}

fn assert_receipt_outcomes(scenario: &str, records: Vec<Value>, first_failed: bool) {
    let expected = [
        ("first receipt", first_failed),
        ("second receipt", !first_failed),
    ];
    let mut rows = vec![
        event("task_started", json!({"turn_id":"turn"})),
        page_user("turn", "request", "objective"),
    ];
    rows.extend(records);
    for preview in [false, true] {
        let snapshot = read_with_history_mode(rows.clone(), preview, Some("paginated")).unwrap();
        let trajectory = omnis_core::import_trajectory(&snapshot);
        let native = omnis_core::native_trajectory_items(&trajectory);
        let outputs: Vec<_> = native
            .iter()
            .filter_map(|item| match item {
                omnis_core::NativeTrajectoryItem::Tool {
                    output, is_error, ..
                } => Some((output.as_str(), *is_error)),
                omnis_core::NativeTrajectoryItem::Message { .. } => None,
            })
            .collect();
        assert_eq!(outputs, expected, "{scenario}, preview={preview}");
        let handoff = omnis_core::render_semantic_handoff(&snapshot);
        for text in ["first receipt", "second receipt"] {
            assert_eq!(handoff.matches(text).count(), 1, "{handoff}");
        }
        assert!(!handoff.contains("partial message"));
        let plan = omnis_core::plan_semantic_handoff(&snapshot);
        assert_eq!(plan.tool_outcomes.len(), 2, "{scenario}");
        for (text, failed) in expected {
            let outcome = plan
                .tool_outcomes
                .iter()
                .find(|outcome| outcome.summary.contains(text))
                .unwrap();
            assert_eq!(
                outcome.status,
                if failed {
                    omnis_core::ToolOutcomeStatus::Failed
                } else {
                    omnis_core::ToolOutcomeStatus::Completed
                },
                "{scenario}"
            );
        }
    }
}

#[test]
fn a_revised_tool_display_keeps_the_outcome_of_its_original_call() {
    let first = physical_call("exec_command", &json!({"cmd":"first"}));
    let second = physical_call("exec_command", &json!({"cmd":"second"}));
    let finished = command_display("first", "completed", "first receipt");
    let corrected = command_display("first", "failed", "first receipt");
    for (scenario, records) in [
        "late correction",
        "both displayed",
        "partial output",
        "dynamic tool",
    ]
    .into_iter()
    .zip([
        vec![
            first.clone(),
            physical_result("first receipt"),
            finished.clone(),
            second.clone(),
            physical_result("second receipt"),
            corrected.clone(),
        ],
        vec![
            first.clone(),
            physical_result("first receipt"),
            finished,
            second.clone(),
            physical_result("second receipt"),
            command_display("second", "completed", "second receipt"),
            corrected.clone(),
        ],
        vec![
            first,
            physical_result("first receipt"),
            command_display("first", "in_progress", "partial message"),
            second,
            corrected,
            physical_result("second receipt"),
        ],
        vec![
            physical_call("lookup", &json!({})),
            physical_result("first receipt"),
            completed_lookup("completed"),
            physical_call("other", &json!({})),
            completed_lookup("failed"),
            physical_result("second receipt"),
        ],
    ]) {
        assert_receipt_outcomes(scenario, records, true);
    }
}

#[test]
fn a_delayed_command_display_uses_the_published_command_and_output() {
    for (case, arguments) in [
        ("distinct command", json!({"cmd":"second"})),
        ("yield", json!({"cmd":"first","yield_time_ms":50})),
        (
            "output budget",
            json!({"cmd":"first","max_output_tokens":120}),
        ),
        (
            "approval fields",
            json!({"cmd":"first","justification":"fixture","prefix_rule":["first"]}),
        ),
        (
            "shell defaults",
            json!({"cmd":"first","login":true,"tty":false}),
        ),
    ] {
        let same_command = arguments["cmd"] == "first";
        for result_first in [false, true] {
            let mut records = vec![
                physical_call("exec_command", &json!({"cmd":"first"})),
                physical_result("first receipt"),
            ];
            let failed_output = if same_command {
                records.push(command_display("first", "completed", "first receipt"));
                "second receipt"
            } else {
                "first receipt"
            };
            records.push(physical_call("exec_command", &arguments));
            let failed = command_display("first", "failed", failed_output);
            let second = physical_result("second receipt");
            records.extend(if result_first {
                [second, failed]
            } else {
                [failed, second]
            });
            assert_receipt_outcomes(
                &format!("{case}, result first={result_first}"),
                records,
                !same_command,
            );
        }
    }
}

#[test]
fn a_deferred_failure_requires_the_eventual_result_to_match_its_output() {
    let records = vec![
        event("task_started", json!({"turn_id":"turn"})),
        page_user("turn", "request", "objective"),
        physical_call("exec_command", &json!({"cmd":"same"})),
        physical_result("first receipt"),
        command_display("same", "completed", "first receipt"),
        physical_call("exec_command", &json!({"cmd":"same","yield_time_ms":50})),
        command_display("same", "failed", "unattributed display"),
        physical_result("second receipt"),
    ];
    for mode in ["legacy", "paginated"] {
        for preview in [false, true] {
            let snapshot = read_with_history_mode(records.clone(), preview, Some(mode)).unwrap();
            assert!(
                !omnis_core::trajectory_search_document(&snapshot).source_complete,
                "{mode}"
            );
            let plan = omnis_core::plan_semantic_handoff(&snapshot);
            for (text, status) in [
                ("first receipt", omnis_core::ToolOutcomeStatus::Completed),
                ("second receipt", omnis_core::ToolOutcomeStatus::Completed),
                (
                    "unattributed display",
                    omnis_core::ToolOutcomeStatus::Failed,
                ),
            ] {
                assert_eq!(
                    plan.tool_outcomes
                        .iter()
                        .filter(|item| item.summary.contains(text) && item.status == status)
                        .count(),
                    1,
                    "{mode} {text}"
                );
            }
            let trajectory = omnis_core::import_trajectory(&snapshot);
            let native = omnis_core::native_trajectory_items(&trajectory);
            for receipt in ["first receipt", "second receipt"] {
                let count = native
                    .iter()
                    .filter(|item| match item {
                        omnis_core::NativeTrajectoryItem::Tool {
                            output, is_error, ..
                        } => output == receipt && !is_error,
                        omnis_core::NativeTrajectoryItem::Message { text, .. } => {
                            text.starts_with("[Historical tool result.") && text.contains(receipt)
                        }
                    })
                    .count();
                assert_eq!(count, 1, "{mode}: {receipt}");
            }
            assert!(native.iter().any(|item|matches!(item,omnis_core::NativeTrajectoryItem::Message{text,..} if text.starts_with("[Historical tool failure.") && text.contains("unattributed display"))));
        }
    }
}

#[test]
fn identical_outputs_follow_the_published_command_and_working_directory() {
    for different_dirs in [false, true] {
        let (first_name, second_name) = if different_dirs {
            ("pwd", "pwd")
        } else {
            ("first", "second")
        };
        for fail_first in [true, false] {
            for raw_first in [false, true] {
                let args = |cmd: &str, dir: &str| {
                    if different_dirs {
                        json!({"cmd":cmd,"workdir":format!("{dir}/temporary/..")})
                    } else {
                        json!({"cmd":cmd})
                    }
                };
                let display = |name: &str, dir: &str, status: &str| {
                    let mut display = command_display(name, status, "same output");
                    display["payload"]["item"]["command"] = json!(["/bin/sh", "-c", name]);
                    display["payload"]["item"]["cwd"] = json!(format!("file://{dir}"));
                    display
                };
                let mut rows = vec![
                    event("task_started", json!({"turn_id":"turn"})),
                    page_user("turn", "request", "objective"),
                    physical_call("exec_command", &args(first_name, "/workspace/alpha")),
                    physical_result("same output"),
                ];
                if different_dirs {
                    rows.push(display(first_name, "/workspace/alpha", "completed"));
                }
                rows.push(physical_call(
                    "exec_command",
                    &args(second_name, "/workspace/beta"),
                ));
                let correction = if fail_first {
                    display(first_name, "/workspace/alpha", "failed")
                } else {
                    display(second_name, "/workspace/beta", "failed")
                };
                let last = physical_result("same output");
                rows.extend(if raw_first {
                    [last, correction]
                } else {
                    [correction, last]
                });
                for mode in ["legacy", "paginated"] {
                    for preview in [false, true] {
                        let snapshot =
                            read_with_history_mode(rows.clone(), preview, Some(mode)).unwrap();
                        let plan = omnis_core::plan_semantic_handoff(&snapshot);
                        let statuses: Vec<_> = plan
                            .tool_outcomes
                            .iter()
                            .filter(|outcome| outcome.summary.contains("same output"))
                            .map(|outcome| outcome.status == omnis_core::ToolOutcomeStatus::Failed)
                            .collect();
                        assert_eq!(
                            statuses,
                            [fail_first, !fail_first],
                            "{different_dirs} {fail_first} {raw_first} {mode}"
                        );
                        let trajectory = omnis_core::import_trajectory(&snapshot);
                        let native = omnis_core::native_trajectory_items(&trajectory);
                        let flags: Vec<_> = native
                            .iter()
                            .filter_map(|item| match item {
                                omnis_core::NativeTrajectoryItem::Tool {
                                    is_error, output, ..
                                } if output == "same output" => Some(*is_error),
                                _ => None,
                            })
                            .collect();
                        assert_eq!(flags, [fail_first, !fail_first], "{mode}");
                        assert!(omnis_core::trajectory_search_document(&snapshot).source_complete);
                    }
                }
            }
        }
    }
}

#[test]
fn an_unrecognized_command_wrapper_does_not_disambiguate_identical_results() {
    let mut correction = command_display("other", "failed", "common result");
    correction["payload"]["item"]["command"] = json!(["fixture-wrapper", "-c", "second"]);
    let mut rows = vec![
        event("task_started", json!({"turn_id":"turn"})),
        page_user("turn", "request", "objective"),
        physical_call("exec_command", &json!({"cmd":"first"})),
        physical_result("common result"),
        physical_call("exec_command", &json!({"cmd":"second"})),
    ];
    for raw_first in [false, true] {
        let result = physical_result("common result");
        rows.extend(if raw_first {
            [result, correction.clone()]
        } else {
            [correction.clone(), result]
        });
        for mode in ["legacy", "paginated"] {
            for preview in [false, true] {
                let snapshot = read_with_history_mode(rows.clone(), preview, Some(mode)).unwrap();
                assert!(!omnis_core::trajectory_search_document(&snapshot).source_complete);
                let plan = omnis_core::plan_semantic_handoff(&snapshot);
                assert_eq!(
                    plan.tool_outcomes
                        .iter()
                        .filter(|outcome| outcome.status
                            == omnis_core::ToolOutcomeStatus::Completed
                            && outcome.summary.contains("common result"))
                        .count(),
                    2,
                    "{mode}"
                );
                assert_eq!(
                    plan.tool_outcomes
                        .iter()
                        .filter(
                            |outcome| outcome.status == omnis_core::ToolOutcomeStatus::Failed
                                && outcome.summary.contains("common result")
                        )
                        .count(),
                    1,
                    "{mode}"
                );
            }
        }
        rows.truncate(5);
    }
}

#[test]
fn a_lookup_correction_does_not_transfer_its_failure_to_a_pending_command() {
    let rows = vec![
        event("task_started", json!({"turn_id":"turn"})),
        page_user("turn", "request", "objective"),
        physical_call("lookup", &json!({})),
        physical_result("first receipt"),
        completed_lookup("completed"),
        physical_call("exec_command", &json!({"cmd":"pwd"})),
        completed_lookup("failed"),
        physical_result("second receipt"),
    ];
    for mode in ["legacy", "paginated"] {
        for preview in [false, true] {
            let snapshot = read_with_history_mode(rows.clone(), preview, Some(mode)).unwrap();
            let trajectory = omnis_core::import_trajectory(&snapshot);
            let native = omnis_core::native_trajectory_items(&trajectory);
            let results: Vec<_> = native
                .iter()
                .filter_map(|entry| match entry {
                    omnis_core::NativeTrajectoryItem::Tool {
                        output, is_error, ..
                    } => Some((output.as_str(), *is_error)),
                    omnis_core::NativeTrajectoryItem::Message { .. } => None,
                })
                .collect();
            assert_eq!(
                results,
                [("first receipt", true), ("second receipt", false)],
                "{mode}"
            );
            assert!(omnis_core::trajectory_search_document(&snapshot).source_complete);
        }
    }
}

#[test]
fn an_overlapping_or_conflicting_command_keeps_its_unattributed_failure() {
    for (overlap, raw_first) in [(false, false), (false, true), (true, false), (true, true)] {
        let first = physical_call("exec_command", &json!({"cmd":"first"}));
        let second = physical_call("exec_command", &json!({"cmd":"second"}));
        let mut display = command_display(
            "second",
            "failed",
            if overlap { "alpha" } else { "published" },
        );
        display["payload"]["item"]["command"] = json!(["/bin/sh", "-c", "second"]);
        let mut rows = vec![
            event("task_started", json!({"turn_id":"turn"})),
            page_user("turn", "request", "objective"),
            first,
        ];
        if overlap {
            rows.extend([second, physical_result("alpha")]);
        }
        let raw = physical_result(if overlap { "beta" } else { "alpha" });
        rows.extend(if raw_first {
            [raw, display]
        } else {
            [display, raw]
        });
        for mode in ["legacy", "paginated"] {
            for preview in [false, true] {
                let snapshot = read_with_history_mode(rows.clone(), preview, Some(mode)).unwrap();
                let search = omnis_core::trajectory_search_document(&snapshot);
                assert!(
                    !search.source_complete,
                    "{mode} overlap={overlap} raw_first={raw_first}"
                );
                let plan = omnis_core::plan_semantic_handoff(&snapshot);
                let failed: Vec<_> = plan
                    .tool_outcomes
                    .iter()
                    .filter(|entry| entry.status == omnis_core::ToolOutcomeStatus::Failed)
                    .collect();
                assert_eq!(failed.len(), 1);
                assert!(
                    failed[0]
                        .summary
                        .contains(if overlap { "alpha" } else { "published" })
                );
                let completed: Vec<_> = plan
                    .tool_outcomes
                    .iter()
                    .filter(|entry| entry.status == omnis_core::ToolOutcomeStatus::Completed)
                    .collect();
                assert_eq!(completed.len(), if overlap { 2 } else { 1 });
                assert!(
                    completed
                        .iter()
                        .any(|entry| entry.summary.contains("alpha"))
                );
                assert!(!overlap || completed.iter().any(|entry| entry.summary.contains("beta")));
                let trajectory = omnis_core::import_trajectory(&snapshot);
                let native = omnis_core::native_trajectory_items(&trajectory);
                assert!(native.iter().all(|entry| !matches!(
                    entry,
                    omnis_core::NativeTrajectoryItem::Tool { is_error: true, .. }
                )));
            }
        }
    }
}

#[test]
fn a_command_published_before_its_call_requires_compatible_execution_evidence() {
    for (name, args, argv, diagnostic, paired) in [
        (
            "exec_command",
            json!({"cmd":"second"}),
            json!(["/bin/sh", "-c", "second"]),
            "published diagnostic",
            true,
        ),
        (
            "exec_command",
            json!({"cmd":"first"}),
            json!(["/bin/sh", "-c", "second"]),
            "published diagnostic",
            false,
        ),
        (
            "lookup",
            json!({}),
            json!(["/bin/sh", "-c", "second"]),
            "published diagnostic",
            false,
        ),
        (
            "exec_command",
            json!({"cmd":"first"}),
            json!(["fixture-wrapper", "--run", "second"]),
            "published diagnostic",
            false,
        ),
        (
            "exec_command",
            json!({"cmd":"second"}),
            json!(["fixture-wrapper", "--run", "second"]),
            "raw receipt",
            true,
        ),
    ] {
        for display_first in [false, true] {
            for turn in ["turn", "rollout-2"] {
                let mut display = command_display("second", "failed", diagnostic);
                display["payload"]["turn_id"] = turn.into();
                display["payload"]["item"]["command"] = argv.clone();
                let call = physical_call(name, &args);
                let mut rows = vec![
                    event("task_started", json!({"turn_id":turn})),
                    page_user(turn, "user", "request"),
                ];
                rows.extend(if display_first {
                    [display, call]
                } else {
                    [call, display]
                });
                rows.push(physical_result("raw receipt"));
                for mode in ["paginated", "legacy"] {
                    for preview in [false, true] {
                        let snapshot =
                            read_with_history_mode(rows.clone(), preview, Some(mode)).unwrap();
                        let search = omnis_core::trajectory_search_document(&snapshot);
                        let handoff = omnis_core::render_semantic_handoff(&snapshot);
                        assert_eq!(
                            search.source_complete, paired,
                            "{name} {argv} {display_first} {mode}"
                        );
                        for text in [search.text, handoff] {
                            assert_eq!(text.matches("raw receipt").count(), 1, "{text}");
                            assert_eq!(text.contains("published diagnostic"), !paired, "{text}");
                        }
                        let raw = snapshot
                            .events
                            .iter()
                            .find(|event| {
                                event.source.raw_record_type.as_deref()
                                    == Some("response_item.function_call_output")
                            })
                            .unwrap();
                        assert_eq!(raw.kind == EventKind::ToolFailed, paired);
                        let trajectory = omnis_core::import_trajectory(&snapshot);
                        let native = omnis_core::native_trajectory_items(&trajectory);
                        assert_eq!(
                            native.iter().any(|entry| matches!(
                                entry,
                                omnis_core::NativeTrajectoryItem::Tool { is_error: true, .. }
                            )),
                            paired
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn shell_command_receipts_match_the_executed_command_in_each_record_order() {
    let diagnostic = "cat: absent-fixture: No such file or directory";
    for (command, code, body) in [
        ("cat absent-fixture", 1, diagnostic),
        ("printf first", 0, "first output"),
        ("cat absent-fixture", 0, "first output"),
        ("cat absent-fixture", 1, "first output"),
        ("cat absent-fixture", 0, diagnostic),
    ] {
        let paired = code == 1 && body == diagnostic;
        let call = physical_call(
            "shell_command",
            &json!({"command":command,"workdir":"//workspace/fixtures"}),
        );
        let result = physical_result(&format!(
            "Exit code: {code}\nWall time: 0 seconds\nOutput:\n{body}"
        ));
        let mut display = command_display("cat absent-fixture", "failed", diagnostic);
        display["payload"]["item"]["command"] = json!(["/bin/sh", "-c", "cat absent-fixture"]);
        display["payload"]["item"]["cwd"] = "file:///workspace/fixtures".into();
        display["payload"]["item"]["exit_code"] = 1.into();
        for order in [
            [&call, &display, &result],
            [&display, &call, &result],
            [&call, &result, &display],
        ] {
            let mut rows = vec![
                event("task_started", json!({"turn_id":"turn"})),
                page_user("turn", "user", "request"),
            ];
            rows.extend(order.into_iter().cloned());
            for mode in ["paginated", "legacy"] {
                for preview in [false, true] {
                    let snapshot =
                        read_with_history_mode(rows.clone(), preview, Some(mode)).unwrap();
                    let search = omnis_core::trajectory_search_document(&snapshot);
                    assert_eq!(
                        search.source_complete, paired,
                        "{command} {mode} {code} {body} {order:?}"
                    );
                    for text in [search.text, omnis_core::render_semantic_handoff(&snapshot)] {
                        assert_eq!(
                            text.matches(diagnostic).count(),
                            1 + usize::from(!paired && body == diagnostic),
                            "{text}"
                        );
                        assert_eq!(
                            text.matches("first output").count(),
                            usize::from(body == "first output"),
                            "{text}"
                        );
                    }
                    let raw = snapshot
                        .events
                        .iter()
                        .find(|event| {
                            event.source.raw_record_type.as_deref()
                                == Some("response_item.function_call_output")
                        })
                        .unwrap();
                    assert_eq!(raw.kind == EventKind::ToolFailed, paired);
                    let trajectory = omnis_core::import_trajectory(&snapshot);
                    let native = omnis_core::native_trajectory_items(&trajectory);
                    assert_eq!(
                        native.iter().any(|entry| matches!(
                            entry,
                            omnis_core::NativeTrajectoryItem::Tool { is_error: true, .. }
                        )),
                        paired
                    );
                }
            }
        }
    }
}

#[test]
fn repeated_shell_command_matches_each_native_exit_receipt() {
    let command = "cat trial.txt";
    let call = physical_call("shell_command", &json!({"command":command}));
    let mut first = command_display(command, "completed", "trial contents");
    first["payload"]["item"]["exit_code"] = 0.into();
    let mut second = command_display(command, "failed", "trial missing");
    second["payload"]["item"]["exit_code"] = 1.into();
    let result = |code, body| {
        physical_result(&format!(
            "Exit code: {code}\nWall time: 0.1 seconds\nOutput:\n{body}"
        ))
    };
    for display_first in [true, false] {
        let raw = result(1, "trial missing");
        let mut rows = vec![
            event("task_started", json!({"turn_id":"turn"})),
            page_user("turn", "user", "request"),
            call.clone(),
            result(0, "trial contents"),
            first.clone(),
            call.clone(),
        ];
        rows.extend(if display_first {
            [second.clone(), raw]
        } else {
            [raw, second.clone()]
        });
        for mode in ["paginated", "legacy"] {
            for preview in [false, true] {
                let snapshot = read_with_history_mode(rows.clone(), preview, Some(mode)).unwrap();
                let search = omnis_core::trajectory_search_document(&snapshot);
                assert!(search.source_complete, "{mode} {display_first}");
                for text in [search.text, omnis_core::render_semantic_handoff(&snapshot)] {
                    for output in ["trial contents", "trial missing"] {
                        assert_eq!(text.matches(output).count(), 1, "{text}");
                    }
                }
                let items =
                    omnis_core::native_trajectory_items(&omnis_core::import_trajectory(&snapshot));
                let statuses: Vec<_> = items
                    .iter()
                    .filter_map(|item| match item {
                        omnis_core::NativeTrajectoryItem::Tool { is_error, .. } => Some(*is_error),
                        omnis_core::NativeTrajectoryItem::Message { .. } => None,
                    })
                    .collect();
                assert_eq!(statuses, [false, true], "{mode} {display_first}");
            }
        }
    }
}

#[test]
fn provider_command_wrappers_preserve_timeout_truncation_and_exit_evidence() {
    let display = format!("{}missing{}", "α".repeat(20_000), "z".repeat(20_000));
    let truncated = format!(
        "{}…{} chars truncated…{}",
        "α".repeat(4),
        display.chars().count() - 12,
        "z".repeat(8)
    );
    for (name, shown, raw, exit, paired) in [
        ("shell_command","partial output","Exit code: 124\nWall time: 1 seconds\nOutput:\ncommand timed out after 1000 milliseconds\npartial output".to_owned(),124,true),
        ("shell_command","different output","Exit code: 124\nWall time: 1 seconds\nOutput:\ncommand timed out after 1000 milliseconds\npartial output".to_owned(),124,false),
        ("shell_command",display.as_str(), format!("Exit code: 1\nWall time: 0 seconds\nOutput:\n{truncated}"),1,true),
        ("shell_command",display.as_str(),format!("Exit code: 1\nWall time: 0 seconds\nOutput:\n{truncated}X"),1,false),
        ("shell_command","abcdefghijkL","Exit code: 1\nWall time: 0 seconds\nOutput:\nab…2 tokens truncated…kL".to_owned(),1,true),
        ("exec_command","missing","Chunk ID: ab12cd\nWall time: 0.0420 seconds\nProcess exited with code 1\nOriginal token count: 4\nOutput:\nmissing".to_owned(),1,true),
        ("exec_command","missing","Chunk ID: ab12cd\nWall time: 0.0420 seconds\nProcess exited with code 0\nOriginal token count: 4\nOutput:\nfirst".to_owned(),1,false)
    ] {
        for raw_first in [true,false] {
            let field = if name=="exec_command" {"cmd"} else {"command"};
            let mut records=vec![event("task_started",json!({"turn_id":"turn"})),page_user("turn","user","request"),
            physical_call(name,&json!({(field):"cat file"}))];
            let mut item=command_display("cat file","failed",shown);
            item["payload"]["item"]["exit_code"]=exit.into();
            let raw=physical_result(&raw);
            records.extend(if raw_first{[raw,item]} else {[item,raw]});
            for mode in ["legacy","paginated"] {
                for preview in [false,true] {
                    let snapshot=read_with_history_mode(records.clone(),preview,Some(mode)).unwrap();
                    assert_eq!(omnis_core::trajectory_search_document(&snapshot).source_complete, paired, "{name} {exit} {mode} {raw_first} {paired}");
                    let raw=snapshot.events.iter().find(|e|e.source.raw_record_type.as_deref()==Some("response_item.function_call_output")).unwrap();
                    assert_eq!(raw.kind==EventKind::ToolFailed,paired);
                    let items=omnis_core::native_trajectory_items(&omnis_core::import_trajectory(&snapshot));
                    assert_eq!(items.iter().any(|item| matches!(item,omnis_core::NativeTrajectoryItem::Tool{is_error:true,..})), paired);
                }
            }
        }
    }
}

#[test]
fn a_running_exec_result_keeps_its_session_id_and_the_later_process_failure() {
    let diagnostic = "DiskQuotaExceeded";
    let command = physical_call(
        "exec_command",
        &json!({"cmd":"run-check","yield_time_ms":1000}),
    );
    let mut display = command_display("run-check", "failed", diagnostic);
    display["payload"]["item"]["process_id"] = "12345".into();
    display["payload"]["item"]["exit_code"] = 1.into();
    for (header, output, running, complete) in [
        (
            "Process running with session ID 12345",
            "started",
            true,
            true,
        ),
        (
            "Process running with session ID 12345",
            diagnostic,
            true,
            true,
        ),
        (
            "Process running with session ID 999",
            diagnostic,
            true,
            false,
        ),
        ("Process exited with code 1", diagnostic, false, true),
    ] {
        let raw = physical_result(&format!(
            "Chunk ID: a1b2c3\nWall time: 1.0000 seconds\n{header}\nOriginal token count: 2\nOutput:\n{output}"
        ));
        for suffix in [
            [display.clone(), raw.clone()],
            [raw.clone(), display.clone()],
        ] {
            let mut rows = vec![
                event("task_started", json!({"turn_id":"turn"})),
                page_user("turn", "user", "request"),
                command.clone(),
            ];
            rows.extend(suffix);
            for mode in [Some("paginated"), None] {
                for preview in [false, true] {
                    let snap = read_with_history_mode(rows.clone(), preview, mode).unwrap();
                    let doc = omnis_core::trajectory_search_document(&snap);
                    assert_eq!(doc.source_complete, complete, "{header} {mode:?} {preview}");
                    for text in [doc.text, omnis_core::render_semantic_handoff(&snap)] {
                        assert!(text.contains(diagnostic), "{text}");
                        assert!(text.contains(header), "{text}");
                    }
                    let physical = snap
                        .events
                        .iter()
                        .find(|event| {
                            event.source.raw_record_type.as_deref()
                                == Some("response_item.function_call_output")
                        })
                        .unwrap();
                    assert_eq!(physical.kind == EventKind::ToolFailed, !running);
                    assert_eq!(
                        snap.events
                            .iter()
                            .filter(|event| event.kind == EventKind::ToolFailed)
                            .count(),
                        1
                    );
                    if running {
                        assert!(snap.events.iter().any(|e| e.kind == EventKind::ToolFailed
                            && e.payload["process_id"] == "12345"
                            && e.payload["output"] == diagnostic));
                    }
                    let trajectory = omnis_core::import_trajectory(&snap);
                    let native = omnis_core::native_trajectory_items(&trajectory);
                    let failed_raw = native.iter().any(|item| {
                        matches!(item, omnis_core::NativeTrajectoryItem::Tool { is_error: true, output, .. } if output.contains(header))
                    });
                    assert_eq!(failed_raw, !running);
                }
            }
        }
    }
}

#[test]
fn truncated_shell_results_keep_the_display_and_use_source_bytes_for_the_verdict() {
    for (tool, edge, omitted) in [("shell_command", 800, 220), ("exec_command", 20_000, 5_000)] {
        let left = "a".repeat(edge);
        let right = "z".repeat(edge);
        let diagnostic = "DiskQuotaExceeded";
        let middle = format!("{diagnostic}{}", "m".repeat(omitted - diagnostic.len()));
        let display_text = format!("{left}{middle}{right}");
        let mut display = command_display("cat trial", "failed", &display_text);
        display["payload"]["item"]["exit_code"] = 1.into();
        let header = if tool == "exec_command" {
            "Chunk ID: ab12cd\nWall time: 0.0420 seconds\nProcess exited with code 1\nOutput:\n"
        } else {
            "Exit code: 1\nWall time: 0 seconds\nOutput:\n"
        };
        for altered in [false, true] {
            let mut retained = left.clone();
            if altered {
                retained.replace_range(edge - 1..edge, "X");
            }
            let model_text = format!(
                "{header}{retained}…{} tokens truncated…{right}",
                omitted / 4
            );
            for raw_first in [false, true] {
                let key = if tool == "exec_command" {
                    "cmd"
                } else {
                    "command"
                };
                let mut rows = vec![
                    event("task_started", json!({"turn_id":"turn"})),
                    page_user("turn", "user", "request"),
                    physical_call(tool, &json!({(key):"cat trial"})),
                ];
                let raw = physical_result(&model_text);
                rows.extend(if raw_first {
                    [raw, display.clone()]
                } else {
                    [display.clone(), raw]
                });
                for mode in ["paginated", "legacy"] {
                    for preview in [false, true] {
                        let snapshot =
                            read_with_history_mode(rows.clone(), preview, Some(mode)).unwrap();
                        let search = omnis_core::trajectory_search_document(&snapshot);
                        assert_eq!(
                            search.source_complete, !altered,
                            "{tool} {mode} {raw_first}"
                        );
                        if !altered && tool == "shell_command" {
                            assert!(search.text.contains(diagnostic));
                            assert!(
                                omnis_core::render_session_history_preview(&snapshot)
                                    .contains(diagnostic)
                            );
                            assert!(
                                omnis_core::render_semantic_handoff(&snapshot).contains(diagnostic)
                            );
                        }
                        let model = snapshot
                            .events
                            .iter()
                            .find(|e| {
                                e.source.raw_record_type.as_deref()
                                    == Some("response_item.function_call_output")
                            })
                            .unwrap();
                        assert_eq!(model.kind == EventKind::ToolFailed, !altered);
                        assert!(
                            model.payload["output"]
                                .as_str()
                                .unwrap()
                                .starts_with(header)
                        );
                        let native = omnis_core::native_trajectory_items(
                            &omnis_core::import_trajectory(&snapshot),
                        );
                        assert_eq!(
                            native.iter().any(|entry| matches!(
                                entry,
                                omnis_core::NativeTrajectoryItem::Tool { is_error: true, .. }
                            )),
                            !altered
                        );
                    }
                }
            }
        }
    }
}

fn voice(id: &str, content: &Value) -> Value {
    let mut payload = content.clone();
    payload["id"] = id.into();
    payload["realtime_session_id"] = "voice".into();
    json!({"type":"realtime_item","payload":payload})
}

#[test]
fn damaged_realtime_content_leaves_its_ordinal_available_for_a_verified_reply() {
    let reply = turn_item(
        "turn",
        &json!({"type":"AgentMessage","id":"reply","content":[{"type":"Text","text":"DiskQuotaExceeded"}]}),
    );
    for damaged in [
        json!({"type":"transcript_segment","role":"user"}),
        json!({"type":"transcript_segment","role":"speaker","text":"private"}),
        json!({"type":"bem_item_promoted","turn_id":"turn","item_id":"item","presentation":{"type":"inline_visualization","index":-1}}),
        json!({"type":"realtime_session_closed","outcome":"unknown"}),
    ] {
        let rows = vec![
            stamped(page_user("turn", "u", "objective"), 1),
            stamped(voice("damaged", &damaged), 2),
            stamped(reply.clone(), 2),
        ];
        for preview in [false, true] {
            let trailing =
                read_with_history_mode(rows[..2].to_vec(), preview, Some("paginated")).unwrap();
            assert!(!omnis_core::trajectory_search_document(&trailing).source_complete);
            let snap = read_with_history_mode(rows.clone(), preview, Some("paginated")).unwrap();
            let search = omnis_core::trajectory_search_document(&snap);
            assert!(search.source_complete);
            assert!(search.text.contains("DiskQuotaExceeded"));
            assert!(
                omnis_core::render_session_history_preview(&snap).contains("DiskQuotaExceeded")
            );
            assert!(omnis_core::render_semantic_handoff(&snap).contains("DiskQuotaExceeded"));
        }
    }
    let rows = vec![
        stamped(page_user("turn", "u", "objective"), 1),
        stamped(
            voice(
                "spoken",
                &json!({"type":"transcript_segment","role":{"assistant":{}},"text":"verified voice"}),
            ),
            2,
        ),
        stamped(reply, 2),
    ];
    for preview in [false, true] {
        let snap = read_with_history_mode(rows.clone(), preview, Some("paginated")).unwrap();
        let search = omnis_core::trajectory_search_document(&snap);
        assert!(search.text.contains("verified voice"));
        assert!(!search.text.contains("DiskQuotaExceeded"));
    }
}

#[test]
fn realtime_timeline_keeps_spoken_roles_and_first_published_item_ids() {
    let rows = vec![
        event("task_started", json!({"turn_id":"turn"})),
        page_user("turn", "u", "inspect synthetic output"),
        voice("start", &json!({"type":"realtime_session_started"})),
        voice(
            "u",
            &json!({"type":"transcript_segment","role":"user","text":"Check DiskQuotaExceeded"}),
        ),
        voice(
            "a",
            &json!({"type":"transcript_segment","role":"assistant","text":"The quota was exhausted"}),
        ),
        voice(
            "u",
            &json!({"type":"transcript_segment","role":"assistant","text":"duplicate voice"}),
        ),
        voice(
            "b",
            &json!({"type":"bem_item_promoted","turn_id":"turn","item_id":"item","presentation":{"type":"inline_visualization","index":5}}),
        ),
        voice(
            "end",
            &json!({"type":"realtime_session_closed","outcome":"ended"}),
        ),
    ];
    let rows: Vec<_> = rows
        .into_iter()
        .enumerate()
        .map(|(i, r)| stamped(r, u64::try_from(i + 1).unwrap()))
        .collect();
    for mode in ["legacy", "paginated"] {
        let mut rows = rows.clone();
        if mode == "paginated" {
            rows.push(stamped(
                event("thread_rolled_back", json!({"num_turns":1})),
                9,
            ));
        }
        for preview in [false, true] {
            let snap = read_with_history_mode(rows.clone(), preview, Some(mode)).unwrap();
            let speech: Vec<_> = snap
                .events
                .iter()
                .filter(|e| {
                    e.source.raw_record_type.as_deref() == Some("realtime_item.transcript_segment")
                })
                .map(|e| (&e.kind, e.payload["text"].as_str()))
                .collect();
            assert_eq!(
                speech,
                [
                    (&EventKind::MessageUser, Some("Check DiskQuotaExceeded")),
                    (
                        &EventKind::MessageAssistant,
                        Some("The quota was exhausted")
                    )
                ]
            );
            let search = omnis_core::trajectory_search_document(&snap);
            assert!(search.source_complete);
            for text in [
                search.text,
                omnis_core::render_session_history_preview(&snap),
                omnis_core::render_semantic_handoff(&snap),
            ] {
                assert!(
                    text.contains("Check DiskQuotaExceeded")
                        && text.contains("The quota was exhausted"),
                    "{text}"
                );
                assert!(!text.contains("duplicate voice"));
            }
        }
    }
}

#[test]
fn exec_capture_limit_keeps_one_failed_result_with_matching_omission_evidence() {
    let edge = 512 * 1024;
    let retained = 2_000;
    let capture = format!(
        "{}\n... 128 bytes omitted ...\n{}",
        "A".repeat(edge),
        "Z".repeat(edge)
    );
    let tokens = (2 * edge + 128) / 4;
    let model_cut = (capture.len() - 2 * retained).div_ceil(4);
    let call = physical_call(
        "exec_command",
        &json!({"cmd":"run-check","max_output_tokens":1000}),
    );
    let mut display = command_display("run-check", "failed", &capture);
    display["payload"]["item"]["exit_code"] = 1.into();
    for (omitted, count, changed_edge, matched) in [
        (128, tokens, false, true),
        (129, tokens, false, false),
        (128, tokens + 1, false, false),
        (128, tokens, true, false),
    ] {
        let prefix = if changed_edge { "B" } else { "A" }.repeat(retained);
        let suffix = "Z".repeat(retained);
        let raw = physical_result(&format!(
            "Chunk ID: ab12cd\nWall time: 0.0500 seconds\nProcess exited with code 1\nOriginal token count: {tokens}\nOutput:\nWarning: truncated output (original token count: {count})\n... {omitted} bytes omitted ...\n\n{prefix}…{model_cut} tokens truncated…{suffix}"
        ));
        for order in [
            [raw.clone(), display.clone()],
            [display.clone(), raw.clone()],
        ] {
            let mut rows = vec![
                event("task_started", json!({"turn_id":"turn"})),
                page_user("turn", "user", "request"),
                call.clone(),
            ];
            rows.extend(order);
            for mode in ["legacy", "paginated"] {
                for preview in [false, true] {
                    let snap = read_with_history_mode(rows.clone(), preview, Some(mode)).unwrap();
                    let search = omnis_core::trajectory_search_document(&snap);
                    assert_eq!(
                        search.source_complete, matched,
                        "{omitted} {count} {mode} {preview}"
                    );
                    let result = snap
                        .events
                        .iter()
                        .find(|e| {
                            e.source.raw_record_type.as_deref()
                                == Some("response_item.function_call_output")
                        })
                        .unwrap();
                    assert_eq!(result.kind == EventKind::ToolFailed, matched);
                    let trajectory = omnis_core::import_trajectory(&snap);
                    let native = omnis_core::native_trajectory_items(&trajectory);
                    let failed_pair = native.iter().any(|item| matches!(item,
                        omnis_core::NativeTrajectoryItem::Tool { is_error: true, output, .. } if output.contains("Process exited with code 1")
                    ));
                    assert_eq!(failed_pair, matched);
                    let handoff = omnis_core::plan_semantic_handoff(&snap);
                    assert_eq!(
                        handoff
                            .tool_outcomes
                            .iter()
                            .filter(
                                |outcome| outcome.status == omnis_core::ToolOutcomeStatus::Failed
                            )
                            .count(),
                        1
                    );
                }
            }
        }
    }
}

#[test]
fn realtime_timeline_preserves_speech_outside_the_active_checkpoint() {
    let mut rows =
        super::paginated_visibility::exchange("first", "RetainedQuestion", "RetainedReply", true);
    let replacement: Vec<_> = rows
        .iter()
        .filter(|r| r["type"] == "response_item")
        .map(|r| r["payload"].clone())
        .collect();
    let speech = |id, role, text| {
        voice(
            id,
            &json!({"type":"transcript_segment","role":role,"text":text}),
        )
    };
    rows.extend([
        speech("q", "user", "EarlierVoiceQuestion"),
        speech("a", "assistant", "EarlierVoiceReply"),
    ]);
    rows.extend(super::paginated_visibility::exchange(
        "second",
        "EarlierTypedQuestion",
        "EarlierTypedReply",
        false,
    ));
    rows.push(json!({"type":"compacted","payload":{"message":"checkpoint","window_number":1,"replacement_history":replacement}}));
    rows.push(speech("after", "user", "NewVoiceQuestion"));
    let rows: Vec<_> = rows
        .into_iter()
        .zip(1..)
        .map(|(r, n)| stamped(r, n))
        .collect();
    for preview in [false, true] {
        let snap = read_with_history_mode(rows.clone(), preview, Some("paginated")).unwrap();
        let handoff = omnis_core::render_semantic_handoff(&snap);
        let native = omnis_core::native_trajectory_items(&omnis_core::import_trajectory(&snap));
        for historical in ["EarlierVoiceQuestion", "EarlierVoiceReply"] {
            assert!(snap.events.iter().any(|e| e.payload["text"] == historical));
            assert!(!handoff.contains(historical));
            assert!(!native.iter().any(|item|matches!(item,omnis_core::NativeTrajectoryItem::Message{text,..} if text.contains(historical))));
        }
        for active in ["RetainedQuestion", "RetainedReply", "NewVoiceQuestion"] {
            assert!(handoff.contains(active), "{handoff}");
            assert!(native.iter().any(|item|matches!(item,omnis_core::NativeTrajectoryItem::Message{text,..} if text.contains(active))));
        }
    }
}
