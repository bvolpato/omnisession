//! Relate a public assistant spelling to the native model occurrence that can vouch for it.

use std::collections::{BTreeMap, HashMap, VecDeque};

use serde_json::Value;
use sha2::{Digest, Sha256};

use super::{MAX_CANONICAL_EVENTS, Position, TurnAliases, Withdrawal};

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
    turn: Option<Hash>,
    id: Option<Hash>,
    wire: Option<Hash>,
    text: Option<Hash>,
    plan: Option<Hash>,
    ordinal: u64,
    window: u64,
    removed: bool,
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
            id: digest(&item["id"]),
            turn: digest(&item["internal_chat_message_metadata_passthrough"]["turn_id"]),
            wire,
            text,
            plan,
            ordinal,
            window: 0,
            removed: false,
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
enum Evidence {
    Witness(Raw),
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

    fn witness(&self, group: &Group) -> Option<Raw> {
        let &(mut raw, ref position) = self.rows.get(group.kept.or(group.last)?)?;
        raw.removed = position.removed;
        Some(raw.normalized(self.aliases))
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
            && let Some(raw) = self.witness(group)
            && (self.replacement || raw.window == shown.window)
            && (shown.plan.is_some()
                || raw
                    .key()
                    .is_none_or(|k| self.groups[&Key::Id(k)].displays == 0))
        {
            return Some(Evidence::Witness(raw));
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
    prior: Option<(Hash, u64)>,
}

#[derive(Default)]
pub(super) struct AnswerOccurrences {
    model: VecDeque<(Raw, Position)>,
    shown: BTreeMap<u64, Shown>,
    prior: VecDeque<(Shown, String)>,
    bytes: usize,
    rolled: bool,
    rulings: HashMap<u64, Ruling>,
    spelling_index: HashMap<(u64, Hash), usize>,
    window: u64,
}

impl AnswerOccurrences {
    pub(super) fn model(&mut self, record: &Value, ordinal: u64, plan_mode: bool) {
        if self.model.len() == MAX_CANONICAL_EVENTS {
            self.model.pop_front();
        }
        let mut raw = Raw::new(&record["payload"], ordinal, plan_mode);
        raw.window = self.window;
        self.model.push_back((raw, Position::new(record, ordinal)));
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

    pub(super) fn confirmed(
        &self,
        aliases: &TurnAliases,
        items: &[Value],
        plan_mode: impl Fn(&Value) -> bool,
    ) -> ConfirmedAnswers {
        let source = Matches::build(self, &self.model, aliases, false);
        let mut confirmed = ConfirmedAnswers {
            aliases: aliases.clone(),
            copies: Default::default(),
        };
        let incomplete_ids = items.iter().any(|item| {
            is_reply(item)
                && (!item["id"].is_string()
                    || !item["internal_chat_message_metadata_passthrough"]["turn_id"].is_string())
        });
        let mut pending = Vec::new();
        for shown in self.versions() {
            match source.find(shown) {
                Some(Evidence::Witness(raw)) => {
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
            let rows = items
                .iter()
                .filter(|item| is_reply(item))
                .map(|item| {
                    (
                        Raw::new(item, 0, plan_mode(item)),
                        Position {
                            ordinal: 0,
                            order: None,
                            removed: false,
                        },
                    )
                })
                .collect();
            let replacement = Matches::build(self, &rows, aliases, true);
            for (shown, source) in pending {
                if let Some(Evidence::Witness(raw)) = replacement.find(shown)
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

    pub(super) fn rollback(&mut self, ordinal: u64, order: Option<u64>) {
        self.rolled = true;
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
                let prior = &mut rulings.entry(shown.seq).or_default().prior;
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
            rulings.entry(seq).or_default().current = Some(evidence);
            if self.rolled {
                result.insert(seq, evidence.status());
            }
        }
        self.rulings = rulings;
        result
    }

    fn text(&self, sequence: u64, digest: Hash) -> Option<&str> {
        let index = *self.spelling_index.get(&(sequence, digest))?;
        self.prior.get(index).map(|(_, text)| text.as_str())
    }

    pub(super) fn project(
        &self,
        seq: u64,
        checkpoint: Option<&super::super::paginated_checkpoint::PaginatedCheckpoint>,
        baseline: Option<Withdrawal>,
    ) -> (Option<Withdrawal>, Option<&str>) {
        let Some(shown) = self.shown.get(&seq) else {
            return (baseline, None);
        };
        let ruling = self.rulings.get(&seq);
        let current = ruling.and_then(|r| r.current);
        let status = current
            .and_then(Evidence::status)
            .unwrap_or(Withdrawal::Unverified);
        let prior = ruling.and_then(|r| r.prior);
        if let Some(checkpoint) = checkpoint.filter(|c| c.applies_to(seq)) {
            let boundary = checkpoint.source_ordinal();
            if matches!(current,Some(Evidence::Witness(raw)) if !raw.removed && raw.ordinal>boundary)
            {
                return (None, None);
            }
            let kept = prior
                .filter(|(_, ordinal)| *ordinal > boundary)
                .map(|(text, _)| text)
                .or_else(|| checkpoint.retained_answer(seq));
            return match kept {
                Some(text) if shown.text == text => (None, None),
                Some(text) => (Some(status), self.text(seq, text)),
                None => (baseline, None),
            };
        }
        let active = baseline
            .is_some()
            .then_some(prior)
            .flatten()
            .and_then(|(digest, _)| self.text(seq, digest));
        (baseline, active)
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
