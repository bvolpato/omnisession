//! Relate a public assistant spelling to the native model occurrence that can vouch for it.

use std::collections::{BTreeMap, HashMap, VecDeque};

use serde_json::Value;
use sha2::{Digest, Sha256};

use super::super::paginated_checkpoint::{CheckpointHistory, PaginatedCheckpoint};
use super::{
    MAX_CANONICAL_EVENTS, Position, ReplyAnchor, ReplyProjection, ReplyVersion, TurnAliases,
    Withdrawal,
};

type Hash = [u8; 32];
type Scoped = (Hash, Hash);
type NativeId = (Option<Hash>, Hash);
type ModelVersion = (Option<Hash>, Option<Hash>, Hash);
type DisplayCopy = (u64, Hash);

fn hash(text: &str) -> Hash {
    Sha256::digest(text.as_bytes()).into()
}

fn scope(key: Scoped, aliases: &TurnAliases) -> Scoped {
    (aliases.get(&key.0).copied().unwrap_or(key.0), key.1)
}

fn is_reply(item: &Value) -> bool {
    item["type"] == "message"
        && item["role"] == "assistant"
        && !super::super::paginated_rollback::model_instruction(item)
}

fn raw_text(item: &Value) -> Option<Hash> {
    let parts = item["content"].as_array()?;
    if let [part] = parts.as_slice()
        && part["type"] == "output_text"
        && let Some(text) = part["text"].as_str()
        && let Some(digest) = super::super::message_mirrors::model_citation_spelling(text)
    {
        return Some(digest);
    }
    let mut digest = Sha256::new();
    for part in parts {
        if part["type"] != "output_text" {
            return None;
        }
        digest.update(part["text"].as_str()?.as_bytes());
    }
    Some(digest.finalize().into())
}

#[derive(Clone, Copy)]
struct Raw {
    reserved: Option<(u64, Option<chrono::DateTime<chrono::Utc>>)>,
    turn: Option<Hash>,
    id: Option<Hash>,
    wire: Option<Hash>,
    text: Option<Hash>,
    plan: Option<Hash>,
    ordinal: u64,
    window: u64,
}

impl Raw {
    fn new(item: &Value, ordinal: u64, plan_mode: bool) -> Self {
        let digest = |v: &Value| v.as_str().filter(|s| !s.is_empty()).map(hash);
        let wire = raw_text(item);
        let (text, plan) = plan_mode
            .then(|| super::super::display_text::model_plan(item))
            .flatten()
            .map_or((wire, None), |(outer, plan)| (outer, Some(plan)));
        Self {
            reserved: None,
            id: digest(&item["id"]),
            turn: digest(&item["internal_chat_message_metadata_passthrough"]["turn_id"]),
            wire,
            text,
            plan,
            ordinal,
            window: 0,
        }
    }

    fn normalized(mut self, aliases: &TurnAliases) -> Self {
        self.turn = self
            .turn
            .map(|turn| aliases.get(&turn).copied().unwrap_or(turn));
        self
    }

    fn key(self) -> Option<NativeId> {
        self.id.map(|id| (self.turn, id))
    }

    fn version(self) -> Option<ModelVersion> {
        Some((self.turn, self.id, self.wire?))
    }
}

#[derive(Clone, Copy)]
struct Shown {
    seq: u64,
    ordinal: u64,
    key: Scoped,
    text: Hash,
    generated: bool,
    revised: bool,
    plan: Option<Hash>,
    window: u64,
}

impl Shown {
    fn text_key(self, turn: Hash) -> Key {
        Key::Text(self.plan.is_some(), turn, self.plan.unwrap_or(self.text))
    }
}

#[derive(Hash, PartialEq, Eq)]
enum Key {
    Id(NativeId),
    Version(NativeId, Hash),
    Text(bool, Hash, Hash),
}

#[derive(Default)]
struct Group {
    last: Option<usize>,
    kept: Option<usize>,
    several: bool,
    removed: bool,
    displays: usize,
}

