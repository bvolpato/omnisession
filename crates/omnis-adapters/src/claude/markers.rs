//! Find retained attachments and progress records on an original parent chain.
//! Each link points to a parent that existed at arrival. Later children preserve
//! the tree order used by existing retained lists.

use super::{ClaudeRecord, membership::Paths};
use std::collections::HashMap;
use uuid::Uuid;

#[derive(Default)]
pub(super) struct MarkerPaths {
    paths: Paths,
    rows: Vec<MarkerRow>,
    // Each bucket contains twice as many replacements as the one after it.
    replacements: Vec<(Vec<usize>, Markers)>,
}

struct MarkerRow {
    id: Uuid,
    // The shallowest transparent ancestor sharing the same physical source.
    first_depth: Option<usize>,
}

#[derive(Default)]
pub(super) enum Markers {
    #[default]
    None,
    Single(usize),
    Along(Vec<usize>),
    Listed(Vec<Visit>),
    Segment {
        tail: usize,
        head_depth: usize,
    },
    Unindexed,
}

#[derive(Debug)]
pub(super) struct Unindexed;

pub(super) struct MarkerQuery {
    endpoint: usize,
    first_depth: usize,
    pub(super) duplicate: Option<Uuid>,
}

pub(super) struct Visit {
    at: (usize, bool),
    owner: Option<usize>,
}

impl MarkerPaths {
    pub(super) fn start(
        &mut self,
        records: &HashMap<Uuid, ClaudeRecord>,
        originals: &HashMap<Uuid, ClaudeRecord>,
    ) {
        if self.paths.active() {
            return;
        }
        let first = |id: &Uuid| originals.get(id).or_else(|| records.get(id));
        let mut rows = vec![None; records.len()];
        for id in records.keys() {
            let record = first(id).expect("record exists");
            rows[record.order] = Some((*id, record));
        }
        for row in rows {
            let (id, record) = row.expect("first record order is contiguous");
            self.push(id, record, record.parent.and_then(|id| first(&id)));
        }
        for original in originals.values() {
            self.record_replacement(original.order);
        }
    }

    pub(super) fn active(&self) -> bool {
        self.paths.active()
    }

    pub(super) fn push(
        &mut self,
        id: Uuid,
        record: &ClaudeRecord,
        raw_parent: Option<&ClaudeRecord>,
    ) {
        let parent = raw_parent.filter(|p| p.order < record.order);
        self.paths.push(parent.map(|p| p.order));
        let first_depth = if !record.branch.transparent
            || record.branch.missing_ancestor
            || (record.parent.is_some() && parent.is_none())
        {
            None
        } else if let Some(p) =
            parent.filter(|p| p.branch.transparent && p.branch.source == record.branch.source)
        {
            self.rows[p.order].first_depth
        } else {
            Some(self.paths.depth(record.order))
        };
        self.rows.push(MarkerRow { id, first_depth });
    }

    pub(super) fn segment(&self, head: usize, tail: usize, len: usize) -> Markers {
        if self.paths.distance(head, tail) == Some(len - 1) {
            Markers::Segment {
                tail,
                head_depth: self.paths.depth(head),
            }
        } else {
            // The raw chain used a parent that was absent when the child arrived.
            Markers::Unindexed
        }
    }

    pub(super) fn listed(&self, ids: &[usize]) -> Markers {
        match ids {
            [] => return Markers::None,
            [one] => return Markers::Single(*one),
            _ => {}
        }
        let tail = *ids
            .iter()
            .max_by_key(|&&id| self.paths.depth(id))
            .expect("nonempty marker list");
        if ids
            .iter()
            .all(|&id| self.paths.distance(id, tail).is_some())
        {
            let mut chain = ids.to_vec();
            chain.sort_unstable_by_key(|&id| self.paths.depth(id));
            let head = chain[0];
            if self.paths.depth(tail) - self.paths.depth(head) + 1 == chain.len() {
                return Markers::Segment {
                    tail,
                    head_depth: self.paths.depth(head),
                };
            }
            return Markers::Along(chain);
        }
        // Most provider lists already follow tree order. Sort the entries, then emit
        // each exit when the next marked path leaves its subtree.
        let mut nodes = ids.to_vec();
        nodes.sort_by(|&a, &b| self.paths.walk_order((a, false), (b, false)));
        let mut stack = Vec::new();
        let mut visits = Vec::with_capacity(2 * nodes.len());
        for node in nodes {
            while let Some(&parent) = stack.last() {
                if self.paths.distance(parent, node).is_some() {
                    break;
                }
                stack.pop();
                visits.push(Visit {
                    at: (parent, true),
                    owner: stack.last().copied(),
                });
            }
            visits.push(Visit {
                at: (node, false),
                owner: Some(node),
            });
            stack.push(node);
        }
        while let Some(node) = stack.pop() {
            visits.push(Visit {
                at: (node, true),
                owner: stack.last().copied(),
            });
        }
        Markers::Listed(visits)
    }

    pub(super) fn record_replacement(&mut self, order: usize) {
        if !self.active() {
            return;
        }
        let mut ids = vec![order];
        while self
            .replacements
            .last()
            .is_some_and(|(last, _)| last.len() == ids.len())
        {
            let (last, _) = self.replacements.pop().expect("bucket exists");
            ids.extend(last);
        }
        let marks = self.listed(&ids);
        self.replacements.push((ids, marks));
    }

