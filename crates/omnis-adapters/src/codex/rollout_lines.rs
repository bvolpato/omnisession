//! Codex paginates the public display by ordinal. Its cold model reader uses physical order.

use chrono::DateTime;
use serde_json::Value;

use super::{checkpoint_wire, completed_item};

mod events;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Admission {
    Complete,
    ModelOnly,
    Skip,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Wire {
    Decoded,
    Invalid,
    Undetermined,
}

impl Wire {
    fn checked(valid: bool) -> Self {
        if valid { Self::Decoded } else { Self::Invalid }
    }
}

#[derive(Clone, Copy, Default, Eq, Ord, PartialEq, PartialOrd)]
pub(super) enum OrdinalGap {
    #[default]
    None,
    Possible,
    Confirmed,
}

#[derive(Default)]
pub(super) struct RolloutLines {
    next: Option<u64>,
    // A documentary record may decode in Codex without a locally known schema.
    possible_next: Option<u64>,
    contiguous_next: u64,
    pub(super) invalid_checkpoints: usize,
    pub(super) invalid_messages: usize,
    pub(super) skipped: usize,
    pub(super) gap: OrdinalGap,
    pub(super) contested: bool,
}

impl RolloutLines {
    /// Each physical file has its own cursor, including a child that starts after an inherited prefix.
    pub(super) fn start_segment(&mut self, header: &Value) {
        self.possible_next = None;
        self.next = (header["type"] == "session_meta"
            && header["payload"]["history_mode"] == "paginated")
            .then(|| {
                header["payload"]["history_base"]["end_ordinal_exclusive"]
                    .as_u64()
                    .unwrap_or(0)
            });
        self.contiguous_next = self.next.unwrap_or(0);
    }

    pub(super) fn paginated(&self) -> bool {
        self.next.is_some()
    }

    pub(super) fn malformed(&mut self) {
        if self.paginated() {
            self.skipped += 1;
        }
    }

    pub(super) fn prepare(
        &mut self,
        record: &mut Value,
        terminated: bool,
        checkpoint_mode: Option<bool>,
    ) -> Admission {
        let wire = self.prepare_wire(record, self.paginated(), checkpoint_mode);
        if wire == Wire::Invalid {
            return Admission::Skip;
        }
        let Some(next) = self.next else {
            return Admission::Complete;
        };
        // SQLite stores both the ordinal and the next exclusive ordinal.
        let Some(ordinal) = record["ordinal"]
            .as_u64()
            .filter(|n| terminated && *n >= next && *n < i64::MAX as u64)
        else {
            self.skipped += 1;
            return Admission::ModelOnly;
        };
        let following = ordinal + 1;
        // Count a position if this physical record could fill it without skipping another.
        if ordinal == self.contiguous_next {
            self.contiguous_next = following;
        } else if ordinal > self.contiguous_next {
            let gap = if wire == Wire::Decoded {
                OrdinalGap::Confirmed
            } else {
                OrdinalGap::Possible
            };
            self.gap = self.gap.max(gap);
        }
        let possible_next = self.possible_next.unwrap_or(next);
        if ordinal < possible_next {
            // A prior documentary record could already have consumed this position.
            self.skipped += 1;
            self.contested = true;
            if wire == Wire::Decoded {
                self.next = Some(following);
                if following == possible_next {
                    self.possible_next = None;
                }
            }
            return Admission::ModelOnly;
        }
        if wire == Wire::Decoded {
            self.next = Some(following);
            self.possible_next = None;
        } else {
            self.possible_next = Some(following);
        }
        let payload = &record["payload"];
        let event = payload
            .get("type")
            .or_else(|| payload.get(0))
            .and_then(Value::as_str);
        let needs_timestamp = record["type"] == "realtime_item"
            || (record["type"] == "event_msg"
                && event == Some("item_completed")
                && payload
                    .get("started_at_ms")
                    .or_else(|| payload.get(4))
                    .is_none_or(Value::is_null));
        if needs_timestamp
            && DateTime::parse_from_rfc3339(record["timestamp"].as_str().unwrap_or_default())
                .is_err()
        {
            self.skipped += 1;
            return Admission::ModelOnly;
        }
        Admission::Complete
    }
    /// Decode the cold history without making a claim about a display position in a missing prefix.
    pub(super) fn prepare_wire(
        &mut self,
        record: &mut Value,
        paginated: bool,
        checkpoint_mode: Option<bool>,
    ) -> Wire {
        if paginated && !valid_envelope(record) {
            self.skipped += 1;
            return Wire::Invalid;
        }
        // Ordinal admission and model reconstruction must see the same fields.
        let valid_content = match record["type"].as_str() {
            Some("compacted") => {
                let payload = &mut record["payload"];
                if !checkpoint_wire::prepare(payload)
                    || checkpoint_mode.is_some_and(|mode| !checkpoint_wire::valid(payload, mode))
                {
                    self.invalid_checkpoints += 1;
                    return Wire::Invalid;
                }
                true
            }
            Some("event_msg") => {
                let payload = &mut record["payload"];
                if !events::prepare(payload) {
                    self.skipped += 1;
                    return Wire::Invalid;
                }
                completed_item::prepare_event(payload)
            }
            Some("response_item") => checkpoint_wire::prepare_response_item(&mut record["payload"]),
            _ => true,
        };
        if !valid_content {
            self.invalid_messages += 1;
            return Wire::Invalid;
        }
        if !paginated {
            return Wire::Decoded;
        }
        let wire = match record["type"].as_str() {
            Some("response_item") => Wire::checked(checkpoint_wire::response_record(record)),
            Some("event_msg") => events::classify(&record["payload"]),
            Some("compacted") if checkpoint_mode.is_some() => Wire::Decoded,
            _ => Wire::Undetermined,
        };
        if wire == Wire::Invalid {
            self.skipped += 1;
        }
        wire
    }
}

pub(super) fn valid_envelope(record: &Value) -> bool {
    record["timestamp"].is_string()
        && (record["ordinal"].is_null() || record["ordinal"].is_u64())
        && (record["payload"].is_object() || record["payload"].is_array())
        && matches!(
            record["type"].as_str(),
            Some(
                "session_meta"
                    | "dynamic_tools_updated"
                    | "response_item"
                    | "inter_agent_communication"
                    | "inter_agent_communication_metadata"
                    | "compacted"
                    | "turn_context"
                    | "token_usage_record"
                    | "world_state"
                    | "retained_context"
                    | "security_risk_score"
                    | "event_msg"
                    | "realtime_item"
            )
        )
}

/// Move a positional payload into its named representation without copying its contents.
pub(super) fn name_fields(value: &mut Value, names: &[&str]) -> bool {
    let Some(parts) = value.as_array_mut() else {
        return true;
    };
    if parts.len() > names.len() {
        return false;
    }
    *value = Value::Object(
        names
            .iter()
            .zip(std::mem::take(parts))
            .map(|(&name, part)| (name.to_owned(), part))
            .collect(),
    );
    true
}