#[derive(Clone, Copy)]
struct Witness {
    index: usize,
    ordinal: u64,
    removed: bool,
}

#[derive(Clone, Copy)]
enum Evidence {
    Witness(Witness),
    Unverified,
}

impl Evidence {
    fn status(self) -> Option<Withdrawal> {
        match self {
            Self::Witness(raw) => raw.removed.then_some(Withdrawal::Confirmed),
            Self::Unverified => Some(Withdrawal::Unverified),
        }
    }
}

struct Matches<'a> {
    history: &'a AnswerOccurrences,
    rows: &'a VecDeque<(Raw, Position)>,
    aliases: &'a TurnAliases,
    replacement: bool,
    groups: HashMap<Key, Group>,
}

impl<'a> Matches<'a> {
    fn build(
        history: &'a AnswerOccurrences,
        rows: &'a VecDeque<(Raw, Position)>,
        aliases: &'a TurnAliases,
        replacement: bool,
    ) -> Self {
        let mut groups = HashMap::<Key, Group>::new();
        for (index, &(raw, ref position)) in rows.iter().enumerate() {
            let raw = raw.normalized(aliases);
            let id = raw
                .key()
                .filter(|_| raw.plan.is_none() || raw.text.is_some());
            for key in [
                id.map(Key::Id),
                id.zip(raw.text).map(|(id, text)| Key::Version(id, text)),
                raw.turn
                    .zip(raw.text)
                    .map(|(turn, text)| Key::Text(false, turn, text)),
                raw.turn
                    .zip(raw.plan)
                    .map(|(turn, text)| Key::Text(true, turn, text)),
            ]
            .into_iter()
            .flatten()
            {
                let group = groups.entry(key).or_default();
                group.several |= group.last.replace(index).is_some();
                group.removed |= position.removed;
                if !position.removed {
                    group.kept = Some(index);
                }
            }
        }
        for shown in history.versions() {
            let (turn, id) = scope(shown.key, aliases);
            if shown.plan.is_none() {
                groups
                    .entry(Key::Id((Some(turn), id)))
                    .or_default()
                    .displays += 1;
                groups
                    .entry(Key::Version((None, id), shown.text))
                    .or_default()
                    .displays += 1;
            }
            groups.entry(shown.text_key(turn)).or_default().displays += 1;
        }
        Self {
            history,
            rows,
            aliases,
            replacement,
            groups,
        }
    }

    fn witness(&self, group: &Group) -> Option<Witness> {
        let index = group.kept.or(group.last)?;
        let (_, position) = &self.rows[index];
        Some(Witness {
            index,
            ordinal: position.ordinal,
            removed: position.removed,
        })
    }

    fn raw(&self, witness: Witness) -> Raw {
        self.rows[witness.index].0.normalized(self.aliases)
    }

    fn unique(&self, raw: Raw, shown: Shown) -> bool {
        let group = match (shown.plan, raw.key(), raw.turn, raw.text) {
            (Some(plan), _, Some(turn), _) => Key::Text(true, turn, plan),
            (None, Some(id), _, Some(text)) => Key::Version(id, text),
            (None, Some(id), _, None) => Key::Id(id),
            (None, None, Some(turn), Some(text)) => Key::Text(false, turn, text),
            _ => return false,
        };
        self.groups.get(&group).is_some_and(|group| !group.several)
    }

    fn projection_slot(&self, witness: Witness, shown: Shown) -> Option<u64> {
        let raw = self.raw(witness);
        let (slot, _) = raw.reserved?;
        self.unique(raw, shown)
            .then_some(slot + u64::from(shown.plan.is_some()))
    }

