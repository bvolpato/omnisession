use super::{BTreeMap, HashMap, Input, Key, MAX_CANONICAL_EVENTS, Value, failed_attachments};
use failed_attachments::MediaSlot;

#[derive(Default)]
pub(super) struct MediaReceipts {
    rows: BTreeMap<(u64, u64), Box<[MediaSlot]>>,
    slots: usize,
}

impl MediaReceipts {
    pub(super) fn observe(&mut self, input: &Input, item: &Value) {
        if !input.media.is_some_and(|media| media.known_parts) {
            return;
        }
        let Some(slots) = item["content"]
            .as_array()
            .and_then(|p| failed_attachments::media_slots(p))
        else {
            return;
        };
        self.slots += slots.len();
        if let Some(old) = self.rows.insert((input.segment, input.ordinal), slots) {
            self.slots -= old.len();
        }
        while self.slots > MAX_CANONICAL_EVENTS
            && let Some((_, removed)) = self.rows.pop_first()
        {
            self.slots -= removed.len();
        }
    }

    pub(super) fn get(&self, input: &Input) -> Option<&[MediaSlot]> {
        input.media.filter(|m| m.known_parts)?;
        Some(self.rows.get(&(input.segment, input.ordinal))?.as_ref())
    }

    pub(super) fn matches(&self, raw: &Input, shown: &Input) -> bool {
        if raw.media.is_none() || shown.media.is_none() {
            return raw.text.is_some() && raw.text == shown.text;
        }
        self.get(raw)
            .is_some_and(|slots| self.matches_slots(raw, slots, shown))
    }

    pub(super) fn align(&self, raw: &mut Input, shown: &Input) {
        if raw.media.is_some() && shown.media.is_some() {
            // Matching IDs can establish ownership even when a later edit changed the attachments.
            if self.matches(raw, shown) {
                raw.text = shown.text;
                if let (Some(raw), Some(shown)) = (&mut raw.media, shown.media) {
                    raw.local = shown.local;
                }
            } else {
                raw.text = None;
            }
        }
    }

    pub(super) fn matches_slots(&self, raw: &Input, slots: &[MediaSlot], shown: &Input) -> bool {
        raw.media.zip(shown.media).is_some_and(|(a, b)| {
            a.known_parts
                && b.known_parts
                && (a.signature == b.signature
                    || (raw.text.is_some()
                        && raw.text == shown.text
                        && slots.iter().any(|slot| slot.failed)))
                && self.get(shown).is_some_and(|display| {
                    slots.len() == display.len()
                        && slots.iter().zip(display).all(|(a, b)| a.accepts(*b))
                })
        })
    }
}

type Position = (u64, Option<Key>, Key, usize, Key);

#[derive(Default)]
pub(super) struct PublishedMedia<'a> {
    positions: HashMap<Position, Vec<&'a Input>>,
}

impl<'a> PublishedMedia<'a> {
    pub(super) fn insert(
        &mut self,
        shown: &'a Input,
        scope: Option<Key>,
        window: u64,
        receipts: &MediaReceipts,
    ) {
        if let Some((media, slots)) = shown.media.zip(receipts.get(shown)) {
            for (position, slot) in slots.iter().enumerate() {
                self.positions
                    .entry((window, scope, media.signature, position, slot.reference))
                    .or_default()
                    .push(shown);
            }
        }
    }

    pub(super) fn spelling(
        &self,
        raw: &Input,
        slots: &[MediaSlot],
        receipts: &MediaReceipts,
    ) -> Option<Key> {
        if !slots.iter().any(|s| s.path.is_some()) {
            return None;
        }
        let media = raw.media?;
        // Search the rarest slot. Common occurrences cannot independently establish an owner.
        let groups = slots
            .iter()
            .enumerate()
            .map(|(position, slot)| {
                let group = |reference| {
                    self.positions
                        .get(&(raw.window, raw.turn, media.signature, position, reference))
                        .map_or(&[][..], Vec::as_slice)
                };
                (group(slot.reference), slot.path.map_or(&[][..], group))
            })
            .min_by_key(|(wire, paths)| wire.len() + paths.len())?;
        if groups.0.len() + groups.1.len() > 16 {
            return None;
        }
        let mut spellings = groups
            .0
            .iter()
            .chain(groups.1)
            .copied()
            .filter(|&shown| raw.permits_text(*shown) && receipts.matches_slots(raw, slots, shown))
            .filter_map(|shown| shown.text);
        let first = spellings.next()?;
        spellings.all(|other| other == first).then_some(first)
    }
}
