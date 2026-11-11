use std::{borrow::Cow, fmt};

use serde::de::{DeserializeSeed, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum DamagedWorkspace {
    TurnOrRollback,
    PossibleSettings,
    Unclassified,
}

#[derive(Clone, Copy)]
enum Envelope {
    Turn,
    Event,
    Other,
}

#[derive(Clone, Copy, Default)]
enum EventTag {
    #[default]
    Unread,
    Documentary,
    Workspace(DamagedWorkspace),
}

#[derive(Default)]
struct Tags {
    envelope: Option<Envelope>,
    prefix: Prefix,
    event: EventTag,
}

#[derive(Clone, Copy, Default)]
enum Prefix {
    #[default]
    Object,
    Timestamp,
    EnvelopeType,
    Payload,
    PayloadType,
    WorkspaceField,
    Other,
}

/// Reads the discriminators that precede a JSON error. Only the immediate payload can name an event.
pub(super) fn damaged_workspace_record(line: &[u8]) -> Option<DamagedWorkspace> {
    classify(line, false)
}

pub(super) fn oversized_workspace_record(prefix: &[u8]) -> Option<DamagedWorkspace> {
    classify(prefix, true)
}

fn classify(line: &[u8], oversized: bool) -> Option<DamagedWorkspace> {
    let mut tags = Tags::default();
    let mut decoder = serde_json::Deserializer::from_slice(line);
    let result = ObjectTags {
        tags: &mut tags,
        payload: false,
    }
    .deserialize(&mut decoder);
    match (tags.envelope, tags.event) {
        (Some(Envelope::Turn), _) => Some(DamagedWorkspace::TurnOrRollback),
        (None | Some(Envelope::Event), EventTag::Workspace(kind)) => Some(kind),
        (Some(Envelope::Event), EventTag::Unread) => Some(DamagedWorkspace::Unclassified),
        (None, _)
            if oversized
                || result.is_err_and(|error| match tags.prefix {
                    Prefix::Object => {
                        error.is_eof() && line.trim_ascii_start().first() == Some(&b'{')
                    }
                    Prefix::Timestamp
                    | Prefix::EnvelopeType
                    | Prefix::Payload
                    | Prefix::WorkspaceField => true,
                    Prefix::PayloadType => matches!(tags.event, EventTag::Unread),
                    Prefix::Other => false,
                }) =>
        {
            Some(DamagedWorkspace::Unclassified)
        }
        _ => None,
    }
}

struct ObjectTags<'a> {
    tags: &'a mut Tags,
    payload: bool,
}