    fn identified(&self, shown: Shown, id: NativeId) -> Option<Evidence> {
        if let Some(raw) = self
            .groups
            .get(&Key::Version(id, shown.text))
            .and_then(|g| self.witness(g))
        {
            return Some(Evidence::Witness(raw));
        }
        // A missing turn can identify an item only through a unique ID and exact public text.
        let unstamped = (None, id.1);
        if let Some(group) = self
            .groups
            .get(&Key::Version(unstamped, shown.text))
            .filter(|g| g.last.is_some())
        {
            if !group.several && group.displays == 1 {
                return self.witness(group).map(Evidence::Witness);
            }
            return group.removed.then_some(Evidence::Unverified);
        }
        if let Some(group) = self.groups.get(&Key::Id(id)).filter(|g| g.last.is_some()) {
            // Source copies can differ in formatting. A replacement alone must match the public spelling.
            if !self.replacement && !group.several && !self.history.shown.get(&shown.seq)?.revised {
                return self.witness(group).map(Evidence::Witness);
            }
            return (shown.revised || group.removed).then_some(Evidence::Unverified);
        }
        self.groups
            .get(&Key::Id(unstamped))
            .is_some_and(|g| g.removed)
            .then_some(Evidence::Unverified)
    }

    fn unidentified_neighbor(&self, shown: Shown, turn: Hash) -> bool {
        // Without native IDs, adjacent identical records can establish uncertainty only.
        if self.replacement || !self.history.rolled {
            return false;
        }
        [shown.ordinal.checked_sub(1), shown.ordinal.checked_add(1)]
            .into_iter()
            .flatten()
            .filter_map(|n| {
                self.rows
                    .binary_search_by_key(&n, |(raw, _)| raw.ordinal)
                    .ok()
            })
            .any(|index| {
                let (raw, position) = &self.rows[index];
                let raw = raw.normalized(self.aliases);
                let spelling = if shown.plan.is_some() {
                    raw.plan
                } else {
                    raw.text
                };
                position.removed
                    && raw.window == shown.window
                    && raw.turn.is_none_or(|t| t == turn)
                    && (raw.id.is_none()
                        || ((shown.generated || shown.plan.is_some()) && raw.turn.is_none()))
                    && spelling == Some(shown.plan.unwrap_or(shown.text))
            })
    }

    fn find(&self, shown: Shown) -> Option<Evidence> {
        let (turn, id) = scope(shown.key, self.aliases);
        if shown.plan.is_none() {
            if let Some(evidence) = self.identified(shown, (Some(turn), id)) {
                return Some(evidence);
            }
            if !shown.generated {
                return self
                    .unidentified_neighbor(shown, turn)
                    .then_some(Evidence::Unverified);
            }
        }
        let group = self.groups.get(&shown.text_key(turn))?;
        if !group.several
            && group.displays == 1
            && let Some(witness) = self.witness(group)
            && let raw = self.raw(witness)
            && (self.replacement || raw.window == shown.window)
            && (shown.plan.is_some()
                || raw
                    .key()
                    .is_none_or(|k| self.groups[&Key::Id(k)].displays == 0))
        {
            return Some(Evidence::Witness(witness));
        }
        (group.removed || self.unidentified_neighbor(shown, turn)).then_some(Evidence::Unverified)
    }
}

pub(in super::super) struct ConfirmedAnswers {
    aliases: TurnAliases,
    copies: [HashMap<ModelVersion, Option<DisplayCopy>>; 2],
}

impl ConfirmedAnswers {
    pub(in super::super) fn get(&self, item: &Value) -> Option<DisplayCopy> {
        self.projection(item, false)
    }

    pub(in super::super) fn get_plan(&self, item: &Value) -> Option<DisplayCopy> {
        self.projection(item, true)
    }

    fn projection(&self, item: &Value, plan: bool) -> Option<DisplayCopy> {
        let copies = &self.copies[usize::from(plan)];
        if copies.is_empty() || !is_reply(item) {
            return None;
        }
        let key = Raw::new(item, 0, false)
            .normalized(&self.aliases)
            .version()?;
        copies.get(&key).copied().flatten()
    }

