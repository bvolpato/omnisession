use std::{fs, path::Path};

use omnis_adapters::{CodexAdapter, ProviderAdapter};
use omnis_ir::{CanonicalSnapshot, EventKind, Provider, SessionRef};
use serde_json::{Value, json};

const SESSION_ID: &str = "44444444-4444-4444-8444-444444444444";

fn read_records(records: Vec<Value>) -> anyhow::Result<CanonicalSnapshot> {
    read_records_with_preview(records, false)
}

fn read_records_with_preview(
    records: Vec<impl std::fmt::Display>,
    preview: bool,
) -> anyhow::Result<CanonicalSnapshot> {
    let temporary = tempfile::tempdir()?;
    fs::create_dir(temporary.path().join("sessions"))?;
    let source = temporary
        .path()
        .join(format!("sessions/rollout-{SESSION_ID}.jsonl"));
    let mut document = json!({
        "type": "session_meta",
        "payload": {"id": SESSION_ID, "cwd": "/workspace/original"},
    })
    .to_string();
    for record in records {
        document.push('\n');
        document.push_str(&record.to_string());
    }
    document.push('\n');
    fs::write(&source, &document)?;
    let adapter = CodexAdapter::with_root(temporary.path());
    let session = SessionRef::new(Provider::Codex, SESSION_ID);
    let snapshot = if preview {
        adapter.preview_session(&session)
    } else {
        adapter.read_session(&session)
    };
    assert_eq!(fs::read_to_string(source)?, document);
    snapshot
}

fn message(role: &str, text: &str) -> Value {
    json!({"type": "response_item", "payload": {
        "type": "message", "role": role,
        "content": [{"type": "input_text", "text": text}],
    }})
}

fn event(kind: &str, payload: Value) -> Value {
    let mut payload = payload;
    payload["type"] = kind.into();
    json!({"type": "event_msg", "payload": payload})
}

fn visible_text(snapshot: &CanonicalSnapshot) -> Vec<&str> {
    snapshot
        .events
        .iter()
        .filter_map(|event| event.payload["text"].as_str())
        .collect()
}

#[test]
fn codex_session_index_with_an_oversized_row_keeps_titles_and_notes_it() {
    let temporary = tempfile::tempdir().expect("temporary Codex home");
    fs::create_dir(temporary.path().join("sessions")).expect("sessions directory");
    fs::write(
        temporary
            .path()
            .join(format!("sessions/rollout-{SESSION_ID}.jsonl")),
        format!(
            "{}\n{}\n",
            json!({"type": "session_meta", "payload": {"id": SESSION_ID, "cwd": "/workspace/original"}}),
            message("user", "synthetic request")
        ),
    )
    .expect("synthetic rollout");
    fs::write(
        temporary.path().join("session_index.jsonl"),
        [
            json!({"id": SESSION_ID, "thread_name": "Original synthetic title", "updated_at": "2026-01-01T00:00:00Z"}),
            json!({"id": SESSION_ID, "thread_name": "x".repeat(2 * 1024 * 1024), "updated_at": "2026-01-02T00:00:00Z"}),
            json!({"id": SESSION_ID, "thread_name": "Renamed synthetic title", "updated_at": "2026-01-03T00:00:00Z"}),
        ]
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n")
            + "\n",
    )
    .expect("synthetic session index");

    let adapter = CodexAdapter::with_root(temporary.path());
    let sessions = adapter.list_sessions(None).expect("Codex listing");

    assert_eq!(sessions.len(), 1);
    assert_eq!(
        sessions[0].title.as_deref(),
        Some("Renamed synthetic title")
    );
    let notes = adapter.discovery_notes();
    assert!(
        notes
            .iter()
            .any(|note| note.contains("session_index.jsonl") && note.contains("1 oversized")),
        "{notes:?}"
    );
    let snapshot = adapter
        .read_session(&SessionRef::new(Provider::Codex, SESSION_ID))
        .expect("Codex read");
    assert_eq!(snapshot.title.as_deref(), Some("Renamed synthetic title"));
}

#[test]
fn missing_codex_home_does_not_claim_complete_discovery() {
    let temporary = tempfile::tempdir().expect("temporary parent directory");
    let missing_home = temporary.path().join("missing-codex-home");
    let adapter = CodexAdapter::with_root(missing_home);

    assert!(
        adapter
            .list_sessions(None)
            .expect("list missing home")
            .is_empty()
    );
    assert!(!adapter.discovery_is_complete());

    let empty_home = temporary.path().join("empty-codex-home");
    fs::create_dir(&empty_home).expect("empty Codex home");
    let adapter = CodexAdapter::with_root(empty_home);
    assert!(
        adapter
            .list_sessions(None)
            .expect("list empty home")
            .is_empty()
    );
    assert!(adapter.discovery_is_complete());
}

