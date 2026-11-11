use super::*;

fn model_request(content: &Value) -> Value {
    json!({"type":"response_item","payload":{"type":"message","role":"user","content":content}})
}

#[test]
fn rolling_back_an_audio_request_keeps_the_previous_request() {
    let audio = "data:audio/wav;base64,SYNTHETIC";
    let model = model_request(&json!([
        {"type":"input_text","text":"<audio name=[Audio #1] path=\"local.wav\">"},
        {"type":"input_audio","audio_url":audio},
        {"type":"input_text","text":"</audio>"}
    ]));
    let legacy = event(
        "user_message",
        json!({"message":"","local_audio":["local.wav"]}),
    );
    let display = completed(&json!({"type":"UserMessage","id":"audio","content":[
        {"type":"local_audio","path":"local.wav"}
    ]}));
    for copies in [
        vec![display.clone()],
        vec![model.clone()],
        vec![legacy.clone()],
        vec![model.clone(), legacy.clone(), display.clone()],
        vec![display, legacy, model],
    ] {
        let mut records = vec![message("user", "keep this")];
        records.extend(copies);
        for preview in [false, true] {
            let mut rolled_back = records.clone();
            rolled_back.push(event("thread_rolled_back", json!({"num_turns":1})));
            let snapshot = read_records_with_preview(rolled_back, preview).unwrap();
            assert_eq!(visible_text(&snapshot), ["keep this"]);
            let snapshot = read_records_with_preview(records.clone(), preview).unwrap();
            assert_eq!(visible_text(&snapshot), ["keep this", "[audio omitted]"]);
            let serialized = serde_json::to_string(&snapshot).unwrap();
            assert!(!serialized.contains("local.wav") && !serialized.contains("SYNTHETIC"));
        }
    }
}

#[test]
fn file_and_inline_media_have_matching_placeholders_across_formats() {
    let model = model_request(&json!([
        {"type":"input_text","text":" "},
        {"type":"input_image","file_id":"private-image"},
        {"type":"input_audio","file_id":"private-sound"},
        {"type":"input_audio","audio_url":"private-inline-sound"}
    ]));
    let legacy = event(
        "user_message",
        json!({
            "message":" ", "file_ids":["private-image"], "audio":["private-inline-sound"],
            "audio_references":[{"file_id":"private-sound"}], "audio_order":["file","inline"]
        }),
    );
    let display = completed(&json!({"type":"UserMessage","id":"media","content":[
        {"type":"image","file_id":"private-image"}, {"type":"audio","file_id":"private-sound"},
        {"type":"audio","audio_url":"private-inline-sound"}
    ]}));
    let mut reversed = legacy.clone();
    reversed["payload"]["audio_order"] = json!(["inline", "file"]);
    assert_eq!(
        visible_text(&read_records(vec![model.clone(), reversed]).unwrap()),
        ["[1 image omitted] [audio omitted]"; 2]
    );
    let snapshot = read_records(vec![model, legacy, display, message("user", "</audio>")]).unwrap();
    assert_eq!(
        visible_text(&snapshot),
        ["[1 image omitted] [audio omitted]", "</audio>"]
    );
    assert!(
        !serde_json::to_string(&snapshot)
            .unwrap()
            .contains("private-")
    );
}

#[test]
fn multipart_model_text_matches_concatenated_display_text_across_media() {
    let model = model_request(&json!([
        {"type":"input_text","text":"inspect "},
        {"type":"input_text","text":"<image>"}, {"type":"input_image","image_url":"private-image"}, {"type":"input_text","text":"</image>"},
        {"type":"input_text","text":"<audio>"}, {"type":"input_audio","audio_url":"private-audio"}, {"type":"input_text","text":"</audio>"},
        {"type":"input_text","text":"both"}
    ]));
    let plain_model = model_request(&json!([
        {"type":"input_text","text":"inspect "}, {"type":"input_text","text":"both"}
    ]));
    let legacy = event("user_message", json!({"message":"inspect both"}));
    let display = completed(&json!({"type":"UserMessage","id":"request","content":[
        {"type":"text","text":"inspect both"}
    ]}));
    for source in [plain_model, model] {
        for other in [&display, &legacy] {
            for (first, second) in [(&source, other), (other, &source)] {
                let mut records = vec![message("user", "keep this"), first.clone(), second.clone()];
                let snapshot = read_records(records.clone()).unwrap();
                assert_eq!(visible_text(&snapshot), ["keep this", "inspect both"]);
                records.push(event("thread_rolled_back", json!({"num_turns":1})));
                assert_eq!(visible_text(&read_records(records).unwrap()), ["keep this"]);
            }
        }
    }
}

