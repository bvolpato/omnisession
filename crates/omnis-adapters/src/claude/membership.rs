//! Membership in original Claude parent chains. Each UUID needs one ancestor jump,
//! even when many compact boundaries retain overlapping parts of the same chain.

#[derive(Clone, Copy, Default)]
struct Link {
    root: usize,
    parent: usize,
    jump: usize,
    depth: usize,
}

#[derive(Default)]
pub(super) struct Paths {
    links: Vec<Link>,
    cycles: std::collections::HashMap<usize, usize>,
}

impl Paths {
    pub(super) fn len(&self) -> usize {
        self.links.len()
    }

    pub(super) fn active(&self) -> bool {
        !self.links.is_empty()
    }

    fn set(&mut self, i: usize, parent: Option<usize>) {
        self.links[i] = parent.map_or(
            Link {
                root: i,
                parent: i,
                jump: i,
                depth: 0,
            },
            |p| {
                let first = self.links[p].jump;
                let second = self.links[first].jump;
                // Two equally long jumps followed by a parent edge make the next jump.
                let jump = if self.links[p].depth - self.links[first].depth
                    == self.links[first].depth - self.links[second].depth
                {
                    second
                } else {
                    p
                };
                Link {
                    root: self.links[p].root,
                    parent: p,
                    jump,
                    depth: self.links[p].depth + 1,
                }
            },
        );
    }

    pub(super) fn push(&mut self, parent: Option<usize>) {
        let index = self.links.len();
        self.links.push(Link::default());
        self.set(index, parent);
    }

    pub(super) fn rebuild(&mut self, parents: &[Option<usize>]) {
        self.links.resize(parents.len(), Link::default());
        self.cycles.clear();
        let mut state = vec![0u8; parents.len()];
        let mut path = Vec::new();
        for start in 0..parents.len() {
            let mut next = Some(start);
            while let Some(i) = next.filter(|&i| state[i] == 0) {
                state[i] = 1;
                path.push(i);
                next = parents[i];
            }
            // A cycle has one extra edge from the root of this tree back into its path.
            if let (Some(&last), Some(next)) = (path.last(), next.filter(|&i| state[i] == 1)) {
                self.cycles.insert(last, next);
            }
            let mut parent = next.filter(|&i| state[i] == 2);
            while let Some(i) = path.pop() {
                self.set(i, parent);
                state[i] = 2;
                parent = Some(i);
            }
        }
    }

    pub(super) fn distance(&self, ancestor: usize, descendant: usize) -> Option<usize> {
        let link = self.links.get(descendant)?;
        if self.links.get(ancestor)?.root != link.root {
            return None;
        }
        self.tree_distance(ancestor, descendant).or_else(|| {
            let cycle = *self.cycles.get(&link.root)?;
            Some(link.depth + 1 + self.tree_distance(ancestor, cycle)?)
        })
    }

    pub(super) fn depth(&self, node: usize) -> usize {
        self.links[node].depth
    }

    pub(super) fn root(&self, node: usize) -> usize {
        self.links[node].root
    }

    fn at_depth(&self, mut node: usize, depth: usize) -> usize {
        while self.links[node].depth > depth {
            let link = self.links[node];
            node = if self.links[link.jump].depth >= depth {
                link.jump
            } else {
                link.parent
            };
        }
        node
    }

    fn tree_distance(&self, ancestor: usize, descendant: usize) -> Option<usize> {
        let depth = self.links.get(ancestor)?.depth;
        let distance = self.links.get(descendant)?.depth.checked_sub(depth)?;
        (self.at_depth(descendant, depth) == ancestor).then_some(distance)
    }

    pub(super) fn common_ancestor(&self, left: usize, right: usize) -> Option<usize> {
        if self.links[left].root != self.links[right].root {
            return None;
        }
        let depth = self.depth(left).min(self.depth(right));
        let mut left = self.at_depth(left, depth);
        let mut right = self.at_depth(right, depth);
        while left != right {
            let (a, b) = (self.links[left], self.links[right]);
            (left, right) = if a.jump == b.jump {
                (a.parent, b.parent)
            } else {
                (a.jump, b.jump)
            };
        }
        Some(left)
    }

    // Enter and leave positions stay in the same order when later children are appended.
    pub(super) fn walk_order(
        &self,
        left: (usize, bool),
        right: (usize, bool),
    ) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        let ((a, leave_a), (b, leave_b)) = (left, right);
        if a == b {
            return leave_a.cmp(&leave_b);
        }
        match self.common_ancestor(a, b) {
            None => self.links[a].root.cmp(&self.links[b].root),
            Some(ancestor) if ancestor == a => {
                if leave_a {
                    Ordering::Greater
                } else {
                    Ordering::Less
                }
            }
            Some(ancestor) if ancestor == b => {
                if leave_b {
                    Ordering::Less
                } else {
                    Ordering::Greater
                }
            }
            Some(ancestor) => {
                let depth = self.depth(ancestor) + 1;
                self.at_depth(a, depth).cmp(&self.at_depth(b, depth))
            }
        }
    }
}