#[test]
fn fork_candidates_cover_rollouts_filed_under_a_local_date() {
    const SOURCE: &str = "11111111-1111-4111-8111-111111111111";
    const CHILD: &str = "22222222-2222-4222-8222-222222222222";
    // West of UTC, a fork at this instant is filed under the previous day. East, under the next.
    for folder in ["2026/10/31", "2026/11/01", "2026/11/02"] {
        let temporary = tempfile::tempdir().expect("temporary Codex home");
        let day = temporary.path().join("sessions").join(folder);
        fs::create_dir_all(&day).expect("session day");
        fs::write(
            day.join(format!("rollout-{CHILD}.jsonl")),
            json!({
                "timestamp": "2026-11-01T02:24:47Z",
                "type": "session_meta",
                "payload": {"id": CHILD, "cwd": "/workspace/original", "source": "cli"}
            })
            .to_string(),
        )
        .expect("synthetic rollout");

        let candidates = CodexAdapter::with_root(temporary.path())
            .fork_candidates_created_since(
                &SessionRef::new(Provider::Codex, SOURCE),
                Path::new("/workspace/original"),
                "2026-11-01T02:24:00Z".parse().expect("start time"),
            )
            .expect("fork candidates");

        assert_eq!(
            candidates,
            [SessionRef::new(Provider::Codex, CHILD)],
            "{folder}"
        );
    }
}

#[test]
fn codex_listing_skips_guardian_subagent_threads() {
    let temporary = tempfile::tempdir().expect("temporary Codex home");
    fs::create_dir(temporary.path().join("sessions")).expect("sessions directory");
    let user_session = "11111111-1111-4111-8111-111111111111";
    let guardian_session = "22222222-2222-4222-8222-222222222222";
    for (id, source) in [
        (user_session, json!("cli")),
        (guardian_session, json!({"subagent": {"other": "guardian"}})),
    ] {
        let document = [
            json!({"type": "session_meta", "payload": {"id": id, "cwd": "/workspace/original", "source": source}}),
            message("user", "synthetic request"),
        ]
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
        fs::write(
            temporary
                .path()
                .join(format!("sessions/rollout-{id}.jsonl")),
            document + "\n",
        )
        .expect("synthetic rollout");
    }

    let sessions = CodexAdapter::with_root(temporary.path())
        .list_sessions(None)
        .expect("Codex listing");

    assert_eq!(
        sessions
            .iter()
            .map(|session| session.session.id.as_str())
            .collect::<Vec<_>>(),
        [user_session]
    );
}

#[test]
fn codex_rollback_removes_mirrors_tools_and_workspace_before_followup() {
    let snapshot = read_records(vec![
        event("user_message", json!({"message": "keep this request"})),
        json!({"type": "turn_context", "payload": {"cwd": "/workspace/original"}}),
        message("user", "keep this request"),
        message("assistant", "keep this answer"),
        event("task_complete", json!({"turn_id": "kept"})),
        event("task_started", json!({"turn_id": "discarded"})),
        json!({"type": "turn_context", "payload": {"cwd": "/workspace/discarded", "model": "discarded-model"}}),
        event("user_message", json!({"message": "discard this request"})),
        message("user", "discard this request"),
        json!({"type": "response_item", "payload": {"type": "function_call", "name": "discarded-tool", "arguments": "{}"}}),
        json!({"type": "response_item", "payload": {"type": "function_call_output", "output": "discarded-result"}}),
        message("assistant", "discard this answer"),
        event("thread_rolled_back", json!({"num_turns": 1})),
        message("user", "replacement request"),
        message("assistant", "replacement answer"),
    ]).expect("rollback-aware Codex read");
    assert_eq!(
        visible_text(&snapshot),
        [
            "keep this request",
            "keep this answer",
            "replacement request",
            "replacement answer"
        ]
    );
    assert_eq!(
        snapshot.workspace.current_dir.to_str(),
        Some("/workspace/original")
    );
    assert!(
        !serde_json::to_string(&snapshot)
            .unwrap()
            .contains("discarded")
    );
}

#[test]
fn codex_rollback_counts_multipart_and_repeated_requests_once_per_message() {
    let mut multipart = message("user", "first part");
    multipart["payload"]["content"]
        .as_array_mut()
        .unwrap()
        .push(json!({"type": "input_text", "text": "second part"}));
    let snapshot = read_records(vec![
        message("user", "same request"),
        event("task_complete", json!({"turn_id": "first"})),
        event("task_started", json!({"turn_id": "second"})),
        event("user_message", json!({"message": "same request"})),
        message("user", "same request"),
        multipart,
        event(
            "user_message",
            json!({"message": "first part\nsecond part"}),
        ),
        event("thread_rolled_back", json!({"num_turns": 2})),
    ])
    .expect("rollback count");
    assert_eq!(visible_text(&snapshot), ["same request"]);
}

