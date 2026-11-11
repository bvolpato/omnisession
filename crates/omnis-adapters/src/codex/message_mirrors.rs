//! Match the model, legacy display, and completed-item copies of a turn's visible messages.

use std::collections::{BTreeMap, BTreeSet, HashSet};

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

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct MessageIdentity {
    turn: Option<DigestKey>,
    item: Option<DigestKey>,
    generated: bool,
}

impl MessageIdentity {
    pub(super) fn new(turn: Option<&str>, id: Option<&str>, display: bool) -> Self {
        let digest = |value: &str| Sha256::digest(value.as_bytes()).into();
        Self {
            turn: turn.filter(|value| !value.is_empty()).map(digest),
            item: id.filter(|value| !value.is_empty()).map(digest),
            generated: display
                && id
                    .and_then(|id| id.strip_prefix("item-"))
                    .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())),
        }
    }

    pub(super) fn scoped(self, assistant: bool) -> Option<ScopedMessageId> {
        Some((assistant, self.turn?, self.item?))
    }

    fn compatible(self, other: Self) -> bool {
        self.turn.zip(other.turn).is_none_or(|(a, b)| a == b)
            && (self.generated
                || other.generated
                || self.item.zip(other.item).is_none_or(|(a, b)| a == b))
    }

    fn merge(&mut self, other: Self) {
        self.turn = self.turn.or(other.turn);
        if self.item.is_none() || (self.generated && other.item.is_some() && !other.generated) {
            self.item = other.item;
            self.generated = other.generated;
        }
    }
}

#[derive(Clone, Copy, Default)]
pub(super) struct MessageMatch<'a> {
    pub(super) alternate: Option<&'a str>,
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

#[derive(Default)]
pub(super) struct MessageMirrors {
    occurrences: BTreeMap<u64, Occurrence>,
    displayed: HashSet<u64>,
    // Each queue orders equal messages by their first occurrence in the turn.
    waiting: [BTreeSet<(MessageKey, u64)>; 3],
}

impl MessageMirrors {
    pub(super) fn clear(&mut self) {
        self.occurrences.clear();
        self.displayed.clear();
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
        let displayed = std::mem::take(&mut self.displayed);
        for sequence in displayed {
            self.remove(sequence);
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
            .range((key, 0)..=(key, u64::MAX))
            .map(|(_, seq)| *seq)
            .filter(|seq| self.occurrences[seq].identity.compatible(id));
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
        let candidate = |key: MessageKey| {
            self.waiting[wire]
                .range((key, 0)..=(key, u64::MAX))
                .map(|(_, sequence)| *sequence)
                .find(|seq| {
                    let other = &self.occurrences[seq];
                    other.identity.compatible(matching.identity)
                        && other
                            .attachment
                            .zip(matching.attachment)
                            .is_none_or(|(a, b)| a == b)
                })
        };
        // A matched attachment or citation distinguishes messages with identical visible text.
        let matched = keys[2].and_then(candidate).or_else(|| {
            keys[..2]
                .iter()
                .copied()
                .flatten()
                .filter_map(candidate)
                .min()
        });
        self.record(
            matched.unwrap_or(new_sequence),
            wire,
            keys,
            matching.identity,
        );
        matched
    }

    /// An update with a known item ID can change the text of an earlier occurrence.
    pub(super) fn update<'a>(
        &mut self,
        kind: &EventKind,
        text: &str,
        matching: impl Into<MessageMatch<'a>>,
        raw_type: &'static str,
        sequence: u64,
    ) {
        let matching = matching.into();
        self.record(
            sequence,
            encoding(raw_type),
            spellings(kind, text, matching),
            matching.identity,
        );
    }

    fn record(
        &mut self,
        sequence: u64,
        wire: usize,
        keys: [Option<MessageKey>; 4],
        identity: MessageIdentity,
    ) {
        if wire != encoding("response_item.message") {
            self.displayed.insert(sequence);
        }
        let occurrence = self.occurrences.entry(sequence).or_default();
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
    const OPEN: &str = "<oai-mem-citation>";
    const CLOSE: &str = "</oai-mem-citation>";
    let (visible, mut citation) = text.split_once(OPEN)?;
    let mut digest = Sha256::new();
    digest.update(visible);
    while let Some((_, rest)) = citation.split_once(CLOSE) {
        if let Some((visible, next)) = rest.split_once(OPEN) {
            digest.update(visible);
            citation = next;
        } else {
            digest.update(rest);
            break;
        }
    }
    Some(digest.finalize().into())
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
        mirrors.update(&MessageAssistant, "cd", None, DISPLAY, 0);
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
