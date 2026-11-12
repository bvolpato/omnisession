use serde::Deserialize;
use serde_json::Value;

use super::checkpoint_wire::{array, audio, image_detail, optional, optional_phase};
use super::completed_item::{assistant, text_element};

type Field = (&'static str, fn(&Value) -> bool);

fn fields(payload: &Value, schema: &[Field]) -> bool {
    schema
        .iter()
        .all(|(key, valid)| payload.get(*key).map_or(*key != "message", valid))
}

pub(super) fn prepare(payload: &mut Value, user: bool) -> bool {
    let (min, schema) = if user {
        (11, USER_FIELDS)
    } else {
        (2, AGENT_FIELDS)
    };
    let Some(parts) = payload.as_array_mut() else {
        return true;
    };
    if !(min..=schema.len() + 1).contains(&parts.len()) {
        return false;
    }
    let names = std::iter::once("type").chain(schema.iter().map(|(name, _)| *name));
    *payload = Value::Object(
        names
            .map(str::to_owned)
            .zip(std::mem::take(parts))
            .collect(),
    );
    true
}

pub(super) fn agent(payload: &Value) -> bool {
    fields(payload, AGENT_FIELDS)
}

const AGENT_FIELDS: &[Field] = &[
    ("message", Value::is_string),
    ("phase", optional_phase),
    ("memory_citation", |v| optional(v, assistant::citation)),
    ("delivery", |v| optional(v, assistant::is_async)),
    ("questions", |v| {
        optional(v, |v| array(v, assistant::question))
    }),
];

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum MediaKind {
    Inline,
    File,
}

fn strings(v: &Value) -> bool {
    array(v, Value::is_string)
}
fn details(v: &Value) -> bool {
    array(v, image_detail)
}
fn order(v: &Value) -> bool {
    array(v, |kind| MediaKind::deserialize(kind).is_ok())
}

pub(super) fn user(payload: &Value) -> bool {
    fields(payload, USER_FIELDS)
}

const USER_FIELDS: &[Field] = &[
    ("client_id", |v| optional(v, Value::is_string)),
    ("message", Value::is_string),
    ("images", |v| optional(v, strings)),
    ("image_details", details),
    ("file_ids", |v| optional(v, strings)),
    ("file_id_details", details),
    ("image_order", order),
    ("local_images", strings),
    ("local_image_details", details),
    ("audio", |v| optional(v, strings)),
    ("audio_references", |v| optional(v, |v| array(v, audio))),
    ("audio_order", order),
    ("local_audio", strings),
    ("text_elements", |v| array(v, text_element)),
];