#[test]
fn codex_rollback_crossing_compaction_does_not_resurrect_discarded_output() {
    let snapshot = read_records(vec![
        message("user", "surviving request"),
        message("assistant", "surviving answer"),
        message("user", "discarded request"),
        json!({"type": "compacted", "payload": {"message": "discarded summary", "replacement_history": [
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "discarded compacted context"}]}
        ]}}),
        message("assistant", "discarded post-compaction answer"),
        event("thread_rolled_back", json!({"num_turns": 1})),
    ]).expect("rollback across compaction");
    assert_eq!(
        visible_text(&snapshot),
        ["surviving request", "surviving answer"]
    );
}

#[test]
fn codex_rollback_handles_zero_excess_and_consecutive_rollbacks() {
    let snapshot = read_records(vec![
        message("user", "# AGENTS.md instructions for /workspace/original\n\n<INSTRUCTIONS>Project context</INSTRUCTIONS>"),
        message("user", "first request"),
        event("thread_rolled_back", json!({"num_turns": 0})),
        message("assistant", "first answer"),
        message("user", "second request"),
        event("thread_rolled_back", json!({"num_turns": 1})),
        event("thread_rolled_back", json!({"num_turns": u64::MAX})),
        message("user", "fresh request"),
        message("assistant", "fresh answer"),
    ]).expect("saturating rollback");
    let texts = visible_text(&snapshot);
    assert_eq!(texts.len(), 3);
    assert!(texts[0].contains("Project context"));
    assert_eq!(&texts[1..], ["fresh request", "fresh answer"]);
}

#[test]
fn codex_invalid_rollback_is_reported_instead_of_exporting_discarded_history() {
    let error = read_records(vec![
        message("user", "possibly discarded request"),
        event("thread_rolled_back", json!({"num_turns": "one"})),
    ])
    .expect_err("malformed rollback must not silently preserve stale turns");
    assert!(error.to_string().contains("rollback"));
}

#[test]
fn codex_rollback_still_truncates_after_tool_event_pruning() {
    let mut records = vec![
        message("user", "surviving request"),
        message("user", "discarded request"),
    ];
    records.extend((0..1_050).map(|index| json!({"type": "response_item", "payload": {
        "type": "function_call_output", "call_id": index.to_string(), "output": "discarded output",
    }})));
    records.push(event("thread_rolled_back", json!({"num_turns": 1})));
    let snapshot = read_records(records).expect("rollback after tool pruning");
    assert_eq!(visible_text(&snapshot), ["surviving request"]);
    assert!(
        !snapshot
            .events
            .iter()
            .any(|event| event.kind == EventKind::ToolCompleted)
    );
    assert!(!omnis_core::import_conversation(&snapshot).truncated);
}

#[test]
fn codex_rollback_reports_surviving_tools_evicted_during_discarded_turn() {
    let mut records = vec![
        message("user", "surviving request"),
        json!({"type": "response_item", "payload": {
            "type": "function_call_output", "call_id": "surviving", "output": "surviving output",
        }}),
        message("user", "discarded request"),
    ];
    records.extend((0..1_050).map(|index| json!({"type": "response_item", "payload": {
        "type": "function_call_output", "call_id": index.to_string(), "output": "discarded output",
    }})));
    records.push(event("thread_rolled_back", json!({"num_turns": 1})));
    let snapshot = read_records(records).expect("rollback after tool pruning");
    assert_eq!(visible_text(&snapshot), ["surviving request"]);
    assert_eq!(
        snapshot.events.last().unwrap().payload["omitted_tool_events"],
        1
    );
    assert!(omnis_core::import_conversation(&snapshot).truncated);
}

#[test]
fn codex_preview_finds_rollback_outside_prefix_and_tail_samples() {
    let mut records = vec![message("user", "discarded request")];
    records.extend((0..1_050).map(|_| event("token_count", json!({}))));
    records.push(event("thread_rolled_back", json!({"num_turns": 1})));
    records.push(json!({"type": "response_item", "payload": {
        "type": "reasoning", "encrypted_content": "x".repeat(5 * 1024 * 1024),
    }}));
    records.push(message("user", "replacement request"));
    let encoded = records
        .into_iter()
        .map(|record| {
            record
                .to_string()
                .replace("thread_rolled_back", r"thread_rolled_\u0062ack")
        })
        .collect();
    let snapshot = read_records_with_preview(encoded, true).expect("rollback-aware preview");
    assert_eq!(visible_text(&snapshot), ["replacement request"]);
}

#[test]
fn codex_preview_reports_sampling_after_rollback_reconstruction() {
    let mut records = (0..1_100)
        .map(|index| message("user", &format!("request {index}")))
        .collect::<Vec<_>>();
    records.push(event("thread_rolled_back", json!({"num_turns": 1})));
    let snapshot = read_records_with_preview(records, true).expect("bounded active preview");
    assert_eq!(snapshot.events.len(), 1_025);
    assert_eq!(
        snapshot.events.last().unwrap().payload["omitted_events"],
        75
    );
    assert!(omnis_core::import_conversation(&snapshot).truncated);
    assert!(!visible_text(&snapshot).contains(&"request 1099"));
}

