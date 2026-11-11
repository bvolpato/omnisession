//! Relate a public assistant spelling to the native model occurrence that can vouch for it.

use std::collections::{BTreeMap, HashMap, VecDeque};

use serde_json::Value;
use sha2::{Digest, Sha256};

use super::{MAX_CANONICAL_EVENTS, Position, TurnAliases, Withdrawal};

type Hash = [u8; 32];
type Scoped = (Hash, Hash);
type Version = (Scoped, Hash);

fn hash(text: &str) -> Hash {
    Sha256::digest(text.as_bytes()).into()
}

fn scope(key: Scoped, aliases: &TurnAliases) -> Scoped {
    (aliases.get(&key.0).copied().unwrap_or(key.0), key.1)
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
    key: Option<Scoped>,
    turn: Option<Hash>,
    text: Option<Hash>,
    ordinal: u64,
    window: u64,
    removed: bool,
}

impl Raw {
    fn new(item: &Value, ordinal: u64) -> Self {
        let turn = item["internal_chat_message_metadata_passthrough"]["turn_id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(hash);
        let key = turn.zip(item["id"].as_str().filter(|s| !s.is_empty()).map(hash));
        Self {
            key,
            turn,
            text: raw_text(item),
            ordinal,
            window: 0,
            removed: false,
        }
    }

    fn normalized(mut self, aliases: &TurnAliases) -> Self {
        self.key = self.key.map(|key| scope(key, aliases));
        self.turn = self
            .turn
            .map(|turn| aliases.get(&turn).copied().unwrap_or(turn));
        self
    }
}

#[derive(Clone, Copy)]
struct Shown {
    seq: u64,
    key: Scoped,
    text: Hash,
    generated: bool,
    revised: bool,
    window: u64,
}

#[derive(Hash, PartialEq, Eq)]
enum Key {
    Id(Scoped),
    Version(Version),
    Text(Option<Hash>, Hash),
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
    aliases: &'a TurnAliases,
    groups: HashMap<Key, Group>,
}

impl<'a> Matches<'a> {
    fn build(history: &'a AnswerOccurrences, aliases: &'a TurnAliases) -> Self {
        let mut groups = HashMap::<Key, Group>::new();
        for (index, &(raw, ref position)) in history.model.iter().enumerate() {
            let raw = raw.normalized(aliases);
            for key in [
                raw.key.map(Key::Id),
                raw.key.zip(raw.text).map(Key::Version),
                raw.text.map(|text| Key::Text(raw.turn, text)),
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
            let key = scope(shown.key, aliases);
            groups.entry(Key::Id(key)).or_default().displays += 1;
            groups
                .entry(Key::Text(Some(key.0), shown.text))
                .or_default()
                .displays += 1;
        }
        Self {
            history,
            aliases,
            groups,
        }
    }

    fn witness(&self, group: &Group) -> Option<Raw> {
        let &(mut raw, ref position) = self.history.model.get(group.kept.or(group.last)?)?;
        raw.removed = position.removed;
        Some(raw.normalized(self.aliases))
    }

    fn find(&self, shown: Shown) -> Option<Evidence> {
        let key = scope(shown.key, self.aliases);
        if let Some(raw) = self
            .groups
            .get(&Key::Version((key, shown.text)))
            .and_then(|g| self.witness(g))
        {
            return Some(Evidence::Witness(raw));
        }
        if let Some(group) = self.groups.get(&Key::Id(key)).filter(|g| g.last.is_some()) {
            // A single native ID can match different public formatting until the display changes.
            if !group.several && !self.history.shown.get(&shown.seq)?.revised {
                return self.witness(group).map(Evidence::Witness);
            }
            return (shown.revised || group.removed).then_some(Evidence::Unverified);
        }
        if !shown.generated {
            return None;
        }
        let group = self.groups.get(&Key::Text(Some(key.0), shown.text))?;
        if !group.several
            && group.displays == 1
            && let Some(raw) = self.witness(group)
            && raw.window == shown.window
            && raw
                .key
                .is_none_or(|k| self.groups[&Key::Id(k)].displays == 0)
        {
            return Some(Evidence::Witness(raw));
        }
        let unscoped = self.groups.get(&Key::Text(None, shown.text));
        (group.removed || unscoped.is_some_and(|g| g.removed)).then_some(Evidence::Unverified)
    }
}

pub(in super::super) struct ConfirmedAnswers {
    aliases: TurnAliases,
    copies: HashMap<Version, Option<(u64, Hash)>>,
}

impl ConfirmedAnswers {
    pub(in super::super) fn get(&self, item: &Value) -> Option<(u64, Hash)> {
        if item["type"] != "message" || item["role"] != "assistant" {
            return None;
        }
        let raw = Raw::new(item, 0).normalized(&self.aliases);
        self.copies.get(&(raw.key?, raw.text?)).copied().flatten()
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
    pub(super) fn model(&mut self, record: &Value, ordinal: u64) {
        if self.model.len() == MAX_CANONICAL_EVENTS {
            self.model.pop_front();
        }
        let mut raw = Raw::new(&record["payload"], ordinal);
        raw.window = self.window;
        self.model
            .push_back((raw, Position::new(record, ordinal, true)));
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

    pub(super) fn display(&mut self, seq: u64, turn: &str, id: &str, text: &str) {
        let digest = hash(text);
        let old = self.shown.get(&seq);
        if old.is_some_and(|old| old.text == digest) {
            return;
        }
        self.shown.insert(
            seq,
            Shown {
                seq,
                key: (hash(turn), hash(id)),
                text: digest,
                generated: id
                    .strip_prefix("item-")
                    .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())),
                revised: old.is_some(),
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

    pub(super) fn confirmed(&self, aliases: &TurnAliases) -> ConfirmedAnswers {
        let matches = Matches::build(self, aliases);
        let mut copies = HashMap::new();
        for shown in self.versions() {
            if let Some(Evidence::Witness(raw)) = matches.find(shown)
                && let Some(key) = raw.key.zip(raw.text)
            {
                let value = Some((shown.seq, shown.text));
                copies
                    .entry(key)
                    .and_modify(|existing| {
                        if *existing != value {
                            *existing = None;
                        }
                    })
                    .or_insert(value);
            }
        }
        ConfirmedAnswers {
            copies,
            aliases: aliases.clone(),
        }
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
        let matches = Matches::build(self, aliases);
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
