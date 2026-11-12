use super::replay_messages::{
    MirrorGroup, client_submission, history_event_type, push_record, turn_context_cwd,
};
use crate::codex::{
    EventBuilder, EventKind, InheritedRecord, MAX_CANONICAL_EVENTS, MAX_CANONICAL_TOOL_EVENTS,
    PathBuf, Value, rollout_lines, string_at, workspace_check,
};

struct TurnCheckpoint {
    request_at: u64,
    record: usize,
    collapsed: bool,
    sequence: u64,
    project_path: Option<PathBuf>,
    workspace_record: Option<usize>,
    tool_events: usize,
}

struct NativeTask {
    id: String,
    start: usize,
    skipped: usize,
    last_ordinal: Option<u64>,
}

enum ImplicitTurn {
    Closed,
    RawReply(TurnCheckpoint),
    Open,
}

pub(in crate::codex) struct RecoveredHistory {
    pub(super) project_path: Option<PathBuf>,
    pub(super) settings: workspace_check::WorkspaceSettings,
    pub(super) workspace_record: Option<usize>,
    retain_workspace_path: bool,
    workspace_gaps: usize,
    records_before_workspace_gap: usize,
    pub(in crate::codex) records_seen: usize,
    last_visible: Option<MirrorGroup>,
    turns: Vec<TurnCheckpoint>,
    pruned_turn_checkpoints: bool,
    pub(super) workspace_history_incomplete: bool,
    pending_turn: Option<TurnCheckpoint>,
    seen_client_submission: Option<bool>,
    first_native_request: Option<u64>,
    implicit_turn: ImplicitTurn,
    native_start: Option<NativeTask>,
    tool_events: usize,
    pub(super) omitted_tool_events: usize,
    pub(super) omitted_events: usize,
}

impl RecoveredHistory {
    pub(super) fn new(project_path: Option<PathBuf>) -> Self {
        Self {
            project_path,
            settings: workspace_check::WorkspaceSettings::from_header(None),
            workspace_record: Some(1),
            retain_workspace_path: true,
            workspace_gaps: 0,
            records_before_workspace_gap: 0,
            records_seen: 0,
            last_visible: None,
            turns: Vec::new(),
            pruned_turn_checkpoints: false,
            workspace_history_incomplete: false,
            pending_turn: None,
            seen_client_submission: None,
            first_native_request: None,
            implicit_turn: ImplicitTurn::Closed,
            native_start: None,
            tool_events: 0,
            omitted_tool_events: 0,
            omitted_events: 0,
        }
    }

    /// During preview, a checkpoint holds the position of the cwd record. The second pass reads the surviving path.
    pub(in crate::codex) fn preview() -> Self {
        Self {
            retain_workspace_path: false,
            ..Self::new(None)
        }
    }

    pub(super) fn workspace_unknown(&self) -> bool {
        self.workspace_record.is_none()
    }

    pub(super) fn observe_workspace_gaps(&mut self, skipped: usize) {
        if skipped != self.workspace_gaps {
            self.workspace_gaps = skipped;
            // Only a cwd recorded after this gap can establish the workspace again.
            self.records_before_workspace_gap = self.records_seen;
            self.workspace_record = None;
            self.project_path = None;
        }
    }

    /// Applies the retained tool event, event, and turn checkpoint caps.
    pub(in crate::codex) fn compact(&mut self, builder: &mut EventBuilder) {
        self.omitted_tool_events += builder.retain_latest_tool_events(MAX_CANONICAL_TOOL_EVENTS);
        self.omitted_events += builder.retain_latest_events(MAX_CANONICAL_EVENTS);
        self.turns.retain(|turn| !turn.collapsed);
        let excess = self.turns.len().saturating_sub(MAX_CANONICAL_EVENTS);
        if excess > 0 || self.omitted_events > 0 || self.omitted_tool_events > 0 {
            builder.note_rollback_history_gap();
        }
        self.pruned_turn_checkpoints |= excess > 0;
        self.turns.drain(..excess);
    }

    fn checkpoint(&self, builder: &EventBuilder) -> TurnCheckpoint {
        TurnCheckpoint {
            request_at: builder.checkpoint(),
            record: self.records_seen,
            collapsed: false,
            sequence: builder.checkpoint(),
            project_path: self.project_path.clone(),
            workspace_record: self.workspace_record,
            tool_events: self.tool_events,
        }
    }

    fn separate_request(&mut self, client_submission: bool) -> bool {
        if client_submission && let Some(seen) = &mut self.seen_client_submission {
            return !std::mem::replace(seen, true);
        }
        true
    }

