use omnis_core::{NativeTrajectoryItem, import_trajectory, native_trajectory_items};
use serde_json::{Value, json};

use super::{event, page_user, read_with_history_mode, turn_item, visible_text};

const TURN: &str = "turn";

fn model(id: &str, role: &str, text: &str) -> Value {
    let kind = if role == "assistant" {
        "output_text"
    } else {
        "input_text"
    };
    json!({"type":"response_item","payload":{"type":"message","role":role,"id":id,
        "internal_chat_message_metadata_passthrough":{"turn_id":TURN},"content":[{"type":kind,"text":text}]}})
}

fn request(order: u8, generated: bool, text: &str) -> (Value, Value) {
    let id = if generated {
        format!("msg_input_01948226-0000-7000-8000-{order:012}")
    } else {
        format!("request-{order}")
    };
    let mut raw = model(&id, "user", text);
    let displayed_id = if generated {
        raw["metadata"] = json!({"user_input_order":order});
        format!("item-{order}")
    } else {
        id
    };
    (raw, page_user(TURN, &displayed_id, text))
}

fn reply(id: &str, text: &str) -> Value {
    turn_item(
        TURN,
        &json!({"type":"AgentMessage","id":id,"content":[{"type":"Text","text":text}]}),
    )
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Suffix {
    Delayed,
    Repeated,
    Private,
    DisplayOnly,
    ModelAfter,
    Tool,
    FreshBefore,
    FreshAfter,
    DisplayOnlyFresh,
    Generated,
    AmbiguousGenerated,
}

use Suffix::{
    AmbiguousGenerated, Delayed, DisplayOnly, DisplayOnlyFresh, FreshAfter, FreshBefore, Generated,
    ModelAfter, Private, Repeated, Tool,
};

#[test]
fn a_revised_reply_and_its_request_use_their_verified_model_order() {
    for case in [
        Delayed,
        Repeated,
        Private,
        DisplayOnly,
        ModelAfter,
        Tool,
        FreshBefore,
        FreshAfter,
        DisplayOnlyFresh,
        Generated,
        AmbiguousGenerated,
    ] {
        let generated = matches!(case, Generated | AmbiguousGenerated);
        let (first, first_display) = request(1, generated, "first request");
        let mut rows = vec![
            event("task_started", json!({"turn_id":TURN})),
            first.clone(),
            first_display,
            model("reply", "assistant", "original reply"),
            reply("reply", "original reply"),
            json!({"type":"compacted","payload":{"window_number":1,"message":"","replacement_history":[first["payload"]]}}),
        ];
        let second = if case == Repeated {
            "first request"
        } else {
            "second request"
        };
        let (raw, display) = request(2, generated, second);
        if case == Private {
            rows.push(model("private", "user", "private direction"));
        } else if !matches!(case, DisplayOnly | ModelAfter) {
            rows.push(raw.clone());
        }
        if case == Tool {
            rows.extend([
                json!({"type":"response_item","payload":{"type":"function_call","call_id":"fixture","name":"inspect_fixture","arguments":"{}"}}),
                json!({"type":"response_item","payload":{"type":"function_call_output","call_id":"fixture","output":"result"}}),
            ]);
        }
        if case == FreshBefore {
            rows.push(model("fresh", "assistant", "independent answer"));
        }
        rows.push(model("reply", "assistant", "revised reply"));
        if case == FreshAfter {
            rows.push(model("fresh", "assistant", "independent answer"));
        }
        if case == ModelAfter {
            rows.push(raw);
        } else if case == AmbiguousGenerated {
            rows.push(request(3, true, second).0);
        }
        let mut public = vec!["first request", "revised reply"];
        if case != Private {
            rows.push(display);
            public.push(second);
        }
        if matches!(case, FreshBefore | FreshAfter | DisplayOnlyFresh) {
            rows.push(reply("fresh", "independent answer"));
            public.push("independent answer");
        }
        rows.push(reply("reply", "revised reply"));
        check_order(case, &rows, &public, &expected_order(case, second));
    }
}

fn expected_order(case: Suffix, second: &str) -> Vec<&str> {
    match case {
        Delayed | Repeated | Generated => vec!["first request", second, "revised reply"],
        Tool => vec![
            "first request",
            second,
            "hist_codex_inspect_fixture",
            "revised reply",
        ],
        Private => vec!["first request", "revised reply"],
        FreshBefore => vec![
            "first request",
            second,
            "independent answer",
            "revised reply",
        ],
        FreshAfter | DisplayOnlyFresh => vec![
            "first request",
            second,
            "revised reply",
            "independent answer",
        ],
        DisplayOnly | ModelAfter | AmbiguousGenerated => {
            vec!["first request", "revised reply", second]
        }
    }
}

fn check_order(case: impl std::fmt::Debug, rows: &[Value], public: &[&str], expected: &[&str]) {
    for preview in [false, true] {
        let snapshot = read_with_history_mode(rows.to_vec(), preview, Some("paginated")).unwrap();
        assert_eq!(
            visible_text(&snapshot),
            public,
            "{case:?}, preview={preview}"
        );
        let active = native_trajectory_items(&import_trajectory(&snapshot));
        let ordered: Vec<_> = active
            .iter()
            .map(|item| match item {
                NativeTrajectoryItem::Message { text, .. } => text.as_str(),
                NativeTrajectoryItem::Tool { name, .. } => name.as_str(),
            })
            .collect();
        assert_eq!(ordered, expected, "{case:?}, preview={preview}");
    }
}

#[test]
fn an_image_placeholder_uses_the_position_of_its_confirmed_model_request() {
    let file = "private-model-image";
    let marker = "[1 image omitted]";
    let mut raw = model("photo", "user", "");
    raw["payload"]["content"] = json!([{"type":"input_image", "file_id":file}]);
    let mut malformed = raw.clone();
    malformed["payload"]["content"]
        .as_array_mut()
        .unwrap()
        .push(json!({"type":"input_text", "text":7}));
    for (copies, display, before_reply) in [
        (vec![raw.clone()], Some(file), true),
        (vec![raw.clone()], Some("different-image"), false),
        (vec![raw.clone(), raw.clone()], Some(file), false),
        (Vec::new(), Some(file), false),
        (vec![malformed], Some(file), false),
        (vec![raw], None, false),
    ] {
        let mut rows = vec![
            event("task_started", json!({"turn_id":TURN})),
            model("reply", "assistant", "previous reply"),
            reply("reply", "previous reply"),
            json!({"type":"compacted","payload":{"message":"","window_number":1,"replacement_history":[]}}),
        ];
        let count = copies.len();
        rows.extend(copies);
        rows.push(model("reply", "assistant", "revised reply"));
        let mut public = vec!["revised reply"];
        if let Some(image) = display {
            rows.push(turn_item(TURN, &json!({"type":"UserMessage","id":"photo","content":[{"type":"image","file_id":image}]})));
            public.push(marker);
        }
        rows.push(reply("reply", "revised reply"));
        let active = if before_reply {
            &[marker, "revised reply"][..]
        } else {
            &public
        };
        check_order((count, display), &rows, &public, active);
    }
}

#[test]
fn uploaded_media_keeps_the_path_and_file_reference_of_each_original_slot() {
    let path = |p| json!({"type":"local_image","path":p});
    let file = |f| json!({"type":"image","file_id":f});
    for (one, two, caption, matches) in [
        (path("alpha.png"), file("file-b"), "", true),
        (file("file-a"), path("beta.png"), "public caption", true),
        (file("file-a"), file("file-b"), "", true),
        (path("alpha.png"), path("beta.png"), "", true),
        (file("file-b"), path("alpha.png"), "", false),
        (file("file-a"), file("file-a"), "", false),
        (file("file-other"), file("file-b"), "", false),
    ] {
        let mut raw = request(2, true, caption).0;
        let content = raw["payload"]["content"].as_array_mut().unwrap();
        for (i, (p, f)) in [("alpha.png", "file-a"), ("beta.png", "file-b")]
            .into_iter()
            .enumerate()
        {
            content.extend([
                json!({"type":"input_text","text":format!("<image name=[Image #{}] path=\"{p}\">", i + 1)}),
                json!({"type":"input_image","file_id":f}),
                json!({"type":"input_text","text":"</image>"}),
            ]);
        }
        let mut other = raw.clone();
        other["payload"]["id"] = json!("unrelated upload");
        other["payload"]["content"][2]["file_id"] = json!("file-other");
        let shown = turn_item(
            TURN,
            &json!({"type":"UserMessage","id":"item-2","content":[
            {"type":"text","text":caption,"text_elements":[]}, one, two]}),
        );
        let mut rows = vec![
            event("task_started", json!({"turn_id":TURN})),
            model("reply", "assistant", "previous reply"),
            reply("reply", "previous reply"),
            json!({"type":"compacted","payload":{"message":"","window_number":1,"replacement_history":[]}}),
        ];
        let mut shown = shown;
        if one["file_id"] == "file-other" {
            raw["payload"]["id"] = json!("photo");
            shown["payload"]["item"]["id"] = json!("photo");
            rows.push(other);
        }
        rows.extend([
            raw,
            model("reply", "assistant", "revised reply"),
            shown,
            reply("reply", "revised reply"),
        ]);
        let photo = if caption.is_empty() {
            "[2 images omitted]"
        } else {
            caption
        };
        let public = ["revised reply", photo];
        let active = if matches {
            [photo, "revised reply"]
        } else {
            public
        };
        check_order((&one, &two), &rows, &public, &active);
    }
}

#[test]
fn a_numbered_checkpoint_can_keep_the_public_caption_of_a_partly_failed_upload() {
    let mut raw = model("photo", "user", "compare both");
    let parts = raw["payload"]["content"].as_array_mut().unwrap();
    parts.extend([
        json!({"type":"input_text","text":"Codex could not read the local audio at `missing.wav`: Permission denied"}),
        json!({"type":"input_text","text":"<image name=[Image #1] path=\"other.png\">"}),
        json!({"type":"input_image","file_id":"file-other"}),
        json!({"type":"input_text","text":"</image>"}),
    ]);
    for path in ["missing.wav", "different.wav"] {
        let shown = turn_item(
            TURN,
            &json!({"type":"UserMessage","id":"photo","content":[
            {"type":"text","text":"compare both","text_elements":[]},
            {"type":"local_audio","path":path},{"type":"local_image","path":"other.png"}]}),
        );
        let mut rows = vec![
            event("task_started", json!({"turn_id":TURN})),
            raw.clone(),
            shown,
            json!({"type":"compacted","payload":{"message":"","window_number":1,"replacement_history":[raw["payload"]]}}),
        ];
        for preview in [false, true] {
            let snapshot =
                read_with_history_mode(rows.clone(), preview, Some("paginated")).unwrap();
            assert_eq!(visible_text(&snapshot), ["compare both"]);
            let users: Vec<_> = native_trajectory_items(&import_trajectory(&snapshot))
                .into_iter()
                .filter_map(|item| match item {
                    NativeTrajectoryItem::Message {
                        role: omnis_core::HandoffRole::User,
                        text,
                    } => Some(text),
                    _ => None,
                })
                .collect();
            if path == "missing.wav" {
                assert_eq!(users, ["compare both"]);
            } else {
                assert!(users.is_empty());
            }
        }
        rows.push(json!({"type":"compacted","payload":{"message":"summary"}}));
        for preview in [false, true] {
            let snapshot =
                read_with_history_mode(rows.clone(), preview, Some("paginated")).unwrap();
            assert!(
                omnis_core::import_conversation(&snapshot)
                    .messages
                    .iter()
                    .all(|message| message.text != "compare both")
            );
        }
    }
}