#[test]
fn failed_attachment_keeps_the_display_request_once_in_either_order() {
    for (kind, path, diagnostic, prompt, visible) in [
        (
            "local_audio",
            "sample.wav",
            "Codex could not read the local audio at `sample.wav`: Permission denied (os error 13)",
            "identify the sound",
            "identify the sound",
        ),
        (
            "local_image",
            "sample`: frame.png",
            "Codex could not read the local image at `sample`: frame.png`: No such file or directory (os error 2)",
            "",
            "[1 image omitted]",
        ),
        (
            "local_image",
            "sample.webp",
            "Image located at `sample.webp` is invalid: decoding failed",
            "read the label",
            "read the label",
        ),
        (
            "local_audio",
            "sample.xyz",
            "Codex cannot attach audio at `sample.xyz`: unsupported audio format; use wav, mp3, m4a, webm, or ogg.",
            "",
            "[audio omitted]",
        ),
        (
            "local_image",
            "sample.svg",
            "Codex cannot attach image at `sample.svg`: unsupported image `image/svg+xml`.",
            "inspect",
            "inspect",
        ),
    ] {
        let model = model_request(&json!([
            {"type":"input_text","text":prompt}, {"type":"input_text","text":diagnostic}
        ]));
        let display = completed(&json!({"type":"UserMessage", "id":"attachment", "content":[
            {"type":"text","text":prompt}, {"type":kind,"path":path}
        ]}));
        let field = if kind == "local_image" {
            "local_images"
        } else {
            "local_audio"
        };
        let legacy = event("user_message", json!({"message":prompt, field:[path]}));
        for display_copy in [display, legacy] {
            for (first, second) in [(&display_copy, &model), (&model, &display_copy)] {
                let mut records = vec![message("user", "keep"), first.clone(), second.clone()];
                let snapshot = read_records(records.clone()).unwrap();
                assert_eq!(visible_text(&snapshot), ["keep", visible]);
                assert!(!serde_json::to_string(&snapshot).unwrap().contains(path));
                records.push(event("thread_rolled_back", json!({"num_turns":1})));
                assert_eq!(visible_text(&read_records(records).unwrap()), ["keep"]);
            }
        }
    }
}

#[test]
fn attachment_diagnostic_needs_matching_display_text_kind_and_path() {
    let diagnostic =
        "Codex could not read the local image at `sample.png`: Permission denied (os error 13)";
    let model = model_request(&json!([
        {"type":"input_text","text":"inspect"}, {"type":"input_text","text":diagnostic}
    ]));
    let model_text = format!("inspect\n{diagnostic}");
    for content in [
        json!([{"type":"text","text":"inspect"}]),
        json!([{"type":"text","text":"inspect"}, {"type":"local_audio","path":"sample.png"}]),
        json!([{"type":"text","text":"inspect"}, {"type":"local_image","path":"other.png"}]),
        json!([{"type":"text","text":"other"}, {"type":"local_image","path":"sample.png"}]),
    ] {
        let display_text = content[0]["text"].as_str().unwrap();
        let display = completed(&json!({"type":"UserMessage","id":"other","content":content}));
        for (first, second, expected) in [
            (&model, &display, [model_text.as_str(), display_text]),
            (&display, &model, [display_text, model_text.as_str()]),
        ] {
            assert_eq!(
                visible_text(&read_records(vec![first.clone(), second.clone()]).unwrap()),
                expected
            );
        }
    }
}

#[test]
fn failed_attachment_matches_beside_a_successful_local_attachment() {
    let prompt = "compare both";
    let model = model_request(&json!([
        {"type":"input_text","text":"compare "},
        {"type":"input_text","text":"Codex could not read the local audio at `missing.wav`: Permission denied"},
        {"type":"input_text","text":"both"},
        {"type":"input_text","text":"<image name=[Image #1] path=\"other.png\">"},
        {"type":"input_image","image_url":"private-image"},
        {"type":"input_text","text":"</image>"}
    ]));
    let display = event(
        "user_message",
        json!({
            "message":prompt, "local_images":["other.png"], "local_audio":["missing.wav"]
        }),
    );
    let snapshot = read_records(vec![model, display]).unwrap();
    assert_eq!(visible_text(&snapshot), [prompt]);
}