    fn rollback(&mut self, builder: &mut EventBuilder, record: &Value) -> Option<u64> {
        let count = rollout_lines::rollback_turns(&record["payload"])?;
        let mut truncated = None;
        if count > 0 {
            self.turns.retain(|turn| !turn.collapsed);
            let count = usize::try_from(count).unwrap_or(usize::MAX);
            if self.pruned_turn_checkpoints && count > self.turns.len() {
                self.workspace_history_incomplete = true;
            }
            let retained = self.turns.len().saturating_sub(count);
            if retained < self.turns.len() {
                let checkpoint = self.turns.remove(retained);
                self.turns.truncate(retained);
                builder.truncate_from(checkpoint.sequence);
                truncated = Some(checkpoint.sequence);
                self.workspace_record = checkpoint
                    .workspace_record
                    .filter(|position| *position > self.records_before_workspace_gap);
                self.project_path = self.workspace_record.and(checkpoint.project_path);
                self.tool_events = checkpoint.tool_events;
                self.omitted_tool_events = self.omitted_tool_events.min(self.tool_events);
            } else if self.workspace_gaps > 0 {
                self.project_path = None;
                self.workspace_record = None;
            }
            self.native_start = None;
            self.implicit_turn = ImplicitTurn::Closed;
            self.last_visible = None;
            self.pending_turn = None;
            self.seen_client_submission = None;
        }
        truncated
    }

    pub(super) fn observe_inherited_marker(
        &mut self,
        record: &InheritedRecord,
        builder: &EventBuilder,
        inherited: usize,
        recovering: bool,
        withheld_workspaces: &mut usize,
    ) {
        if let InheritedRecord::SegmentEnd(end) = record {
            self.end_segment(*end, recovering);
            return;
        }
        self.native_start = None;
        if matches!(record, InheritedRecord::WithheldWorkspace) {
            *withheld_workspaces += 1;
            self.observe_workspace_gaps(inherited + *withheld_workspaces);
            return;
        }
        if let InheritedRecord::WithheldRequest { client_submission } = record
            && self.separate_request(*client_submission)
        {
            let checkpoint = self
                .pending_turn
                .take()
                .unwrap_or_else(|| self.checkpoint(builder));
            self.turns.push(checkpoint);
        }
        // An intervening reply separates two requests even when its text is withheld.
        MirrorGroup::barrier(&mut self.last_visible);
    }

    fn end_segment(&mut self, end: u64, recovering: bool) {
        if !recovering
            || self
                .native_start
                .as_ref()
                .is_some_and(|task| task.last_ordinal.and_then(|n| n.checked_add(1)) == Some(end))
        {
            self.finish_native_task();
        }
        self.native_start = None;
        self.implicit_turn = ImplicitTurn::Closed;
        self.pending_turn = None;
        self.seen_client_submission = None;
        self.last_visible = None;
    }

    fn finish_native_task(&mut self) {
        let Some(task) = &self.native_start else {
            return;
        };
        let first = self
            .turns
            .partition_point(|checkpoint| checkpoint.record < task.start);
        if first == self.turns.len() {
            if let Some(checkpoint) = self.pending_turn.take() {
                self.turns.push(checkpoint);
            }
        } else {
            for checkpoint in self.turns.iter_mut().skip(first + 1) {
                checkpoint.collapsed = true;
            }
        }
    }

    fn observe_task_boundary(
        &mut self,
        builder: &EventBuilder,
        record: &Value,
        event_type: Option<&str>,
        recovering: bool,
        skipped: usize,
    ) {
        let start = matches!(event_type, Some("task_started" | "turn_started"));
        let ordinal = record["ordinal"].as_u64();
        if let Some(task) = &mut self.native_start {
            let consecutive = task
                .last_ordinal
                .and_then(|n| n.checked_add(1))
                .is_some_and(|n| Some(n) == ordinal);
            if skipped != task.skipped || recovering && !consecutive {
                self.native_start = None;
            } else {
                task.last_ordinal = ordinal;
            }
        }
        if start {
            self.implicit_turn = ImplicitTurn::Closed;
            self.finish_native_task();
            self.pending_turn = Some(self.checkpoint(builder));
            self.first_native_request = None;
            self.native_start = record["payload"]["turn_id"]
                .as_str()
                .filter(|id| !id.is_empty() && (!recovering || ordinal.is_some()))
                .map(|id| NativeTask {
                    id: id.to_owned(),
                    start: self.records_seen,
                    skipped,
                    last_ordinal: ordinal,
                });
            self.seen_client_submission = self.native_start.as_ref().map(|_| false);
            self.last_visible = None;
        } else if matches!(
            event_type,
            Some("task_complete" | "turn_complete" | "turn_aborted")
        ) {
            if builder.codex_saved_users.is_some()
                && matches!(self.implicit_turn, ImplicitTurn::Open)
                && (event_type == Some("turn_aborted") || record["payload"]["turn_id"].is_null())
            {
                return;
            }
            if let Some(task) = &self.native_start {
                let id = &record["payload"]["turn_id"];
                let named = id
                    .as_str()
                    .is_some_and(|id| !id.is_empty() && (!recovering || id == task.id));
                if id.is_null() || (event_type == Some("turn_aborted") && named) {
                    return;
                }
                if named {
                    self.finish_native_task();
                }
            }
            self.native_start = None;
            self.implicit_turn = ImplicitTurn::Closed;
            self.pending_turn = None;
            self.seen_client_submission = None;
            self.last_visible = None;
        }
    }