#[test]
fn codex_skips_oversized_records_and_reports_omission() {
    const STREAMED_LINE_LIMIT: usize = 16 * 1024 * 1024;
    let prefix = r#"{"type":"event_msg","payload":{"type":"item_completed","item":{"changes":""#;
    let suffix = r#""}}}"#;
    let boundary = format!(
        "{prefix}{}{suffix}",
        "x".repeat(STREAMED_LINE_LIMIT - prefix.len() - suffix.len())
    );
    let oversized = format!(
        "{}{}",
        "x".repeat(STREAMED_LINE_LIMIT + 1),
        message("user", "tail of oversized record")
    );
    let snapshot = read_records_with_preview(
        vec![
            message("user", "request before large change").to_string(),
            boundary,
            oversized,
            message("assistant", "answer after large change").to_string(),
        ],
        false,
    )
    .expect("oversized records must not abort streaming read");
    assert_eq!(
        visible_text(&snapshot),
        ["request before large change", "answer after large change"]
    );
    assert_eq!(
        snapshot.events.last().unwrap().payload["omitted_records"],
        2
    );
    assert!(omnis_core::import_conversation(&snapshot).truncated);
}

#[test]
fn codex_image_only_turns_keep_one_placeholder() {
    const IMAGE: &str = "data:image/png;base64,iVBORw0KGgo=";
    let user_content = |content: Value| json!({"type": "response_item", "payload": {"type": "message", "role": "user", "content": content}});
    let snapshot = read_records(vec![
        user_content(json!([{"type": "input_image", "image_url": IMAGE}])),
        event("user_message", json!({"message": "", "images": [IMAGE]})),
        message("assistant", "pasted image seen"),
        // Local images arrive wrapped in Codex's `<image name=...>` tag texts.
        user_content(json!([
            {"type": "input_text", "text": "<image name=[Image #1] path=\"/tmp/screen.png\">"},
            {"type": "input_image", "image_url": IMAGE},
            {"type": "input_text", "text": "</image>"}
        ])),
        event(
            "user_message",
            json!({"message": "", "local_images": ["/tmp/screen.png"]}),
        ),
        message("assistant", "local image seen"),
        // A tag typed without an image is the user's own text.
        message("user", "</image>"),
    ])
    .expect("image-only turns");
    assert_eq!(
        visible_text(&snapshot),
        [
            "[1 image omitted]",
            "pasted image seen",
            "[1 image omitted]",
            "local image seen",
            "</image>"
        ]
    );
    assert!(
        !serde_json::to_string(&snapshot)
            .expect("serialize Codex snapshot")
            .contains("iVBORw0KGgo")
    );
}

#[test]
fn codex_reads_rollouts_above_the_record_budget_keeping_the_newest_events() {
    let mut records = Vec::with_capacity(100_002);
    for index in 0..50_001 {
        records.push(message("user", &format!("request {index}")).to_string());
        records.push(message("assistant", &format!("answer {index}")).to_string());
    }
    let snapshot = read_records_with_preview(records, false).expect("large rollout");
    let text = visible_text(&snapshot);
    assert_eq!(text.len(), 100_000);
    assert_eq!(text.first(), Some(&"request 1"));
    assert_eq!(text.last(), Some(&"answer 50000"));
    assert_eq!(snapshot.events.last().unwrap().payload["omitted_events"], 2);
    assert!(omnis_core::import_conversation(&snapshot).truncated);
}

#[test]
fn codex_contextual_fragments_in_any_block_do_not_add_rollback_turns() {
    let mut context = message("user", "contextual preface");
    context["payload"]["content"].as_array_mut().unwrap().push(json!({"type": "input_text", "text": "<environment_context>project settings</environment_context>"}));
    let snapshot = read_records(vec![
        message("user", "request to remove"),
        context,
        event("thread_rolled_back", json!({"num_turns": 1})),
    ])
    .expect("context fragment classification");
    assert!(visible_text(&snapshot).is_empty());
}

fn encode_rollout(contents: &[u8], extension: &str) -> Vec<u8> {
    use std::io::Write;
    if extension == "gz" {
        let mut writer = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        writer.write_all(contents).unwrap();
        writer.finish().unwrap()
    } else {
        zstd::stream::encode_all(contents, 1).unwrap()
    }
}

