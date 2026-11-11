//! Match the model, legacy display, and completed-item copies of a turn's visible messages.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use omnis_ir::EventKind;
use sha2::{Digest, Sha256};

#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
struct MessageKey {
    assistant: bool,
    digest: [u8; 32],
}

impl MessageKey {
    fn new(kind: &EventKind, text: &str) -> Self {
        Self {
            assistant: *kind == EventKind::MessageAssistant,
            digest: Sha256::digest(text.as_bytes()).into(),
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

    /// Match each encoding once per occurrence. Multipart encodings may use different separators.
    pub(super) fn observe(
        &mut self,
        kind: &EventKind,
        text: &str,
        alternate: Option<&str>,
        raw_type: &'static str,
        new_sequence: u64,
    ) -> Option<u64> {
        let wire = encoding(raw_type);
        let keys = spellings(kind, text, alternate);
        let matched = keys
            .iter()
            .flatten()
            .filter_map(|&key| {
                self.waiting[wire]
                    .range((key, 0)..=(key, u64::MAX))
                    .next()
                    .map(|(_, sequence)| *sequence)
            })
            .min();
        self.record(matched.unwrap_or(new_sequence), wire, keys);
        matched
    }

    /// An update with a known item ID can change the text of an earlier occurrence.
    pub(super) fn update(
        &mut self,
        kind: &EventKind,
        text: &str,
        alternate: Option<&str>,
        raw_type: &'static str,
        sequence: u64,
    ) {
        self.record(
            sequence,
            encoding(raw_type),
            spellings(kind, text, alternate),
        );
    }

    fn record(&mut self, sequence: u64, wire: usize, keys: [Option<MessageKey>; 2]) {
        if wire != encoding("response_item.message") {
            self.displayed.insert(sequence);
        }
        let occurrence = self.occurrences.entry(sequence).or_default();
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

fn spellings(kind: &EventKind, text: &str, alternate: Option<&str>) -> [Option<MessageKey>; 2] {
    [
        Some(MessageKey::new(kind, text)),
        alternate
            .filter(|other| *other != text)
            .map(|other| MessageKey::new(kind, other)),
    ]
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
