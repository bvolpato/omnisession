//! Recover original turns when migration named the public turn from a rollout ordinal.

use super::{
    HashMap, HashSet, IdKind, Input, Key, MAX_CANONICAL_EVENTS, PaginatedRollback, Value,
    Withdrawal, content_digest, key, model_instruction, uuid_v7,
};

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
                    && self.media.matches(r, &shown)
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

    /// Some migrations retain the model copy only inside the compaction checkpoint.
    pub(super) fn match_checkpoint_inputs(
        &self,
        aliases: &HashMap<Key, Key>,
        checkpoint: &[Value],
        copies: &mut HashMap<(Key, Key), (u64, Key)>,
    ) {
        if checkpoint.len() > MAX_CANONICAL_EVENTS || self.history_floor != 0 {
            return;
        }
        let raw_turns: HashSet<_> = self.model.iter().filter_map(|raw| raw.turn).collect();
        let mut displayed = HashMap::new();
        let mut media = super::media::PublishedMedia::default();
        for (&seq, shown) in &self.displayed {
            if shown.segment != self.segment || shown.withdrawal.is_some() {
                continue;
            }
            let Some((turn, spelling)) = shown.turn.zip(shown.text) else {
                continue;
            };
            let turn = aliases.get(&turn).copied().unwrap_or(turn);
            if raw_turns.contains(&turn) {
                continue;
            }
            media.insert(shown, Some(turn), 0, &self.media);
            displayed
                .entry((turn, spelling))
                .and_modify(|prior| *prior = None)
                .or_insert((shown.kind == IdKind::GeneratedDisplay).then_some(seq));
        }
        if displayed.is_empty() {
            return;
        }
        let mut seen_ids = HashSet::new();
        let mut repeated_ids = HashSet::new();
        let mut by_spelling = HashMap::new();
        for item in checkpoint {
            if item["type"] != "message" || item["role"] != "user" || !model_instruction(item) {
                continue;
            }
            let Some(turn) = item["internal_chat_message_metadata_passthrough"]["turn_id"]
                .as_str()
                .filter(|t| uuid_v7(t))
            else {
                continue;
            };
            let turn = key(turn);
            let id = item["id"].as_str();
            if let Some(id) = id
                && !seen_ids.insert((turn, key(id)))
            {
                repeated_ids.insert((turn, key(id)));
            }
            let mut raw = Input::new(None, item, false, 0, self.segment);
            raw.turn = Some(turn);
            let slots = raw
                .media
                .and_then(|_| item["content"].as_array())
                .and_then(|p| super::failed_attachments::media_slots(p));
            let matched = slots
                .as_ref()
                .and_then(|s| media.spelling(&raw, s, &self.media));
            let Some(spelling) = matched.or(raw.text) else {
                continue;
            };
            let scope = (turn, spelling);
            let Some(Some(&seq)) = displayed.get(&scope).map(Option::as_ref) else {
                continue;
            };
            if let Some(slots) = slots
                && !self
                    .media
                    .matches_slots(&raw, &slots, &self.displayed[&seq])
            {
                continue;
            }
            let stable = id.filter(|id| {
                id.strip_prefix("msg_input_")
                    .is_some_and(|suffix| uuid::Uuid::try_parse(suffix).is_ok())
            });
            let entry = stable.map(|id| ((turn, key(id)), (seq, content_digest(item))));
            by_spelling
                .entry(scope)
                .and_modify(|entry| *entry = None)
                .or_insert(entry);
        }
        for (id, witness) in by_spelling.into_values().flatten() {
            if !repeated_ids.contains(&id) {
                copies.insert(id, witness);
            }
        }
    }

    /// A path on one side and an inline or file reference on the other cannot prove identity.
    pub(super) fn mark_unresolved_media(
        &self,
        cut: usize,
        aliases: &HashMap<Key, Key>,
        reserved: &HashSet<u64>,
        assigned: &HashSet<usize>,
        marked: &mut HashMap<u64, Withdrawal>,
    ) {
        let mut candidates = HashMap::new();
        for (n, raw) in self.model.iter().enumerate().skip(cut) {
            let Some(media) = raw.media.filter(|_| !assigned.contains(&n)) else {
                continue;
            };
            for scope in [raw.turn, None] {
                let bounds = candidates
                    .entry((raw.window, scope, media.signature))
                    .or_insert((media.local, media.local));
                bounds.0 = bounds.0.min(media.local);
                bounds.1 = bounds.1.max(media.local);
            }
        }
        for (&seq, shown) in &self.displayed {
            let Some(media) = shown.media else { continue };
            if shown.withdrawal.is_some()
                || reserved.contains(&seq)
                || (shown.id.is_some()
                    && !matches!(shown.kind, IdKind::GeneratedDisplay | IdKind::LiveDisplay))
            {
                continue;
            }
            let scope = if shown.generated_turn {
                shown.turn.and_then(|turn| aliases.get(&turn).copied())
            } else {
                shown.turn
            };
            if candidates
                .get(&(shown.window, scope, media.signature))
                .is_some_and(|&(min, max)| min != media.local || max != media.local)
            {
                marked.entry(seq).or_insert(Withdrawal::Unverified);
            }
        }
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
