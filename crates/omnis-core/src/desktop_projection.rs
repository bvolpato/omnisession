//! Indexed byte coverage for search results from a native Desktop database.

use std::ops::Range;

use omnis_ir::OmniEvent;
use uuid::Uuid;

use super::truncation_notice;

/// The range each verified Desktop row occupies after per-event truncation and before document truncation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DesktopProjection {
    rows: Vec<(Uuid, Range<usize>)>,
    edge_limit: usize,
}

impl DesktopProjection {
    pub(super) fn new(edge_limit: usize) -> Self {
        Self {
            rows: Vec::new(),
            edge_limit,
        }
    }

    pub(super) fn record(&mut self, event: &OmniEvent, bytes: Range<usize>) -> bool {
        if event.source.raw_record_type.as_deref() == Some("omnisession.antigravity_step_limit") {
            return true;
        }
        let Some(id) = event
            .raw_blob_hash
            .as_deref()
            .and_then(|hash| hash.parse().ok())
        else {
            return false;
        };
        self.rows.push((id, bytes));
        true
    }

    /// Checks whether the new indexed text retains each previously searchable row fragment.
    /// Both texts must come from the same per-event projection and native row identities.
    #[must_use]
    pub fn retains(&self, previous: &[Uuid], previous_text: &str, incoming_text: &str) -> bool {
        let mut pending = previous.iter().peekable();
        let mut old_size: usize = 0;
        let mut selected = Vec::new();
        for (id, current) in &self.rows {
            if pending.next_if_eq(&id).is_none() || current.is_empty() {
                continue;
            }
            let start = old_size + usize::from(old_size != 0) * 2;
            old_size = start + current.len();
            selected.push((start..old_size, current));
        }
        if pending.next().is_some() {
            return false;
        }
        let new_size = self.rows.last().map_or(0, |(_, range)| range.end);
        let (Some(old_edges), Some(new_edges)) = (
            Edges::of(previous_text, old_size, self.edge_limit),
            Edges::of(incoming_text, new_size, self.edge_limit),
        ) else {
            return false;
        };
        selected.into_iter().all(|(old, current)| {
            old_edges.0.iter().all(|edge| {
                let start = old.start.max(edge.source.start);
                let end = old.end.min(edge.source.end);
                if start >= end {
                    return true;
                }
                let old_range = edge.index(start)..edge.index(end);
                let new_start = current.start + (start - old.start);
                let new_end = current.start + (end - old.start);
                new_edges.0.iter().any(|new_edge| {
                    if new_start < new_edge.source.start || new_end > new_edge.source.end {
                        return false;
                    }
                    let indexed = new_edge.index(new_start)..new_edge.index(new_end);
                    let Some(fragment) = incoming_text.get(indexed.clone()) else {
                        return false;
                    };
                    previous_text.get(old_range.clone()) == Some(fragment)
                        && (new_start == current.start
                            || new_start == new_edge.source.start
                            || token_boundary(incoming_text, indexed.start))
                        && (new_end == current.end
                            || new_end == new_edge.source.end
                            || token_boundary(incoming_text, indexed.end))
                })
            })
        })
    }
}

// SQLite's unicode61 tokenizer treats ASCII punctuation and whitespace as separators.
// Keep an older clipped token when the new edge exposes more of the same word.
fn token_boundary(text: &str, at: usize) -> bool {
    let separator = |ch: char| ch.is_whitespace() || ch.is_ascii_punctuation();
    text[..at].chars().next_back().is_none_or(separator)
        || text[at..].chars().next().is_none_or(separator)
}

struct Edge {
    source: Range<usize>,
    indexed_start: usize,
}

impl Edge {
    fn index(&self, offset: usize) -> usize {
        self.indexed_start + (offset - self.source.start)
    }
}

struct Edges([Edge; 2]);

impl Edges {
    fn of(text: &str, total: usize, limit: usize) -> Option<Self> {
        let cap = limit.saturating_mul(2);
        if total <= cap {
            return (text.len() == total).then_some(Self([
                Edge {
                    source: 0..total,
                    indexed_start: 0,
                },
                Edge {
                    source: total..total,
                    indexed_start: total,
                },
            ]));
        }
        let notice = truncation_notice(cap);
        let available = cap - notice.len();
        let head_limit = available.div_ceil(2);
        let tail_limit = available / 2;
        // UTF-8 can shorten either edge by up to three bytes. Only the generated marker
        // at this boundary separates the two indexed ranges.
        let head = (head_limit.saturating_sub(3)..=head_limit)
            .find(|&i| text.get(i..).is_some_and(|s| s.starts_with(notice)))?;
        let tail_index = head + notice.len();
        let tail = text.len() - tail_index;
        if !(tail_limit.saturating_sub(3)..=tail_limit).contains(&tail) {
            return None;
        }
        Some(Self([
            Edge {
                source: 0..head,
                indexed_start: 0,
            },
            Edge {
                source: total - tail..total,
                indexed_start: tail_index,
            },
        ]))
    }
}
