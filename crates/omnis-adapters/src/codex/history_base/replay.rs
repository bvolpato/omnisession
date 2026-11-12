use super::recovered::RecoveredHistory;
use crate::codex::{
    CodexHistory, CodexHistoryMode, CodexSession, EventBuilder, EventKind, InheritedRecord,
    JsonLineSkips, Lineage, MAX_CANONICAL_EVENTS, MAX_CANONICAL_TOOL_EVENTS, MAX_PREVIEW_TAIL_SIZE,
    MAX_STREAMED_TRANSCRIPT_LINE_SIZE, PathBuf, ReplayPolicy, Result, RolloutStamp,
    TOOL_COMPACTION_RECORD_INTERVAL, UNAVAILABLE_ANCESTRY, Value, WorkspaceRecords, anyhow, fs,
    json, ordinary_rollback_record, rollout_file, string_at, workspace_check,
};
use crate::support::JsonLinesLimits;

struct Projection {
    native: bool,
    builder: EventBuilder,
    workspace_record: Option<usize>,
    project: Option<PathBuf>,
    settings: workspace_check::WorkspaceSettings,
    incomplete: bool,
    unknown: bool,
    source_prefix_end: Option<u64>,
}

pub(in crate::codex) fn canonical_events_into(
    session: &CodexSession,
    lineage: &Lineage,
    builder: EventBuilder,
    workspace_record: Option<usize>,
    read: impl FnOnce(&mut dyn FnMut(Value, JsonLineSkips, bool) -> Result<()>) -> Result<JsonLineSkips>,
) -> Result<(EventBuilder, Option<PathBuf>, bool)> {
    let mut projection = replay(session, lineage, builder, false, workspace_record, read)?;
    let project = projection.settings.directory(projection.project);
    if project.is_none() && (projection.unknown || projection.settings.has_owned()) {
        projection
            .builder
            .mark_codex_workspace_unknown(session.updated_at);
    }
    Ok((
        projection.builder,
        project,
        projection.settings.has_owned() || !projection.incomplete,
    ))
}

mod native_messages;

fn native_document(record: &Value) -> bool {
    if crate::codex::has_native_request_evidence(record) {
        return true;
    }
    match record["type"].as_str() {
        Some("compacted" | "realtime_item" | "inter_agent_communication") => true,
        Some("response_item") => record["payload"]["type"].as_str().is_some_and(|kind| {
            kind.ends_with("_call")
                || kind.ends_with("_call_output")
                || kind == "tool_search_output"
        }),
        Some("event_msg") => {
            let payload = &record["payload"];
            let kind = payload["type"]
                .as_str()
                .or_else(|| payload.get(0)?.as_str());
            match kind {
                Some("item_completed" | "item_started") => {
                    let item = payload.get("item").or_else(|| payload.get(3));
                    item.and_then(|i| i.get("type").or_else(|| i.get(0)))
                        .and_then(Value::as_str)
                        .is_some_and(|kind| kind != "UserMessage")
                }
                Some(kind) => {
                    kind.starts_with("exec_command_")
                        || kind.starts_with("mcp_tool_call_")
                        || kind.starts_with("patch_apply_")
                }
                None => false,
            }
        }
        _ => false,
    }
}

fn replay(
    session: &CodexSession,
    lineage: &Lineage,
    builder: EventBuilder,
    preview: bool,
    workspace_record: Option<usize>,
    read: impl FnOnce(&mut dyn FnMut(Value, JsonLineSkips, bool) -> Result<()>) -> Result<JsonLineSkips>,
) -> Result<Projection> {
    let mut replay = Replay::new(session, builder, preview, workspace_record);
    let inherited = lineage.visit(|row, skips, recovery, end| {
        replay.observe(row, JsonLineSkips::default(), skips, recovery, end, false)
    })?;
    let mut first_leaf = true;
    let unreadable =
        lineage.visit_confirmed_leaf_lines(&session.id, read, |row, skips, end, proof| {
            replay.observe(
                InheritedRecord::request(row, proof),
                skips,
                inherited,
                false,
                end,
                std::mem::take(&mut first_leaf),
            )
        })?;
    Ok(replay.finish(lineage, inherited, unreadable))
}