#[test]
fn a_message_only_checkpoint_restores_one_request_for_a_legacy_upload() {
    let caption = "inspect the upload";
    let model = model_request(&json!([
        {"type":"input_text","text":caption},
        {"type":"input_text","text":"<image name=[Image #1] path=\"local-only.png\">"},
        {"type":"input_image","file_id":"upload-id"},
        {"type":"input_text","text":"</image>"}
    ]));
    let shown = event(
        "user_message",
        json!({"message":caption,"file_ids":["upload-id"]}),
    );
    let summary = json!({"type":"compacted","payload":{"message":"A source summary."}});
    for preview in [false, true] {
        let source =
            read_records_with_preview(vec![model.clone(), shown.clone(), summary.clone()], preview)
                .unwrap();
        let active = omnis_core::import_conversation(&source);
        let users: Vec<_> = active
            .messages
            .iter()
            .filter(|row| row.role == omnis_core::HandoffRole::User)
            .map(|row| row.text.as_str())
            .collect();
        assert_eq!(users, [caption]);
        let published = serde_json::to_string(&source).unwrap();
        assert!(!published.contains("upload-id") && !published.contains("local-only.png"));
    }
}

#[test]
fn a_rolled_back_migrated_attachment_keeps_only_the_native_display() {
    let turn = "01948226-0000-7001-8000-000000000001";
    let id = "msg_input_01948226-0000-7003-8000-000000000003";
    let prompt = "inspect the image";
    let image = json!({"type":"input_image","file_id":"private-image"});
    let local = |path: &str| json!({"type":"local_image","path":path});
    let file = |id: &str| json!({"type":"image","file_id":id});
    for (wrapped, shown_parts, expected) in [
        (true, vec![local("private.png")], Some("withdrawn")),
        (true, vec![local("different.png")], None),
        (false, vec![file("private-image")], Some("withdrawn")),
        (false, vec![file("different-file")], None),
        (false, vec![local("private.png")], Some("unverified")),
        (false, vec![file("other-file"), file("private-image")], None),
    ] {
        let mut parts = vec![json!({"type":"input_text","text":prompt})];
        if wrapped {
            parts.push(
                json!({"type":"input_text","text":"<image name=[Image #1] path=\"private.png\">"}),
            );
        }
        parts.push(image.clone());
        if wrapped {
            parts.push(json!({"type":"input_text","text":"</image>"}));
        }
        if shown_parts.len() == 2 {
            parts.push(json!({"type":"input_image","file_id":"other-file"}));
        }
        let mut model = model_request(&json!(parts));
        model["payload"]["id"] = id.into();
        model["payload"]["internal_chat_message_metadata_passthrough"] = json!({"turn_id":turn});
        let mut content = vec![json!({"type":"text","text":prompt})];
        content.extend(shown_parts);
        let mut shown = completed(&json!({"type":"UserMessage","id":"item-1","content":content}));
        shown["payload"]["turn_id"] = "rollout-2".into();
        let records = vec![
            model,
            event("task_started", json!({"turn_id":"rollout-2"})),
            shown,
            event("thread_rolled_back", json!({"num_turns":1})),
        ];
        for preview in [false, true] {
            let snapshot =
                read_with_history_mode(records.clone(), preview, Some("paginated")).unwrap();
            assert_eq!(visible_text(&snapshot), [prompt]);
            let user = snapshot
                .events
                .iter()
                .find(|e| e.kind == EventKind::MessageUser)
                .unwrap();
            assert_eq!(
                user.payload["omnisession_codex_rollback"].as_str(),
                expected
            );
            assert_eq!(
                omnis_core::import_conversation(&snapshot)
                    .messages
                    .is_empty(),
                expected.is_some()
            );
            let encoded = serde_json::to_string(&snapshot).unwrap();
            assert!(!encoded.contains("private-image") && !encoded.contains("private.png"));
        }
    }
}

