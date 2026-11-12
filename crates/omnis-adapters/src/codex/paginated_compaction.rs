//! Match replacement messages to the public display and track which checkpoint survives rollback.

use super::{
    EventBuilder, PaginatedRollback, Withdrawal,
    paginated_checkpoint::{
        CheckpointHistory, CheckpointSelection, EffectiveRollback, LegacySource,
        PaginatedCheckpoint,
    },
};
use omnis_ir::EventKind;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, VecDeque},
    ops::Range,
};

type TextKey = (bool, [u8; 32]);

#[derive(Default)]
pub(super) struct DisplayEvidence {
    text: HashMap<TextKey, Option<u64>>,
}

impl DisplayEvidence {
    pub(super) fn collect(builder: &EventBuilder) -> Self {
        let mut display = Self::default();
        for e in builder.events() {
            if e.source.raw_record_type.as_deref() != Some("event_msg.item_completed") {
                continue;
            }
            if let Some(k) = Self::key(&e.kind, &e.payload) {
                display
                    .text
                    .entry(k)
                    .and_modify(|seq| *seq = None)
                    .or_insert(Some(e.sequence));
            }
        }
        display
    }

    fn key(kind: &EventKind, payload: &Value) -> Option<TextKey> {
        let user = match kind {
            EventKind::MessageUser => true,
            EventKind::MessageAssistant => false,
            _ => return None,
        };
        Some((
            user,
            Sha256::digest(payload["text"].as_str()?.as_bytes()).into(),
        ))
    }

    pub(super) fn unique_unscoped(&self, builder: &mut EventBuilder, sequence: u64) -> Option<u64> {
        let projected = builder.event_at_mut(sequence)?;
        let key = Self::key(&projected.kind, &projected.payload)?;
        self.text.get(&key).copied().flatten()
    }
}

/// Canonical positions emitted by each item in an explicit model replacement.
#[derive(Default)]
pub(super) struct ProjectedReplacement {
    starts: VecDeque<(usize, u64)>,
    tool_sources: HashMap<u64, usize>,
    questions: HashMap<u64, u64>,
}

impl ProjectedReplacement {
    pub(super) fn record_start(&mut self, item: usize, builder: &EventBuilder) {
        let start = builder.checkpoint();
        let first = builder.first_sequence();
        while self.starts.get(1).is_some_and(|(_, end)| *end <= first) {
            self.starts.pop_front();
        }
        if self
            .starts
            .back()
            .is_some_and(|(_, previous)| *previous == start)
        {
            self.starts.pop_back();
        }
        self.starts.push_back((item, start));
    }

    pub(super) fn project_tool(
        &mut self,
        builder: &mut EventBuilder,
        index: usize,
        source: Option<u64>,
        timestamp: Option<chrono::DateTime<chrono::Utc>>,
    ) {
        if let Some(seq) = source
            && let Some(original) = builder.event_at_mut(seq)
            && original.replay_policy == omnis_ir::ReplayPolicy::HistoricalOnly
        {
            if let std::collections::hash_map::Entry::Vacant(entry) = self.tool_sources.entry(seq) {
                entry.insert(index);
                let kind = original.kind.clone();
                let mut payload = original.payload.clone();
                for field in ["omnisession_codex_rollback", "omnisession_codex_checkpoint"] {
                    if let Some(fields) = payload.as_object_mut() {
                        fields.remove(field);
                    }
                }
                builder.push(
                    kind,
                    payload,
                    timestamp,
                    omnis_ir::ReplayPolicy::HistoricalOnly,
                    Some("omnisession.codex_checkpoint_tool".into()),
                    None,
                );
            }
            return;
        }
        builder.push(EventKind::ProviderEvent, serde_json::json!({"omitted_events":1,"reason":"The Codex checkpoint tool record has no matching source receipt."}), timestamp,
            omnis_ir::ReplayPolicy::HistoricalOnly, Some("omnisession.codex_compaction_unavailable".into()), None);
    }

    pub(super) fn project_question(
        &mut self,
        builder: &mut EventBuilder,
        source: u64,
        text: String,
        timestamp: Option<chrono::DateTime<chrono::Utc>>,
    ) {
        if let std::collections::hash_map::Entry::Vacant(entry) = self.questions.entry(source) {
            entry.insert(builder.checkpoint());
            let mut payload = serde_json::json!({"omnisession_codex_replacement":true});
            payload["text"] = text.into();
            builder.push(
                EventKind::MessageAssistant,
                payload,
                timestamp,
                omnis_ir::ReplayPolicy::Contextual,
                Some("response_item.message".into()),
                None,
            );
        }
    }

    fn retire(&self, builder: &mut EventBuilder, evidence: &PaginatedCheckpoint) {
        for (&source, &sequence) in &self.questions {
            if evidence.status(source, None, None).is_some() {
                retire(builder, sequence);
            }
        }
        let ambiguous: std::collections::HashSet<_> = self
            .tool_sources
            .iter()
            .filter_map(|(&seq, &index)| {
                evidence.status(seq, None, None).is_some().then_some(index)
            })
            .collect();
        let first = builder.first_sequence();
        for (&(item, start), &(_, end)) in self.starts.iter().zip(self.starts.iter().skip(1)) {
            if evidence.item_withdrawn(item) || ambiguous.contains(&item) {
                for sequence in start.max(first)..end {
                    retire(builder, sequence);
                }
            }
        }
    }
}

