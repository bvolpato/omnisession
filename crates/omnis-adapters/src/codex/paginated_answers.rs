//! Match paginated replies and documentary tool items to the native model window.

use std::collections::{BTreeMap, HashMap, VecDeque};

use omnis_ir::{EventKind, ReplayPolicy};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::message_mirrors::{MessageIdentity, ScopedMessageId};
use super::paginated_rollback::Withdrawal;
use super::{EventBuilder, MAX_CANONICAL_EVENTS};

type TurnAliases = HashMap<[u8; 32], [u8; 32]>;

mod checkpoint_tools;
mod occurrences;
use occurrences::{AnswerOccurrences, ConfirmedAnswers, PendingPlanSlot, PendingPlanWindow};

fn resolved(aliases: &TurnAliases, (role, turn, id): ScopedMessageId) -> ScopedMessageId {
    (role, aliases.get(&turn).copied().unwrap_or(turn), id)
}

pub(super) enum ReplyAnchor {
    Checkpoint,
    // The raw response precedes the next canonical event at this sequence.
    Source { ordinal: u64, sequence: u64 },
}

pub(super) struct ReplyVersion<'a> {
    pub(super) spelling: Option<&'a str>,
    pub(super) anchor: ReplyAnchor,
}

pub(super) struct ReplyProjection<'a> {
    pub(super) status: Option<Withdrawal>,
    pub(super) prior: Option<&'a str>,
    pub(super) versions: Vec<ReplyVersion<'a>>,
}

#[derive(Clone, Copy)]
pub(super) struct ToolId {
    turn: Option<[u8; 32]>,
    id: [u8; 32],
    generated_turn: bool,
    empty: bool,
}

