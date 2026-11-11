use std::collections::{BTreeMap, HashMap};

use serde_json::Value;
use sha2::{Digest, Sha256};

use super::super::paginated_rollback::json_digest;
use super::super::{model_call_id, model_result_id};
use super::{PaginatedAnswers, TurnAliases};

type Key = [u8; 32];
type Scope = Option<Key>;
// A result belongs to the original call that produced it.
type Body = (Key, Option<u64>);

pub(super) fn payload_digest(item: &Value) -> Option<Key> {
    let id = model_call_id(item).or_else(|| model_result_id(item))?;
    // Codex may assign another response item ID or omit its turn stamp when it saves a checkpoint.
    // The provider call ID and the complete tool payload still have to agree.
    let fields: BTreeMap<_, _> = item
        .as_object()?
        .iter()
        .filter(|(name, _)| {
            !matches!(
                name.as_str(),
                "id" | "internal_chat_message_metadata_passthrough"
            )
        })
        .collect();
    Some(json_digest(&(id, fields)))
}

#[derive(Default)]
struct OriginalCopies {
    scoped: HashMap<(Body, Scope), Option<u64>>,
    all: HashMap<Body, Option<u64>>,
}

impl OriginalCopies {
    fn add(&mut self, digest: Body, turn: Scope, seq: u64) {
        self.scoped
            .entry((digest, turn))
            .and_modify(|seq| *seq = None)
            .or_insert(Some(seq));
        self.all
            .entry(digest)
            .and_modify(|seq| *seq = None)
            .or_insert(Some(seq));
    }

    fn find(&self, digest: Body, turn: Scope) -> Option<u64> {
        let Some(turn) = turn else {
            return self.all.get(&digest).copied().flatten();
        };
        if let Some(found) = self.scoped.get(&(digest, Some(turn))) {
            return *found;
        }
        // An unstamped original can match only if no other occurrence competes with it.
        self.all
            .get(&digest)
            .copied()
            .flatten()
            .filter(|seq| self.scoped.get(&(digest, None)) == Some(&Some(*seq)))
    }
}

struct PendingCall {
    open: usize,
    matched: Option<(u64, usize)>,
}

impl PaginatedAnswers {
    pub(in super::super) fn checkpoint_tool_copies(
        &self,
        aliases: &TurnAliases,
        items: &[Value],
        start: usize,
    ) -> Vec<(usize, u64, Option<usize>)> {
        let resolve = |turn: Key| aliases.get(&turn).copied().unwrap_or(turn);
        let mut original = OriginalCopies::default();
        for (&seq, tool) in &self.tools {
            if let Some(digest) = tool.digest {
                original.add(
                    (digest, tool.call),
                    tool.key.and_then(|key| key.turn).map(resolve),
                    seq,
                );
            }
        }
        let mut copies = Vec::new();
        let mut pending = HashMap::new();
        for (index, item) in items.iter().enumerate() {
            let call = model_call_id(item);
            let Some(id) = call.or_else(|| model_result_id(item)) else {
                continue;
            };
            // An open call in the skipped prefix can still compete for a later result.
            let digest = (index >= start).then(|| payload_digest(item)).flatten();
            let turn = item["internal_chat_message_metadata_passthrough"]["turn_id"]
                .as_str()
                .filter(|turn| !turn.is_empty())
                .map(|turn| resolve(Sha256::digest(turn.as_bytes()).into()));
            if call.is_some() {
                let matched = digest.and_then(|digest| original.find((digest, None), turn));
                // Until every overlapping call with this ID has a result, its results have no unique owner.
                pending
                    .entry(id)
                    .and_modify(|call: &mut PendingCall| {
                        call.open += 1;
                        call.matched = None;
                    })
                    .or_insert(PendingCall {
                        open: 1,
                        matched: matched.map(|seq| (seq, index)),
                    });
                if let Some(seq) = matched {
                    copies.push((index, seq, None));
                }
            } else if let Some(call) = pending.get_mut(id) {
                call.open -= 1;
                if call.open == 0
                    && let Some((source, position)) =
                        pending.remove(id).and_then(|call| call.matched)
                    && let Some(seq) =
                        digest.and_then(|digest| original.find((digest, Some(source)), turn))
                {
                    copies.push((index, seq, Some(position)));
                }
            }
        }
        copies
    }
}
