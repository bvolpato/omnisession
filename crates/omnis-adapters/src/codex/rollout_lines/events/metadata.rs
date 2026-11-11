use serde::Deserialize;
use serde_json::Value;

use super::{AbortReason, CollaborationMode, messages::MediaKind};
use crate::codex::{checkpoint_wire::ImageDetail, wire_enums};

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Metadata {
    #[serde(alias = "turn_started")]
    TaskStarted {
        #[serde(rename = "collaboration_mode_kind")]
        _mode: CollaborationMode,
    },
    TurnAborted {
        #[serde(rename = "reason")]
        _reason: AbortReason,
    },
    UserMessage {
        #[serde(rename = "image_order")]
        _images: Option<Vec<MediaKind>>,
        #[serde(rename = "audio_order")]
        _audio: Option<Vec<MediaKind>>,
        #[serde(rename = "image_details")]
        _details: Option<Vec<Option<ImageDetail>>>,
        #[serde(rename = "file_id_details")]
        _file_details: Option<Vec<Option<ImageDetail>>>,
        #[serde(rename = "local_image_details")]
        _local: Option<Vec<Option<ImageDetail>>>,
    },
}

const MEDIA: [&str; 6] = [
    "type",
    "image_order",
    "audio_order",
    "image_details",
    "file_id_details",
    "local_image_details",
];

pub(super) fn prepare(payload: &mut Value) -> bool {
    if payload["type"] == "user_message" {
        if !MEDIA[1..].iter().any(|key| {
            payload[*key]
                .as_array()
                .is_some_and(|v| v.iter().any(Value::is_object))
        }) {
            return true;
        }
        if !wire_enums::accepts::<Metadata, _>(payload, MEDIA) {
            return false;
        }
        for key in &MEDIA[1..] {
            if let Some(values) = payload.get_mut(*key).and_then(Value::as_array_mut) {
                values.iter_mut().for_each(wire_enums::name_unit);
            }
        }
    } else {
        let key = if payload["type"] == "turn_aborted" {
            "reason"
        } else {
            "collaboration_mode_kind"
        };
        if payload[key].is_object() {
            if !wire_enums::accepts::<Metadata, _>(payload, ["type", key]) {
                return false;
            }
            wire_enums::name_unit(&mut payload[key]);
        }
    }
    true
}