impl ToolId {
    pub(super) fn new(turn: Option<&str>, id: &str) -> Option<Self> {
        let turn = turn.filter(|s| !s.is_empty());
        if id.is_empty() && turn.is_none_or(super::is_implicit_turn) {
            return None;
        }
        Some(Self {
            turn: turn.map(|s| Sha256::digest(s.as_bytes()).into()),
            id: Sha256::digest(id.as_bytes()).into(),
            generated_turn: turn.is_some_and(super::is_implicit_turn),
            empty: id.is_empty(),
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

struct Position {
    ordinal: u64,
    order: Option<u64>,
    removed: bool,
}

impl Position {
    fn new(record: &Value, ordinal: u64) -> Self {
        let order = super::model_cut::acceptance_order(&record["payload"], &record["metadata"]);
        Self {
            ordinal,
            order,
            removed: false,
        }
    }

    fn rollback(&mut self, ordinal: u64, order: Option<u64>) {
        self.removed |=
            self.ordinal >= ordinal || super::model_cut::removes_accepted(self.order, order);
    }
}

struct ModelTool {
    digest: Option<[u8; 32]>,
    position: Position,
    // Outputs refer to the matched call's canonical sequence, even when the call was unstamped.
    call: Option<u64>,
    key: Option<ToolId>,
    output: bool,
}

struct PendingReplacement {
    key: ScopedMessageId,
    digest: [u8; 32],
    multipart: Option<[u8; 32]>,
    ordinal: u64,
}

#[derive(Default)]
pub(super) struct PaginatedAnswers {
    answers: AnswerOccurrences,
    projected_revisions: BTreeMap<u64, Position>,
    tools: BTreeMap<u64, ModelTool>,
    tool_displays: BTreeMap<u64, (ToolId, Option<[u8; 32]>)>,
    removed_calls: HashMap<ScopedMessageId, CallCopies>,
    call_scopes: HashMap<([u8; 32], Option<bool>), CallCopies>,
    // Completed matches survive a physical file boundary. Unpaired IDs do not.
    model_positions: BTreeMap<u64, u64>,
    pending_replacements: BTreeMap<u64, PendingReplacement>,
    pending_plan_slots: Vec<PendingPlanSlot>,
    plan_windows: VecDeque<(PendingPlanWindow, Vec<PendingPlanSlot>)>,
    plan_window_rows: usize,
    aliases: TurnAliases,
}

impl PaginatedAnswers {
    pub(super) fn segment(&mut self) {
        self.answers = AnswerOccurrences::default();
        self.tools.clear();
        self.tool_displays.clear();
        self.removed_calls.clear();
        self.call_scopes.clear();
        self.aliases.clear();
        self.pending_replacements.clear();
        self.pending_plan_slots.clear();
        self.plan_windows.clear();
        self.plan_window_rows = 0;
    }

    pub(super) fn resolve_turns(&mut self, aliases: TurnAliases) {
        self.aliases = aliases;
    }

    pub(super) fn pending_replacement(
        &mut self,
        sequence: u64,
        item: &Value,
        text: &str,
        ordinal: u64,
    ) {
        let turn = item["internal_chat_message_metadata_passthrough"]["turn_id"].as_str();
        let id = item["id"].as_str();
        if let Some(key) = MessageIdentity::new(turn, id, false).scoped(true) {
            let multipart = item["content"]
                .as_array()
                .filter(|parts| parts.len() > 1)
                .map(|parts| {
                    let mut digest = Sha256::new();
                    for part in super::message_parts(parts, |_, _| true) {
                        digest.update(part);
                    }
                    digest.finalize().into()
                });
            self.pending_replacements.insert(
                sequence,
                PendingReplacement {
                    key,
                    digest: Sha256::digest(text).into(),
                    multipart,
                    ordinal,
                },
            );
        }
    }

    pub(super) fn pending_replacement_plan(
        &mut self,
        sequence: u64,
        native_index: usize,
        item: &Value,
        ordinal: u64,
        is_plan_part: bool,
    ) {
        if native_index < MAX_CANONICAL_EVENTS
            && self.pending_plan_slots.len() < 2 * MAX_CANONICAL_EVENTS
            && item["type"] == "message"
            && item["role"] == "assistant"
        {
            self.pending_plan_slots.push(PendingPlanSlot {
                ordinal,
                sequence,
                index: native_index as u64,
                plan: is_plan_part,
            });
        }
    }

    pub(super) fn pending_plan_window(
        &self,
        ordinal: u64,
        items: &[Value],
        plan_mode: impl Fn(&Value) -> bool,
    ) -> Option<PendingPlanWindow> {
        (!self.pending_plan_slots.is_empty() && items.len() <= MAX_CANONICAL_EVENTS)
            .then(|| PendingPlanWindow::read(ordinal, items, plan_mode))
    }

    pub(super) fn store_plan_window(&mut self, window: Option<PendingPlanWindow>) {
        let slots = std::mem::take(&mut self.pending_plan_slots);
        if let Some(window) = window.filter(|window| window.len() > 0) {
            self.plan_window_rows += window.len();
            self.plan_windows.push_back((window, slots));
            while self.plan_window_rows > MAX_CANONICAL_EVENTS {
                let (old, _) = self
                    .plan_windows
                    .pop_front()
                    .expect("retained plan rows belong to a window");
                self.plan_window_rows -= old.len();
            }
        }
    }

    /// Resolve a file's model positions before matching starts on the next physical file.
    pub(super) fn resolve_positions(&mut self, builder: &mut EventBuilder, ordinal: u64) {
        let published = self.answers.placement(
            &self.aliases,
            &mut self.model_positions,
            !self.pending_replacements.is_empty(),
        );
        for (sequence, pending) in std::mem::take(&mut self.pending_replacements) {
            let Some(&display) = published
                .get(&resolved(&self.aliases, pending.key))
                .and_then(Option::as_ref)
                .filter(|&&i| i > sequence)
            else {
                continue;
            };
            let Some(text) = builder
                .event_at_mut(display)
                .and_then(|e| e.payload["text"].as_str())
            else {
                continue;
            };
            let digest = Sha256::digest(text).into();
            if digest != pending.digest && pending.multipart != Some(digest) {
                continue;
            }
            let text = text.to_owned();
            if fill_pending_reply(builder, sequence, text) {
                let origin = self.model_positions.entry(display).or_default();
                *origin = (*origin).max(pending.ordinal);
            }
        }
        if let Some((window, slots)) = self.plan_windows.iter().find(|(w, _)| w.ordinal == ordinal)
        {
            self.answers.resolve_plan_window(
                window,
                slots,
                builder,
                &self.aliases,
                &mut self.model_positions,
            );
        }
    }

    pub(super) fn project_revisions(
        &mut self,
        builder: &mut EventBuilder,
        checkpoints: &super::paginated_checkpoint::CheckpointHistory,
        inputs: &super::paginated_rollback::PaginatedRollback,
        after: u64,
        end: u64,
    ) {
        if !self
            .answers
            .project_revisions(builder, checkpoints, end, &mut self.projected_revisions)
        {
            return;
        }
        // Confirmed model occurrences can precede the corresponding public completions.
        let users = inputs.confirmed_suffix_positions(after, &self.aliases);
        builder.visit_events_mut(|event| {
            let sequence = event.sequence;
            if sequence < end
                || event.source.raw_record_type.as_deref() != Some("event_msg.item_completed")
            {
                return;
            }
            let position = match event.kind {
                EventKind::MessageUser => users.get(&sequence).copied(),
                EventKind::MessageAssistant
                    if event.payload["omnisession_codex_positioned_reply"].is_null() =>
                {
                    self.answers.surviving_position(sequence, after)
                }
                _ => None,
            };
            if let Some((ordinal, slot)) = position.filter(|(_, slot)| *slot >= end) {
                event.payload["omnisession_codex_checkpoint_position"] =
                    serde_json::json!([slot, ordinal, 0]);
            }
        });
    }

    pub(super) fn finish_checkpoint(&mut self, builder: &mut EventBuilder, ordinal: u64, end: u64) {
        self.resolve_positions(builder, ordinal);
        // A match at the checkpoint ordinal is already present in the recorded replacement.
        for (&seq, &position) in self.model_positions.range(end..) {
            if position <= ordinal
                && let Some(event) = builder.event_at_mut(seq)
            {
                event.payload["omnisession_codex_before_compaction"] = true.into();
            }
        }
    }

    pub(super) fn model(
        &mut self,
        record: &Value,
        ordinal: u64,
        reserved: Option<u64>,
        plan_mode: bool,
        active_turn: Option<&str>,
    ) {
        self.answers
            .model(record, ordinal, reserved, plan_mode, active_turn);
    }

    pub(super) fn end_response(&mut self, turn: &str) {
        self.answers.end_response(turn);
    }

    pub(super) fn surviving_plans(&self) -> impl Iterator<Item = ([u8; 32], [u8; 32], u64)> + '_ {
        self.answers.surviving_plans()
    }

    pub(super) fn archive_changed_reply(
        &mut self,
        builder: &mut super::EventBuilder,
        previous: Option<&(u64, super::PreviousDisplay)>,
    ) {
        if let Some((seq, previous)) = previous
            && let Some(prior) = previous.displayed_text()
            && builder
                .event_at_mut(*seq)
                .and_then(|e| e.payload["text"].as_str())
                != Some(prior)
        {
            self.answers.archive(*seq, prior);
        }
    }

    pub(super) fn public_spelling(&self, sequence: u64, digest: [u8; 32]) -> Option<&str> {
        self.answers.archived(sequence, digest)
    }

    pub(super) fn confirmed_answer_copies(
        &self,
        aliases: &TurnAliases,
        items: &[Value],
        plan_mode: impl Fn(&Value) -> bool,
    ) -> ConfirmedAnswers {
        self.answers.confirmed(aliases, items, plan_mode)
    }

    pub(super) fn finish_answers(&mut self) -> HashMap<u64, Option<Withdrawal>> {
        self.answers.rulings(&self.aliases)
    }

    pub(super) fn project_answer(
        &self,
        seq: u64,
        checkpoint: &super::paginated_checkpoint::CheckpointHistory,
        status: Option<Withdrawal>,
    ) -> ReplyProjection<'_> {
        self.answers.project(seq, checkpoint, status)
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
        let tool = ModelTool {
            digest: checkpoint_tools::source_digest(
                record,
                stored.is_some(),
                key.is_some_and(|key| key.turn.is_some() && !key.generated_turn),
            ),
            position: Position::new(record, ordinal),
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

    pub(super) fn call_turn(&self, sequence: u64, aliases: &TurnAliases) -> Option<[u8; 32]> {
        self.tools
            .get(&sequence)?
            .key?
            .scoped(aliases)
            .map(|(_, turn, _)| turn)
    }

    pub(super) fn move_tool_display(&mut self, from: u64, to: u64) {
        if let Some(display) = self.tool_displays.remove(&from) {
            self.tool_displays.insert(to, display);
        }
    }

    pub(super) fn forget_tool_display(&mut self, sequence: u64) {
        self.tool_displays.remove(&sequence);
    }

    pub(super) fn display_tool(
        &mut self,
        sequence: u64,
        turn: &str,
        id: &str,
        item: &Value,
    ) -> Option<Withdrawal> {
        let key = ToolId::new(Some(turn), id)?;
        let receipt = (item["type"] == "FunctionCallOutput")
            .then(|| checkpoint_tools::receipt_digest(item))
            .flatten();
        self.tool_displays.insert(sequence, (key, receipt));
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
        if key.empty {
            return None;
        }
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
    pub(super) fn display(
        &mut self,
        sequence: u64,
        turn: &str,
        id: &str,
        text: &str,
        ordinal: u64,
        plan: bool,
    ) {
        self.answers
            .display(sequence, turn, id, text, ordinal, plan);
    }

    pub(super) fn rollback(
        &mut self,
        boundary_ordinal: u64,
        order: Option<u64>,
    ) -> HashMap<u64, Option<Withdrawal>> {
        self.answers.rollback(boundary_ordinal, order);
        let mut marked = HashMap::new();
        for (&sequence, position) in &mut self.projected_revisions {
            position.rollback(boundary_ordinal, order);
            marked.insert(sequence, position.removed.then_some(Withdrawal::Confirmed));
        }
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
        for (&sequence, &(key, _)) in &self.tool_displays {
            if let Some(withdrawal) = self.call_withdrawal(key) {
                marked.entry(sequence).or_insert(Some(withdrawal));
            }
        }
        marked
    }

    pub(super) fn discard_before(&mut self, first_sequence: u64) {
        self.model_positions = self.model_positions.split_off(&first_sequence);
        self.pending_replacements = self.pending_replacements.split_off(&first_sequence);
        while self.plan_windows.front().is_some_and(|(_, slots)| {
            slots
                .last()
                .is_some_and(|slot| slot.sequence < first_sequence)
        }) {
            let (old, _) = self.plan_windows.pop_front().expect("window was present");
            self.plan_window_rows -= old.len();
        }
        self.answers.discard_before(first_sequence);
        self.tools = self.tools.split_off(&first_sequence);
        self.projected_revisions = self.projected_revisions.split_off(&first_sequence);
        self.tool_displays = self.tool_displays.split_off(&first_sequence);
    }
}

fn fill_pending_reply(builder: &mut EventBuilder, sequence: u64, text: String) -> bool {
    let Some(slot) = builder.event_at_mut(sequence) else {
        return false;
    };
    let mut payload = serde_json::json!({"omnisession_codex_replacement":true});
    payload["text"] = Value::String(text);
    if let Some(status) = slot.payload.get("omnisession_codex_rollback") {
        payload["omnisession_codex_rollback"] = status.clone();
    }
    slot.kind = EventKind::MessageAssistant;
    slot.replay_policy = ReplayPolicy::Contextual;
    slot.source.raw_record_type = Some("response_item.message".to_owned());
    slot.payload = payload;
    true
}
