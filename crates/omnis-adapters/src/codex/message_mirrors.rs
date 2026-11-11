//! Match the model, legacy display, and completed-item copies of a turn's visible messages.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use omnis_ir::EventKind;
use sha2::{Digest, Sha256};

#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
enum Spelling {
    Text,
    Attachment,
    Citation,
    NativeId,
}

#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
struct MessageKey {
    assistant: bool,
    spelling: Spelling,
    digest: [u8; 32],
}

impl MessageKey {
    fn new(kind: &EventKind, text: &str) -> Self {
        Self {
            assistant: *kind == EventKind::MessageAssistant,
            spelling: Spelling::Text,
            digest: Sha256::digest(text.as_bytes()).into(),
        }
    }
}

type DigestKey = [u8; 32];
pub(super) type ScopedMessageId = (bool, DigestKey, DigestKey);

#[derive(Clone, Copy, Debug, Default, PartialEq)]
enum ItemIdKind {
    #[default]
    Stable,
    Migrated,
    UserDisplay,
    ModelInput,
}

fn uuid_v7(id: &str) -> bool {
    id.len() == 36 && uuid::Uuid::try_parse(id).is_ok_and(|id| id.get_version_num() == 7)
}

fn model_user_input_id(id: &str) -> bool {
    id.strip_prefix("msg_input_")
        .is_some_and(|uuid| uuid.len() == 36 && uuid::Uuid::try_parse(uuid).is_ok())
        || id.strip_prefix("msg_").is_some_and(uuid_v7)
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct MessageIdentity {
    turn: Option<DigestKey>,
    item: Option<DigestKey>,
    item_kind: ItemIdKind,
    generated_turn: bool,
    pub(super) ordered_user_input: bool,
}

impl MessageIdentity {
    pub(super) fn new(turn: Option<&str>, id: Option<&str>, display: bool) -> Self {
        let digest = |value: &str| Sha256::digest(value.as_bytes()).into();
        Self {
            turn: turn.filter(|value| !value.is_empty()).map(digest),
            item: id.filter(|value| !value.is_empty()).map(digest),
            generated_turn: display && turn.is_some_and(super::is_implicit_turn),
            ordered_user_input: false,
            item_kind: if display
                && id
                    .and_then(|id| id.strip_prefix("item-"))
                    .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
            {
                ItemIdKind::Migrated
            } else if !display && id.is_some_and(model_user_input_id) {
                ItemIdKind::ModelInput
            } else {
                ItemIdKind::Stable
            },
        }
    }

    pub(super) fn completed(turn: &str, id: &str, user: bool) -> Self {
        let mut identity = Self::new(Some(turn), Some(id), true);
        // Current Codex gives the displayed user input its own UUIDv7.
        if user && uuid_v7(id) {
            identity.item_kind = ItemIdKind::UserDisplay;
        }
        identity
    }

    pub(super) fn scoped(self, assistant: bool) -> Option<ScopedMessageId> {
        Some((assistant, self.turn?, self.item?))
    }

    fn turn_alias(self, other: Self) -> Option<(DigestKey, DigestKey)> {
        let (first, second) = self.turn.zip(other.turn)?;
        match (self.generated_turn, other.generated_turn) {
            (true, false) => Some((first, second)),
            (false, true) => Some((second, first)),
            _ => None,
        }
    }

    fn compatible(self, other: Self, aliases: &HashMap<DigestKey, DigestKey>) -> bool {
        let same_turn = self.turn.zip(other.turn).is_none_or(|(a, b)| a == b);
        let compatible_turn = same_turn
            || self.turn_alias(other).is_some_and(|(generated, original)| {
                aliases
                    .get(&generated)
                    .is_none_or(|prior| *prior == original)
            });
        compatible_turn
            && (self.item_kind == ItemIdKind::Migrated
                || other.item_kind == ItemIdKind::Migrated
                || matches!(
                    (self.item_kind, other.item_kind),
                    (ItemIdKind::UserDisplay, ItemIdKind::ModelInput)
                        | (ItemIdKind::ModelInput, ItemIdKind::UserDisplay)
                )
                || self.item.zip(other.item).is_none_or(|(a, b)| a == b))
    }

    fn merge(&mut self, other: Self) {
        if self.turn.is_none()
            || (self.generated_turn && other.turn.is_some() && !other.generated_turn)
        {
            self.turn = other.turn;
            self.generated_turn = other.generated_turn;
        }
        if self.item.is_none()
            || (self.item_kind == ItemIdKind::Migrated
                && other.item.is_some()
                && other.item_kind != ItemIdKind::Migrated)
            || (self.item_kind == ItemIdKind::UserDisplay
                && other.item_kind == ItemIdKind::ModelInput)
        {
            self.item = other.item;
            self.item_kind = other.item_kind;
        }
    }
}

#[derive(Clone, Copy, Default)]
pub(super) struct MessageMatch<'a> {
    pub(super) alternate: Option<&'a str>,
    pub(super) adjacent: Option<u64>,
    pub(super) attachment: Option<DigestKey>,
    pub(super) citation: Option<DigestKey>,
    pub(super) identity: MessageIdentity,
}

