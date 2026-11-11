//! Relate native model rollbacks to completed display inputs in paginated Codex history.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use serde_json::Value;
use sha2::{Digest, Sha256};

use super::{MAX_CANONICAL_EVENTS, hook_attribution, is_contextual_user_text, legacy_hook_context};

type Key = [u8; 32];
type IdIndex<T> = HashMap<(u64, Option<Key>, Key), Vec<(T, Input)>>;

fn key(text: &str) -> Key {
    Sha256::digest(text.as_bytes()).into()
}

fn uuid_v7(value: &str) -> bool {
    value.len() == 36
        && uuid::Uuid::try_parse(value)
            .is_ok_and(|id| id.get_variant() == uuid::Variant::RFC4122 && id.get_version_num() == 7)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum IdKind {
    Stable,
    ModelInput,
    GeneratedDisplay,
    LiveDisplay,
}

#[derive(Clone, Copy)]
struct Input {
    segment: u64,
    window: u64,
    closed: bool,
    turn: Option<Key>,
    id: Option<Key>,
    kind: IdKind,
    text: Option<Key>,
    ordinal: u64,
    order: Option<u64>,
    inherited: bool,
}

impl Input {
    fn new(turn: Option<&str>, payload: &Value, display: bool, ordinal: u64, segment: u64) -> Self {
        let tag = if display { "text" } else { "input_text" };
        let mut hash = Sha256::new();
        let mut nonempty = false;
        let text = payload["content"].as_array().and_then(|parts| {
            for part in parts {
                if part["type"] != tag {
                    return None;
                }
                let text = part["text"].as_str()?;
                nonempty |= !text.is_empty();
                hash.update(text.as_bytes());
            }
            nonempty.then(|| hash.finalize().into())
        });
        let id = payload["id"].as_str().filter(|id| !id.is_empty());
        let kind = match id {
            Some(id) if display && uuid_v7(id) => IdKind::LiveDisplay,
            Some(id)
                if display
                    && id.strip_prefix("item-").is_some_and(|n| {
                        !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())
                    }) =>
            {
                IdKind::GeneratedDisplay
            }
            Some(id)
                if !display
                    && (id.strip_prefix("msg_").is_some_and(uuid_v7)
                        || id.strip_prefix("msg_input_").is_some_and(|s| {
                            s.len() == 36 && uuid::Uuid::try_parse(s).is_ok()
                        })) =>
            {
                IdKind::ModelInput
            }
            _ => IdKind::Stable,
        };
        Self {
            segment,
            window: 0,
            closed: false,
            turn: turn.filter(|v| !v.is_empty()).map(key),
            id: id.map(key),
            kind,
            text,
            ordinal,
            order: None,
            inherited: false,
        }
    }

    fn permits_text(self, shown: Self) -> bool {
        self.id
            .zip(shown.id)
            .is_none_or(|(raw, display)| raw == display)
            || shown.kind == IdKind::GeneratedDisplay
            || (self.kind == IdKind::ModelInput && shown.kind == IdKind::LiveDisplay)
    }
}

#[derive(Default)]
pub(super) struct PaginatedRollback {
    model: VecDeque<Input>,
    displayed: BTreeMap<u64, Input>,
    segment: u64,
    window: u64,
    evicted: usize,
    // Text on opposite sides of a retention boundary cannot prove a shared input.
    history_floor: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Withdrawal {
    Confirmed,
    Unverified,
}

impl Withdrawal {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Confirmed => "withdrawn",
            Self::Unverified => "unverified",
        }
    }
}

impl PaginatedRollback {
    pub(super) fn segment(&mut self) {
        self.segment = self.segment.saturating_add(1);
        self.window = self.window.saturating_add(1);
    }

