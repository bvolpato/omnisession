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
            "audio_references":[{"file_id":"private-sound"}], "audio_order":["inline","file"]
        }),
    );
    let display = completed(&json!({"type":"UserMessage","id":"media","content":[
        {"type":"image","file_id":"private-image"}, {"type":"audio","file_id":"private-sound"},
        {"type":"audio","audio_url":"private-inline-sound"}
    ]}));
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
            assert_eq!(visible_text(&snapshot), ["read the image"]);
            assert_eq!(
                omnis_core::import_conversation(&snapshot).messages.len(),
                usize::from(active)
            );
        }
    }
}