#[derive(Default, Eq, PartialEq)]
enum ModelContexts {
    #[default]
    None,
    First(String),
    Distinct,
    NamedTask,
}

#[derive(Default)]
pub(in crate::codex) struct Protocol {
    use_native: bool,
    contexts: ModelContexts,
    published_user: bool,
    transcript: TranscriptEvidence,
}

#[derive(Default)]
struct TranscriptEvidence {
    has_message_body: bool,
    saved_user: bool,
    legacy_wire: bool,
}

impl Protocol {
    pub(in crate::codex) fn observe(&mut self, session: &CodexSession, record: &Value) {
        if !session.needs_visible_projection() && session.history_mode == CodexHistoryMode::Legacy {
            if record["type"] == "event_msg"
                && record["payload"]["type"] == "task_started"
                && record["payload"]["turn_id"]
                    .as_str()
                    .is_some_and(|id| !id.is_empty())
            {
                self.contexts = ModelContexts::NamedTask;
            } else if record["type"] == "turn_context"
                && let Some(id) = record["payload"]["turn_id"]
                    .as_str()
                    .filter(|id| !id.is_empty())
            {
                match &self.contexts {
                    ModelContexts::None => self.contexts = ModelContexts::First(id.to_owned()),
                    ModelContexts::First(first) if first != id => {
                        self.contexts = ModelContexts::Distinct;
                    }
                    _ => {}
                }
            }
        }
        self.use_native |= native_document(record)
            || !session.needs_visible_projection() && native_messages::requires_native(record);
        self.transcript.legacy_wire |=
            record["type"] == "session_meta" && record["payload"].get("history_mode").is_none();
        self.transcript.saved_user |=
            record["type"] == "event_msg" && record["payload"]["type"] == "user_message";
        self.published_user |= record["type"] == "event_msg"
            && record["payload"]["type"] == "item_completed"
            && record["payload"]["item"]["type"] == "UserMessage";
        self.transcript.has_message_body |= record["type"] == "response_item"
            && record["payload"]["type"] == "message"
            || record["type"] == "event_msg"
                && matches!(
                    record["payload"]["type"].as_str(),
                    Some("user_message" | "agent_message")
                );
    }
    pub(in crate::codex) fn native(&self, session: &CodexSession) -> bool {
        self.use_native
            || self.published_user
                && (!self.transcript.has_message_body
                    || self.transcript.legacy_wire
                        && !session.needs_visible_projection()
                        && !self.transcript.saved_user)
            || self.contexts == ModelContexts::Distinct
    }
}

struct Replay<'a> {
    session: &'a CodexSession,
    protocol: Protocol,
    native_builder: EventBuilder,
    builder: EventBuilder,
    native: CodexHistory,
    history: RecoveredHistory,
    workspace_record: Option<usize>,
    first_gap: Option<u64>,
    native_gap: Option<u64>,
    withheld_workspaces: usize,
    selected_project: Option<PathBuf>,
    native_project: Option<PathBuf>,
    malformed: usize,
}

