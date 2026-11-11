use chrono::DateTime;
use serde_json::Value;
use uuid::Uuid;

use super::record_uuid;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Choice {
    Node(usize),
    Detached,
    Unverified,
}

#[derive(Default)]
pub(super) struct CheckpointClock {
    epoch: Option<Epoch>,
}

struct Epoch {
    boundary: Uuid,
    anchor: Uuid,
    anchor_seen: bool,
    anchor_pending: bool,
    sides: [Side; 3],
}

#[derive(Default)]
struct Side {
    latest: Option<(i64, Option<Uuid>, Choice)>,
    uncertain: bool,
}

impl CheckpointClock {
    pub(super) fn observe(&mut self, record: &Value, selected: Option<usize>, transparent: bool) {
        if record["type"] == "system" && record["subtype"] == "compact_boundary" {
            self.epoch = Epoch::start(record);
            return;
        }
        let Some(epoch) = &mut self.epoch else { return };
        if !eligible(record) {
            return;
        }
        let id = record_uuid(record, "uuid");
        let side = side(record);
        let conversation = matches!(record["type"].as_str(), Some("user" | "assistant"))
            && !transparent
            && side.is_some();
        if id == Some(epoch.anchor) {
            let attached = record_uuid(record, "parentUuid") == Some(epoch.boundary)
                || (epoch.anchor_pending && record.get("parentUuid").is_none());
            epoch.anchor_seen = attached && conversation && selected.is_some();
        }
        epoch.anchor_pending = false;
        if let Some(side) = side {
            let choice = if conversation && id.is_some() {
                selected.map_or(Choice::Unverified, Choice::Node)
            } else {
                Choice::Unverified
            };
            epoch.sides[side].observe(record, id, choice);
        }
    }

    pub(super) fn resolve(&self, anchor: Option<Uuid>, record: &Value) -> Choice {
        let Some(epoch) = self
            .epoch
            .as_ref()
            .filter(|e| Some(e.anchor) == anchor && e.anchor_seen)
        else {
            return Choice::Unverified;
        };
        let Some(side) = side(record) else {
            return Choice::Unverified;
        };
        let side = &epoch.sides[side];
        if !matches!(record["type"].as_str(), Some("user" | "assistant")) {
            return Choice::Unverified;
        }
        let Some(now) = timestamp(record) else {
            return Choice::Unverified;
        };
        if side.uncertain {
            return Choice::Unverified;
        }
        let Some((at, id, choice)) = side.latest else {
            return Choice::Detached;
        };
        match now - at {
            0..=5_000 if id.is_some() && id == record_uuid(record, "uuid") => Choice::Unverified,
            0..=5_000 => choice,
            5_001.. => Choice::Detached,
            _ => Choice::Unverified,
        }
    }

    pub(super) fn interrupt(&mut self) {
        if let Some(epoch) = &mut self.epoch {
            for side in &mut epoch.sides {
                side.uncertain = true;
            }
        }
    }

    pub(super) fn uncertain(&self, anchor: Option<Uuid>) -> bool {
        // The main reader visits records with a missing or explicit false side flag.
        self.epoch.as_ref().is_some_and(|epoch| {
            anchor == Some(epoch.anchor) && epoch.sides[..2].iter().any(|side| side.uncertain)
        })
    }
}

impl Epoch {
    fn start(record: &Value) -> Option<Self> {
        let preserved = &record["compactMetadata"]["preservedMessages"];
        if !preserved["uuids"].as_array()?.is_empty() {
            return None;
        }
        let boundary = record_uuid(record, "uuid")?;
        let anchor = record_uuid(preserved, "anchorUuid")?;
        if boundary == anchor {
            return None;
        }
        let mut sides = [Side::default(), Side::default(), Side::default()];
        sides[side(record)?].observe(record, Some(boundary), Choice::Detached);
        Some(Self {
            boundary,
            anchor,
            sides,
            anchor_seen: false,
            anchor_pending: true,
        })
    }
}

fn side(record: &Value) -> Option<usize> {
    // Claude compares the stored property. A missing flag and an explicit false are distinct.
    match record.get("isSidechain") {
        None => Some(0),
        Some(Value::Bool(false)) => Some(1),
        Some(Value::Bool(true)) => Some(2),
        _ => None,
    }
}

impl Side {
    fn observe(&mut self, record: &Value, id: Option<Uuid>, choice: Choice) {
        let Some(at) = timestamp(record) else {
            self.uncertain = true;
            return;
        };
        // With equal timestamps, Claude selects the first eligible record in display order.
        if let Some((previous, _, _)) = self.latest
            && at <= previous
        {
            self.uncertain |= at < previous;
            return;
        }
        self.latest = Some((at, id, choice));
    }
}

fn eligible(record: &Value) -> bool {
    match record["type"].as_str() {
        Some("system") => record["subtype"] != "file_snapshot",
        Some("user" | "assistant" | "attachment") => true,
        None => matches!(
            super::string_at(record, &[&["message", "role"], &["role"]]),
            Some("user" | "assistant")
        ),
        _ => false,
    }
}

fn timestamp(record: &Value) -> Option<i64> {
    let value = record.get("timestamp")?;
    if let Some(value) = value.as_str() {
        return DateTime::parse_from_rfc3339(value)
            .ok()
            .map(|t| t.timestamp_millis());
    }
    value
        .as_i64()
        .filter(|n| n.unsigned_abs() <= 8_640_000_000_000_000)
}
