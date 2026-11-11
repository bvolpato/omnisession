//! Match paginated replies and documentary tool items to the native model window.

use std::collections::{BTreeMap, HashMap, VecDeque};

use serde_json::Value;
use sha2::{Digest, Sha256};

use super::MAX_CANONICAL_EVENTS;
use super::message_mirrors::{MessageIdentity, ScopedMessageId};
use super::paginated_rollback::Withdrawal;

type TurnAliases = HashMap<[u8; 32], [u8; 32]>;

fn resolved(aliases: &TurnAliases, (role, turn, id): ScopedMessageId) -> ScopedMessageId {
    (role, aliases.get(&turn).copied().unwrap_or(turn), id)
}

#[derive(Clone, Copy)]
pub(super) struct ToolId {
    turn: Option<[u8; 32]>,
    id: [u8; 32],
    generated_turn: bool,
}

impl ToolId {
    pub(super) fn new(turn: Option<&str>, id: &str) -> Option<Self> {
        (!id.is_empty()).then(|| Self {
            turn: turn
                .filter(|s| !s.is_empty())
                .map(|s| Sha256::digest(s.as_bytes()).into()),
            id: Sha256::digest(id.as_bytes()).into(),
            generated_turn: turn.is_some_and(super::is_implicit_turn),
        })
    }

    fn scoped(self, aliases: &TurnAliases) -> Option<ScopedMessageId> {
        Some(resolved(aliases, (true, self.turn?, self.id)))
    }

    fn generated(self, aliases: &TurnAliases) -> Option<bool> {
        Some(self.generated_turn && !aliases.contains_key(&self.turn?))
    }
}

#[derive(Default)]
struct CallCopies {
    retained: bool,
    removed: bool,
}

impl CallCopies {
    fn add(&mut self, removed: bool) {
        self.retained |= !removed;
        self.removed |= removed;
    }

    fn withdrawal(&self) -> Option<Withdrawal> {
        self.removed.then_some(if self.retained {
            Withdrawal::Unverified
        } else {
            Withdrawal::Confirmed
        })
    }
}

struct ModelAnswer {
    key: ScopedMessageId,
    position: Position,
}

struct Position {
    ordinal: u64,
    order: Option<u64>,
    removed: bool,
}

impl Position {
    fn new(record: &Value, ordinal: u64, ordered: bool) -> Self {
        let metadata = &record["metadata"];
        let order = metadata["user_input_order"]
            .as_u64()
            .filter(|_| ordered && metadata["inherited_user_message"] != true);
        Self {
            ordinal,
            order,
            removed: false,
        }
    }

    fn rollback(&mut self, ordinal: u64, order: Option<u64>) {
        self.removed |= self.ordinal >= ordinal
            || order
                .zip(self.order)
                .is_some_and(|(cut, accepted)| accepted >= cut);
    }
}

struct ModelTool {
    position: Position,
    // Outputs refer to the matched call's canonical sequence, even when the call was unstamped.
    call: Option<u64>,
    key: Option<ToolId>,
    output: bool,
}

#[derive(Default)]
pub(super) struct PaginatedAnswers {
    model: VecDeque<ModelAnswer>,
    displayed: BTreeMap<u64, ScopedMessageId>,
    after_rollback: HashMap<ScopedMessageId, bool>,
    tools: BTreeMap<u64, ModelTool>,
    tool_displays: BTreeMap<u64, ToolId>,
    removed_calls: HashMap<ScopedMessageId, CallCopies>,
    call_scopes: HashMap<([u8; 32], Option<bool>), CallCopies>,
    aliases: TurnAliases,
}

impl PaginatedAnswers {
    pub(super) fn segment(&mut self) {
        self.model.clear();
        self.displayed.clear();
        self.after_rollback.clear();
        self.tools.clear();
        self.tool_displays.clear();
        self.removed_calls.clear();
        self.call_scopes.clear();
        self.aliases.clear();
    }

    pub(super) fn resolve_turns(&mut self, aliases: TurnAliases) {
        self.aliases = aliases;
    }

    pub(super) fn model(&mut self, record: &Value, ordinal: u64) {
        let payload = &record["payload"];
        let identity = MessageIdentity::new(
            payload["internal_chat_message_metadata_passthrough"]["turn_id"].as_str(),
            payload["id"].as_str(),
            false,
        );
        let Some(key) = identity.scoped(true) else {
            return;
        };
        if self.model.len() == MAX_CANONICAL_EVENTS {
            self.model.pop_front();
        }
        self.model.push_back(ModelAnswer {
            key,
            position: Position::new(record, ordinal, true),
        });
        self.after_rollback.remove(&resolved(&self.aliases, key));
    }