impl<'a> Replay<'a> {
    fn new(
        session: &'a CodexSession,
        builder: EventBuilder,
        preview: bool,
        workspace_record: Option<usize>,
    ) -> Self {
        let native_builder = if preview {
            let mut events = session.event_builder();
            events.track_rollback_opener(true);
            events
        } else {
            builder.empty_with_same_plan()
        };
        let mut native = CodexHistory::new(session.project_path.clone());
        let mut history = if preview {
            RecoveredHistory::preview()
        } else {
            RecoveredHistory::new(session.project_path.clone())
        };
        if session.needs_visible_projection() {
            history.records_seen = 1;
            native.records_seen = 1;
        }
        Self {
            session,
            protocol: Protocol::default(),
            native_builder,
            builder,
            native,
            history,
            workspace_record,
            first_gap: None,
            native_gap: None,
            withheld_workspaces: 0,
            selected_project: None,
            native_project: None,
            malformed: 0,
        }
    }
    fn observe(
        &mut self,
        record: InheritedRecord,
        mut skipped: JsonLineSkips,
        inherited: usize,
        recovering: bool,
        terminated: bool,
        first_leaf: bool,
    ) -> Result<()> {
        let confirmed = matches!(record, InheritedRecord::ConfirmedRequest(_));
        let header = matches!(record, InheritedRecord::SegmentStart(_));
        let (InheritedRecord::Native(record)
        | InheritedRecord::ConfirmedRequest(record)
        | InheritedRecord::SegmentStart(record)) = record
        else {
            self.history.observe_inherited_marker(
                &record,
                &self.builder,
                inherited,
                recovering,
                &mut self.withheld_workspaces,
            );
            if matches!(record, InheritedRecord::WithheldWorkspace) {
                self.native.observe_workspace_gaps(
                    inherited + self.withheld_workspaces,
                    self.native_builder.checkpoint(),
                );
            }
            return Ok(());
        };
        if !header {
            if self.workspace_record == Some(self.history.records_seen + 1) {
                self.selected_project =
                    string_at(&record, &[&["payload", "cwd"]]).map(PathBuf::from);
            }
            if self.builder.tracks_rollback_opener()
                && !ordinary_rollback_record(&record, self.history.records_seen == 0)
            {
                self.builder.discard_rollback_opener();
            }
            self.history.observe_workspace_gaps(
                skipped.workspace_gaps() + inherited + self.withheld_workspaces,
            );
        }
        if self.native_builder.tracks_rollback_opener()
            && !ordinary_rollback_record(&record, self.native.records_seen == 0)
        {
            self.native_builder.discard_rollback_opener();
        }
        self.native.observe_malformed(&mut self.malformed, skipped);
        self.native.observe_workspace_gaps(
            skipped.workspace_gaps() + inherited + self.withheld_workspaces,
            self.native_builder.checkpoint(),
        );
        skipped.possible_settings += inherited;
        if skipped.total() + inherited > 0 && self.first_gap.is_none() {
            self.first_gap = Some(self.builder.checkpoint());
            self.native_gap = Some(self.native_builder.checkpoint());
            self.builder.note_rollback_history_gap();
            self.native_builder.note_rollback_history_gap();
        }
        if !header {
            self.history.settings.observe_gaps(skipped);
            self.history.settings.observe(&self.session.id, &record);
            self.history.push_recovered(
                &mut self.builder,
                &record,
                recovering,
                skipped.total() + inherited,
                confirmed,
            );
            if self
                .history
                .records_seen
                .is_multiple_of(TOOL_COMPACTION_RECORD_INTERVAL)
            {
                self.history.compact(&mut self.builder);
            }
        }
        self.push_native(record, skipped, terminated, header || first_leaf)
    }
    fn push_native(
        &mut self,
        mut record: Value,
        skipped: JsonLineSkips,
        terminated: bool,
        segment_start: bool,
    ) -> Result<()> {
        if self.workspace_record == Some(self.native.records_seen + 1) {
            self.native_project = string_at(&record, &[&["payload", "cwd"]]).map(PathBuf::from);
        }
        if segment_start && record["type"] == "session_meta" && self.native.source_ordinal > 0 {
            self.native.rollout_lines.start_segment(&record);
            self.native.paginated = self.native.rollout_lines.paginated();
            self.native.items.tool_projection.paginated = self.native.paginated;
        }
        self.protocol.observe(self.session, &record);
        self.native.settings.observe_gaps(skipped);
        if self.session.history_mode != CodexHistoryMode::Paginated
            || WorkspaceRecords::accepts(true, &record)
        {
            self.native.settings.observe(&self.session.id, &record);
        }
        self.native
            .push(&mut self.native_builder, &mut record, terminated)?;
        if self
            .native
            .records_seen
            .is_multiple_of(TOOL_COMPACTION_RECORD_INTERVAL)
        {
            self.native.compact(&mut self.native_builder);
        }
        Ok(())
    }
    fn finish(
        mut self,
        lineage: &Lineage,
        inherited: usize,
        mut unreadable: JsonLineSkips,
    ) -> Projection {
        let gaps = unreadable.workspace_gaps() + inherited + self.withheld_workspaces;
        self.history.observe_workspace_gaps(gaps);
        self.native
            .observe_workspace_gaps(gaps, self.native_builder.checkpoint());
        self.native
            .observe_malformed(&mut self.malformed, unreadable);
        unreadable.possible_settings += inherited;
        unreadable.oversized += inherited;
        let native = self.protocol.native(self.session);
        let (builder, selected, project, settings, incomplete, unknown, omitted, tools, gap) =
            if native {
                self.native.settings.observe_gaps(unreadable);
                self.native.compact(&mut self.native_builder);
                self.native.finish(&mut self.native_builder);
                let unknown = self.native.workspace.unknown();
                (
                    self.native_builder,
                    self.native.workspace.record,
                    if self.workspace_record.is_some() {
                        self.native_project
                    } else {
                        self.native.workspace.path
                    },
                    self.native.settings,
                    self.native.workspace.incomplete,
                    unknown,
                    self.native.omitted_events,
                    self.native.omitted_tool_events,
                    self.native_gap,
                )
            } else {
                if self.session.history_mode == CodexHistoryMode::Legacy
                    && (self.native.rollout_lines.skipped > 0
                        || self.native.rollout_lines.invalid_messages > 0
                        || (self.protocol.transcript.legacy_wire
                            && self.native.uncertain_legacy_rollback))
                {
                    self.native.report_unverified_history(&mut self.builder);
                }
                self.history.settings.observe_gaps(unreadable);
                self.history.compact(&mut self.builder);
                let unknown = self.history.workspace_unknown();
                (
                    self.builder,
                    self.history.workspace_record,
                    if self.workspace_record.is_some() {
                        self.selected_project
                    } else {
                        self.history.project_path
                    },
                    self.history.settings,
                    self.history.workspace_history_incomplete,
                    unknown,
                    self.history.omitted_events,
                    self.history.omitted_tool_events,
                    self.first_gap,
                )
            };

        let projection = Projection {
            native,
            builder,
            workspace_record: selected,
            project,
            settings,
            incomplete,
            unknown,
            source_prefix_end: None,
        };
        projection.finish(
            self.session,
            lineage,
            unreadable,
            gap,
            (omitted, tools),
            self.workspace_record.is_some(),
        )
    }
}

