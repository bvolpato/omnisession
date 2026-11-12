use serde::{Deserialize, Serialize};
use serde_json::Value;

pub(super) fn project(mut record: Value) -> Value {
    normalize(&mut record);
    record
}

pub(super) fn normalize(record: &mut Value) {
    if record["type"] != "event_msg" {
        return;
    }
    let payload = &mut record["payload"];
    let positional = payload.is_array();
    let tag = if positional {
        payload.get_mut(0)
    } else {
        payload.get_mut("type")
    };
    let Some(tag) = tag else { return };
    let original = if let Some(index) = tag.as_u64() {
        let name = match index {
            14 => "thread_rolled_back",
            19 => "agent_message",
            20 => "user_message",
            _ => return,
        };
        Some(std::mem::replace(tag, name.into()))
    } else if positional && tag.is_string() {
        None
    } else {
        return;
    };
    if let Ok(message) = SavedMessage::deserialize(&*payload) {
        if positional && let Ok(named) = serde_json::to_value(message) {
            *payload = named;
        }
    } else if let Some(original) = original {
        if positional {
            payload[0] = original;
        } else {
            payload["type"] = original;
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum SavedMessage {
    AgentMessage {
        message: String,
        #[serde(default)]
        phase: Option<Phase>,
        #[serde(default)]
        memory_citation: Option<Citation>,
        #[serde(default)]
        delivery: Option<Delivery>,
        #[serde(default)]
        questions: Option<Vec<Question>>,
    },
    UserMessage(Box<UserMessage>),
    ThreadRolledBack {
        num_turns: u32,
    },
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum Phase {
    Commentary,
    PartialAnswer,
    FinalAnswer,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum Delivery {
    Async,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Citation {
    entries: Vec<CitationEntry>,
    rollout_ids: Vec<String>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct CitationEntry {
    path: String,
    line_start: u32,
    line_end: u32,
    note: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Question {
    title: String,
    options: Option<Vec<String>>,
}

#[derive(Deserialize, Serialize)]
struct UserMessage {
    #[serde(default)]
    client_id: Option<String>,
    message: String,
    images: Option<Vec<String>>,
    #[serde(default)]
    image_details: Vec<Option<ImageDetail>>,
    #[serde(default)]
    file_ids: Option<Vec<String>>,
    #[serde(default)]
    file_id_details: Vec<Option<ImageDetail>>,
    #[serde(default)]
    image_order: Vec<MediaKind>,
    #[serde(default)]
    local_images: Vec<String>,
    #[serde(default)]
    local_image_details: Vec<Option<ImageDetail>>,
    audio: Option<Vec<String>>,
    #[serde(default)]
    audio_references: Option<Vec<AudioReference>>,
    #[serde(default)]
    audio_order: Vec<MediaKind>,
    #[serde(default)]
    local_audio: Vec<String>,
    #[serde(default)]
    text_elements: Vec<TextElement>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum ImageDetail {
    Auto,
    Low,
    High,
    Original,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum MediaKind {
    Inline,
    File,
}

#[derive(Deserialize, Serialize)]
#[serde(untagged)]
enum AudioReference {
    Inline { audio_url: String },
    File { file_id: String },
}

#[derive(Deserialize, Serialize)]
struct TextElement {
    byte_range: ByteRange,
    placeholder: Option<String>,
}

#[derive(Deserialize, Serialize)]
struct ByteRange {
    start: usize,
    end: usize,
}