    pub(super) fn query(&self, endpoint: usize) -> Result<MarkerQuery, Unindexed> {
        let mut first_depth = self
            .rows
            .get(endpoint)
            .and_then(|row| row.first_depth)
            .ok_or(Unindexed)?;
        // A replaced boundary can become transparent. Check the first record outside the original run too.
        let last_depth = first_depth.saturating_sub(1);
        let mut boundary = None;
        for (_, markers) in &self.replacements {
            if let Some(node) = self.closest(markers, endpoint)?
                && self.paths.depth(node) >= last_depth
                && boundary
                    .is_none_or(|previous| self.paths.depth(previous) < self.paths.depth(node))
            {
                boundary = Some(node);
            }
        }
        if let Some(node) = boundary {
            first_depth = first_depth.max(self.paths.depth(node) + 1);
        }
        Ok(MarkerQuery {
            endpoint,
            first_depth,
            duplicate: boundary.map(|i| self.rows[i].id),
        })
    }

    pub(super) fn nearest(
        &self,
        markers: &Markers,
        query: &MarkerQuery,
    ) -> Result<Option<Uuid>, Unindexed> {
        Ok(self
            .closest(markers, query.endpoint)?
            .filter(|&n| self.paths.depth(n) >= query.first_depth)
            .map(|n| self.rows[n].id))
    }

    fn closest(&self, markers: &Markers, endpoint: usize) -> Result<Option<usize>, Unindexed> {
        let candidate = match markers {
            Markers::None => None,
            Markers::Single(one) => self.paths.distance(*one, endpoint).map(|_| *one),
            Markers::Along(chain) => self
                .paths
                .common_ancestor(*chain.last().expect("nonempty marker chain"), endpoint)
                .and_then(|common| {
                    let end = chain
                        .partition_point(|&id| self.paths.depth(id) <= self.paths.depth(common));
                    end.checked_sub(1).map(|i| chain[i])
                }),
            Markers::Listed(visits) => {
                let end = visits
                    .partition_point(|v| !self.paths.walk_order(v.at, (endpoint, false)).is_gt());
                end.checked_sub(1).and_then(|i| visits[i].owner)
            }
            Markers::Segment { tail, head_depth } => self
                .paths
                .common_ancestor(*tail, endpoint)
                .filter(|&n| self.paths.depth(n) >= *head_depth),
            Markers::Unindexed => return Err(Unindexed),
        };
        Ok(candidate)
    }
}

#[cfg(test)]
mod tests {
    use super::{MarkerPaths, MarkerRow, Markers};
    use uuid::Uuid;

    #[test]
    fn indexed_cutoffs_match_parent_walks_on_branches_and_partial_runs() {
        let parents = [
            None,
            Some(0),
            Some(1),
            Some(2),
            Some(1),
            Some(4),
            Some(2),
            None,
            Some(7),
            Some(8),
        ];
        let mut raw = MarkerPaths::default();
        let uuid = |r: usize| Uuid::from_u128(r as u128 + 1);
        for (i, &parent) in parents.iter().enumerate() {
            raw.paths.push(parent);
            let first = if i == 2 || i == 8 {
                raw.paths.depth(i)
            } else {
                parent.and_then(|p| raw.rows[p].first_depth).unwrap_or(0)
            };
            raw.rows.push(MarkerRow {
                id: uuid(i),
                first_depth: Some(first),
            });
        }
        let check = |markers: &Markers, inside: &[usize]| {
            for endpoint in 0..parents.len() {
                let mut cursor = Some(endpoint);
                let mut expected = None;
                while let Some(i) = cursor {
                    if raw.paths.depth(i) < raw.rows[endpoint].first_depth.unwrap() {
                        break;
                    }
                    if inside.contains(&i) {
                        expected = Some(uuid(i));
                        break;
                    }
                    cursor = parents[i];
                }
                assert_eq!(
                    raw.nearest(markers, &raw.query(endpoint).unwrap()).unwrap(),
                    expected,
                    "{inside:?} at {endpoint}"
                );
            }
        };
        for mask in 0..(1 << parents.len()) {
            let inside: Vec<_> = (0..parents.len())
                .filter(|i| mask & (1 << i) != 0)
                .rev()
                .collect();
            check(&raw.listed(&inside), &inside);
        }
        for tail in 0..parents.len() {
            let mut inside = Vec::new();
            let mut head = Some(tail);
            while let Some(i) = head {
                inside.push(i);
                check(&raw.segment(i, tail, inside.len()), &inside);
                head = parents[i];
            }
        }
        let mut changed = Vec::new();
        let all = raw.listed(&(0..parents.len()).collect::<Vec<_>>());
        for replacement in [3, 2, 8, 7, 1] {
            changed.push(replacement);
            raw.record_replacement(replacement);
            for endpoint in 0..parents.len() {
                let query = raw.query(endpoint).unwrap();
                let stop = raw.rows[endpoint].first_depth.unwrap().saturating_sub(1);
                let duplicate = std::iter::successors(Some(endpoint), |&node| parents[node])
                    .take_while(|&node| raw.paths.depth(node) >= stop)
                    .find(|node| changed.contains(node));
                assert_eq!(query.duplicate, duplicate.map(uuid));
                let nearest = (duplicate != Some(endpoint)).then_some(uuid(endpoint));
                assert_eq!(raw.nearest(&all, &query).unwrap(), nearest);
            }
        }
    }
}
