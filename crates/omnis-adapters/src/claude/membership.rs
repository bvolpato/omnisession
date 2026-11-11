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

    fn tree_distance(&self, ancestor: usize, mut descendant: usize) -> Option<usize> {
        let depth = self.links.get(ancestor)?.depth;
        let distance = self.links.get(descendant)?.depth.checked_sub(depth)?;
        while self.links[descendant].depth > depth {
            let link = self.links[descendant];
            descendant = if self.links[link.jump].depth >= depth {
                link.jump
            } else {
                link.parent
            };
        }
        (descendant == ancestor).then_some(distance)
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