impl<'a> From<Option<&'a str>> for MessageMatch<'a> {
    fn from(alternate: Option<&'a str>) -> Self {
        Self {
            alternate,
            ..Self::default()
        }
    }
}

fn encoding(raw_type: &str) -> usize {
    match raw_type {
        "response_item.message" => 0,
        "event_msg" => 1,
        "event_msg.item_completed" => 2,
        _ => unreachable!("unsupported visible message encoding"),
    }
}

#[derive(Default)]
struct Occurrence {
    seen: u8,
    spellings: HashSet<MessageKey>,
    identity: MessageIdentity,
    attachment: Option<DigestKey>,
}

struct OrderedInputCopy {
    original: u64,
    accepted: u64,
    reply: u64,
    keys: [Option<MessageKey>; 4],
    identity: MessageIdentity,
}

pub(super) struct ConfirmedInputCopy {
    pub(super) original: u64,
    pub(super) accepted: u64,
    pub(super) scoped_id: Option<ScopedMessageId>,
}

struct DisplayedReply {
    request: Option<u64>,
    sequence: u64,
    end: u64,
}

#[derive(Default)]
pub(super) struct MessageMirrors {
    occurrences: BTreeMap<u64, Occurrence>,
    displayed: HashSet<u64>,
    generated_turns: HashMap<DigestKey, DigestKey>,
    context_start: u64,
    displayed_user: Option<u64>,
    displayed_reply: Option<DisplayedReply>,
    ordered_copy: Option<OrderedInputCopy>,
    // Each queue orders equal messages by their first occurrence in the turn.
    waiting: [BTreeSet<(MessageKey, u64)>; 3],
}

impl MessageMirrors {
    pub(super) fn clear(&mut self) {
        self.occurrences.clear();
        self.context_start = 0;
        self.displayed_user = None;
        self.displayed_reply = None;
        self.ordered_copy = None;
        self.displayed.clear();
        self.generated_turns.clear();
        self.waiting.iter_mut().for_each(BTreeSet::clear);
    }

    pub(super) fn discard_before(&mut self, first_retained: u64) {
        while let Some((&sequence, _)) = self.occurrences.first_key_value() {
            if sequence >= first_retained {
                break;
            }
            self.remove(sequence);
            self.displayed.remove(&sequence);
        }
    }

    /// Codex can emit model messages before it generates their display turns.
    pub(super) fn retain_unpaired_models(&mut self) {
        self.displayed_user = None;
        self.displayed_reply = None;
        self.ordered_copy = None;
        self.generated_turns.clear();
        let displayed = std::mem::take(&mut self.displayed);
        for sequence in displayed {
            self.remove(sequence);
        }
    }

