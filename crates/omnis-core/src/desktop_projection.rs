//! Indexed byte coverage for native Antigravity database and IDE log searches.

use std::ops::Range;

use omnis_ir::OmniEvent;
use uuid::Uuid;

use super::truncation_notice;

/// Event boundaries after redaction and per-event truncation, before document truncation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DesktopProjection {
    rows: Rows,
    edge_limit: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Rows {
    Database(Vec<(Uuid, Range<usize>)>),
    IdeLog(Vec<(usize, usize)>),
}

impl DesktopProjection {
    pub(super) fn new(edge_limit: usize) -> Self {
        Self {
            rows: Rows::Database(Vec::new()),
            edge_limit,
        }
    }

    pub(super) fn ide_log(edge_limit: usize) -> Self {
        Self {
            rows: Rows::IdeLog(Vec::new()),
            edge_limit,
        }
    }

    /// Checks whether the prior indexed document ends at an observed IDE message boundary.
    #[must_use]
    pub fn retains_ide_log_prefix(
        &self,
        previous_bytes: usize,
        previous_text: &str,
        incoming_text: &str,
    ) -> bool {
        let Rows::IdeLog(ends) = &self.rows else {
            return false;
        };
        let Some(&(total_source, total_projected)) = ends.last() else {
            return false;
        };
        if previous_bytes >= total_source {
            return false;
        }
        let old_end = if previous_bytes == 0 {
            0
        } else {
            let Ok(row) = ends.binary_search_by_key(&previous_bytes, |&(source, _)| source) else {
                return false;
            };
            ends[row].1
        };
        // A shifted document tail may displace old hits. Only a fully retained prefix
        // can prove an append without replaying those previously indexed tail bytes.
        let Some(edges) = Edges::of(incoming_text, total_projected, self.edge_limit) else {
            return false;
        };
        old_end <= edges.0[0].source.end && incoming_text.get(..old_end) == Some(previous_text)
    }

    pub(super) fn record(
        &mut self,
        event: &OmniEvent,
        bytes: Range<usize>,
        source_end: usize,
    ) -> bool {
        let rows = match &mut self.rows {
            Rows::Database(rows) => rows,
            Rows::IdeLog(ends) => {
                if !bytes.is_empty() {
                    ends.push((source_end, bytes.end));
                }
                return true;
            }
        };
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
        rows.push((id, bytes));
        true
    }

    /// Checks that the previous rows survive in order and every additional row contributes no text.
    #[must_use]
    pub fn same_visible_rows(&self, previous: &[Uuid]) -> bool {
        let Rows::Database(rows) = &self.rows else {
            return false;
        };
        let mut pending = previous.iter().peekable();
        rows.iter()
            .all(|(id, range)| pending.next_if_eq(&id).is_some() || range.is_empty())
            && pending.next().is_none()
    }

    /// Checks whether the new indexed text retains each previously searchable row fragment.
    /// Both texts must come from the same per-event projection and native row identities.
    #[must_use]
    pub fn retains(&self, previous: &[Uuid], previous_text: &str, incoming_text: &str) -> bool {
        let Rows::Database(rows) = &self.rows else {
            return false;
        };
        let mut pending = previous.iter().peekable();
        let mut old_size: usize = 0;
        let mut selected = Vec::new();
        for (id, current) in rows {
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
        let new_size = rows.last().map_or(0, |(_, range)| range.end);
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

#[cfg(test)]
mod tests {
    use super::{DesktopProjection, Rows};

    #[test]
    fn ide_log_append_requires_the_complete_old_projection_at_a_message_boundary() {
        let old = "first\n\nsecond";
        let new = format!("{old}\n\n{}", "later ".repeat(30));
        let layout = DesktopProjection {
            rows: Rows::IdeLog(vec![(old.len(), old.len()), (new.len(), new.len())]),
            edge_limit: 64,
        };
        let indexed = crate::head_tail_text(&new, 64).0;
        assert!(layout.retains_ide_log_prefix(old.len(), old, &indexed));
        assert!(!layout.retains_ide_log_prefix(5, "first", &indexed));
        assert!(!layout.retains_ide_log_prefix(old.len(), "first\n\nabsent", &indexed));
        let longer = "before ".repeat(15);
        let new = format!("{longer}\n\n{}", "later ".repeat(30));
        let layout = DesktopProjection {
            rows: Rows::IdeLog(vec![(longer.len(), longer.len()), (new.len(), new.len())]),
            edge_limit: 64,
        };
        assert!(!layout.retains_ide_log_prefix(
            longer.len(),
            &longer,
            &crate::head_tail_text(&new, 64).0
        ));
    }
}