pub(super) enum WindowKind {
    Replacement,
    Opaque,
    Legacy { summary: Option<u64> },
}

impl WindowKind {
    pub(super) fn from_checkpoint(legacy: bool, explicit: bool, summary: Option<u64>) -> Self {
        if legacy {
            Self::Legacy { summary }
        } else if explicit {
            Self::Replacement
        } else {
            Self::Opaque
        }
    }
}

pub(super) struct Window {
    pub(super) range: Range<u64>,
    pub(super) ordinal: u64,
    pub(super) kind: WindowKind,
    pub(super) projection: ProjectedReplacement,
}

#[derive(Default)]
pub(super) struct PaginatedCompactions {
    windows: VecDeque<Window>,
    public: CheckpointHistory,
}

impl PaginatedCompactions {
    pub(super) fn record(
        &mut self,
        window: Window,
        evidence: Option<PaginatedCheckpoint>,
        legacy_message: Option<&str>,
        inputs: &PaginatedRollback,
        selection: CheckpointSelection,
    ) {
        if let Some(message) = legacy_message {
            self.public.record_legacy(
                window.ordinal,
                window.range.start,
                message,
                inputs,
                selection,
            );
        } else {
            self.public.record(evidence, selection);
        }
        self.windows.push_back(window);
    }

    pub(super) fn has_model_window(&self) -> bool {
        self.public.as_ref().is_some() || self.public.saw_legacy()
    }

    pub(super) fn selected_checkpoint(&self) -> Option<&PaginatedCheckpoint> {
        self.public.as_ref()
    }

    pub(super) fn public_status(
        &self,
        sequence: u64,
        owner: Option<u64>,
        baseline: Option<Withdrawal>,
    ) -> Option<Withdrawal> {
        if self.public.is_empty() {
            baseline
        } else {
            self.public.status(sequence, owner, baseline)
        }
    }

    pub(super) fn latest(&self) -> Option<(u64, u64)> {
        let selected = self
            .public
            .as_ref()
            .map(PaginatedCheckpoint::source_ordinal);
        self.windows
            .iter()
            .rev()
            .find(|w| !matches!(w.kind, WindowKind::Replacement) || Some(w.ordinal) == selected)
            .map(|w| (w.ordinal, w.range.end))
    }

    pub(super) fn finish(&self, builder: &mut EventBuilder) {
        let selected = self.public.as_ref();
        let base = self.public.selected_base();
        for window in &self.windows {
            if let WindowKind::Legacy { summary: Some(seq) } = window.kind
                && self.public.saw_legacy()
                && !self.public.legacy_summary_active(window.ordinal)
            {
                retire(builder, seq);
                if let Some(boundary) = builder.event_at_mut(window.range.start) {
                    boundary.payload["omnisession_codex_summary_withdrawn"] = true.into();
                }
            }
            if !matches!(window.kind, WindowKind::Replacement) {
                continue;
            }
            if let Some(evidence) =
                selected.filter(|evidence| evidence.source_ordinal() == window.ordinal)
            {
                window.projection.retire(builder, evidence);
            } else if match base {
                CheckpointSelection::Selected(ordinal) => {
                    window.ordinal > ordinal || selected.is_none()
                }
                CheckpointSelection::Original | CheckpointSelection::Unverified => true,
            } {
                for seq in window.range.start.max(builder.first_sequence())..window.range.end {
                    retire(builder, seq);
                }
            }
        }
    }

    pub(super) fn discard_before(&mut self, first: u64) {
        while self.windows.front().is_some_and(|w| w.range.end <= first) {
            self.windows.pop_front();
        }
    }

    pub(super) fn contains(&self, sequence: u64) -> bool {
        self.windows
            .get(self.windows.partition_point(|w| w.range.end <= sequence))
            .is_some_and(|w| w.range.contains(&sequence))
    }

    pub(super) fn ordinal(&self, boundary: u64) -> Option<u64> {
        self.windows
            .iter()
            .rev()
            .find(|window| window.range.start == boundary)
            .map(|window| window.ordinal)
    }

    pub(super) fn public_history(&self) -> &CheckpointHistory {
        &self.public
    }

    pub(super) fn legacy_users(
        &self,
        boundary: u64,
    ) -> impl Iterator<Item = (u64, Option<usize>)> + '_ {
        self.ordinal(boundary)
            .into_iter()
            .flat_map(|ordinal| self.public.legacy_retained_items(ordinal))
            .filter_map(|(source, budget)| {
                let sequence = match source {
                    LegacySource::User(seq) => seq,
                    LegacySource::Summary(ordinal) => {
                        let index = self
                            .windows
                            .binary_search_by_key(&ordinal, |w| w.ordinal)
                            .ok()?;
                        let WindowKind::Legacy { summary: Some(seq) } = self.windows[index].kind
                        else {
                            return None;
                        };
                        seq
                    }
                };
                Some((sequence, budget))
            })
    }

    pub(super) fn apply_rollback(
        &mut self,
        inputs: &PaginatedRollback,
        count: usize,
        selection: CheckpointSelection,
    ) -> EffectiveRollback {
        self.public.rollback(count, inputs, selection)
    }
}

fn retire(builder: &mut EventBuilder, seq: u64) {
    if let Some(e) = builder.event_at_mut(seq) {
        e.payload["omnisession_codex_rollback"] = "withdrawn".into();
    }
}