#[test]
fn a_checkpoint_must_retain_the_same_attachment_under_its_original_or_migrated_id() {
    let turn = "01948226-0000-7001-8000-000000000001";
    let mut raw = model_request(&json!([
        {"type":"input_text","text":"read the image"},
        {"type":"input_text","text":"<image name=[Image #1] path=\"private.png\">"},
        {"type":"input_image","image_url":"private-original"},
        {"type":"input_text","text":"</image>"}
    ]));
    raw["payload"]["id"] = "msg_input_01948226-0000-7003-8000-000000000003".into();
    raw["payload"]["internal_chat_message_metadata_passthrough"] = json!({"turn_id":turn});
    let mut shown = completed(&json!({"type":"UserMessage","id":"item-1","content":[
        {"type":"text","text":"read the image"}, {"type":"local_image","path":"private.png"}
    ]}));
    shown["payload"]["turn_id"] = turn.into();
    for (display_id, value, active) in [
        ("item-1", Some("private-original"), true),
        ("item-1", Some("private-revised"), false),
        (raw["payload"]["id"].as_str().unwrap(), None, false),
    ] {
        shown["payload"]["item"]["id"] = display_id.into();
        let mut retained = raw["payload"].clone();
        if let Some(value) = value {
            retained["content"][2]["image_url"] = value.into();
        } else {
            retained["content"].as_array_mut().unwrap().truncate(1);
        }
        let checkpoint = json!({"type":"compacted","payload":{
            "message":"", "window_number":1, "replacement_history":[retained]
        }});
        for preview in [false, true] {
            let snapshot = read_with_history_mode(
                vec![raw.clone(), shown.clone(), checkpoint.clone()],
                preview,
                Some("paginated"),
            )
            .unwrap();
            assert_eq!(super::published_text(&snapshot), ["read the image"]);
            assert_eq!(
                omnis_core::import_conversation(&snapshot).messages.len(),
                usize::from(active)
            );
        }
    }
}

fn display_with_selection(display: &Value, selection: Value, keep_media: bool) -> Value {
    let mut item = display.clone();
    let parts = item["payload"]["item"]["content"].as_array_mut().unwrap();
    if !keep_media {
        parts.pop();
    }
    parts.push(selection);
    item
}

const CAPTION: &str = "read the chart";
const IMAGE_ID: &str = "private-image";

fn check_compacted_caption(source: &[Value], shown: &Value, expected: &str, active: bool) {
    for selected in [false, true] {
        let mut rows = source.to_vec();
        rows.push(shown.clone());
        if selected {
            let replacement: Vec<_> = source.iter().map(|row| &row["payload"]).collect();
            rows.push(json!({"type":"compacted","payload":{"message":"","window_number":1,"replacement_history":replacement}}));
        }
        rows.push(json!({"type":"compacted","payload":{"message":"A source summary."}}));
        for preview in [false, true] {
            let snapshot =
                read_with_history_mode(rows.clone(), preview, Some("paginated")).unwrap();
            let imported = omnis_core::import_conversation(&snapshot);
            assert_eq!(imported.messages.iter().any(|m| m.text == expected), active);
            let serialized = serde_json::to_string(&snapshot).unwrap();
            assert!(!serialized.contains(IMAGE_ID) && !serialized.contains("private-selector"));
            if expected.is_empty() {
                assert!(published_text(&snapshot).is_empty());
                assert!(imported.truncated);
            } else if expected == CAPTION {
                assert!(published_text(&snapshot).contains(&CAPTION));
                let mut recovery = Vec::new();
                omnis_core::write_recovery_handoff(&snapshot, &mut recovery).unwrap();
                let recovery = String::from_utf8(recovery).unwrap();
                assert!(recovery.contains(CAPTION));
                assert!(!recovery.contains(IMAGE_ID) && !recovery.contains("private-selector"));
            }
        }
    }
}