    pub(super) fn model_user(&mut self, record: &Value, ordinal: u64) {
        let payload = &record["payload"];
        if payload["type"] != "message" || payload["role"] != "user" {
            return;
        }
        let content = payload["content"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default();
        let attribution = &payload["internal_chat_message_metadata_passthrough"];
        if !content.is_empty()
            && !content
                .iter()
                .enumerate()
                .any(|(i, _)| hook_attribution(attribution, i) == Some(false))
            && (legacy_hook_context(payload, content, attribution)
                || content.iter().enumerate().all(|(i, part)| {
                    part["type"] == "input_text"
                        && match hook_attribution(attribution, i) {
                            Some(hook) => hook,
                            None => part["text"].as_str().is_some_and(is_contextual_user_text),
                        }
                }))
        {
            return;
        }
        let mut input = Input::new(
            attribution["turn_id"].as_str(),
            payload,
            false,
            ordinal,
            self.segment,
        );
        input.window = self.window;
        input.inherited = record["metadata"]["inherited_user_message"] == true;
        if !input.inherited {
            input.order = record["metadata"]["user_input_order"].as_u64();
        }
        if self.model.len() == MAX_CANONICAL_EVENTS
            && let Some(old) = self.model.pop_front()
        {
            self.evicted = self.evicted.saturating_add(1);
            self.history_floor = self.history_floor.max(old.ordinal.saturating_add(1));
        }
        self.model.push_back(input);
    }

    pub(super) fn display_user(&mut self, sequence: u64, turn: &str, item: &Value, ordinal: u64) {
        let mut input = Input::new(Some(turn), item, true, ordinal, self.segment);
        input.window = self.window;
        self.displayed
            .entry(sequence)
            .and_modify(|old| {
                // A revised display stays at its original position. Changed text loses its old
                // content-based identity until the model and display IDs identify the same item.
                if old.text != input.text {
                    old.text = None;
                }
            })
            .or_insert(input);
        if self.displayed.len() > MAX_CANONICAL_EVENTS
            && let Some((_, old)) = self.displayed.pop_first()
        {
            self.history_floor = self.history_floor.max(old.ordinal.saturating_add(1));
        }
    }

    pub(super) fn discard_before(&mut self, sequence: u64) {
        while self
            .displayed
            .first_key_value()
            .is_some_and(|(seq, _)| *seq < sequence)
        {
            if let Some((_, old)) = self.displayed.pop_first() {
                self.history_floor = self.history_floor.max(old.ordinal.saturating_add(1));
            }
        }
    }

    pub(super) fn boundary(&self, count: usize) -> Option<(u64, Option<u64>)> {
        if count == 0 || (count > self.model.len() && self.evicted != 0) {
            return None;
        }
        let first = self.model.get(self.model.len().saturating_sub(count))?;
        (first.segment == self.segment).then_some((first.ordinal, first.order))
    }

    /// Identify displayed owners of withdrawn raw inputs without exposing raw message text.
    pub(super) fn withdraw(&mut self, count: usize) -> HashMap<u64, Withdrawal> {
        let mut marked = HashMap::new();
        if count == 0 {
            return marked;
        }
        // Later events cannot supply a missing copy for a boundary already rolled back.
        self.window = self.window.saturating_add(1);
        if self.evicted != 0 {
            if let Some(first) = self.model.front() {
                self.history_floor = self.history_floor.max(first.ordinal);
            }
        } else if self.model.is_empty() {
            return marked;
        }
        let cut = self.model.len().saturating_sub(count);
        let extra = count.saturating_sub(self.model.len());
        if extra != 0 && self.evicted != 0 {
            for (&seq, shown) in &self.displayed {
                if !shown.closed && (self.model.is_empty() || shown.ordinal < self.history_floor) {
                    marked.insert(seq, Withdrawal::Unverified);
                }
            }
            self.evicted = self.evicted.saturating_sub(extra);
        }
        let mut raw_ids = IdIndex::<usize>::new();
        for (n, &raw) in self.model.iter().enumerate() {
            if let Some(id) = raw.id {
                raw_ids
                    .entry((raw.window, raw.turn, id))
                    .or_default()
                    .push((n, raw));
            }
        }
        let mut by_id = IdIndex::<u64>::new();
        let mut by_text = HashMap::<(u64, Option<Key>), Vec<(u64, Input)>>::new();
        for (&seq, &shown) in &self.displayed {
            if shown.closed {
                continue;
            }
            if let Some(id) = shown.id.filter(|_| shown.kind == IdKind::Stable) {
                for scope in shown
                    .turn
                    .into_iter()
                    .map(Some)
                    .chain(std::iter::once(None))
                {
                    let identity = (shown.window, scope, id);
                    if raw_ids.contains_key(&identity) {
                        by_id.entry(identity).or_default().push((seq, shown));
                    }
                }
            }
            by_text
                .entry((shown.window, shown.text))
                .or_default()
                .push((seq, shown));
        }
        let (reserved, assigned) = self.match_ids(cut, &raw_ids, &by_id, &mut marked);
        let mut pending = HashMap::<_, Vec<(bool, Input)>>::new();
        for (n, &raw) in self.model.iter().enumerate() {
            if !assigned.contains(&n) {
                pending
                    .entry((raw.window, raw.text))
                    .or_default()
                    .push((n >= cut, raw));
            }
        }
        for (key, raw) in pending {
            if let Some(shown) = by_text.get(&key) {
                self.match_text(&raw, shown, &reserved, &mut marked);
            }
        }
        self.model.truncate(cut);
        for seq in marked.keys() {
            if let Some(shown) = self.displayed.get_mut(seq) {
                shown.closed = true;
            }
        }
        marked
    }

    fn match_ids(
        &self,
        cut: usize,
        raw_ids: &IdIndex<usize>,
        by_id: &IdIndex<u64>,
        marked: &mut HashMap<u64, Withdrawal>,
    ) -> (HashSet<u64>, HashSet<usize>) {
        let mut reserved = HashSet::new();
        let mut assigned = HashSet::new();
        // Resolve scoped IDs before considering an unstamped input with the same ID.
        for scoped in [true, false] {
            for (identity, raw) in raw_ids {
                if identity.1.is_some() != scoped {
                    continue;
                }
                let Some(shown) = by_id.get(identity) else {
                    continue;
                };
                // An unstamped repetition of this same native ID can own the display in any turn.
                let unstamped = raw_ids
                    .get(&(identity.0, None, identity.2))
                    .filter(|_| scoped);
                let dropped = raw.last().is_some_and(|(n, _)| *n >= cut)
                    || unstamped
                        .and_then(|copies| copies.last())
                        .is_some_and(|(n, _)| *n >= cut);
                let confirmed = unstamped.is_none()
                    && raw.len() == 1
                    && shown.len() == 1
                    && scoped
                    && !raw[0].1.inherited
                    && raw[0].1.segment == self.segment
                    && raw[0].1.ordinal.min(shown[0].1.ordinal) >= self.history_floor;
                for &(seq, _) in shown {
                    if reserved.insert(seq) && dropped {
                        marked.insert(
                            seq,
                            if confirmed {
                                Withdrawal::Confirmed
                            } else {
                                Withdrawal::Unverified
                            },
                        );
                    }
                }
                assigned.extend(raw.iter().map(|(n, _)| *n));
            }
        }
        (reserved, assigned)
    }

    fn match_text(
        &self,
        raw: &[(bool, Input)],
        shown: &[(u64, Input)],
        reserved: &HashSet<u64>,
        marked: &mut HashMap<u64, Withdrawal>,
    ) {
        let mut groups = HashMap::<Option<Key>, (Vec<(bool, Input)>, Vec<(u64, Input)>)>::new();
        for &(dropped, input) in raw {
            groups
                .entry(input.turn)
                .or_default()
                .0
                .push((dropped, input));
        }
        let unscoped = groups.contains_key(&None);
        for &(seq, input) in shown {
            if !reserved.contains(&seq) {
                groups.entry(input.turn).or_default().1.push((seq, input));
                if unscoped && input.turn.is_some() {
                    groups.entry(None).or_default().1.push((seq, input));
                }
            }
        }
        for (turn, (raw, mut displayed)) in groups {
            if !raw.iter().any(|(d, _)| *d) {
                continue;
            }
            displayed.sort_unstable_by_key(|(_, item)| item.ordinal);
            let paired = turn.is_some()
                && raw.len() == displayed.len()
                && raw.iter().zip(&displayed).all(|((_, r), (_, d))| {
                    r.text.is_some()
                        && !r.inherited
                        && r.segment == self.segment
                        && r.ordinal.min(d.ordinal) >= self.history_floor
                        && r.ordinal.abs_diff(d.ordinal) == 1
                        && r.permits_text(*d)
                })
                && raw
                    .iter()
                    .zip(&displayed)
                    .zip(raw.iter().zip(&displayed).skip(1))
                    .all(|(((_, a), (_, b)), ((_, c), (_, d)))| {
                        a.ordinal.max(b.ordinal) < c.ordinal.min(d.ordinal)
                    });
            if paired {
                for ((dropped, _), (seq, _)) in raw.into_iter().zip(displayed) {
                    if dropped {
                        marked.entry(seq).or_insert(Withdrawal::Confirmed);
                    }
                }
                continue;
            }
            // Precompute compatible ID classes, including orphan model records, so a long run
            // of identical requests takes linear work after sorting.
            let mut ids = HashSet::new();
            let mut unspecified = false;
            let mut live_input = false;
            for (_, r) in raw.into_iter().filter(|(d, _)| *d) {
                if let Some(id) = r.id {
                    ids.insert(id);
                } else {
                    unspecified = true;
                }
                live_input |= r.kind == IdKind::ModelInput;
            }
            for (seq, d) in displayed {
                if unspecified
                    || d.id.is_none_or(|id| ids.contains(&id))
                    || d.kind == IdKind::GeneratedDisplay
                    || (live_input && d.kind == IdKind::LiveDisplay)
                {
                    marked.insert(seq, Withdrawal::Unverified);
                }
            }
        }
    }
}
