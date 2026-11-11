use std::collections::{BTreeMap, HashMap};

use serde_json::Value;
use sha2::{Digest, Sha256};

use super::super::paginated_rollback::json_digest;
use super::super::{scoped_model_call_id, scoped_model_result_id};
use super::{PaginatedAnswers, TurnAliases};

type Key = [u8; 32];
type Scope = Option<Key>;
// A result belongs to the original call that produced it.
type Body = (Key, Option<u64>);

fn named_output(item: &Value) -> bool {
    item["type"] == "function_call_output" && item["call_id"].is_null() && item["name"].is_string()
}

pub(super) fn receipt_digest(item: &Value) -> Option<Key> {
    let id = item["id"].as_str().filter(|id| !id.is_empty())?;
    let name = item["name"].as_str()?;
    let namespace = &item["namespace"];
    let output = &item["output"];
    if !(namespace.is_null() || namespace.is_string()) || !(output.is_string() || output.is_array())
    {
        return None;
    }
    Some(json_digest(&(id, name, namespace, output)))
}

pub(super) fn source_digest(record: &Value, repeated: bool, scoped: bool) -> Option<Key> {
    let item = &record["payload"];
    if repeated && named_output(item) {
        return None;
    }
    payload_digest(item, &record["metadata"], scoped)
}

fn payload_digest(item: &Value, metadata: &Value, scoped: bool) -> Option<Key> {
    if named_output(item) {
        // Codex retains a standalone output only when its client marked it for retention.
        if !super::super::model_cut::harness_metadata(metadata).client_authored
            || item.as_object()?.keys().any(|key| {
                !matches!(
                    key.as_str(),
                    "type"
                        | "id"
                        | "call_id"
                        | "name"
                        | "namespace"
                        | "output"
                        | "internal_chat_message_metadata_passthrough"
                )
            })
        {
            return None;
        }
        return receipt_digest(item);
    }
    let id = scoped_model_call_id(item, scoped).or_else(|| scoped_model_result_id(item, scoped))?;
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
    fn published_receipt(
        &self,
        seq: u64,
        digest: Key,
        turn: Scope,
        aliases: &TurnAliases,
    ) -> Option<u64> {
        let source = self.tools.get(&seq)?;
        let (public, displayed) = self.tool_displays.get(&seq)?;
        let public_turn = public
            .turn
            .map(|turn| aliases.get(&turn).copied().unwrap_or(turn));
        (source.call.is_none()
            && source.digest == Some(digest)
            && *displayed == Some(digest)
            && (turn.is_none() || public_turn == turn))
            .then_some(seq)
    }

    pub(in super::super) fn checkpoint_tool_copies(
        &self,
        aliases: &TurnAliases,
        items: &[Value],
        metadata: Option<&[Value]>,
        start: usize,
    ) -> Vec<(usize, u64, Option<usize>)> {
        let resolve = |turn: Key| aliases.get(&turn).copied().unwrap_or(turn);
        let mut original = OriginalCopies::default();
        let mut output_ids = HashMap::new();
        let includes_receipts = items[start..].iter().any(named_output);
        for (&seq, tool) in &self.tools {
            if includes_receipts
                && tool.output
                && let Some(key) = tool.key
            {
                output_ids
                    .entry(key.id)
                    .and_modify(|seq| *seq = None)
                    .or_insert(Some(seq));
            }
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
        let mut receipts = HashMap::new();
        for (index, item) in items.iter().enumerate() {
            // Codex decodes these legacy tool records as opaque Other items in a checkpoint.
            if matches!(
                item["type"].as_str(),
                Some("computer_call" | "computer_call_output" | "local_shell_call_output")
            ) {
                continue;
            }
            let call = scoped_model_call_id(item, true);
            let Some(id) = call.or_else(|| scoped_model_result_id(item, true)) else {
                continue;
            };
            let meta = metadata
                .and_then(|entries| entries.get(index))
                .unwrap_or(&Value::Null);
            let turn = item["internal_chat_message_metadata_passthrough"]["turn_id"]
                .as_str()
                .filter(|turn| !turn.is_empty())
                .map(|turn| resolve(Sha256::digest(turn.as_bytes()).into()));
            if named_output(item) {
                let native_id: Key = Sha256::digest(id.as_bytes()).into();
                let Some(&Some(seq)) = output_ids.get(&native_id) else {
                    continue;
                };
                let matched = (index >= start)
                    .then(|| payload_digest(item, meta, true))
                    .flatten()
                    .and_then(|digest| self.published_receipt(seq, digest, turn, aliases))
                    .map(|seq| (index, seq, None));
                // Repeated item IDs in replacement history do not identify a unique occurrence.
                receipts
                    .entry(native_id)
                    .and_modify(|copy| *copy = None)
                    .or_insert(matched);
                continue;
            }
            // An open call in the skipped prefix can still compete for a later result.
            let digest = (index >= start)
                .then(|| payload_digest(item, meta, true))
                .flatten();
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
        copies.extend(receipts.into_values().flatten());
        copies
    }
}