    /// An original context can change before the migrator emits any display turns.
    pub(super) fn start_context(&mut self, sequence: u64) {
        self.retain_unpaired_models();
        self.context_start = sequence;
    }

    fn first_candidate(&self, identity: MessageIdentity) -> u64 {
        if identity.generated_turn {
            0
        } else {
            self.context_start
        }
    }

    fn remove(&mut self, sequence: u64) {
        if let Some(occurrence) = self.occurrences.remove(&sequence) {
            for key in occurrence.spellings {
                for queue in &mut self.waiting {
                    queue.remove(&(key, sequence));
                }
            }
        }
    }

    /// An unstamped model message can match a named display in the pending turn.
    pub(super) fn native_match(
        &self,
        kind: &EventKind,
        id: MessageIdentity,
        raw_type: &str,
    ) -> Option<u64> {
        let key = MessageKey {
            assistant: *kind == EventKind::MessageAssistant,
            spelling: Spelling::NativeId,
            digest: id.item?,
        };
        let mut candidates = self.waiting[encoding(raw_type)]
            .range((key, self.first_candidate(id))..=(key, u64::MAX))
            .map(|(_, seq)| *seq)
            .filter(|seq| {
                self.occurrences[seq]
                    .identity
                    .compatible(id, &self.generated_turns)
            });
        let sequence = candidates.next()?;
        candidates.next().is_none().then_some(sequence)
    }