    fn push_turn_record(
        &mut self,
        builder: &mut EventBuilder,
        record: &Value,
        confirmed: bool,
    ) -> bool {
        let outside_turn = builder.codex_saved_users.is_some()
            && self.native_start.is_none()
            && !matches!(self.implicit_turn, ImplicitTurn::Open);
        if outside_turn
            && builder.codex_saved_users == Some(false)
            && !builder.codex_saved_assistant
            && record["type"] == "response_item"
            && record["payload"]["type"] == "message"
            && record["payload"]["role"] == "assistant"
        {
            self.implicit_turn = ImplicitTurn::RawReply(self.checkpoint(builder));
        }
        let position = builder.checkpoint();
        let user = push_record(builder, record, &mut self.last_visible, confirmed);
        let saved_reply = record["type"] == "event_msg"
            && record["payload"]["type"] == "agent_message"
            && string_at(&record["payload"], &[&["message"], &["text"]])
                .is_some_and(|text| !text.is_empty() && text != omnis_core::CODEX_IMPORT_MARKER);
        let authored_marker = builder.codex_saved_users == Some(true)
            && record["type"] == "response_item"
            && record["payload"]["type"] == "message"
            && record["payload"]["role"] == "assistant"
            && builder.checkpoint() > position;
        let implicit_reply = outside_turn && (saved_reply || authored_marker);
        if !user && !implicit_reply {
            return false;
        }
        let next = if self.native_start.is_some() {
            ImplicitTurn::Closed
        } else {
            ImplicitTurn::Open
        };
        let previous = std::mem::replace(&mut self.implicit_turn, next);
        if implicit_reply
            && self.pending_turn.is_none()
            && let ImplicitTurn::RawReply(raw) = previous
            && let Some(paired) = self
                .last_visible
                .as_ref()
                .and_then(|group| group.paired_position)
            && usize::try_from(raw.sequence) == Ok(paired)
        {
            self.pending_turn = Some(raw);
        }
        true
    }

    pub(in crate::codex) fn push_recovered(
        &mut self,
        builder: &mut EventBuilder,
        record: &Value,
        recovering: bool,
        skipped: usize,
        confirmed: bool,
    ) -> Option<u64> {
        self.records_seen += 1;
        let record_type = record.get("type").and_then(Value::as_str);
        let event_type = (record_type == Some("event_msg"))
            .then(|| history_event_type(&record["payload"]))
            .flatten();
        self.observe_task_boundary(builder, record, event_type, recovering, skipped);
        if event_type == Some("thread_rolled_back") {
            if rollout_lines::rollback_turns(&record["payload"]).is_some_and(|count| count > 0) {
                self.finish_native_task();
            }
            return self.rollback(builder, record);
        }
        if record_type == Some("turn_context") {
            if self.pending_turn.is_none()
                && (self.workspace_unknown()
                    || self
                        .last_visible
                        .as_ref()
                        .is_none_or(|message| message.kind != EventKind::MessageUser))
            {
                self.pending_turn = Some(self.checkpoint(builder));
            }
            let cwd = turn_context_cwd(record);
            self.workspace_record = cwd.map(|_| self.records_seen);
            if self.retain_workspace_path {
                self.project_path = cwd.map(PathBuf::from);
            }
        }
        let checkpoint = self.checkpoint(builder);
        let client_submission = client_submission(record);
        if self.push_turn_record(builder, record, confirmed) {
            if let Some(start) = self
                .last_visible
                .as_mut()
                .and_then(|group| group.batch_checkpoint.take())
            {
                let first = self.turns.partition_point(|turn| turn.request_at < start);
                self.turns.truncate(first.saturating_add(1));
                self.pending_turn = None;
            } else if self.separate_request(client_submission) {
                let at = checkpoint.request_at;
                let mut checkpoint = self.pending_turn.take().unwrap_or(checkpoint);
                checkpoint.request_at = at;
                self.turns.push(checkpoint);
                if client_submission && self.seen_client_submission.is_some() {
                    self.first_native_request = Some(at);
                }
            }
        } else if client_submission && self.seen_client_submission.is_some() {
            if let Some(at) = self
                .last_visible
                .as_ref()
                .and_then(|group| group.paired_position)
                && let Ok(at) = u64::try_from(at)
            {
                if let Some(first) = self.first_native_request
                    && let Ok(index) = self
                        .turns
                        .binary_search_by_key(&first.max(at), |turn| turn.request_at)
                    && first != at
                {
                    self.turns[index].collapsed = true;
                }
                self.first_native_request =
                    Some(self.first_native_request.map_or(at, |first| first.min(at)));
                self.seen_client_submission = Some(true);
            }
        } else if record_type == Some("response_item")
            && record["payload"]["type"] != "message"
            && builder.checkpoint() > checkpoint.sequence
        {
            self.tool_events += 1;
        }
        None
    }
}
