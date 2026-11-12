use std::collections::HashSet;

use chrono::{DateTime, Utc};
use omnis_ir::{EventKind, ReplayPolicy};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::super::{EventBuilder, MAX_CANONICAL_EVENTS, paginated_rollback::json_digest};

#[derive(Deserialize, Serialize)]
struct Item {
    id: String,
    realtime_session_id: String,
    #[serde(flatten)]
    content: Content,
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Content {
    RealtimeSessionStarted,
    TranscriptSegment {
        role: Role,
        text: String,
    },
    BemItemPromoted {
        turn_id: String,
        item_id: String,
        presentation: Presentation,
    },
    RealtimeSessionClosed {
        outcome: Outcome,
    },
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum Role {
    User,
    Assistant,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum Outcome {
    Ended,
    Failed,
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Presentation {
    WholeItem,
    InlineMarkdown,
    InlineVisualization { index: u32 },
}

pub(super) fn prepare(payload: &mut Value) -> bool {
    let Ok(item) = serde_json::from_value::<Item>(std::mem::take(payload)) else {
        return false;
    };
    *payload = serde_json::to_value(item).expect("realtime item has JSON fields");
    true
}

#[derive(Default)]
pub(super) struct Timeline {
    ids: HashSet<[u8; 32]>,
    pub(super) start: u64,
}

impl Timeline {
    pub(super) fn project(
        &mut self,
        record: &Value,
        ordinal: u64,
        builder: &mut EventBuilder,
    ) -> bool {
        if ordinal < self.start {
            return true;
        }
        let Some(timestamp) = record["timestamp"]
            .as_str()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        else {
            return false;
        };
        let item = &record["payload"];
        let key = json_digest(&item["id"]);
        if self.ids.contains(&key) {
            return true;
        }
        if self.ids.len() >= MAX_CANONICAL_EVENTS {
            return false;
        }
        self.ids.insert(key);
        if item["type"] == "transcript_segment" {
            let kind = if item["role"] == "user" {
                EventKind::MessageUser
            } else {
                EventKind::MessageAssistant
            };
            builder.push(kind,json!({"id":item["id"],"realtime_session_id":item["realtime_session_id"],"text":item["text"]}),
                Some(timestamp.with_timezone(&Utc)),ReplayPolicy::Contextual, Some(super::TRANSCRIPT_SEGMENT.into()),None);
        }
        true
    }
}