#[test]
fn codex_discovers_and_reads_compressed_active_and_archived_rollouts() {
    use omnis_adapters::NativeResumeSupport;
    for extension in ["gz", "zst"] {
        for directory in ["sessions/2026/05/15", "archived_sessions"] {
            let home = tempfile::tempdir().unwrap();
            let folder = home.path().join(directory);
            fs::create_dir_all(&folder).unwrap();
            let path = folder.join(format!(
                "rollout-2026-05-15T01-02-03-{SESSION_ID}.jsonl.{extension}"
            ));
            let rows = [
                json!({"type":"session_meta","payload":{"id":SESSION_ID,"cwd":"/sample/work","model_provider":"synthetic-provider"}}),
                message("user", "a discarded request"),
                event("thread_rolled_back", json!({"num_turns":1})),
                message("user", "the active request"),
                message("assistant", "the complete reply"),
            ];
            let bytes = encode_rollout(
                rows.map(|row| row.to_string()).join("\n").as_bytes(),
                extension,
            );
            fs::write(&path, &bytes).unwrap();
            let adapter = CodexAdapter::with_root(home.path());
            let listed = adapter.list_sessions(None).unwrap();
            assert_eq!(listed.len(), 1);
            assert_eq!(listed[0].source_path.as_ref(), Some(&path));
            let session = &listed[0].session;
            assert_eq!(
                adapter
                    .recorded_model_provider(SESSION_ID)
                    .unwrap()
                    .as_deref(),
                Some("synthetic-provider")
            );
            assert_eq!(
                adapter.native_resume_support(session).unwrap(),
                if directory.starts_with("sessions/") && extension == "zst" {
                    NativeResumeSupport::Since("0.137.0")
                } else {
                    NativeResumeSupport::ImportOnly
                }
            );
            for snapshot in [
                adapter.read_session(session).unwrap(),
                adapter.preview_session(session).unwrap(),
                adapter
                    .index_session(session, Some(&path), 100)
                    .unwrap()
                    .snapshot,
            ] {
                assert_eq!(
                    visible_text(&snapshot),
                    ["the active request", "the complete reply"]
                );
            }
            assert_eq!(fs::read(&path).unwrap(), bytes);
            assert_eq!(fs::read_dir(&folder).unwrap().count(), 1);
        }
    }
}

#[test]
fn codex_reports_unavailable_inheritance_and_compaction_before_restore() {
    for inherited in [true, false] {
        let home = tempfile::tempdir().unwrap();
        let folder = home.path().join("archived_sessions");
        fs::create_dir(&folder).unwrap();
        let source = folder.join(format!("rollout-{SESSION_ID}.jsonl.zst"));
        let mut header =
            json!({"type":"session_meta","payload":{"id":SESSION_ID,"cwd":"/example"}});
        let mut rows = Vec::new();
        if inherited {
            header["payload"]["history_mode"] = json!("paginated");
            header["payload"]["history_base"] = json!({
                "thread_id":"55555555-5555-4555-8555-555555555555",
                "end_byte_offset":512, "end_ordinal_exclusive":4
            });
        } else {
            rows.push(json!({"type":"compacted","payload":{
                "message":"Checkpoint for the remaining work",
                "replacement_history":[{"type":"message","role":"user","content":[{"type":"input_text","text":"retained request"}]}]
            }}));
        }
        rows.insert(0, header);
        rows.push(message("user", "request in the retained rollout"));
        let text = rows
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        let encoded = encode_rollout(text.as_bytes(), "zst");
        fs::write(&source, &encoded).unwrap();
        let adapter = CodexAdapter::with_root(home.path());
        let session = SessionRef::new(Provider::Codex, SESSION_ID);
        let indexed = adapter
            .index_session(&session, Some(&source), 4 * 1024 * 1024)
            .unwrap();
        assert!(
            indexed.source_complete,
            "the complete leaf rollout was read"
        );
        for snapshot in [
            indexed.snapshot,
            adapter.read_session(&session).unwrap(),
            adapter.preview_session(&session).unwrap(),
        ] {
            assert!(visible_text(&snapshot).contains(&"request in the retained rollout"));
            let trajectory = omnis_core::import_trajectory(&snapshot);
            let restored_checkpoint = !inherited
                && trajectory
                    .items
                    .iter()
                    .any(|item| item.text.contains("retained request"));
            if !restored_checkpoint {
                assert!(!omnis_core::trajectory_search_document(&snapshot).source_complete);
                assert!(omnis_core::import_conversation(&snapshot).truncated);
                assert!(trajectory.truncated);
            }
        }
        assert_eq!(fs::read(&source).unwrap(), encoded);
    }
}

#[test]
fn codex_uses_plain_sibling_and_reports_truncated_compressed_history() {
    let home = tempfile::tempdir().unwrap();
    let folder = home.path().join("sessions");
    fs::create_dir(&folder).unwrap();
    let path = folder.join(format!("rollout-{SESSION_ID}.jsonl"));
    let header = json!({"type":"session_meta","payload":{"id":SESSION_ID,"cwd":"/example"}});
    let source = SessionRef::new(Provider::Codex, SESSION_ID);
    for extension in ["zst", "gz"] {
        let compressed = path.with_extension(format!("jsonl.{extension}"));
        let mut bytes = encode_rollout(
            format!("{header}\n{}\n", message("user", "stale")).as_bytes(),
            extension,
        );
        bytes.truncate(bytes.len() - 2);
        fs::write(&compressed, bytes).unwrap();
        fs::write(&path, format!("{header}\n{}\n", message("user", "newest"))).unwrap();
        let adapter = CodexAdapter::with_root(home.path());
        assert_eq!(
            visible_text(&adapter.read_session(&source).unwrap()),
            ["newest"]
        );
        assert_eq!(adapter.list_sessions(None).unwrap().len(), 1);
        fs::remove_file(&path).unwrap();
        assert!(adapter.read_session(&source).is_err());
        fs::remove_file(&compressed).unwrap();
    }
}

