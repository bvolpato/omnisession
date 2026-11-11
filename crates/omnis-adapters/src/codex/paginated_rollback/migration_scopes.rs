//! Recover original turns when migration named the public turn from a rollout ordinal.

use super::{HashMap, HashSet, IdKind, Input, Key, PaginatedRollback, Withdrawal};

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
enum Signature {
    Id(Key),
    Text(Key),
}

impl Input {
    fn signatures(self) -> impl Iterator<Item = (u64, Signature)> {
        self.id
            .map(Signature::Id)
            .into_iter()
            .chain(self.text.map(Signature::Text))
            .map(move |s| (self.window, s))
    }
}

fn item_ids(items: impl Iterator<Item = Input>) -> HashMap<(u64, Key), Option<Input>> {
    let mut entries = HashMap::new();
    for item in items {
        if let Some(id) = item.id {
            entries
                .entry((item.window, id))
                .and_modify(|entry| *entry = None)
                .or_insert(Some(item));
        }
    }
    entries
}

fn associate(candidates: &mut HashMap<Key, Option<Key>>, shown: Key, raw: Key) {
    candidates
        .entry(shown)
        .and_modify(|found| {
            if *found != Some(raw) {
                *found = None;
            }
        })
        .or_insert(Some(raw));
}

impl PaginatedRollback {
    pub(in super::super) fn resolved_turns(&self) -> HashMap<Key, Key> {
        let model_rows = self
            .model
            .iter()
            .copied()
            .filter(|r| r.segment == self.segment);
        let display_rows = self
            .displayed
            .values()
            .copied()
            .filter(|d| d.segment == self.segment);
        if !display_rows.clone().any(|d| d.generated_turn) {
            return HashMap::new();
        }
        let model = item_ids(model_rows.clone());
        let display = item_ids(display_rows.clone());
        let mut candidates = HashMap::new();
        for (id, shown) in display {
            let Some(shown) = shown.filter(|s| s.generated_turn) else {
                continue;
            };
            let Some(raw) = model.get(&id).copied().flatten() else {
                continue;
            };
            let (Some(generated), Some(original)) = (shown.turn, raw.turn) else {
                continue;
            };
            if !raw.generated_turn
                && !raw.inherited
                && raw.ordinal.min(shown.ordinal) >= self.history_floor
            {
                associate(&mut candidates, generated, original);
            }
        }
        // The migrator may insert a turn opening between a raw request and its displayed
        // copy. Pair neighboring occurrences in both directions, including repeated text.
        let ordinals: HashMap<_, _> = model_rows.map(|r| (r.ordinal, r)).collect();
        let mut neighbors = HashMap::<u64, Option<(Key, Key)>>::new();
        for shown in display_rows
            .filter(|d| d.generated_turn && d.text.is_some() && d.ordinal >= self.history_floor)
        {
            let Some(generated) = shown.turn else {
                continue;
            };
            let mut found = [
                shown.ordinal.checked_sub(1),
                shown.ordinal.checked_add(1),
                (shown.display_start != shown.ordinal)
                    .then(|| shown.display_start.checked_sub(1))
                    .flatten(),
            ]
            .into_iter()
            .flatten()
            .filter_map(|n| ordinals.get(&n))
            .filter(|r| {
                r.window == shown.window
                    && r.ordinal >= self.history_floor
                    && !r.inherited
                    && !r.generated_turn
                    && r.text == shown.text
                    && r.permits_text(shown)
            });
            let Some(raw) = found.next() else { continue };
            if found.next().is_some() {
                continue;
            }
            if let Some(original) = raw.turn {
                neighbors
                    .entry(raw.ordinal)
                    .and_modify(|pair| *pair = None)
                    .or_insert(Some((generated, original)));
            }
        }
        for (generated, original) in neighbors.into_values().flatten() {
            associate(&mut candidates, generated, original);
        }
        candidates
            .into_iter()
            .filter_map(|(generated, original)| Some((generated, original?)))
            .collect()
    }

    pub(super) fn mark_unresolved_migration(
        &self,
        cut: usize,
        aliases: &HashMap<Key, Key>,
        reserved: &HashSet<u64>,
        assigned: &HashSet<usize>,
        marked: &mut HashMap<u64, Withdrawal>,
    ) {
        let mut candidates = HashSet::new();
        let mut flexible_text = HashSet::new();
        for (index, raw) in self.model.iter().enumerate().skip(cut) {
            if assigned.contains(&index) || raw.generated_turn || raw.turn.is_none() {
                continue;
            }
            candidates.extend(raw.signatures());
            if let Some(text) = raw.text
                && (raw.id.is_none() || raw.kind == IdKind::ModelInput)
            {
                flexible_text.insert((raw.window, text));
            }
        }
        for (&seq, &shown) in &self.displayed {
            if shown.withdrawal.is_some()
                || !shown.generated_turn
                || reserved.contains(&seq)
                || shown.turn.is_some_and(|turn| aliases.contains_key(&turn))
            {
                continue;
            }
            let possible = shown.signatures().any(|sig| {
                candidates.contains(&sig)
                    && match sig.1 {
                        Signature::Id(_) => true,
                        Signature::Text(text) => {
                            shown.id.is_none()
                                || shown.kind == IdKind::GeneratedDisplay
                                || (shown.kind == IdKind::LiveDisplay
                                    && flexible_text.contains(&(shown.window, text)))
                        }
                    }
            });
            if possible {
                marked.insert(seq, Withdrawal::Unverified);
            }
        }
    }
}