#[test]
fn message_only_compaction_keeps_the_verified_caption_without_its_image() {
    let turn = "01948226-0000-7001-8000-000000000001";
    let id = "msg_input_01948226-0000-7003-8000-000000000003";
    let model = |text: &str, image: &str| {
        let mut row = model_request(&json!([
            {"type":"input_text","text":text}, {"type":"input_image","file_id":image}
        ]));
        row["payload"]["id"] = id.into();
        row["payload"]["internal_chat_message_metadata_passthrough"] = json!({"turn_id":turn});
        row
    };
    let display = |text: &str, image: &str| {
        let mut row = completed(&json!({"type":"UserMessage","id":"item-1","content":[
            {"type":"text","text":text}, {"type":"image","file_id":image}
        ]}));
        row["payload"]["turn_id"] = turn.into();
        row
    };
    let caption = CAPTION;
    let image = IMAGE_ID;
    let original = model(caption, image);
    let published = display(caption, image);
    let selected = |part, media| display_with_selection(&published, part, media);
    let skill = json!({"type":"skill", "name":"fixture", "path":"private-selector/SKILL.md"});
    let mention = json!({"type":"mention", "name":"fixture", "path":"app://private-selector"});
    let mut plain = original.clone();
    plain["payload"]["content"].as_array_mut().unwrap().pop();
    let mut failed = original.clone();
    failed["payload"]["content"][1] = json!({"type":"input_text","text":"Codex could not read the local image at `private-image`: Permission denied"});
    let mut unavailable = published.clone();
    unavailable["payload"]["item"]["content"][1] = json!({"type":"local_image","path":image});
    let oversized = "x".repeat(80_001);
    for (source, shown, expected, active) in [
        (vec![original.clone()], published.clone(), caption, true),
        (
            vec![original.clone()],
            selected(skill.clone(), true),
            caption,
            true,
        ),
        (
            vec![original.clone()],
            selected(mention.clone(), true),
            caption,
            true,
        ),
        (vec![plain], selected(skill, false), caption, true),
        (
            vec![original.clone()],
            selected(json!({"type":"mention","name":"fixture"}), true),
            "",
            false,
        ),
        (
            vec![original.clone()],
            selected(
                json!({"type":"unknown","name":"fixture","path":"unknown"}),
                true,
            ),
            "",
            false,
        ),
        (
            vec![original.clone()],
            display("new caption", image),
            "new caption",
            false,
        ),
        (
            vec![original.clone()],
            display(caption, "other image"),
            caption,
            false,
        ),
        (vec![failed], unavailable, caption, false),
        (
            vec![model("", image)],
            display("", image),
            "[1 image omitted]",
            false,
        ),
        (
            vec![original.clone(), original.clone()],
            published,
            caption,
            false,
        ),
        (
            vec![model(&oversized, image)],
            display(&oversized, image),
            oversized.as_str(),
            false,
        ),
    ] {
        check_compacted_caption(&source, &shown, expected, active);
    }
}

#[test]
fn a_framed_image_caption_survives_a_damaged_revision_and_a_later_summary() {
    let turn = "image-turn";
    let mut model = model_request(&json!([
        {"type":"input_text","text":CAPTION},
        {"type":"input_text","text":"<image name=[Image #1] path=\"unpublished.png\">"},
        {"type":"input_image","file_id":IMAGE_ID},
        {"type":"input_text","text":"</image>"}
    ]));
    model["payload"]["id"] = "request".into();
    model["payload"]["internal_chat_message_metadata_passthrough"] = json!({"turn_id":turn});
    let published = turn_item(
        turn,
        &json!({"type":"UserMessage","id":"request","content":[
            {"type":"text","text":CAPTION}, {"type":"image","file_id":IMAGE_ID}
        ]}),
    );
    let broken = turn_item(
        turn,
        &json!({"type":"UserMessage","id":"request","content":[
            {"type":"text","text":"damaged edit"}, {"type":"image"}
        ]}),
    );
    let checkpoint = json!({"type":"compacted","payload":{
        "message":"","window_number":1,"replacement_history":[model["payload"]]
    }});
    let mut rows = vec![model, published, checkpoint, broken];
    for final_summary in [false, true] {
        if final_summary {
            rows.push(json!({"type":"compacted","payload":{"message":"A source summary."}}));
        }
        for preview in [false, true] {
            let source = read_with_history_mode(rows.clone(), preview, Some("paginated")).unwrap();
            assert_eq!(published_text(&source), [CAPTION]);
            let active = omnis_core::import_conversation(&source);
            assert_eq!(
                active.messages.first().map(|m| m.text.as_str()),
                Some(CAPTION)
            );
            assert!(active.truncated);
            let mut recovery = Vec::new();
            omnis_core::write_recovery_handoff(&source, &mut recovery).unwrap();
            let recovered = String::from_utf8(recovery).unwrap();
            assert!(recovered.contains(CAPTION));
            for private in [IMAGE_ID, "unpublished.png", "damaged edit"] {
                assert!(!recovered.contains(private));
            }
        }
    }
}

#[test]
fn empty_user_revisions_clear_only_the_previous_request_with_the_same_id() {
    for contents in [
        json!([]),
        json!([{"type":"text","text":""}]),
        json!([{"type":"mention","name":"choice","path":"app://test"}]),
        json!([{"type":"skill","name":"choice","path":"private-skill"}]),
    ] {
        let cleared = turn_item(
            "two",
            &json!({"type":"UserMessage","id":"edited","content":contents}),
        );
        let rows = vec![
            page_user("one", "kept", "other request"),
            page_user("two", "edited", "obsolete request"),
            cleared,
            turn_item(
                "three",
                &json!({"type":"UserMessage","id":"fresh","content":[]}),
            ),
        ];
        let source = read_with_history_mode(rows, false, Some("paginated")).unwrap();
        assert_eq!(visible_text(&source), ["other request", ""]);
        let conversation = omnis_core::import_conversation(&source);
        let active: Vec<_> = conversation
            .messages
            .iter()
            .map(|item| item.text.as_str())
            .collect();
        assert_eq!(active, ["other request"]);
        assert!(!conversation.truncated);
        let trajectory = omnis_core::import_trajectory(&source);
        assert_eq!(trajectory.items.len(), 1);
        assert_eq!(trajectory.items[0].text, "other request");
    }
}

