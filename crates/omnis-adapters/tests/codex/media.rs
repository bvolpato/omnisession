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
            for (first, second, expected) in [
                (&source, other, "inspect \nboth"),
                (other, &source, "inspect both"),
            ] {
                let mut records = vec![message("user", "keep this"), first.clone(), second.clone()];
                let snapshot = read_records(records.clone()).unwrap();
                assert_eq!(visible_text(&snapshot), ["keep this", expected]);
                records.push(event("thread_rolled_back", json!({"num_turns":1})));
                assert_eq!(visible_text(&read_records(records).unwrap()), ["keep this"]);
            }
        }
    }
}