pub(super) enum Members {
    None,
    Listed(Vec<usize>),
    Range(std::ops::RangeInclusive<usize>),
    Segment { tail: usize, len: usize },
}

impl Members {
    pub(super) fn listed(mut ids: Vec<usize>) -> Self {
        ids.sort_unstable();
        ids.dedup();
        match (ids.first(), ids.last()) {
            (Some(&first), Some(&last)) if last - first + 1 == ids.len() => {
                Self::Range(first..=last)
            }
            _ => Self::Listed(ids),
        }
    }

    pub(super) fn contains(&self, index: usize, paths: &Paths) -> bool {
        match self {
            Self::Listed(ids) => ids.binary_search(&index).is_ok(),
            Self::Range(range) => range.contains(&index),
            Self::Segment { tail, len } => paths.distance(index, *tail).is_some_and(|d| d < *len),
            Self::None => false,
        }
    }
}

/// Finds the most recently created descendant of a copied message. Children can
/// arrive on older branches, so the newest leaf need not be under the last direct child.
#[derive(Default)]
pub(super) struct Descendants {
    indexed: usize,
    blocks: Vec<NodeBlock>,
}

// The leaves are in tree order. Each internal entry is the newest record under it.
struct NodeBlock {
    newest: Vec<(usize, usize)>,
}

impl NodeBlock {
    fn new(
        first: usize,
        len: usize,
        paths: &Paths,
        source_order: &impl Fn(usize) -> usize,
    ) -> Self {
        let mut newest = vec![(0, 0); len];
        newest.extend((first..first + len).map(|node| (source_order(node), node)));
        newest[len..].sort_unstable_by(|&(_, a), &(_, b)| paths.walk_order((a, false), (b, false)));
        for i in (1..len).rev() {
            newest[i] = newest[2 * i].max(newest[2 * i + 1]);
        }
        Self { newest }
    }

    fn len(&self) -> usize {
        self.newest.len() / 2
    }

    fn last_in(&self, node: usize, paths: &Paths) -> Option<(usize, usize)> {
        let len = self.len();
        let leaves = &self.newest[len..];
        let boundary = |leave| {
            len + leaves
                .partition_point(|&(_, i)| paths.walk_order((i, false), (node, leave)).is_lt())
        };
        let (mut start, mut end) = (boundary(false), boundary(true));
        if start == end {
            return None;
        }
        let mut newest = (0, 0);
        while start < end {
            if !start.is_multiple_of(2) {
                newest = newest.max(self.newest[start]);
                start += 1;
            }
            if !end.is_multiple_of(2) {
                end -= 1;
                newest = newest.max(self.newest[end]);
            }
            start /= 2;
            end /= 2;
        }
        Some(newest)
    }
}

impl Descendants {
    pub(super) fn latest(
        &mut self,
        node: usize,
        paths: &Paths,
        source_order: impl Fn(usize) -> usize,
    ) -> usize {
        if self.indexed != paths.len() {
            let mut first = 0;
            let mut index = 0;
            while first < paths.len() {
                let len = 1 << (paths.len() - first).ilog2();
                // Full chronological blocks retain their tree order as later nodes arrive.
                if self.blocks.get(index).is_none_or(|b| b.len() != len) {
                    self.blocks.truncate(index);
                    self.blocks
                        .push(NodeBlock::new(first, len, paths, &source_order));
                }
                first += len;
                index += 1;
            }
            self.indexed = paths.len();
        }
        self.blocks
            .iter()
            .filter_map(|block| block.last_in(node, paths))
            .max()
            .map_or(node, |(_, index)| index)
    }
}

#[cfg(test)]
mod tests {
    use super::Paths;

    #[test]
    fn indexed_parent_distances_match_original_chains() {
        // Includes a fork, out-of-order parents, a disconnected root and a cycle.
        let parents = [
            None,
            Some(6),
            Some(1),
            Some(1),
            Some(5),
            Some(4),
            Some(0),
            None,
            Some(5),
        ];
        let mut index = Paths::default();
        index.rebuild(&parents);
        for start in 0..parents.len() {
            let mut distances = vec![None; parents.len()];
            let mut current = Some(start);
            let mut steps = 0;
            while let Some(i) = current {
                if distances[i].is_some() {
                    break;
                }
                distances[i] = Some(steps);
                current = parents[i];
                steps += 1;
            }
            for (ancestor, expected) in distances.iter().enumerate() {
                assert_eq!(index.distance(ancestor, start), *expected);
            }
        }
        index.push(Some(2));
        assert_eq!(index.distance(0, parents.len()), Some(4));
    }
}
