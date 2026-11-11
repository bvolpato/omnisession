//! Reproduces historical search text for local index upgrades.

use super::{HeadTailText, SearchEventText, utf8_prefix_boundary};

/// Evidence for comparing an older local index with the current bounded, redacted search text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LegacyUtf8Projection {
    historical: Option<String>,
    source_bytes: usize,
    edge_limit: usize,
    // Ends in the source before per-event truncation and in the bounded event stream, respectively.
    ends: Vec<(usize, usize)>,
}

impl LegacyUtf8Projection {
    /// Returns the historical rendering when its bytes differ from the current rendering.
    #[must_use]
    pub fn historical_text(&self) -> Option<&str> {
        self.historical.as_deref()
    }

    /// Verifies the exact old rendering of the same visible rows or an ordered subset.
    #[must_use]
    pub fn retains_legacy_utf8_rows(
        &self,
        previous_source_bytes: usize,
        previous_text: &str,
        incoming_text: &str,
    ) -> bool {
        if previous_source_bytes == self.source_bytes {
            return self.historical_text() == Some(previous_text);
        }
        // With a document cap, the new tail may have displaced previously indexed rows.
        if previous_source_bytes > self.source_bytes
            || self
                .ends
                .last()
                .is_none_or(|&(_, total)| total != incoming_text.len())
        {
            return false;
        }
        // The crossing character can leave at most three head bytes for later chunks.
        // Removing a visible event and its separator saves at least three bytes, so the
        // shorter history fits in the original tail even if the head stopped early.
        (self.edge_limit.saturating_sub(3)..=self.edge_limit).any(|head_len| {
            let Some((head, tail)) = previous_text.split_at_checked(head_len) else {
                return false;
            };
            let mut remaining = LegacyMatch {
                head,
                tail,
                space: self.edge_limit,
            };
            let mut corrected = Some(previous_text);
            let mut source_bytes = 0;
            let mut before_source = 0;
            let mut before_end = 0;
            for &(after_source, after_end) in &self.ends {
                let separator_bytes = if before_end == 0 { 0 } else { 2 };
                let Some(event) = incoming_text.get(before_end + separator_bytes..after_end) else {
                    return false;
                };
                let event_bytes = after_source - before_source - separator_bytes;
                before_end = after_end;
                before_source = after_source;
                let separator = if source_bytes == 0 { "" } else { "\n\n" };
                let next_bytes = source_bytes + separator.len() + event_bytes;
                if next_bytes > previous_source_bytes {
                    continue;
                }
                if let Some(next) = remaining
                    .take(separator)
                    .and_then(|state| state.take(event))
                    && (next_bytes == previous_source_bytes || next.take("\n\n").is_some())
                {
                    remaining = next;
                    source_bytes = next_bytes;
                    corrected =
                        corrected.and_then(|s| s.strip_prefix(separator)?.strip_prefix(event));
                }
            }
            source_bytes == previous_source_bytes
                && remaining.head.is_empty()
                && remaining.tail.is_empty()
                && corrected != Some("")
        })
    }
}

// Compare a whole native chunk against both strings the historical renderer assembled.
#[derive(Clone, Copy)]
struct LegacyMatch<'a> {
    head: &'a str,
    tail: &'a str,
    space: usize,
}

impl LegacyMatch<'_> {
    fn take(self, chunk: &str) -> Option<Self> {
        let split = utf8_prefix_boundary(chunk, self.space);
        Some(Self {
            head: self.head.strip_prefix(&chunk[..split])?,
            tail: self.tail.strip_prefix(&chunk[split..])?,
            space: self.space - split,
        })
    }
}

pub(super) struct LegacyRenderer {
    stream: Option<HeadTailText>,
    ends: Option<Vec<(usize, usize)>>,
}

impl LegacyRenderer {
    pub(super) fn new(compare_partial_rows: bool) -> Self {
        Self {
            stream: None,
            ends: compare_partial_rows.then(Vec::new),
        }
    }

    pub(super) fn observe(
        &mut self,
        current: &HeadTailText,
        separate: bool,
        source_bytes: usize,
        event: &SearchEventText,
    ) {
        let separator = if separate { "\n\n" } else { "" };
        let start = current.total_byte_count + separator.len();
        if self.stream.is_none()
            && let Some(split) = current.edge_limit.checked_sub(start)
            && split < event.text.len()
            && !event.text.is_char_boundary(split)
        {
            // Both renderers produce the same bytes until a UTF-8 character crosses the head boundary.
            self.stream = Some(current.clone());
        }
        if let Some(stream) = &mut self.stream {
            stream.push_legacy(separator);
            stream.push_legacy(&event.text);
        }
        let end = start + event.text.len();
        if end > current.edge_limit.saturating_mul(2) {
            self.ends = None;
        } else if let Some(ends) = &mut self.ends {
            ends.push((
                source_bytes + separator.len() + event.source_byte_count,
                end,
            ));
        }
    }

    pub(super) fn finish(
        self,
        current_text: &str,
        source_bytes: usize,
        edge_limit: usize,
    ) -> Option<LegacyUtf8Projection> {
        let historical = self
            .stream
            .map(HeadTailText::finish)
            .filter(|text| text != current_text);
        let ends = self
            .ends
            .filter(|_| current_text.len() > edge_limit && !current_text.is_ascii())
            .unwrap_or_default();
        if historical.is_none() && ends.is_empty() {
            return None;
        }
        Some(LegacyUtf8Projection {
            historical,
            source_bytes,
            edge_limit,
            ends,
        })
    }
}