#[test]
fn a_malformed_completed_user_message_is_omitted_as_a_whole() {
    let turn = "source-turn";
    let mut valid = turn_item(
        turn,
        &json!({"type":"UserMessage","id":"request","client_id":null,"content":[
            {"type":"text","text":"previously "}, ["text","published "],
            ["text","request",[
                {"byte_range":{"start":1,"end":3,"future":true},"placeholder":null}
            ]],
            {"type":"image","file_id":"private-image","detail":"low"},{"type":"audio","audio_url":"private-audio"},
            {"type":"local_image","path":"private-path","detail":null},{"type":"local_audio","path":"private-path"},
            {"type":"skill","name":"private-skill","path":"private-path"},{"type":"mention","name":"private-name","path":"private-path"}
        ]}),
    );
    valid["payload"]["thread_id"] = json!("00000000-0000-4000-8000-000000000001");
    valid["payload"]["started_at_ms"] = Value::Null;
    valid["payload"]["future"] = json!({"private-test":true});
    let broken = |id: &str, text: Value| {
        turn_item(
            turn,
            &json!({"type":"UserMessage", "id":id, "content":[
                {"type":"image", "file_id":"private-image"},
                {"type":"text", "text":"unpublished caption"}, text
            ]}),
        )
    };
    let mut rows = vec![valid, broken("new", json!({"type":"text","text":7}))];
    for part in [
        json!({"type":"text"}),
        json!(["text", "unpublished caption", [], true]),
        json!({"type":"image"}),
        json!({"type":"audio","file_id":42}),
        json!({"type":"image","file_id":"private-image","detail":"future"}),
        json!({"type":"local_image","detail":"high"}),
        json!({"type":"local_audio","path":false}),
        json!({"type":"mention","name":"private-name"}),
        json!({"type":"skill","path":"private-path"}),
        json!({"type":"text","text":"unpublished caption","text_elements":null}),
        json!({"type":"text","text":"unpublished caption","text_elements":[{"placeholder":"private-span"}]}),
        json!({"type":"text","text":"unpublished caption","text_elements":[{"byte_range":{"start":0,"end":-1}}]}),
        json!({"type":"text","text":"unpublished caption","text_elements":[{"byte_range":{"start":0,"end":1},"placeholder":false}]}),
    ] {
        rows.push(broken("request", part));
    }
    for (key, value) in [
        ("client_id", json!(22)),
        ("id", Value::Null),
        ("content", json!({})),
    ] {
        let mut row = page_user(turn, "request", "unpublished caption");
        row["payload"]["item"][key] = value;
        rows.push(row);
    }
    for (key, value) in [
        ("thread_id", json!("unknown")),
        ("turn_id", Value::Null),
        ("started_at_ms", json!(0.5)),
        ("completed_at_ms", Value::Null),
    ] {
        let mut row = page_user(turn, "request", "unpublished caption");
        row["payload"][key] = value;
        rows.push(row);
    }
    for preview in [false, true] {
        let source = read_with_history_mode(rows.clone(), preview, Some("paginated")).unwrap();
        assert_eq!(visible_text(&source), ["previously published request"]);
        let omissions: Vec<_> = source
            .events
            .iter()
            .filter(|e| omnis_core::reports_omitted_events(e))
            .collect();
        assert_eq!(omissions.len(), 1);
        assert_eq!(omissions[0].payload["omitted_events"], rows.len() - 1);
        let imported = omnis_core::import_conversation(&source);
        assert_eq!(imported.messages.len(), 1);
        assert_eq!(imported.messages[0].text, "previously published request");
        let mut recovery = Vec::new();
        omnis_core::write_recovery_handoff(&source, &mut recovery).unwrap();
        for output in [
            serde_json::to_string(&source).unwrap(),
            String::from_utf8(recovery).unwrap(),
        ] {
            assert!(!output.contains("unpublished caption") && !output.contains("private-"));
        }
    }
}