    pub(super) fn model_tool(
        &mut self,
        sequence: u64,
        record: &Value,
        ordinal: u64,
        call: Option<u64>,
        key: Option<ToolId>,
    ) -> Option<Withdrawal> {
        let stored = self.tools.get(&sequence);
        let output = record["payload"]["type"]
            .as_str()
            .is_some_and(|t| t.ends_with("_output"));
        if !output && let Some(key) = key {
            if let Some(prior) = key
                .scoped(&self.aliases)
                .and_then(|k| self.removed_calls.get_mut(&k))
            {
                prior.add(false);
            }
            if [None, Some(true), Some(false)]
                .iter()
                .any(|&scope| self.call_scopes.contains_key(&(key.id, scope)))
            {
                self.call_scopes
                    .entry((key.id, key.generated(&self.aliases)))
                    .or_default()
                    .add(false);
            }
        }
        // Among tool items, Codex's acceptance cutoff applies only to function calls.
        let ordered = record["payload"]["type"] == "function_call";
        let tool = ModelTool {
            position: Position::new(record, ordinal, ordered),
            call: call.or_else(|| stored?.call),
            key,
            output,
        };
        let removed = self.tool_withdrawal(&tool);
        self.tools.insert(sequence, tool);
        if self.tools.len() > MAX_CANONICAL_EVENTS {
            self.tools.pop_first();
        }
        removed
    }

    pub(super) fn display_tool(
        &mut self,
        sequence: u64,
        turn: &str,
        id: &str,
    ) -> Option<Withdrawal> {
        let key = ToolId::new(Some(turn), id)?;
        self.tool_displays.insert(sequence, key);
        if self.tool_displays.len() > MAX_CANONICAL_EVENTS {
            self.tool_displays.pop_first();
        }
        match self.tools.get(&sequence) {
            Some(tool) => self.tool_withdrawal(tool),
            None => self.call_withdrawal(key),
        }
    }

    fn tool_withdrawal(&self, tool: &ModelTool) -> Option<Withdrawal> {
        if tool.position.removed {
            return Some(Withdrawal::Confirmed);
        }
        if let Some(call) = tool.call.and_then(|seq| self.tools.get(&seq)) {
            return call.position.removed.then_some(Withdrawal::Confirmed);
        }
        tool.key
            .filter(|_| tool.output)
            .and_then(|key| self.call_withdrawal(key))
    }

    fn call_withdrawal(&self, key: ToolId) -> Option<Withdrawal> {
        let exact = key
            .scoped(&self.aliases)
            .and_then(|k| self.removed_calls.get(&k));
        let generated = key.generated(&self.aliases);
        for scope in [Some(true), Some(false), None] {
            if (generated.is_none() || generated != scope)
                && let Some(other) = self.call_scopes.get(&(key.id, scope))
                && ((other.removed && exact.is_none_or(|copy| copy.retained))
                    || (other.retained && exact.is_some_and(|copy| copy.removed)))
            {
                return Some(Withdrawal::Unverified);
            }
        }
        exact.and_then(CallCopies::withdrawal)
    }

    /// A completed item can arrive after Codex has already rolled back its model copy.
    pub(super) fn display(&mut self, sequence: u64, turn: &str, id: &str) -> Option<bool> {
        let key = MessageIdentity::new(Some(turn), Some(id), true).scoped(true)?;
        self.displayed.insert(sequence, key);
        if self.displayed.len() > MAX_CANONICAL_EVENTS {
            self.displayed.pop_first();
        }
        self.after_rollback
            .get(&resolved(&self.aliases, key))
            .copied()
    }

    pub(super) fn rollback(
        &mut self,
        boundary_ordinal: u64,
        order: Option<u64>,
    ) -> HashMap<u64, Option<Withdrawal>> {
        self.after_rollback.clear();
        for answer in &mut self.model {
            // Codex can accept a reply after a queued instruction and write it before that instruction.
            answer.position.rollback(boundary_ordinal, order);
            // If Codex recorded this ID more than once, one surviving copy keeps it in the model window.
            *self
                .after_rollback
                .entry(resolved(&self.aliases, answer.key))
                .or_insert(true) &= answer.position.removed;
        }
        let mut marked: HashMap<_, _> = self
            .displayed
            .iter()
            .filter_map(|(&sequence, &key)| {
                Some((
                    sequence,
                    self.after_rollback
                        .get(&resolved(&self.aliases, key))?
                        .then_some(Withdrawal::Confirmed),
                ))
            })
            .collect();
        self.removed_calls.clear();
        self.call_scopes.clear();
        for tool in self.tools.values_mut() {
            tool.position.rollback(boundary_ordinal, order);
            if let Some(key) = tool.key.filter(|_| !tool.output) {
                if let Some(scoped) = key.scoped(&self.aliases) {
                    self.removed_calls
                        .entry(scoped)
                        .or_default()
                        .add(tool.position.removed);
                }
                self.call_scopes
                    .entry((key.id, key.generated(&self.aliases)))
                    .or_default()
                    .add(tool.position.removed);
            }
        }
        for (&sequence, tool) in &self.tools {
            marked.insert(sequence, self.tool_withdrawal(tool));
        }
        for (&sequence, &key) in &self.tool_displays {
            if let Some(withdrawal) = self.call_withdrawal(key) {
                marked.entry(sequence).or_insert(Some(withdrawal));
            }
        }
        marked
    }

    pub(super) fn discard_before(&mut self, first_sequence: u64) {
        while self
            .displayed
            .first_key_value()
            .is_some_and(|(&seq, _)| seq < first_sequence)
        {
            self.displayed.pop_first();
        }
        self.tools = self.tools.split_off(&first_sequence);
        self.tool_displays = self.tool_displays.split_off(&first_sequence);
    }
}