#[test]
fn compressed_rollback_preview_bounds_message_bytes_and_recovers_surviving_edges() {
    use std::io::Write;

    let temporary = tempfile::tempdir().unwrap();
    let folder = temporary.path().join("archived_sessions");
    fs::create_dir(&folder).unwrap();
    let path = folder.join(format!("rollout-{SESSION_ID}.jsonl.zst"));
    let mut writer =
        zstd::stream::write::Encoder::new(fs::File::create(&path).unwrap(), 1).unwrap();
    let header = json!({"type":"session_meta","payload":{"id":SESSION_ID,"cwd":"/synthetic","cli_version":"v".repeat(32 * 1024)}});
    writeln!(writer, "{header}").unwrap();
    writeln!(
        writer,
        "{}",
        json!({"type":"turn_context","payload":{"cwd":"/synthetic/surviving"}})
    )
    .unwrap();
    writeln!(writer, "{}", message("user", "surviving head")).unwrap();
    let surviving = message("assistant", &"s".repeat(128 * 1024)).to_string();
    for _ in 0..96 {
        writeln!(writer, "{surviving}").unwrap();
    }
    writeln!(writer, "{}", message("assistant", "reply before rollback")).unwrap();
    let discarded = message("user", &format!("discarded {}", "d".repeat(128 * 1024))).to_string();
    let path_component = "a/".repeat(16 * 1024);
    for index in 0..160 {
        let cwd = format!("/synthetic/{index}/{path_component}");
        writeln!(
            writer,
            "{}",
            json!({"type":"turn_context","payload":{"cwd":cwd}})
        )
        .unwrap();
        writeln!(writer, "{discarded}").unwrap();
    }
    writeln!(
        writer,
        "{}",
        event("thread_rolled_back", json!({"num_turns":160}))
    )
    .unwrap();
    writeln!(writer, "{}", message("user", "follow-up")).unwrap();
    writer.finish().unwrap();

    let adapter = CodexAdapter::with_root(temporary.path());
    let session = SessionRef::new(Provider::Codex, SESSION_ID);
    let read = adapter
        .index_session(&session, Some(&path), 64 * 1024)
        .unwrap();
    let text = visible_text(&read.snapshot);
    assert_eq!(text.first(), Some(&"surviving head"));
    assert!(text.ends_with(&["reply before rollback", "follow-up"]));
    assert!(text.iter().all(|message| !message.starts_with("discarded")));
    let text_bytes = text.iter().map(|text| text.len()).sum::<usize>();
    let metadata_bytes: usize = read
        .snapshot
        .events
        .iter()
        .filter_map(|e| e.source.provider_version.as_ref())
        .map(String::len)
        .sum();
    assert!(text_bytes + metadata_bytes <= 8 * 1024 * 1024);
    assert_eq!(
        read.snapshot.workspace.root,
        Path::new("/synthetic/surviving")
    );
    assert!(!read.source_complete);
    assert!(omnis_core::import_conversation(&read.snapshot).truncated);
}

#[test]
fn codex_index_budgets_the_decoded_size_of_compressed_rollouts() {
    let temporary = tempfile::tempdir().unwrap();
    let folder = temporary.path().join("archived_sessions");
    fs::create_dir(&folder).unwrap();
    let path = folder.join(format!("rollout-{SESSION_ID}.jsonl.zst"));
    let header = json!({"type":"session_meta", "payload":{"id":SESSION_ID, "cwd":"/synthetic"}});
    let padding = format!(
        "{}\n",
        json!({"type":"progress", "data":"x".repeat(64 * 1024)})
    )
    .repeat(80);
    let raw = format!(
        "{header}\n{}\n{padding}{}\n{padding}{}\n",
        message("user", "head"),
        message("assistant", "middle"),
        message("user", "tail")
    );
    let encoded = encode_rollout(raw.as_bytes(), "zst");
    let budget = 64 * 1024;
    assert!((encoded.len() as u64) < budget);
    fs::write(&path, encoded).unwrap();
    let adapter = CodexAdapter::with_root(temporary.path());
    let session = SessionRef::new(Provider::Codex, SESSION_ID);
    let sampled = adapter
        .index_session(&session, Some(&path), budget)
        .unwrap();
    assert!(!sampled.source_complete);
    assert_eq!(visible_text(&sampled.snapshot), ["head", "tail"]);
    assert_eq!(
        visible_text(&adapter.read_session(&session).unwrap()),
        ["head", "middle", "tail"]
    );
}