impl Projection {
    fn bounded_preview(
        mut self,
        session: &CodexSession,
        per_edge: usize,
    ) -> (EventBuilder, Option<PathBuf>, bool, bool) {
        let project = self.settings.directory(self.project);
        let owned = self.settings.has_owned();
        if project.is_none() && (owned || self.unknown) {
            self.builder
                .mark_codex_workspace_unknown(session.updated_at);
        }
        let (omitted, prefix) = self
            .builder
            .retain_bounded_preview(rollout_file::SAMPLE_RECORDS, per_edge);
        session.finish_preview_prefix(
            &mut self.builder,
            Some(omitted),
            self.source_prefix_end.map_or(prefix, |end| end.min(prefix)),
            self.source_prefix_end.is_none(),
        );
        (
            self.builder,
            project,
            omitted == 0,
            owned || !self.incomplete,
        )
    }

    fn finish(
        mut self,
        session: &CodexSession,
        lineage: &Lineage,
        unreadable: JsonLineSkips,
        gap: Option<u64>,
        (omitted, tools): (usize, usize),
        loading_workspace: bool,
    ) -> Self {
        if loading_workspace && self.workspace_record == Some(1) && self.project.is_none() {
            self.project.clone_from(&session.project_path);
        }
        let prefix_end = gap.unwrap_or_else(|| self.builder.checkpoint());
        self.source_prefix_end = if lineage.unavailable.is_some() || omitted > 0 {
            Some(0)
        } else {
            (unreadable.total() > 0).then_some(prefix_end)
        };
        if unreadable.total() > 0 {
            self.builder.note_rollback_history_gap();
        }
        if session.needs_visible_projection() || session.is_subagent {
            self.builder.discard_rollback_opener();
        }
        if omitted > 0 {
            self.builder.push(
                EventKind::ProviderEvent,
                json!({"omitted_events":omitted,"retained_latest_events":MAX_CANONICAL_EVENTS}),
                session.updated_at,
                ReplayPolicy::HistoricalOnly,
                Some("omnisession.codex_event_limit".into()),
                None,
            );
        }
        if tools > 0 {
            self.builder.push(EventKind::ProviderEvent,json!({"omitted_events":tools,"omitted_tool_events":tools,"event_kind":"tool","retained_latest_tool_events":MAX_CANONICAL_TOOL_EVENTS}),session.updated_at,ReplayPolicy::HistoricalOnly,Some("omnisession.codex_tool_limit".into()),None);
        }
        self.builder.push_json_line_omissions(
            unreadable,
            MAX_STREAMED_TRANSCRIPT_LINE_SIZE,
            Some(prefix_end),
            session.updated_at,
        );
        if let Some(reason) = lineage.unavailable {
            self.incomplete |= self.workspace_record.is_none_or(|r| r <= 1);
            self.builder.push(
                EventKind::ProviderEvent,
                json!({"omitted_events":1,"native_history_unverified":true,"reason":reason}),
                session.updated_at,
                ReplayPolicy::HistoricalOnly,
                Some(
                    if session.history_base.mode_is_unverified() {
                        "omnisession.codex_history_mode"
                    } else {
                        UNAVAILABLE_ANCESTRY
                    }
                    .into(),
                ),
                None,
            );
        }
        for ty in lineage.hidden_context_types() {
            self.builder.push(
                EventKind::ProviderEvent,
                json!({}),
                session.updated_at,
                ReplayPolicy::HistoricalOnly,
                Some(ty.into()),
                None,
            );
        }
        self
    }
}

