use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use super::{MAX_CANONICAL_EVENTS, TurnAliases, Withdrawal};

#[derive(Clone, Copy, PartialEq, Eq)]
enum TurnState {
    Present,
    Withdrawn,
    Unverified,
    Mixed,
}

impl TurnState {
    fn merge(self, other: Self) -> Self {
        if self == other {
            self
        } else if self == Self::Unverified || other == Self::Unverified {
            Self::Unverified
        } else {
            Self::Mixed
        }
    }
}

/// A paginated rollback keeps the display, including the context of a withdrawn request.
pub(super) struct PaginatedWorkspace {
    before: Option<PathBuf>,
    contexts: VecDeque<(Option<String>, PathBuf, usize)>,
    first_sequence: u64,
}

impl PaginatedWorkspace {
    pub(super) fn new(before: Option<PathBuf>) -> Self {
        Self {
            before,
            contexts: VecDeque::new(),
            first_sequence: 0,
        }
    }

    pub(super) fn new_segment(&mut self, sequence: u64) {
        self.contexts.clear();
        self.before = None;
        self.first_sequence = sequence;
    }

    pub(super) fn observe(&mut self, turn: Option<&str>, directory: &Path, position: usize) {
        if self
            .contexts
            .back()
            .is_some_and(|(previous, path, _)| previous.as_deref() == turn && path == directory)
        {
            return;
        }
        if self.contexts.len() == MAX_CANONICAL_EVENTS {
            self.contexts.pop_front();
            self.before = None;
        }
        self.contexts
            .push_back((turn.map(str::to_owned), directory.to_owned(), position));
    }

    pub(super) fn surviving<'a>(
        &'a self,
        aliases: &'a TurnAliases,
        paginated_aliases: &HashMap<[u8; 32], [u8; 32]>,
        requests: impl Iterator<Item = (u64, Option<&'a str>, Option<Withdrawal>)>,
    ) -> Option<(PathBuf, usize)> {
        // A turn can contain several requests. Withdrawing a steer leaves its primary request.
        let mut keys = HashMap::new();
        let mut turn_key = |id: &'a str| {
            let id = aliases.resolve(id);
            *keys.entry(id).or_insert_with(|| {
                let key: [u8; 32] = Sha256::digest(id.as_bytes()).into();
                paginated_aliases.get(&key).copied().unwrap_or(key)
            })
        };
        let mut turns = HashMap::<[u8; 32], TurnState>::new();
        for (sequence, turn, status) in requests {
            if sequence < self.first_sequence {
                continue;
            }
            if let Some(turn) = turn {
                let state = match status {
                    None => TurnState::Present,
                    Some(Withdrawal::Confirmed) => TurnState::Withdrawn,
                    Some(Withdrawal::Unverified | Withdrawal::OutsideCheckpoint) => {
                        TurnState::Unverified
                    }
                };
                turns
                    .entry(turn_key(turn))
                    .and_modify(|prior| *prior = prior.merge(state))
                    .or_insert(state);
            }
        }
        let mut mixed_dirs = HashMap::new();
        for (turn, directory, _) in &self.contexts {
            if let Some(turn) = turn.as_deref().map(&mut turn_key)
                && turns.get(&turn) == Some(&TurnState::Mixed)
            {
                let path = mixed_dirs.entry(turn).or_insert(Some(directory));
                if *path != Some(directory) {
                    *path = None;
                }
            }
        }
        let mut possible = None;
        for (turn, directory, position) in self.contexts.iter().rev() {
            let turn = turn.as_deref().map(&mut turn_key);
            let state = turn.and_then(|id| turns.get(&id)).copied();
            if state == Some(TurnState::Withdrawn) {
                continue;
            }
            if possible.is_some_and(|path| path != directory) {
                return None;
            }
            if state == Some(TurnState::Present)
                || turn.is_some_and(|id| mixed_dirs.get(&id) == Some(&Some(directory)))
            {
                return Some((directory.clone(), *position));
            }
            // A missing or ambiguous owner can survive. Every possible directory must agree.
            possible = Some(directory);
        }
        self.before
            .clone()
            .filter(|path| possible.is_none_or(|other| other == path))
            .map(|path| (path, 1))
    }
}
