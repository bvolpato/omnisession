use serde::{Deserialize, Serialize};
use serde_json::Value;

pub(super) fn project(mut record: Value) -> Value {
    normalize(&mut record);
    record
}

pub(super) fn normalize(record: &mut Value) {
    if record["type"] == "event_msg"
        && record["payload"].is_array()
        && let Ok(message) = SavedMessage::deserialize(&record["payload"])
        && let Ok(payload) = serde_json::to_value(message)
    {
        record["payload"] = payload;
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