    /// Match each encoding once per occurrence. Multipart encodings may use different separators.
    pub(super) fn observe<'a>(
        &mut self,
        kind: &EventKind,
        text: &str,
        matching: impl Into<MessageMatch<'a>>,
        raw_type: &'static str,
        new_sequence: u64,
    ) -> Option<u64> {
        let wire = encoding(raw_type);
        let matching = matching.into();
        let keys = spellings(kind, text, matching);
        let lower = matching.adjacent.unwrap_or_else(|| {
            let start = self.first_candidate(matching.identity);
            if matching.identity.ordered_user_input {
                // Input accepted after a displayed reply can begin another exchange in the same turn.
                self.displayed_reply
                    .as_ref()
                    .map_or(start, |reply| start.max(reply.end))
            } else {
                start
            }
        });
        let upper = matching.adjacent.unwrap_or(u64::MAX);
        let candidate = |key: MessageKey, require_turn: bool| {
            self.waiting[wire]
                .range((key, lower)..=(key, upper))
                .map(|(_, sequence)| *sequence)
                .find(|seq| {
                    let other = &self.occurrences[seq];
                    (!require_turn
                        || matching.identity.turn.is_some()
                        || other.identity.turn.is_some())
                        && other
                            .identity
                            .compatible(matching.identity, &self.generated_turns)
                        && other
                            .attachment
                            .zip(matching.attachment)
                            .is_none_or(|(a, b)| a == b)
                })
        };
        // A matched attachment or citation distinguishes messages with identical visible text.
        let matched = keys[2].and_then(|key| candidate(key, false)).or_else(|| {
            // Codex can strip a citation without producing structured citation metadata.
            let uncited = match (kind, raw_type) {
                (EventKind::MessageAssistant, "response_item.message") => {
                    keys[2].map(|key| MessageKey {
                        spelling: Spelling::Text,
                        ..key
                    })
                }
                (EventKind::MessageAssistant, _) if matching.citation.is_none() => {
                    keys[0].map(|key| MessageKey {
                        spelling: Spelling::Citation,
                        ..key
                    })
                }
                _ => None,
            };
            keys[..2]
                .iter()
                .copied()
                .flatten()
                .filter_map(|key| candidate(key, false))
                .chain(uncited.and_then(|key| candidate(key, true)))
                .min()
        });
        let copy = if matching.identity.ordered_user_input && matched.is_none() {
            self.possible_ordered_copy(new_sequence, keys, matching)
        } else {
            None
        };
        let sequence = matched.unwrap_or(new_sequence);
        self.display(
            sequence,
            wire,
            kind,
            new_sequence + u64::from(matched.is_none()),
        );
        self.record(sequence, wire, keys, matching.identity);
        if copy.is_some() {
            self.ordered_copy = copy;
        }
        matched
    }

    fn possible_ordered_copy(
        &self,
        accepted: u64,
        keys: [Option<MessageKey>; 4],
        matching: MessageMatch<'_>,
    ) -> Option<OrderedInputCopy> {
        let reply = self.displayed_reply.as_ref()?;
        let original = reply.request?;
        let occurrence = self.occurrences.get(&original)?;
        if original < self.first_candidate(matching.identity)
            || occurrence.seen & 1 != 0
            || !occurrence
                .identity
                .compatible(matching.identity, &self.generated_turns)
            || occurrence
                .attachment
                .zip(matching.attachment)
                .is_some_and(|(a, b)| a != b)
            || !keys[..3]
                .iter()
                .flatten()
                .any(|key| occurrence.spellings.contains(key))
        {
            return None;
        }
        Some(OrderedInputCopy {
            original,
            accepted,
            reply: reply.sequence,
            keys,
            identity: matching.identity,
        })
    }

    /// A matching model reply confirms that a delayed request and reply repeat the displayed pair.
    pub(super) fn confirm_ordered_copy(
        &mut self,
        reply: Option<u64>,
        end: u64,
    ) -> Option<ConfirmedInputCopy> {
        let copy = self.ordered_copy.take()?;
        if reply != Some(copy.reply)
            || end != copy.accepted + 1
            || !self.occurrences.contains_key(&copy.original)
        {
            return None;
        }
        self.remove(copy.accepted);
        self.record(
            copy.original,
            encoding("response_item.message"),
            copy.keys,
            copy.identity,
        );
        Some(ConfirmedInputCopy {
            original: copy.original,
            accepted: copy.accepted,
            scoped_id: copy.identity.scoped(false),
        })
    }

    /// An update with a known item ID can change the text of an earlier occurrence.
    pub(super) fn update<'a>(
        &mut self,
        kind: &EventKind,
        text: &str,
        matching: impl Into<MessageMatch<'a>>,
        raw_type: &'static str,
        sequence: u64,
        end: u64,
    ) {
        let matching = matching.into();
        self.display(sequence, encoding(raw_type), kind, end);
        self.record(
            sequence,
            encoding(raw_type),
            spellings(kind, text, matching),
            matching.identity,
        );
    }

    fn display(&mut self, sequence: u64, wire: usize, kind: &EventKind, end: u64) {
        if wire == encoding("response_item.message") || !self.displayed.insert(sequence) {
            return;
        }
        if *kind == EventKind::MessageAssistant {
            // The model can write this answer before the display writes its request.
            self.displayed_reply = Some(DisplayedReply {
                request: self.displayed_user,
                sequence,
                end,
            });
        } else {
            self.displayed_user = Some(sequence);
        }
    }

    fn record(
        &mut self,
        sequence: u64,
        wire: usize,
        keys: [Option<MessageKey>; 4],
        identity: MessageIdentity,
    ) {
        if keys[0].is_some_and(|key| !key.assistant) {
            self.ordered_copy = None;
        }
        let occurrence = self.occurrences.entry(sequence).or_default();
        if let Some((generated, original)) = occurrence.identity.turn_alias(identity) {
            self.generated_turns.entry(generated).or_insert(original);
        }
        occurrence.identity.merge(identity);
        if let Some(key) = keys[2].filter(|key| key.spelling == Spelling::Attachment) {
            occurrence.attachment = Some(key.digest);
        }
        if occurrence.seen & (1 << wire) == 0 {
            for key in &occurrence.spellings {
                self.waiting[wire].remove(&(*key, sequence));
            }
            occurrence.seen |= 1 << wire;
        }
        for key in keys.into_iter().flatten() {
            if occurrence.spellings.insert(key) {
                for (index, queue) in self.waiting.iter_mut().enumerate() {
                    if occurrence.seen & (1 << index) == 0 {
                        queue.insert((key, sequence));
                    }
                }
            }
        }
    }
}