    fn insert(&mut self, raw: Raw, shown: Shown) {
        if let Some(key) = raw.version() {
            let entry = Some((shown.seq, shown.text));
            self.copies[usize::from(shown.plan.is_some())]
                .entry(key)
                .and_modify(|prior| {
                    if *prior != entry {
                        *prior = None;
                    }
                })
                .or_insert(entry);
        }
    }
}

#[derive(Default)]
struct Ruling {
    current: Option<Evidence>,
    position: Option<(u64, u64)>,
    prior: Option<(Hash, u64)>,
    // A revised public item can have several accepted spellings in the same model window.
    copies: Vec<(Hash, u64, u64)>,
}

#[derive(Clone, Copy)]
pub(super) struct PendingPlanSlot {
    pub(super) ordinal: u64,
    pub(super) sequence: u64,
    pub(super) index: u64,
    pub(super) plan: bool,
}

pub(in super::super) struct PendingPlanWindow {
    pub(super) ordinal: u64,
    rows: VecDeque<(Raw, Position)>,
}

impl PendingPlanWindow {
    pub(super) fn read(ordinal: u64, items: &[Value], plan_mode: impl Fn(&Value) -> bool) -> Self {
        Self {
            ordinal,
            rows: replacement_rows(items, plan_mode),
        }
    }

    pub(super) fn len(&self) -> usize {
        self.rows.len()
    }
}

fn replacement_rows(
    items: &[Value],
    plan_mode: impl Fn(&Value) -> bool,
) -> VecDeque<(Raw, Position)> {
    items
        .iter()
        .enumerate()
        .filter(|(_, item)| is_reply(item))
        .map(|(index, item)| {
            let ordinal = index as u64;
            (
                Raw::new(item, ordinal, plan_mode(item)),
                Position {
                    ordinal,
                    order: None,
                    removed: false,
                },
            )
        })
        .collect()
}

#[derive(Default)]
pub(super) struct AnswerOccurrences {
    model: VecDeque<(Raw, Position)>,
    expected_plans: HashMap<u64, Hash>,
    open_plans: HashMap<Hash, u64>,
    shown: BTreeMap<u64, Shown>,
    prior: VecDeque<(Shown, String)>,
    bytes: usize,
    rolled: bool,
    rulings: HashMap<u64, Ruling>,
    spelling_index: HashMap<(u64, Hash), usize>,
    window: u64,
}

impl AnswerOccurrences {
    pub(super) fn model(
        &mut self,
        record: &Value,
        ordinal: u64,
        reserved: Option<u64>,
        plan_mode: bool,
        active_turn: Option<&str>,
    ) {
        if self.model.len() == MAX_CANONICAL_EVENTS
            && let Some((raw, _)) = self.model.pop_front()
            && let Some(turn) = self.expected_plans.remove(&raw.ordinal)
            && self.open_plans.get(&turn) == Some(&raw.ordinal)
        {
            self.open_plans.remove(&turn);
        }
        let mut raw = Raw::new(&record["payload"], ordinal, plan_mode);
        // Codex completes only the first Plan in each Responses API response.
        if raw.plan.is_some()
            && let Some(turn) = raw.turn.or_else(|| active_turn.map(hash))
            && let std::collections::hash_map::Entry::Vacant(slot) = self.open_plans.entry(turn)
        {
            slot.insert(ordinal);
            self.expected_plans.insert(ordinal, turn);
        }
        raw.window = self.window;
        raw.reserved =
            reserved.map(|seq| (seq, super::super::parse_timestamp(record.get("timestamp"))));
        self.model.push_back((raw, Position::new(record, ordinal)));
    }