pub(in crate::codex) fn reconstructed_lineage_preview(
    session: &CodexSession,
    lineage: &Lineage,
    limits: JsonLinesLimits,
) -> Result<(EventBuilder, Option<PathBuf>, bool, bool)> {
    let stamp = || {
        lineage
            .source_paths(&session.path)
            .map(|path| fs::metadata(path).map(|metadata| RolloutStamp::new(&metadata)))
            .collect::<std::io::Result<Vec<_>>>()
    };
    let before = stamp()?;
    let per_edge = usize::try_from(MAX_PREVIEW_TAIL_SIZE)?;
    let mut builder = session.event_builder();
    builder.plan_preview(None);
    builder.track_rollback_opener(false);
    let (
        (selected, omitted, prefix_end),
        workspace_record,
        source_prefix_end,
        first_settings,
        first_title,
    ) = {
        let projection = replay(session, lineage, builder, true, None, |visit| {
            rollout_file::visit_with_limits(&session.path, limits, visit)
        })?;
        if projection.native {
            if stamp()? != before {
                return Err(anyhow!(
                    "Codex rollout changed while reconstructing its preview"
                ));
            }
            return Ok(projection.bounded_preview(session, per_edge));
        }
        let Projection {
            builder: mut plan,
            workspace_record,
            source_prefix_end,
            settings,
            ..
        } = projection;
        (
            plan.select_preview(rollout_file::SAMPLE_RECORDS, per_edge),
            workspace_record,
            source_prefix_end,
            settings,
            plan.rollback_title(),
        )
    };
    let expected = selected.len();
    let mut builder = session.event_builder();
    builder.plan_preview(Some(selected.clone()));
    builder.track_rollback_opener(false);
    let mut history = replay(session, lineage, builder, true, workspace_record, |visit| {
        rollout_file::visit_with_limits(&session.path, limits, visit)
    })?;
    let events = &mut history.builder;
    if history.native
        || stamp()? != before
        || history.workspace_record != workspace_record
        || history.settings != first_settings
        || events.rollback_title() != first_title
        || history.source_prefix_end != source_prefix_end
        || events.select_preview(rollout_file::SAMPLE_RECORDS, per_edge)
            != (selected, omitted, prefix_end)
    {
        return Err(anyhow!(
            "Codex rollout changed while reconstructing its preview"
        ));
    }
    events.finish_preview(expected)?;
    let verified_eof =
        omitted == 0 && source_prefix_end.is_none() && prefix_end == events.checkpoint();
    let workspace_unknown = history.unknown;
    let project = history.settings.directory(history.project);
    let owned_settings = history.settings.has_owned();
    if project.is_none() && (workspace_unknown || owned_settings) {
        events.mark_codex_workspace_unknown(session.updated_at);
    }
    let prefix_end = if verified_eof {
        events.checkpoint()
    } else {
        source_prefix_end.map_or(prefix_end, |end| end.min(prefix_end))
    };
    session.finish_preview_prefix(
        events,
        Some(omitted),
        prefix_end,
        source_prefix_end.is_none(),
    );
    Ok((
        history.builder,
        project,
        omitted == 0,
        owned_settings || !history.incomplete,
    ))
}