fn spellings(kind: &EventKind, text: &str, matching: MessageMatch<'_>) -> [Option<MessageKey>; 4] {
    [
        Some(MessageKey::new(kind, text)),
        matching
            .alternate
            .filter(|other| *other != text)
            .map(|other| MessageKey::new(kind, other)),
        matching
            .attachment
            .map(|digest| MessageKey {
                assistant: false,
                spelling: Spelling::Attachment,
                digest,
            })
            .or_else(|| {
                matching.citation.map(|digest| MessageKey {
                    assistant: true,
                    spelling: Spelling::Citation,
                    digest,
                })
            }),
        matching.identity.item.map(|digest| MessageKey {
            assistant: *kind == EventKind::MessageAssistant,
            spelling: Spelling::NativeId,
            digest,
        }),
    ]
}

/// Codex removes literal, non-nested citation spans from its displayed assistant message.
/// An unfinished citation extends to the end of the text.
pub(super) fn model_citation_spelling(text: &str) -> Option<DigestKey> {
    let mut digest = Sha256::new();
    super::display_text::visit_uncited(text, |chunk| digest.update(chunk))
        .then(|| digest.finalize().into())
}

pub(super) fn display_citation_spelling(text: &str) -> DigestKey {
    Sha256::digest(text.as_bytes()).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use EventKind::{MessageAssistant, MessageUser};
    const MODEL: &str = "response_item.message";
    const DISPLAY: &str = "event_msg.item_completed";
    const LEGACY: &str = "event_msg";

    #[test]
    fn delayed_messages_match_by_role_and_occurrence_in_either_order() {
        for (first, second) in [(DISPLAY, MODEL), (MODEL, DISPLAY)] {
            let mut mirrors = MessageMirrors::default();
            for (index, kind) in [MessageUser, MessageAssistant, MessageUser]
                .iter()
                .enumerate()
            {
                assert_eq!(
                    mirrors.observe(kind, "repeat", None, first, index as u64),
                    None
                );
            }
            for wire in [second, LEGACY] {
                assert_eq!(
                    mirrors.observe(&MessageUser, "repeat", None, wire, 3),
                    Some(0)
                );
                assert_eq!(
                    mirrors.observe(&MessageUser, "repeat", None, wire, 3),
                    Some(2)
                );
                assert_eq!(
                    mirrors.observe(&MessageAssistant, "repeat", None, wire, 3),
                    Some(1)
                );
            }
            mirrors.clear();
            assert_eq!(
                mirrors.observe(&MessageUser, "repeat", None, second, 3),
                None
            );
        }
    }

    #[test]
    fn multi_part_spellings_and_item_updates_keep_their_reference() {
        let mut mirrors = MessageMirrors::default();
        assert_eq!(
            mirrors.observe(&MessageAssistant, "ab", None, DISPLAY, 0),
            None
        );
        assert_eq!(
            mirrors.observe(&MessageAssistant, "a\nb", Some("ab"), MODEL, 1),
            Some(0)
        );
        assert_eq!(
            mirrors.observe(&MessageAssistant, "cd", None, DISPLAY, 1),
            None
        );
        mirrors.update(&MessageAssistant, "cd", None, DISPLAY, 0, 2);
        assert_eq!(
            mirrors.observe(&MessageAssistant, "cd", None, LEGACY, 2),
            Some(0)
        );
        assert_eq!(
            mirrors.observe(&MessageAssistant, "cd", None, LEGACY, 2),
            Some(1)
        );
        mirrors.discard_before(1);
        assert_eq!(
            mirrors.observe(&MessageAssistant, "a\nb", None, LEGACY, 2),
            None
        );
    }
}
