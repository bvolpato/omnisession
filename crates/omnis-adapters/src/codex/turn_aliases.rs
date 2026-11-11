//! A migrated display turn can name the same native input as its original model turn.

use std::collections::HashMap;

use sha2::{Digest, Sha256};

use super::message_mirrors::ScopedMessageId;

#[derive(Default)]
pub(super) struct TurnAliases(HashMap<String, String>);

impl TurnAliases {
    pub(super) fn resolve<'a>(&'a self, id: &'a str) -> &'a str {
        self.0.get(id).map_or(id, String::as_str)
    }

    pub(super) fn bind(&mut self, generated: &str, original: &str) -> bool {
        if !super::is_implicit_turn(generated) || super::is_implicit_turn(original) {
            return false;
        }
        match self.0.entry(generated.to_owned()) {
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(original.to_owned());
                true
            }
            std::collections::hash_map::Entry::Occupied(_) => false,
        }
    }

    pub(super) fn retain(&mut self, named: &HashMap<String, u64>) {
        self.0.retain(|_, original| named.contains_key(original));
    }
}

type ItemIds = HashMap<(bool, [u8; 32]), u64>;

#[derive(Default)]
pub(super) struct MessageIds(HashMap<[u8; 32], ItemIds>);

impl MessageIds {
    pub(super) fn get(&self, &(assistant, turn, id): &ScopedMessageId) -> Option<&u64> {
        self.0.get(&turn)?.get(&(assistant, id))
    }

    pub(super) fn insert(&mut self, (assistant, turn, id): ScopedMessageId, seq: u64) {
        self.0.entry(turn).or_default().insert((assistant, id), seq);
    }

    pub(super) fn bind(&mut self, generated: &str, original: &str) {
        let key: [u8; 32] = Sha256::digest(generated.as_bytes()).into();
        if let Some(known) = self.0.remove(&key) {
            self.0
                .entry(Sha256::digest(original.as_bytes()).into())
                .or_default()
                .extend(known);
        }
    }

    pub(super) fn retain(&mut self, mut kept: impl FnMut(u64) -> bool) {
        self.0.retain(|_, ids| {
            ids.retain(|_, seq| kept(*seq));
            !ids.is_empty()
        });
    }
}