impl<'de> DeserializeSeed<'de> for ObjectTags<'_> {
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(self, decoder: D) -> Result<(), D::Error> {
        decoder.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for ObjectTags<'_> {
    type Value = ();

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a Codex rollout object")
    }

    // A non-object payload has no subtype. Continue to the envelope tag if it follows.
    fn visit_unit<E>(self) -> Result<(), E> {
        Ok(())
    }
    fn visit_bool<E>(self, _: bool) -> Result<(), E> {
        Ok(())
    }
    fn visit_i64<E>(self, _: i64) -> Result<(), E> {
        Ok(())
    }
    fn visit_u64<E>(self, _: u64) -> Result<(), E> {
        Ok(())
    }
    fn visit_f64<E>(self, _: f64) -> Result<(), E> {
        Ok(())
    }
    fn visit_str<E>(self, _: &str) -> Result<(), E> {
        Ok(())
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        Ok(())
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        while let Some(key) = map.next_key::<Cow<'de, str>>()? {
            self.tags.prefix = match (self.payload, key.as_ref(), self.tags.prefix) {
                (false, "type", _) => Prefix::EnvelopeType,
                (false, "payload", _) => Prefix::Payload,
                (false, "timestamp", Prefix::Object) => Prefix::Timestamp,
                (true, "cwd" | "num_turns" | "thread_settings" | "thread_id", _) => {
                    Prefix::WorkspaceField
                }
                (true, "type", _) => Prefix::PayloadType,
                (_, _, Prefix::Object | Prefix::Timestamp | Prefix::Payload) => Prefix::Other,
                (_, _, previous) => previous,
            };
            match key.as_ref() {
                "type" => {
                    let tag = map.next_value::<Cow<'de, str>>()?;
                    if self.payload {
                        self.tags.event = match tag.as_ref() {
                            "thread_settings_applied" => {
                                EventTag::Workspace(DamagedWorkspace::PossibleSettings)
                            }
                            "thread_rolled_back" => {
                                EventTag::Workspace(DamagedWorkspace::TurnOrRollback)
                            }
                            _ => EventTag::Documentary,
                        };
                    } else {
                        self.tags.envelope = Some(match tag.as_ref() {
                            "turn_context" => Envelope::Turn,
                            "event_msg" => Envelope::Event,
                            _ => Envelope::Other,
                        });
                    }
                }
                "payload" if !self.payload => {
                    map.next_value_seed(ObjectTags {
                        tags: self.tags,
                        payload: true,
                    })?;
                }
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
            // If the envelope came first, later payload text cannot change its kind.
            match self.tags.envelope {
                Some(Envelope::Turn | Envelope::Other) => break,
                Some(Envelope::Event) if !matches!(self.tags.event, EventTag::Unread) => break,
                _ => {}
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DamagedWorkspace::{PossibleSettings, TurnOrRollback, Unclassified},
        damaged_workspace_record,
    };

    #[test]
    fn recovers_only_structural_workspace_tags_before_the_error() {
        for (line, expected) in [
            (&br#"{"type":"turn_context","payload":"#[..], Some(TurnOrRollback)),
            (br#"{"type":"event_msg","payload":{"type":"thread_rolled_back","#, Some(TurnOrRollback)),
            (br#"{"payload":{"t\u0079pe":"thread_settings_appl\u0069ed"},"type":"event_msg","#, Some(PossibleSettings)),
            (br#"{"payload":{"type":"thread_rolled_back","broken":"#, Some(TurnOrRollback)),
            (br#"{"type":"event_msg","payload":{"text":"thread_rolled_back","#, Some(Unclassified)),
            (br#"{"payload":null,"type":"event_msg","#, Some(Unclassified)),
            (br#"{"payload":[{"type":"thread_settings_applied"}],"type":"event_msg","#, Some(Unclassified)),
            (br#"{"type":"#, Some(Unclassified)),
            (br#"{"timestamp":"2026-01-17","t"#, Some(Unclassified)),
            (br#"{"timestamp":!,"type":"turn_context","payload":{"cwd":"example"}}"#, Some(Unclassified)),
            (br#"{"payload":{"cwd":"example""#, Some(Unclassified)),
            (br#"{"payload":{"type":"thread_settings_appl"#, Some(Unclassified)),
            (br#"{"t"#, Some(Unclassified)),
            (b"{", Some(Unclassified)),
            (b"{\"type\":\"turn_context\",\"payload\":\xff", Some(TurnOrRollback)),
            (br#"{"type":"response_item","payload":{"type":"thread_settings_applied","#, None),
            (br#"{"payload":{"type":"thread_rolled_back"},"type":"response_item","#, None),
            (br#"{"type":"event_msg","payload":{"type":"agent_message","message":"thread_rolled_back"#, None),
            (br#"{"text":"{\"type\":\"turn_context\"}","nested":{"type":"thread_rolled_back"},"#, None),
            (br#"{"payload":{"nested":{"type":"thread_settings_applied"}},"#, None),
            (br#""thread_settings_applied""#, None),
            (b"{invalid garbage", None),
            (b"", None),
        ] {
            assert_eq!(damaged_workspace_record(line), expected, "{}", String::from_utf8_lossy(line));
        }
    }
}
