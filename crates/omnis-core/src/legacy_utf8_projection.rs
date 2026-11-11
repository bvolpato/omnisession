//! Reproduces historical search text for local index upgrades.

use std::ops::Deref;

use super::{HeadTailText, SearchEventText};

/// An older search projection and the native-event boundaries needed to verify an append.
/// All text uses the same redaction and byte limits as the current search document.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LegacyUtf8Projection {
    text: String,
    source_bytes: usize,
    edge_limit: usize,
    first_event_start: usize,
    // Ends in the original redacted source and in the per-event bounded text, respectively.
    ends: Vec<(usize, usize)>,
}

impl Deref for LegacyUtf8Projection {
    type Target = str;

    fn deref(&self) -> &str {
        &self.text
    }
}

impl LegacyUtf8Projection {
    /// Verifies the prior text as the exact old rendering of the same rows or an intact event prefix.
    #[must_use]
    pub fn retains_legacy_utf8_prefix(
        &self,
        previous_source_bytes: usize,
        previous_text: &str,
        incoming_text: &str,
    ) -> bool {
        if previous_source_bytes == self.source_bytes {
            return previous_text == self.text;
        }
        // A document cap can move the indexed tail after an append. Only reconstruct prefixes
        // when the current document still includes the entire sequence of bounded event texts.
        let Some(&(_, total)) = self.ends.last() else {
            return false;
        };
        if total != incoming_text.len() {
            return false;
        }
        let Ok(last) = self
            .ends
            .binary_search_by_key(&previous_source_bytes, |&(source, _)| source)
        else {
            return false;
        };
        let Some(prefix) = incoming_text.get(..self.first_event_start) else {
            return false;
        };
        let mut old = HeadTailText::new(self.edge_limit);
        old.push_legacy(prefix);
        let mut start = self.first_event_start;
        for (i, &(_, end)) in self.ends[..=last].iter().enumerate() {
            if i > 0 {
                old.push_legacy("\n\n");
                start += 2;
            }
            let Some(event) = incoming_text.get(start..end) else {
                return false;
            };
            old.push_legacy(event);
            start = end;
        }
        previous_text == old.finish()
    }
}

pub(super) struct LegacyRenderer {
    stream: HeadTailText,
    first_event_start: usize,
    ends: Vec<(usize, usize)>,
}

pub(super) fn observe(
    legacy: &mut Option<LegacyRenderer>,
    current: &HeadTailText,
    separate: bool,
    source_bytes: usize,
    event: &SearchEventText,
) {
    let separator = if separate { "\n\n" } else { "" };
    let start = current.total_byte_count + separator.len();
    if legacy.is_none()
        && let Some(split) = current.edge_limit.checked_sub(start)
        && split < event.text.len()
        && !event.text.is_char_boundary(split)
    {
        // The outputs are identical up to the first character that straddles the head budget.
        *legacy = Some(LegacyRenderer {
            stream: current.clone(),
            first_event_start: start,
            ends: Vec::new(),
        });
    }
    if let Some(legacy) = legacy {
        legacy.stream.push_legacy(separator);
        legacy.stream.push_legacy(&event.text);
        if legacy.stream.truncated() {
            legacy.ends.clear();
        } else {
            let source_end = source_bytes + separator.len() + event.source_byte_count;
            legacy
                .ends
                .push((source_end, legacy.stream.total_byte_count));
        }
    }
}

impl LegacyRenderer {
    pub(super) fn finish(
        self,
        current_text: &str,
        source_bytes: usize,
    ) -> Option<LegacyUtf8Projection> {
        let edge_limit = self.stream.edge_limit;
        let text = self.stream.finish();
        if text == current_text && self.ends.len() < 2 {
            return None;
        }
        Some(LegacyUtf8Projection {
            text,
            source_bytes,
            edge_limit,
            first_event_start: self.first_event_start,
            ends: self.ends,
        })
    }
}