    pub(super) fn surviving_plans(&self) -> impl Iterator<Item = (Hash, Hash, u64)> + '_ {
        self.model.iter().filter_map(|(raw, position)| {
            let body = raw.plan.filter(|_| !position.removed)?;
            let turn = *self.expected_plans.get(&raw.ordinal)?;
            Some((turn, body, raw.ordinal))
        })
    }

    pub(super) fn end_response(&mut self, turn: &str) {
        if !self.open_plans.is_empty() {
            self.open_plans.remove(&hash(turn));
        }
    }

    pub(super) fn archive(&mut self, seq: u64, text: &str) {
        const LIMIT: usize = 2 * 1024 * 1024;
        let Some(&old) = self.shown.get(&seq) else {
            return;
        };
        let size = text.len();
        if size > LIMIT || old.text != hash(text) {
            return;
        }
        while self.prior.len() >= 256 || self.bytes + size > LIMIT {
            let Some((_, expired)) = self.prior.pop_front() else {
                break;
            };
            self.bytes -= expired.len();
        }
        self.bytes += size;
        self.prior.push_back((old, text.to_owned()));
    }

    pub(super) fn display(
        &mut self,
        seq: u64,
        turn: &str,
        id: &str,
        text: &str,
        ordinal: u64,
        plan: bool,
    ) {
        let plan = plan.then(|| super::super::display_text::display_digest(text));
        let digest = hash(text);
        let old = self.shown.get(&seq);
        if old.is_some_and(|old| old.text == digest) {
            return;
        }
        self.shown.insert(
            seq,
            Shown {
                seq,
                ordinal,
                key: (hash(turn), hash(id)),
                text: digest,
                generated: id
                    .strip_prefix("item-")
                    .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())),
                revised: old.is_some(),
                plan,
                window: self.window,
            },
        );
        if self.shown.len() > MAX_CANONICAL_EVENTS {
            self.shown.pop_first();
        }
    }

    fn versions(&self) -> impl Iterator<Item = Shown> + '_ {
        self.prior
            .iter()
            .map(|(shown, _)| *shown)
            .chain(self.shown.values().copied())
    }

    pub(super) fn archived(&self, seq: u64, digest: Hash) -> Option<&str> {
        self.prior
            .iter()
            .rev()
            .find(|(s, _)| s.seq == seq && s.text == digest)
            .map(|(_, s)| s.as_str())
    }

    pub(super) fn project_revisions(
        &self,
        builder: &mut super::EventBuilder,
        checkpoints: &CheckpointHistory,
        end: u64,
        projected: &mut BTreeMap<u64, Position>,
    ) -> bool {
        // The display keeps its first position. The model keeps each accepted reply in source order.
        let mut events = BTreeMap::new();
        for shown in self.shown.values().filter(|shown| shown.revised) {
            let projection = self.project(shown.seq, checkpoints, None);
            if projection.versions.is_empty() {
                continue;
            }
            let mut older: Vec<&str> = Vec::new();
            for version in projection.versions {
                if let Some(text) = version.spelling
                    && !older.contains(&text)
                {
                    older.push(text);
                }
                let (ordinal, sequence) = match version.anchor {
                    ReplyAnchor::Source { ordinal, sequence } if sequence >= end => {
                        (ordinal, sequence)
                    }
                    // The checkpoint writer already materialized these native entries in order.
                    ReplyAnchor::Checkpoint | ReplyAnchor::Source { .. } => continue,
                };
                let Ok(index) = self
                    .model
                    .binary_search_by_key(&ordinal, |(raw, _)| raw.ordinal)
                else {
                    continue;
                };
                let (raw, position) = &self.model[index];
                let Some(mut event) = builder.event_at_mut(shown.seq).cloned() else {
                    continue;
                };
                let Some(text) = version.spelling.or_else(|| event.payload["text"].as_str()) else {
                    continue;
                };
                event.sequence = sequence;
                event.timestamp = raw.reserved.and_then(|(_, timestamp)| timestamp);
                event.source.raw_record_type = Some("response_item.message".into());
                event.payload =
                    serde_json::json!({"text":text,"omnisession_codex_replacement":true});
                events.insert(sequence, event);
                projected.insert(
                    sequence,
                    Position {
                        ordinal,
                        order: position.order,
                        removed: false,
                    },
                );
            }
            if let Some(display) = builder.event_at_mut(shown.seq) {
                display.payload["omnisession_codex_positioned_reply"] = older.into();
            }
        }
        let positioned = !events.is_empty();
        builder.fill_reserved_sequences(events.into_values().collect());
        positioned
    }

    pub(super) fn placement(
        &self,
        aliases: &TurnAliases,
        positions: &mut BTreeMap<u64, u64>,
        pending: bool,
    ) -> HashMap<super::ScopedMessageId, Option<u64>> {
        let matches = Matches::build(self, &self.model, aliases, false);
        let mut published = HashMap::new();
        let mut publish = |key: Scoped, seq| {
            if pending {
                published
                    .entry((true, key.0, key.1))
                    .and_modify(|prior| {
                        if *prior != Some(seq) {
                            *prior = None;
                        }
                    })
                    .or_insert(Some(seq));
            }
        };
        for &shown in self.shown.values() {
            let scoped = scope(shown.key, aliases);
            if shown.plan.is_none() {
                publish(scoped, shown.seq);
            }
            positions.remove(&shown.seq);
            if let Some(Evidence::Witness(witness)) = matches.find(shown)
                && let raw = matches.raw(witness)
                && matches.unique(raw, shown)
            {
                positions.insert(shown.seq, raw.ordinal);
                if shown.plan.is_none()
                    && let Some(key) = raw.turn.zip(raw.id)
                {
                    publish(key, shown.seq);
                }
            }
        }
        published
    }

    pub(super) fn confirmed(
        &self,
        aliases: &TurnAliases,
        items: &[Value],
        plan_mode: impl Fn(&Value) -> bool,
    ) -> ConfirmedAnswers {
        let incomplete_ids = items.iter().any(|item| {
            is_reply(item)
                && (!item["id"].is_string()
                    || !item["internal_chat_message_metadata_passthrough"]["turn_id"].is_string())
        });
        self.confirmed_rows(aliases, incomplete_ids, None, || {
            replacement_rows(items, plan_mode)
        })
    }

    fn confirmed_rows<R: std::borrow::Borrow<VecDeque<(Raw, Position)>>>(
        &self,
        aliases: &TurnAliases,
        incomplete_ids: bool,
        pending_checkpoint: Option<u64>,
        rows: impl FnOnce() -> R,
    ) -> ConfirmedAnswers {
        let source = Matches::build(self, &self.model, aliases, false);
        let mut confirmed = ConfirmedAnswers {
            aliases: aliases.clone(),
            copies: Default::default(),
        };
        let mut pending = Vec::new();
        for shown in self.versions() {
            match source.find(shown) {
                Some(Evidence::Witness(witness)) => {
                    let raw = source.raw(witness);
                    if pending_checkpoint
                        .is_some_and(|ordinal| raw.ordinal > ordinal || !source.unique(raw, shown))
                    {
                        continue;
                    }
                    confirmed.insert(raw, shown);
                    if incomplete_ids || raw.turn.is_none() || raw.id.is_none() {
                        pending.push((shown, Some(raw)));
                    }
                }
                Some(Evidence::Unverified) if shown.generated || shown.plan.is_some() => {}
                None | Some(Evidence::Unverified) => pending.push((shown, None)),
            }
        }
        drop(source);
        if !pending.is_empty() {
            let rows = rows();
            let replacement = Matches::build(self, rows.borrow(), aliases, true);
            for (shown, source) in pending {
                if let Some(Evidence::Witness(witness)) = replacement.find(shown)
                    && let raw = replacement.raw(witness)
                    && source.is_none_or(|s| {
                        s.wire == raw.wire
                            && s.id.zip(raw.id).is_none_or(|(a, b)| a == b)
                            && s.turn.zip(raw.turn).is_none_or(|(a, b)| a == b)
                    })
                {
                    confirmed.insert(raw, shown);
                }
            }
        }
        confirmed
    }

    pub(super) fn resolve_plan_window(
        &self,
        window: &PendingPlanWindow,
        slots: &[PendingPlanSlot],
        builder: &mut super::EventBuilder,
        aliases: &TurnAliases,
        model_positions: &mut BTreeMap<u64, u64>,
    ) {
        let incomplete_ids = window
            .rows
            .iter()
            .any(|(raw, _)| raw.turn.is_none() || raw.id.is_none());
        let confirmed = self.confirmed_rows(aliases, incomplete_ids, Some(window.ordinal), || {
            &window.rows
        });
        let matches = Matches::build(self, &window.rows, aliases, true);
        let versions: HashMap<_, _> = self.versions().map(|s| ((s.seq, s.text), s)).collect();
        for slot in slots.iter().filter(|slot| slot.ordinal == window.ordinal) {
            let Ok(index) = window
                .rows
                .binary_search_by_key(&slot.index, |(raw, _)| raw.ordinal)
            else {
                continue;
            };
            let raw = window.rows[index].0.normalized(aliases);
            let Some(version) = raw.version() else {
                continue;
            };
            let Some((seq, digest)) = confirmed.copies[usize::from(slot.plan)]
                .get(&version)
                .copied()
                .flatten()
            else {
                continue;
            };
            let Some(&shown) = versions.get(&(seq, digest)) else {
                continue;
            };
            if shown.ordinal <= window.ordinal || !matches.unique(raw, shown) {
                continue;
            }
            let Some(current) = builder
                .event_at_mut(seq)
                .and_then(|e| e.payload["text"].as_str())
            else {
                continue;
            };
            let Some(text) = (hash(current) == digest)
                .then_some(current)
                .or_else(|| self.archived(seq, digest))
            else {
                continue;
            };
            let text = text.to_owned();
            if super::fill_pending_reply(builder, slot.sequence, text) {
                let position = model_positions.entry(seq).or_default();
                *position = (*position).max(window.ordinal);
            }
        }
    }

    pub(super) fn rollback(&mut self, ordinal: u64, order: Option<u64>) {
        self.rolled = true;
        self.open_plans.clear();
        self.window = self.window.saturating_add(1);
        for (_, position) in &mut self.model {
            position.rollback(ordinal, order);
        }
    }

    pub(super) fn rulings(&mut self, aliases: &TurnAliases) -> HashMap<u64, Option<Withdrawal>> {
        self.spelling_index = self
            .prior
            .iter()
            .enumerate()
            .map(|(i, (shown, _))| ((shown.seq, shown.text), i))
            .collect();
        if !self.rolled && !self.shown.values().any(|s| s.revised) {
            return HashMap::new();
        }
        let matches = Matches::build(self, &self.model, aliases, false);
        let mut rulings = HashMap::<u64, Ruling>::new();
        for (shown, _) in &self.prior {
            if let Some(Evidence::Witness(raw)) = matches.find(*shown)
                && !raw.removed
            {
                let ruling = rulings.entry(shown.seq).or_default();
                if let Some(slot) = matches.projection_slot(raw, *shown) {
                    ruling.copies.push((shown.text, raw.ordinal, slot));
                }
                let prior = &mut ruling.prior;
                if prior.is_none_or(|(_, ordinal)| ordinal <= raw.ordinal) {
                    *prior = Some((shown.text, raw.ordinal));
                }
            }
        }
        let mut result = HashMap::new();
        for (&seq, &shown) in &self.shown {
            let Some(evidence) = matches
                .find(shown)
                .or_else(|| rulings.contains_key(&seq).then_some(Evidence::Unverified))
            else {
                continue;
            };
            let ruling = rulings.entry(seq).or_default();
            if let Evidence::Witness(raw) = evidence
                && !raw.removed
                && let Some(slot) = matches.projection_slot(raw, shown)
            {
                ruling.position = Some((raw.ordinal, slot));
                if shown.revised {
                    ruling.copies.push((shown.text, raw.ordinal, slot));
                }
            }
            ruling
                .copies
                .sort_unstable_by_key(|(_, ordinal, _)| *ordinal);
            ruling.copies.dedup_by_key(|(_, ordinal, _)| *ordinal);
            ruling.current = Some(evidence);
            if self.rolled {
                result.insert(seq, evidence.status());
            }
        }
        self.rulings = rulings;
        result
    }

    pub(super) fn surviving_position(&self, sequence: u64, after: u64) -> Option<(u64, u64)> {
        self.rulings
            .get(&sequence)?
            .position
            .filter(|(ordinal, _)| *ordinal > after)
    }

    fn text(&self, sequence: u64, digest: Hash) -> Option<&str> {
        let index = *self.spelling_index.get(&(sequence, digest))?;
        self.prior.get(index).map(|(_, text)| text.as_str())
    }

    fn version(&self, shown: &Shown, text: Hash, anchor: ReplyAnchor) -> Option<ReplyVersion<'_>> {
        let spelling = if shown.text == text {
            None
        } else {
            Some(self.text(shown.seq, text)?)
        };
        Some(ReplyVersion { spelling, anchor })
    }

    pub(super) fn project(
        &self,
        seq: u64,
        checkpoints: &CheckpointHistory,
        baseline: Option<Withdrawal>,
    ) -> ReplyProjection<'_> {
        let mut projection = ReplyProjection {
            status: baseline,
            prior: None,
            versions: Vec::new(),
        };
        let Some(shown) = self.shown.get(&seq) else {
            return projection;
        };
        let ruling = self.rulings.get(&seq);
        let current = ruling.and_then(|r| r.current);
        let legacy = checkpoints.legacy_ordinal();
        let explicit = checkpoints.as_ref().filter(|_| legacy.is_none());
        let Some(ordinal) = legacy.or_else(|| explicit.map(PaginatedCheckpoint::source_ordinal))
        else {
            if baseline.is_some()
                && let Some((digest, _)) = ruling.and_then(|r| r.prior)
            {
                projection.prior = self.text(seq, digest);
            }
            return projection;
        };
        if !shown.revised {
            if let Some(checkpoint) = explicit.filter(|c| c.applies_to(seq))
                && (checkpoint.retained_answer(seq) == Some(shown.text)
                    || matches!(current, Some(Evidence::Witness(raw)) if !raw.removed && raw.ordinal > ordinal))
            {
                projection.status = None;
            }
            return projection;
        }
        // The replacement can retain a previous spelling while a revised reply survives in the suffix.
        if let Some(checkpoint) = explicit {
            projection.versions.extend(
                checkpoint
                    .retained_answers(seq)
                    .filter_map(|(_, text)| self.version(shown, text, ReplyAnchor::Checkpoint)),
            );
        }
        if let Some(ruling) = ruling {
            projection.versions.extend(
                ruling
                    .copies
                    .iter()
                    .filter(|(_, source, _)| *source > ordinal)
                    .filter_map(|&(text, ordinal, sequence)| {
                        self.version(shown, text, ReplyAnchor::Source { ordinal, sequence })
                    }),
            );
        }
        if !projection.versions.is_empty() {
            let current_active = projection
                .versions
                .iter()
                .any(|version| version.spelling.is_none());
            projection.status = (!current_active).then(|| {
                current
                    .and_then(Evidence::status)
                    .unwrap_or(Withdrawal::Unverified)
            });
        } else if ruling.is_some() {
            // An ambiguous or expired witness cannot place a revised reply in the suffix.
            projection.status = baseline
                .or_else(|| current.and_then(Evidence::status))
                .or(Some(Withdrawal::Unverified));
        }
        projection
    }

    pub(super) fn discard_before(&mut self, first: u64) {
        self.shown = self.shown.split_off(&first);
        self.prior.retain(|(shown, text)| {
            if shown.seq >= first {
                return true;
            }
            self.bytes -= text.len();
            false
        });
        self.rulings.retain(|&seq, _| seq >= first);
    }
}