#[test]
fn codex_list_read_index_and_resume_choose_the_same_rollout() -> anyhow::Result<()> {
    use std::time::{Duration, SystemTime};

    use omnis_adapters::NativeResumeSupport;

    let home = tempfile::tempdir()?;
    let archive_dir = home.path().join("archived_sessions");
    let active_dir = home.path().join("sessions/2026/03/10");
    fs::create_dir_all(&active_dir)?;
    fs::create_dir(&archive_dir)?;
    let name = format!("rollout-2026-03-10T09-00-00-{SESSION_ID}.jsonl");
    let archive = archive_dir.join(format!("{name}.gz"));
    let active = active_dir.join(name);
    let source = |text| {
        format!(
            "{}\n{}\n",
            json!({"type":"session_meta","payload":{"id":SESSION_ID}}),
            message("user", text)
        )
    };
    fs::write(
        &archive,
        encode_rollout(source("archived request").as_bytes(), "gz"),
    )?;
    let picker = CodexAdapter::with_root(home.path());
    let cached = picker.list_sessions(None)?.pop().unwrap();
    assert_eq!(cached.source_path.as_ref(), Some(&archive));
    fs::write(&active, source("active request"))?;
    fs::File::options()
        .write(true)
        .open(&active)?
        .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(100))?;
    let adapter = CodexAdapter::with_root(home.path());
    let listed = adapter.list_sessions(None)?;
    assert_eq!(listed.len(), 1);
    let session = &listed[0].session;
    assert_eq!(listed[0].source_path.as_ref(), Some(&active));
    assert_eq!(
        adapter.native_resume_support(session)?,
        NativeResumeSupport::Direct
    );
    assert_eq!(
        visible_text(&adapter.read_session(session)?),
        ["active request"]
    );
    assert_eq!(
        visible_text(
            &picker
                .index_session(session, cached.source_path.as_deref(), u64::MAX)?
                .snapshot
        ),
        ["active request"]
    );
    fs::remove_file(&active)?;
    assert_eq!(
        visible_text(
            &adapter
                .index_session(session, Some(&active), u64::MAX)?
                .snapshot
        ),
        ["archived request"]
    );
    assert_eq!(
        adapter.native_resume_support(session)?,
        NativeResumeSupport::ImportOnly
    );
    Ok(())
}

#[test]
fn unreadable_active_headers_fall_back_to_verified_archives_and_siblings() -> anyhow::Result<()> {
    use omnis_adapters::NativeResumeSupport;

    let home = tempfile::tempdir()?;
    let active = home.path().join("sessions/2026/03/10");
    let archive = home.path().join("archived_sessions");
    fs::create_dir_all(&active)?;
    fs::create_dir(&archive)?;
    let name = format!("rollout-2026-03-10T09-00-00-{SESSION_ID}.jsonl");
    let wrong_id = "11111111-1111-4111-8111-111111111111";
    let contents = |id, text| {
        format!(
            "{}\n{}\n",
            json!({"type":"session_meta","payload":{"id":id,"model_provider":text}}),
            message("user", text)
        )
    };
    let plain = active.join(&name);
    let compressed = plain.with_extension("jsonl.zst");
    fs::write(&plain, contents(wrong_id, "unrelated"))?;
    fs::write(&compressed, [0x28, 0xb5, 0x2f])?;
    let fallback = archive.join(name);
    fs::write(&fallback, contents(SESSION_ID, "archived"))?;
    let source = SessionRef::new(Provider::Codex, SESSION_ID);
    for (path, expected) in [(&fallback, "archived"), (&compressed, "compressed")] {
        if expected == "compressed" {
            fs::write(
                &compressed,
                encode_rollout(contents(SESSION_ID, expected).as_bytes(), "zst"),
            )?;
        }
        let adapter = CodexAdapter::with_root(home.path());
        let listed = adapter.list_sessions(None)?;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].source_path.as_ref(), Some(path));
        assert_eq!(
            adapter
                .find_native_session(SESSION_ID)?
                .unwrap()
                .source_path
                .as_ref(),
            Some(path)
        );
        assert_eq!(
            adapter.recorded_model_provider(SESSION_ID)?.as_deref(),
            Some(expected)
        );
        assert_eq!(visible_text(&adapter.read_session(&source)?), [expected]);
        assert_eq!(
            visible_text(
                &adapter
                    .index_session(&source, Some(&plain), u64::MAX)?
                    .snapshot
            ),
            [expected]
        );
        assert_eq!(
            adapter.native_resume_support(&source)?,
            NativeResumeSupport::ImportOnly
        );
    }
    Ok(())
}

