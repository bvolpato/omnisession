use super::{AttachmentReceipt, BTreeMap, HashMap, Input, Key, MAX_CANONICAL_EVENTS};

#[derive(Default)]
pub(super) struct MediaReceipts {
    rows: BTreeMap<(u64, u64), AttachmentReceipt>,
    slots: usize,
}

impl MediaReceipts {
    pub(super) fn observe(&mut self, input: &Input, receipt: Option<AttachmentReceipt>) {
        let Some(receipt) = receipt else { return };
        self.slots += receipt.len();
        if let Some(old) = self.rows.insert((input.segment, input.ordinal), receipt) {
            self.slots -= old.len();
        }
        while self.slots > MAX_CANONICAL_EVENTS
            && let Some((_, removed)) = self.rows.pop_first()
        {
            self.slots -= removed.len();
        }
    }

    pub(super) fn get(&self, input: &Input) -> Option<&AttachmentReceipt> {
        input.media?;
        self.rows.get(&(input.segment, input.ordinal))
    }

    pub(super) fn matches(&self, raw: &Input, shown: &Input) -> bool {
        if raw.media.is_none() || shown.media.is_none() {
            return raw.text.is_some() && raw.text == shown.text;
        }
        self.get(raw)
            .zip(self.get(shown))
            .is_some_and(|(a, b)| a.matches(b))
    }

    pub(super) fn align(&self, raw: &mut Input, shown: &Input) {
        if raw.media.is_some() && shown.media.is_some() {
            raw.text = if self.matches(raw, shown) {
                shown.text
            } else {
                None
            };
        }
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
            for (position, (reference, _)) in slots.anchors().enumerate() {
                self.positions
                    .entry((window, scope, media.signature, position, reference))
                    .or_default()
                    .push(shown);
            }
        }
    }

    pub(super) fn spelling(
        &self,
        raw: &Input,
        receipt: &AttachmentReceipt,
        receipts: &MediaReceipts,
    ) -> Option<Key> {
        if !receipt.anchors().any(|(_, path)| path.is_some()) {
            return None;
        }
        let media = raw.media?;
        // Search the rarest slot. Common occurrences cannot independently establish an owner.
        let groups = receipt
            .anchors()
            .enumerate()
            .map(|(position, (reference, path))| {
                let group = |reference| {
                    self.positions
                        .get(&(raw.window, raw.turn, media.signature, position, reference))
                        .map_or(&[][..], Vec::as_slice)
                };
                (group(reference), path.map_or(&[][..], group))
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
            .filter(|&shown| {
                raw.permits_text(*shown)
                    && receipts
                        .get(shown)
                        .is_some_and(|display| receipt.matches(display))
            })
            .filter_map(|shown| shown.text);
        let first = spellings.next()?;
        spellings.all(|other| other == first).then_some(first)
    }
}
