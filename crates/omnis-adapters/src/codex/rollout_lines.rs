//! Codex paginates the public display by ordinal. Its cold model reader uses physical order.

use chrono::DateTime;
use serde_json::Value;

use super::{checkpoint_wire, completed_item};

mod events;

pub(super) use events::valid as valid_event;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Admission {
    Complete,
    ModelOnly,
    Skip,
}

#[derive(Default)]
pub(super) struct RolloutLines {
    next: Option<u64>,
    pub(super) invalid_checkpoints: usize,
    pub(super) invalid_messages: usize,
    pub(super) skipped: usize,
    pub(super) gap: bool,
}

impl RolloutLines {
    /// Each physical file has its own cursor, including a child that starts after an inherited prefix.
    pub(super) fn start_segment(&mut self, header: &Value) {
        self.next = (header["type"] == "session_meta"
            && header["payload"]["history_mode"] == "paginated")
            .then(|| {
                header["payload"]["history_base"]["end_ordinal_exclusive"]
                    .as_u64()
                    .unwrap_or(0)
            });
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
        if !self.prepare_wire(record, self.paginated(), checkpoint_mode) {
            return Admission::Skip;
        }
        let Some(next) = self.next else {
            return Admission::Complete;
        };
        let Some(ordinal) = record["ordinal"]
            .as_u64()
            .filter(|n| terminated && *n >= next)
        else {
            self.skipped += 1;
            return Admission::ModelOnly;
        };
        // The SQLite projection also stores the next exclusive ordinal.
        if ordinal >= i64::MAX as u64 {
            self.skipped += 1;
            return Admission::ModelOnly;
        }
        self.gap |= ordinal > next;
        self.next = Some(ordinal + 1);
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
    ) -> bool {
        if paginated && !valid_envelope(record) {
            self.skipped += 1;
            return false;
        }
        // Neither reader consumes the ordinal of an item it could not decode.
        if !completed_item::prepare_record(record) {
            self.invalid_messages += 1;
            return false;
        }
        if paginated {
            let decoded = match record["type"].as_str() {
                Some("response_item") => checkpoint_wire::response_record(record),
                Some("event_msg") => valid_event(&record["payload"]),
                _ => true,
            };
            if !decoded {
                self.skipped += 1;
                return false;
            }
        }
        if let Some(paginated) = checkpoint_mode
            && record["type"] == "compacted"
            && !checkpoint_wire::valid(&record["payload"], paginated)
        {
            self.invalid_checkpoints += 1;
            return false;
        }
        true
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