#[test]
fn cached_lookup_finds_a_new_archive_after_all_old_headers_fail() -> anyhow::Result<()> {
    use omnis_adapters::NativeResumeSupport;

    let home = tempfile::tempdir()?;
    let day = home.path().join("sessions/2026/03/10");
    fs::create_dir_all(&day)?;
    let name = format!("rollout-2026-03-10T09-00-00-{SESSION_ID}.jsonl");
    let active = day.join(&name);
    let folder = home.path().join("archived_sessions");
    fs::create_dir_all(folder.join("older"))?;
    let rows = format!(
        "{}\n{}\n",
        json!({"type":"session_meta","payload":{"id":SESSION_ID,"cwd":"/example"}}),
        message("user", "preserved request")
    );
    fs::write(active.with_extension("jsonl.zst"), [0x28, 0xb5, 0x2f])?;
    for archive in [folder.join(&name), folder.join("older").join(&name)] {
        fs::write(&active, &rows)?;
        let picker = CodexAdapter::with_root(home.path());
        let direct = CodexAdapter::with_root(home.path());
        let session = picker.list_sessions(None)?.pop().unwrap().session;
        assert_eq!(
            direct.native_resume_support(&session)?,
            NativeResumeSupport::Direct
        );
        fs::rename(&active, &archive)?;
        let read = picker.index_session(&session, Some(&archive), u64::MAX)?;
        assert_eq!(visible_text(&read.snapshot), ["preserved request"]);
        assert_eq!(
            visible_text(&direct.read_session(&session)?),
            ["preserved request"]
        );
        assert_eq!(
            direct.native_resume_support(&session)?,
            NativeResumeSupport::ImportOnly
        );
        assert_eq!(
            direct
                .find_native_session(SESSION_ID)?
                .unwrap()
                .source_path
                .as_deref(),
            Some(archive.as_path())
        );
        fs::remove_file(&archive)?;
        assert!(format!("{:#}", direct.read_session(&session).unwrap_err()).contains("frame"));
        #[cfg(unix)]
        {
            let outside = tempfile::tempdir()?;
            let outside_rollout = outside.path().join(&name);
            fs::write(&outside_rollout, &rows)?;
            std::os::unix::fs::symlink(&outside_rollout, &archive)?;
            assert!(
                picker
                    .index_session(&session, Some(&archive), u64::MAX)
                    .is_err()
            );
            fs::remove_file(&archive)?;
        }
    }
    Ok(())
}

#[test]
fn picker_learns_a_preferred_new_source_while_its_archive_stays_readable() -> anyhow::Result<()> {
    use omnis_adapters::NativeResumeSupport;

    let home = tempfile::tempdir()?;
    let archive = home
        .path()
        .join("archived_sessions")
        .join(format!("rollout-2026-03-10T09-00-00-{SESSION_ID}.jsonl"));
    let active_dir = home.path().join("sessions/2026/03/11");
    let name = format!("rollout-2026-03-11T11-00-00-{SESSION_ID}.jsonl");
    let active = active_dir.join(&name);
    let rows = |id, text| {
        format!(
            "{}\n{}\n",
            json!({"type":"session_meta","payload":{"id":id}}),
            message("user", text)
        )
    };
    fs::create_dir_all(archive.parent().unwrap())?;
    fs::create_dir_all(&active_dir)?;
    fs::write(&archive, rows(SESSION_ID, "archive"))?;
    let worker = CodexAdapter::with_root(home.path());
    let session = worker.list_sessions(None)?.pop().unwrap().session;
    let index = |path: &Path| worker.index_session(&session, Some(path), u64::MAX);
    let outside = home.path().join("other");
    fs::create_dir(&outside)?;
    fs::write(outside.join(&name), rows(SESSION_ID, "unrelated"))?;
    assert_eq!(
        visible_text(&index(&outside.join(&name))?.snapshot),
        ["archive"]
    );
    #[cfg(unix)]
    {
        let link = active_dir.join("alias");
        std::os::unix::fs::symlink(&outside, &link)?;
        assert_eq!(
            visible_text(&index(&link.join(&name))?.snapshot),
            ["archive"]
        );
    }
    let other_id = "11111111-1111-4111-8111-111111111111";
    let other = active.with_file_name(name.replace(SESSION_ID, other_id));
    fs::write(&other, rows(other_id, "another session"))?;
    assert_eq!(visible_text(&index(&other)?.snapshot), ["archive"]);
    fs::write(&active, rows(other_id, "mismatched header"))?;
    assert_eq!(visible_text(&index(&active)?.snapshot), ["archive"]);
    fs::remove_file(&active)?;

    // The other picker discovers the new rollout after the worker cached its archive.
    let worker = CodexAdapter::with_root(home.path());
    worker.find_native_session(SESSION_ID)?;
    fs::write(&active, rows(SESSION_ID, "active"))?;
    let fresh = CodexAdapter::with_root(home.path())
        .find_native_session(SESSION_ID)?
        .unwrap();
    assert_eq!(fresh.source_path.as_deref(), Some(active.as_path()));
    let read = worker.index_session(&session, fresh.source_path.as_deref(), u64::MAX)?;
    assert_eq!(visible_text(&read.snapshot), ["active"]);
    assert_eq!(visible_text(&worker.read_session(&session)?), ["active"]);
    assert_eq!(
        worker.native_resume_support(&session)?,
        NativeResumeSupport::Direct
    );
    assert_eq!(
        visible_text(
            &worker
                .index_session(&session, Some(&archive), u64::MAX)?
                .snapshot
        ),
        ["active"]
    );
    Ok(())
}
