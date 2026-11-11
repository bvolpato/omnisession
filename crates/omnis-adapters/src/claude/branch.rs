//! Select the conversation that Claude would continue after a rewind.

use std::{
    collections::{HashMap, HashSet},
    num::NonZeroUsize,
    ops::Range,
};

use omnis_ir::EventKind;
use serde_json::Value;
use uuid::Uuid;

use super::{
    ClaudeEvent, ClaudeRecord, Compactions, RejectionScope, ResponseId,
    empty_cutoff::{CheckpointClock, Choice},
    response_id, string_at,
};

#[derive(Clone, Copy, Default)]
pub(super) struct Parent {
    pub(super) node: Option<usize>,
    // Transparent records keep the physical conversation parent across summary rewrites.
    pub(super) source: Option<usize>,
    pub(super) transparent: bool,
    // One-based source order of the nearest opaque native record on this path.
    opaque_link_order: Option<NonZeroUsize>,
    unverified: Option<usize>,
    suffix: Option<Uuid>,
    pub(super) missing_ancestor: bool,
}

impl Parent {
    fn through_link(mut self, record: &Value, order: usize) -> Self {
        self.transparent = true;
        if super::is_transcript_record(record) {
            self.opaque_link_order = NonZeroUsize::new(order + 1);
        }
        self
    }
}

#[derive(Clone, Copy)]
pub(super) struct RetainedParent {
    pub(super) uuid: Uuid,
    pub(super) node: Option<usize>,
    source: Option<usize>,
}

pub(super) enum Detached {
    Timestamp { uncertain: bool },
    // The selected summary can survive even when its retained replies do not.
    Attachment { anchor: Option<Uuid> },
}

#[derive(Clone, Copy)]
struct OpaqueReturn {
    cutoff: usize,
    anchor: Option<Uuid>,
}

#[derive(Clone, Copy)]
pub(super) struct TrailingPath {
    pub(super) cutoff: usize,
    pub(super) start: usize,
    tail: usize,
}

#[derive(Default)]
struct Node {
    has_children: bool,
    uuid: Option<Uuid>,
    parent: Option<usize>,
    summary: Option<usize>,
    events: Range<usize>,
    response: Option<ResponseId>,
    turn: usize,
    writer: Option<String>,
    user: bool,
    missing_ancestor: bool,
}

type ResponseKey<'a> = (&'a ResponseId, Option<&'a str>, usize, Option<usize>);

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum ResponseUnit<'a> {
    Named(ResponseKey<'a>),
    Record(usize),
}

impl Node {
    fn response_key(&self, summary: Option<usize>) -> Option<ResponseKey<'_>> {
        Some((
            self.response.as_ref()?,
            self.writer.as_deref(),
            self.turn,
            summary,
        ))
    }
}

#[derive(Default)]
pub(super) struct Ambiguity {
    pub(super) results: bool,
    pub(super) missing_source: bool,
    pub(super) assistants: bool,
}

fn is_tool_outcome(event: &ClaudeEvent) -> bool {
    matches!(event.kind, EventKind::ToolCompleted | EventKind::ToolFailed)
}

#[derive(Default)]
pub(super) struct Rewind {
    pub(super) ambiguous_response: bool,
    pub(super) excluded: HashSet<usize>,
    excluded_paths: HashMap<usize, usize>,
    retained_results: HashMap<usize, usize>,
    archival_results: HashSet<usize>,
    call_scopes: HashMap<usize, Vec<usize>>,
}

impl Rewind {
    pub(super) fn keep_calls<'a>(
        &mut self,
        summary: usize,
        kept: &[usize],
        branch: &'a Branch,
        events: &'a [ClaudeEvent],
        queries: &mut RetainedCallQueries<'a>,
    ) {
        for &index in kept {
            let node = &branch.nodes[index];
            for position in node.events.clone() {
                let event = &events[position];
                if event.kind != EventKind::ToolCalled
                    || self.call_scopes(position).last() == Some(&summary)
                {
                    continue;
                }
                let Some(id) = event.payload["id"].as_str() else {
                    continue;
                };
                // Two distinct calls establish that an ID alone is ambiguous. Even an
                // overlapping retained segment needs at most two positions per lookup.
                let mut first_two = false;
                for owner in branch.candidate_owners(index) {
                    if let Some(count) = queries.by_id.get_mut(&(node.writer.as_deref(), owner, id))
                        && *count < 2
                    {
                        *count += 1;
                        first_two = true;
                    }
                }
                if first_two || queries.exact.contains(&position) {
                    self.call_scopes.entry(position).or_default().push(summary);
                }
            }
        }
    }

    fn call_scopes(&self, position: usize) -> &[usize] {
        self.call_scopes.get(&position).map_or(&[], Vec::as_slice)
    }

    fn epochs_for_call(
        &self,
        position: usize,
        original: Option<usize>,
    ) -> impl Iterator<Item = Option<usize>> {
        std::iter::once(original).chain(self.call_scopes(position).iter().copied().map(Some))
    }

    fn kept_call(&self, call: usize, summary: usize) -> bool {
        // The caller visits compaction summaries from newest to oldest.
        self.call_scopes(call)
            .binary_search_by(|epoch| summary.cmp(epoch))
            .is_ok()
    }

    fn results_only(&self, node: usize) -> bool {
        self.excluded.contains(&node)
            && (self.archival_results.contains(&node) || self.retained_results.contains_key(&node))
    }

    pub(super) fn conflicts_with(
        &self,
        node: usize,
        branch: &Branch,
        events: &[ClaudeEvent],
    ) -> bool {
        self.excluded.contains(&node)
            && (!self.results_only(node)
                || events[branch.nodes[node].events.clone()]
                    .iter()
                    .any(|event| !is_tool_outcome(event)))
    }

    pub(super) fn apply_to_path(
        &self,
        path: Vec<usize>,
        moved: &HashMap<usize, (usize, usize)>,
    ) -> Vec<usize> {
        if moved.is_empty() && self.excluded.is_empty() {
            return path;
        }
        let mut after_summary = HashMap::<usize, Vec<(usize, usize)>>::new();
        for (&node, &(summary, rank)) in moved {
            if !self.excluded.contains(&node) || self.results_only(node) {
                after_summary.entry(summary).or_default().push((rank, node));
            }
        }
        let mut selected = Vec::new();
        for node in path {
            // An unlisted result stays at its original position in recovery. Listed
            // results beyond a cutoff compete for calls in the retaining summary.
            let archival =
                self.archival_results.contains(&node) && !self.retained_results.contains_key(&node);
            if (self.excluded.contains(&node) && !archival) || moved.contains_key(&node) {
                continue;
            }
            selected.push(node);
            if let Some(mut additions) = after_summary.remove(&node) {
                additions.sort_unstable_by_key(|&(rank, _)| rank);
                selected.extend(additions.into_iter().map(|(_, node)| node));
            }
        }
        selected
    }

    pub(super) fn finish_exclusions(&mut self) {
        self.excluded
            .extend(std::mem::take(&mut self.excluded_paths).into_keys());
    }
}

/// Index the logical response and child links only if a rewind discarded retained turns.
pub(super) struct RetainedBranch<'a> {
    branch: &'a Branch,
    events: &'a [ClaudeEvent],
    record_calls: HashSet<(usize, &'a str)>,
    response_calls: HashMap<(ResponseUnit<'a>, &'a str), usize>,
    subtree: Vec<Range<usize>>,
    children: Vec<Vec<usize>>,
    siblings: HashMap<ResponseKey<'a>, Vec<usize>>,
    duplicates: &'a HashSet<Uuid>,
    repeated_at: HashMap<(ResponseKey<'a>, Uuid), usize>,
}

impl RetainedBranch<'_> {
    pub(super) fn kept_before(
        &self,
        summary: usize,
        retained: &[usize],
        end: usize,
        parent: Option<usize>,
        rewind: &mut Rewind,
    ) -> (Vec<usize>, Vec<usize>) {
        let mut kept = retained[..end].to_vec();
        let kept_responses: HashSet<_> = kept
            .iter()
            .copied()
            .chain(parent)
            .filter_map(|node| self.response_to(node))
            .collect();
        let mut discarded = Vec::new();
        for &node in &retained[end..] {
            let selected_response = self
                .response_to(node)
                .is_some_and(|unit| kept_responses.contains(&unit));
            if self.branch.nodes[node].user {
                // Validate each result block against the retained calls. The rest of the
                // user record and its descendants stay behind the cutoff.
                if self.events[self.branch.nodes[node].events.clone()]
                    .iter()
                    .any(is_tool_outcome)
                {
                    rewind.retained_results.entry(node).or_insert(summary);
                }
            } else if selected_response {
                kept.push(node);
                continue;
            }
            discarded.push(node);
        }
        self.exclude_before(summary, &discarded, &kept, parent, rewind);
        (kept, discarded)
    }

    fn response_to(&self, node: usize) -> Option<ResponseUnit<'_>> {
        self.response_node(node)
            .and_then(|source| self.branch.response_unit(source))
    }

    fn matches_retained_response(
        &self,
        index: usize,
        summary: usize,
        units: &HashSet<ResponseUnit<'_>>,
        selected_versions: &HashMap<Uuid, usize>,
        ambiguous: &mut bool,
    ) -> bool {
        let Some(source) = self.response_node(index) else {
            return false;
        };
        let Some(unit) = self.branch.response_unit(source) else {
            return false;
        };
        if !units.contains(&unit) {
            return false;
        }
        let Some(uuid) = self.branch.nodes[source].uuid else {
            return true;
        };
        if let Some(&selected) = selected_versions.get(&uuid) {
            return source == selected;
        }
        if let ResponseUnit::Named(key) = unit
            && self
                .repeated_at
                .get(&(key, uuid))
                .is_some_and(|&second| second < summary)
        {
            *ambiguous = true;
            return false;
        }
        true
    }

    fn keeps_ancestors(&self, index: usize) -> bool {
        let node = &self.branch.nodes[index];
        !node.user
            || self.events[node.events.clone()]
                .iter()
                .any(|event| !is_tool_outcome(event))
    }

    /// A native retained list can end before the physical conversation did.
    /// Preserve listed records and their verified response siblings in the old history.
    pub(super) fn exclude_unlisted_tail(
        &self,
        summary: usize,
        path: TrailingPath,
        kept: &[usize],
        listed_discards: &[usize],
        rewind: &mut Rewind,
    ) {
        let TrailingPath {
            cutoff,
            start,
            tail,
        } = path;
        let known: HashSet<_> = kept.iter().copied().collect();
        let units: HashSet<_> = kept
            .iter()
            .copied()
            .chain([cutoff])
            .filter_map(|i| self.response_to(i))
            .collect();
        let selected_versions: HashMap<_, _> = kept
            .iter()
            .copied()
            .chain([cutoff])
            .flat_map(|i| std::iter::once(i).chain(self.response_node(i)))
            .filter_map(|i| {
                let uuid = self.branch.nodes[i].uuid?;
                self.duplicates.contains(&uuid).then_some((uuid, i))
            })
            .collect();
        let selected_response = |i, ambiguous: &mut bool| {
            self.matches_retained_response(i, summary, &units, &selected_versions, ambiguous)
        };
        let on_path = |i: usize| self.subtree[i].contains(&self.subtree[tail].start);
        let start = if on_path(start) {
            start
        } else {
            // Parallel blocks can share a user parent. Follow a proven block of the
            // cutoff response on the old path, even if the cutoff itself is its sibling.
            let Some(ResponseUnit::Named(key)) = self.response_to(cutoff) else {
                return;
            };
            let mut ambiguous = false;
            let Some(sibling) = self.siblings.get(&key).and_then(|siblings| {
                siblings
                    .iter()
                    .rev()
                    .copied()
                    .find(|&i| on_path(i) && selected_response(i, &mut ambiguous))
            }) else {
                rewind.ambiguous_response |= ambiguous;
                return;
            };
            sibling
        };
        let issuing = self.response_node(cutoff).map(|i| &self.branch.nodes[i]);
        let mut protected: Vec<_> = kept
            .iter()
            .copied()
            .filter(|&i| self.keeps_ancestors(i))
            .collect();
        let mut discarded = Vec::new();
        let mut outcomes = Vec::new();
        let mut ambiguous = false;
        let mut index = tail;
        while index != start {
            let node = &self.branch.nodes[index];
            let belongs = known.contains(&index) || selected_response(index, &mut ambiguous);
            if belongs && self.keeps_ancestors(index) {
                protected.push(index);
                break;
            }
            if node.user && self.events[node.events.clone()].iter().any(is_tool_outcome) {
                // An outcome can follow another user turn while answering the original
                // call. Its position does not establish ownership of its ancestors.
                outcomes.push(index);
            }
            if !belongs {
                if !node.user
                    && issuing.is_some_and(|source| {
                        source.turn == node.turn
                            && source.writer == node.writer
                            && (source.response.is_none() || node.response.is_none())
                    })
                {
                    ambiguous = true;
                }
                discarded.push(index);
            }
            index = node
                .parent
                .expect("strict descendant of the retained cutoff");
        }
        let Some(&first) = discarded.last() else {
            return;
        };
        // A sparse list can discard a separate reply to the same later request. Its
        // shared parent and the surviving logical reply still belong in recovery.
        if listed_discards.iter().any(|&id| {
            !on_path(id)
                && self.subtree[first].contains(&self.subtree[id].start)
                && !self
                    .response_to(id)
                    .is_some_and(|unit| units.contains(&unit))
        }) {
            return;
        }
        rewind.ambiguous_response |= ambiguous;
        rewind.archival_results.extend(outcomes);
        self.exclude_before(summary, &discarded, &protected, Some(start), rewind);
    }

    // A tool result can link to any block in a response. Ordinary user text starts another turn.
    fn response_node(&self, index: usize) -> Option<usize> {
        let node = &self.branch.nodes[index];
        if !node.user {
            return self.branch.response_unit(index).map(|_| index);
        }
        let parent = self.branch.issuing_parent(index);
        let explicit = self.branch.tool_source(index);
        let source = match explicit {
            ToolSource::Record(source) => source,
            ToolSource::Absent => parent?,
            ToolSource::Missing | ToolSource::Unresolved | ToolSource::Duplicate { .. } => {
                return None;
            }
        };
        let unit = self.branch.response_unit(source)?;
        if self.branch.nodes[source].writer != node.writer || node.events.is_empty() {
            return None;
        }
        let parent_unit = parent.and_then(|id| self.branch.response_unit(id));
        self.events[node.events.clone()]
            .iter()
            .all(|event| {
                matches!(event.kind, EventKind::ToolCompleted | EventKind::ToolFailed)
                    && event.payload["tool_use_id"].as_str().is_some_and(|id| {
                        let was_called = |unit| {
                            self.response_calls
                                .get(&(unit, id))
                                .is_some_and(|&first| first < index)
                        };
                        if matches!(explicit, ToolSource::Record(_)) {
                            self.record_calls.contains(&(source, id))
                                && !parent_unit
                                    .is_some_and(|other| other != unit && was_called(other))
                        } else {
                            was_called(unit)
                        }
                    })
            })
            .then_some(source)
    }

    /// Bound each cutoff at its summary. The recovery history follows the summary's
    /// logical parent, but the replacement branch starts at the summary.
    fn exclude_before(
        &self,
        summary: usize,
        discarded: &[usize],
        kept: &[usize],
        cutoff: Option<usize>,
        rewind: &mut Rewind,
    ) {
        let mut protected: Vec<_> = kept
            .iter()
            .copied()
            .chain(cutoff)
            .map(|id| self.subtree[id].start)
            .collect();
        protected.sort_unstable();
        let protects = |index: usize, points: &[usize]| {
            let range = &self.subtree[index];
            points
                .get(points.partition_point(|&p| p < range.start))
                .is_some_and(|&p| p < range.end)
        };
        let mut path_ends = discarded.to_vec();
        let mut pending = discarded.to_vec();
        let mut responses = HashSet::new();
        while let Some(index) = pending.pop() {
            let node = &self.branch.nodes[index];
            if index >= summary
                || node.summary == Some(index)
                || protects(index, &protected)
                || !rewind.excluded.insert(index)
            {
                continue;
            }
            pending.extend(self.children[index].iter().copied());
            if let Some(key) = node.response_key(node.summary)
                && responses.insert(key)
                && let Some(group) = self.siblings.get(&key)
            {
                path_ends.extend(group.iter().copied());
                pending.extend(group.iter().copied());
            }
        }
        let Some(cutoff) = cutoff else {
            return;
        };
        // A surviving logical parent has its own history before the summary. Shared
        // ancestors of that history and the kept records must survive a sparse list.
        if let Some(parent) = self.branch.nodes[summary]
            .parent
            .filter(|p| !rewind.excluded.contains(p))
        {
            let point = self.subtree[parent].start;
            protected.insert(protected.partition_point(|&p| p < point), point);
        }
        let scope = &self.subtree[cutoff];
        let mut walked = Vec::new();
        for last in path_ends {
            if !rewind.excluded.contains(&last) || !scope.contains(&self.subtree[last].start) {
                continue;
            }
            let Some(mut index) = self.branch.nodes[last].parent else {
                continue;
            };
            while index != cutoff
                && scope.contains(&self.subtree[index].start)
                && !protects(index, &protected)
            {
                walked.push(index);
                index = rewind
                    .excluded_paths
                    .get(&index)
                    .copied()
                    .unwrap_or_else(|| {
                        self.branch.nodes[index]
                            .parent
                            .expect("strict descendant of cutoff")
                    });
            }
            // Later discarded replies can share this path. Jump over ancestors already excluded.
            for node in walked.drain(..) {
                rewind.excluded_paths.insert(node, index);
            }
        }
    }
}

#[derive(Clone, Copy)]
struct CachedMarker {
    revision: usize,
    owner: Option<(usize, Uuid)>,
}

/// A progress or attachment record points to its nearest conversation ancestor in the UUID map.
#[derive(Default)]
pub(super) struct Branch {
    nodes: Vec<Node>,
    tip: Option<usize>,
    ever_retained: HashSet<Uuid>,
    earlier_owners: HashMap<Uuid, Option<usize>>,
    transparent_owners: HashMap<Uuid, Option<(usize, Uuid)>>,
    latest_marker_summary: HashMap<usize, Option<usize>>,
    replaced_retained_uuid: bool,
    #[cfg(test)]
    pub(super) transparent_links_read: usize,
    // Only lookups requested by an input record get a cross-summary entry.
    queried_owners: HashMap<(usize, Uuid), CachedMarker>,
    marker_revision: usize,
    // Unretained links have the same next possible cutoff in every summary.
    next_markers: HashMap<Uuid, Option<Uuid>>,
    unverified_nodes: HashMap<usize, usize>,
    uncertain_cutoffs: HashSet<usize>,
    timestamp_parents: HashSet<usize>,
    pub(super) clock: CheckpointClock,
    pub(super) detached_events: HashMap<usize, Detached>,
    opaque_returns: HashMap<NonZeroUsize, OpaqueReturn>,
    pub(super) verified_cutoffs: Vec<usize>,
    indexed_cutoffs: usize,
    cutoffs_by_root: HashMap<usize, usize>,
    active_rejection: Option<usize>,
    // Each observed parent can ask about one source and one user turn.
    rejected_queries: HashMap<(usize, RejectionScope), bool>,
    suffix_parents: HashMap<usize, RetainedParent>,
    tool_sources: HashMap<usize, ToolSource>,
    duplicate_tool_sources: HashSet<Uuid>,
    forked: bool,
    copy_paths: super::membership::Paths,
    copy_descendants: super::membership::Descendants,
    external_parents: Option<HashMap<Uuid, usize>>,
}

fn only_tool_results(record: &Value) -> bool {
    super::value_at(record, &[&["message", "content"], &["content"]])
        .and_then(Value::as_array)
        .is_some_and(|parts| {
            !parts.is_empty() && parts.iter().all(|part| part["type"] == "tool_result")
        })
}

impl Branch {
    pub(super) fn start_sampled_tail(&mut self, connected: bool) {
        self.clock.interrupt();
        self.external_parents = Some(HashMap::new());
        if !connected {
            self.tip = None;
            self.forked = true;
            self.active_rejection = None;
            self.clear_transparent_paths();
        }
    }

    pub(super) fn observe_existing(&mut self, parent: Parent) {
        let Some(node) = parent.node else {
            return;
        };
        let leaf = if self.nodes[node].has_children {
            if !self.forked && self.tip.is_some() {
                return;
            }
            self.index_parents();
            self.copy_descendants.latest(node, &self.copy_paths)
        } else {
            node
        };
        if self.tip == Some(leaf) {
            return;
        }
        self.tip = Some(leaf);
        self.active_rejection = self.unverified_retention();
        self.select_retention();
    }

    fn index_parents(&mut self) {
        for original in &self.nodes[self.copy_paths.len()..] {
            self.copy_paths.push(original.parent);
        }
    }

    fn cutoff_for_origin(&mut self, source: usize) -> Option<usize> {
        if self.verified_cutoffs.is_empty() {
            return None;
        }
        self.index_parents();
        for &cutoff in &self.verified_cutoffs[self.indexed_cutoffs..] {
            self.cutoffs_by_root
                .insert(self.copy_paths.root(cutoff), cutoff);
        }
        self.indexed_cutoffs = self.verified_cutoffs.len();
        self.cutoffs_by_root
            .get(&self.copy_paths.root(source))
            .copied()
    }

    fn unresolved_parent(&mut self, value: &Value) -> Parent {
        let id = value.as_str().and_then(|s| Uuid::parse_str(s).ok());
        if let (Some(id), Some(parents)) = (id, &mut self.external_parents) {
            // Siblings can share an unobserved parent. Its ancestors remain unknown.
            let index = *parents.entry(id).or_insert_with(|| {
                let index = self.nodes.len();
                self.nodes.push(Node {
                    turn: index,
                    ..Node::default()
                });
                index
            });
            return Parent {
                node: Some(index),
                ..Parent::default()
            };
        }
        Parent {
            missing_ancestor: true,
            ..Parent::default()
        }
    }

    fn resolve_parent(
        &mut self,
        value: &Value,
        is_summary: bool,
        records: &HashMap<Uuid, ClaudeRecord>,
        duplicates: &HashSet<Uuid>,
        compactions: &Compactions,
    ) -> Parent {
        let Some((id, mut parent)) = value.as_str().and_then(|s| {
            let id = s.parse::<Uuid>().ok()?;
            if !is_summary && let Some(gap) = compactions.boundary_child(id, records) {
                return Some((id, gap));
            }
            let parent = records
                .get(&id)
                .map(|r| r.branch)
                .or_else(|| Some(compactions.absent_anchors.get(&id)?.as_ref()?.anchor_child))?;
            Some((id, parent))
        }) else {
            return self.unresolved_parent(value);
        };
        if let Some(unsupported) = self.active_rejection
            && self.selected_summary() <= Some(unsupported)
            && parent.source.is_some_and(|source| {
                self.rejected(unsupported, RejectionScope::Source(source), compactions)
            })
        {
            parent.node = parent.source;
            parent.suffix = None;
            parent.unverified = Some(unsupported);
        } else if let Some((summary, tip)) =
            self.retained_parent(id, parent, records, duplicates, compactions)
        {
            parent.node = Some(summary);
            parent.suffix = Some(tip);
            parent.missing_ancestor = self.nodes[summary].missing_ancestor;
        }
        if parent.unverified.is_none() {
            parent.unverified = parent
                .node
                .and_then(|i| self.unverified_nodes.get(&i).copied());
        }
        parent
    }

    fn parent_before_cutoff(
        &self,
        id: Uuid,
        parent: Parent,
        summary: usize,
        records: &HashMap<Uuid, ClaudeRecord>,
        compactions: &Compactions,
    ) -> Option<bool> {
        let Some(cutoff) = compactions
            .by_summary
            .get(&summary)
            .and_then(|link| link.cutoff)
        else {
            return Some(false);
        };
        let Some(link) = records.get(&id) else {
            return Some(false); // Missing direct parents have their own coverage warning.
        };
        if !link.branch.transparent {
            return Some(link.order < cutoff);
        }
        if parent.suffix.is_some() && parent.node == Some(summary) {
            return Some(false);
        }
        if link.order < cutoff {
            return Some(true);
        }
        // A later progress or attachment can still refer to a conversation record removed by the cutoff.
        let source = link.branch.source?;
        if source >= summary {
            return Some(false);
        }
        let uuid = self.nodes[source].uuid?;
        let matching = |record: &&ClaudeRecord| record.branch.source == Some(source);
        let origin = records
            .get(&uuid)
            .filter(matching)
            .or_else(|| compactions.originals.get(&uuid).filter(matching))?;
        if origin.order >= cutoff {
            return Some(false);
        }
        // A surviving native attachment keeps its own parent link. Its lost ancestry
        // is different from a progress link, which Claude removes before resuming.
        if link
            .branch
            .opaque_link_order
            .is_some_and(|row| row.get() > cutoff)
        {
            return None;
        }
        Some(true)
    }

    fn apply_checkpoint_cutoff(
        &mut self,
        parent: &mut Parent,
        record: &Value,
        records: &HashMap<Uuid, ClaudeRecord>,
        duplicates: &HashSet<Uuid>,
        compactions: &Compactions,
    ) -> Option<Detached> {
        let mut uncertain = parent
            .node
            .is_some_and(|node| self.uncertain_cutoffs.contains(&node));
        // A later unrelated branch cannot restore a conversation parent removed
        // before this attachment was written. A newer cutoff can remove the attachment itself.
        if let Some(row) = parent.opaque_link_order
            && let Some(decision) = self.opaque_returns.get(&row).copied()
            && !self
                .selected_summary()
                .is_some_and(|s| compactions.precedes_cutoff(s, row.get() - 1))
            && parent
                .source
                .and_then(|source| self.cutoff_for_origin(source))
                == Some(decision.cutoff)
        {
            parent.node = Some(decision.cutoff);
            parent.source = None;
            parent.suffix = None;
            parent.unverified = None;
            parent.missing_ancestor = self.nodes[decision.cutoff].missing_ancestor;
            self.uncertain_cutoffs.insert(self.nodes.len());
            return Some(Detached::Attachment {
                anchor: decision.anchor,
            });
        }
        let Some(summary) = self.selected_summary() else {
            if uncertain {
                self.uncertain_cutoffs.insert(self.nodes.len());
            }
            return None;
        };
        let mut detached = None;
        let before = if (parent.node != Some(summary) || compactions.empty_cutoff(summary))
            && parent.unverified.is_none()
            && let Some(id) = super::record_uuid(record, "parentUuid")
            && !duplicates.contains(&id)
        {
            self.parent_before_cutoff(id, *parent, summary, records, compactions)
        } else {
            Some(false)
        };
        if before != Some(false) {
            parent.node = Some(summary);
            if before.is_none() {
                parent.source = None;
                uncertain = true;
                detached = Some(Detached::Timestamp { uncertain });
            } else if compactions.empty_cutoff(summary) {
                match self.clock.resolve(self.nodes[summary].uuid, record) {
                    Choice::Node(node) => {
                        self.timestamp_parents.insert(self.nodes.len());
                        parent.node = Some(node);
                        parent.source = Some(node);
                    }
                    Choice::Detached => {
                        self.timestamp_parents.insert(self.nodes.len());
                        parent.source = None;
                        detached = Some(Detached::Timestamp { uncertain: false });
                    }
                    Choice::Unverified => {
                        // Keep the earlier observed continuation available in private recovery.
                        parent.node = self.tip;
                        parent.source = None;
                        uncertain = true;
                        detached = Some(Detached::Timestamp { uncertain });
                    }
                }
            }
            parent.suffix = None;
            parent.missing_ancestor = self.nodes[parent.node.unwrap_or(summary)].missing_ancestor;
        }
        let owner = self.summary_for(*parent);
        if let (Some(rejected), Some(older)) = (self.active_rejection, owner)
            && older != rejected
            && self.previous_summary(rejected) == Some(older)
        {
            // The raw parent and the earlier checkpoint can disagree after a rejected replacement.
            uncertain = true;
        }
        if uncertain || (self.uncertain_cutoff() && owner == Some(summary)) {
            self.uncertain_cutoffs.insert(self.nodes.len());
        }
        detached
    }

    fn remember_attachment_anchor(
        &mut self,
        record: &Value,
        parent: Parent,
        records: &HashMap<Uuid, ClaudeRecord>,
        duplicates: &HashSet<Uuid>,
        compactions: &Compactions,
    ) {
        let Some(source) = parent.source else { return };
        let Some(summary) = self.cutoff_for_origin(source) else {
            return;
        };
        let Some(id) = super::record_uuid(record, "parentUuid") else {
            return;
        };
        if parent.missing_ancestor
            || duplicates.contains(&id)
            || compactions.contains(summary, id, records)
            || self.nodes[source]
                .uuid
                .is_some_and(|id| compactions.contains(summary, id, records))
        {
            return;
        }
        let inherited = parent
            .opaque_link_order
            .and_then(|row| self.opaque_returns.get(&row))
            .copied()
            .filter(|prior| prior.cutoff == summary);
        let decision = if let Some(prior) = inherited {
            prior
        } else {
            if self.parent_before_cutoff(id, parent, summary, records, compactions) != Some(true) {
                return;
            }
            let anchor = self.nodes[summary].uuid.filter(|id| {
                record["type"] == "attachment"
                    && record["attachment"]["type"].is_string()
                    && self.clock.ordered()
                    && parent.unverified.is_none()
                    && !duplicates.contains(id)
                    && self.clock.resolve(Some(*id), record) == Choice::Node(summary)
            });
            OpaqueReturn {
                cutoff: summary,
                anchor,
            }
        };
        if let Some(order) = NonZeroUsize::new(records.len() + 1) {
            self.opaque_returns.insert(order, decision);
        }
    }

    fn remember_suffix(
        &mut self,
        index: usize,
        parent: Parent,
        records: &HashMap<Uuid, ClaudeRecord>,
        duplicates: &HashSet<Uuid>,
    ) {
        if let Some(id) = parent.suffix {
            let retained = records
                .get(&id)
                .filter(|_| !duplicates.contains(&id))
                .map(|record| record.branch);
            self.suffix_parents.insert(
                index,
                RetainedParent {
                    uuid: id,
                    node: retained.and_then(|parent| parent.node),
                    source: retained.and_then(|parent| parent.source),
                },
            );
        }
    }

    pub(super) fn observe(
        &mut self,
        record: &Value,
        is_summary: bool,
        records: &HashMap<Uuid, ClaudeRecord>,
        duplicates: &HashSet<Uuid>,
        compactions: &Compactions,
        events: Range<usize>,
    ) -> Parent {
        let previous = Parent {
            node: self.tip,
            source: self.tip,
            unverified: self.unverified_retention(),
            missing_ancestor: self.missing_ancestor(),
            ..Parent::default()
        };
        let mut resolve = |value: &Value| {
            self.resolve_parent(value, is_summary, records, duplicates, compactions)
        };
        if record["type"] == "system" && record["subtype"] == "compact_boundary" {
            // The logical parent connects earlier conversation for the recovery file.
            let parent = record
                .get("logicalParentUuid")
                .filter(|id| !id.is_null())
                .map_or(previous, resolve);
            return Parent {
                transparent: false,
                ..parent
            };
        }
        let mut parent = match record.get("parentUuid") {
            None => previous, // Older records have no parent field.
            Some(Value::Null) => Parent::default(),
            Some(value) => resolve(value),
        };
        let role = string_at(record, &[&["message", "role"], &["role"], &["type"]]);
        if record["type"] == "progress"
            || !matches!(role, Some("user" | "assistant"))
            || record["isMeta"] == true
        {
            if super::is_transcript_record(record) {
                self.remember_attachment_anchor(record, parent, records, duplicates, compactions);
            }
            return parent.through_link(record, records.len());
        }
        let detached = if is_summary {
            None
        } else {
            self.apply_checkpoint_cutoff(&mut parent, record, records, duplicates, compactions)
        };
        let starts_turn = detached.is_some();
        if let Some(detached) = detached
            && !events.is_empty()
        {
            self.detached_events.insert(events.start, detached);
        }
        self.forked |= parent.node != previous.node;
        let index = self.nodes.len();
        if let Some(summary) = parent.unverified {
            self.unverified_nodes.insert(index, summary);
        }
        self.remember_suffix(index, parent, records, duplicates);
        if role == Some("user") {
            self.observe_tool_source(index, record, records, duplicates);
        }
        // A tool-result transport continues its parent's turn. A mixed result with a
        // new request starts a turn for its descendants, as does an independent root.
        let result_only = role == Some("user") && only_tool_results(record);
        let turn = if is_summary || starts_turn || (role == Some("user") && !result_only) {
            index
        } else {
            // Retention rewrites the logical parent to a summary. The source still
            // identifies the turn that issued the retained response.
            parent
                .source
                .or(parent.node)
                .map_or(index, |node| self.nodes[node].turn)
        };
        if let Some(index) = parent.node {
            self.nodes[index].has_children = true;
        }
        self.nodes.push(Node {
            has_children: false,
            turn,
            uuid: super::record_uuid(record, "uuid"),
            parent: parent.node,
            summary: if is_summary {
                Some(index)
            } else {
                parent.node.and_then(|i| self.nodes[i].summary)
            },
            events,
            response: (role == Some("assistant"))
                .then(|| response_id(record))
                .flatten(),
            writer: record["agentId"].as_str().map(str::to_owned),
            user: role == Some("user"),
            missing_ancestor: parent.missing_ancestor,
        });
        self.continue_rejection(index, parent.unverified, result_only, compactions);
        self.tip = Some(index);
        Parent {
            node: Some(index),
            source: Some(index),
            transparent: false,
            opaque_link_order: None,
            unverified: parent.unverified,
            suffix: None,
            missing_ancestor: parent.missing_ancestor,
        }
    }

    pub(super) fn turn(&self, node: usize) -> usize {
        self.nodes[node].turn
    }

    fn rejected(
        &mut self,
        summary: usize,
        scope: RejectionScope,
        compactions: &Compactions,
    ) -> bool {
        if let Some(&answer) = self.rejected_queries.get(&(summary, scope)) {
            return answer;
        }
        // Records written after a boundary cannot enter its rejected segment.
        let answer = compactions.rejected(summary, scope, self);
        self.rejected_queries.insert((summary, scope), answer);
        answer
    }

    fn continue_rejection(
        &mut self,
        index: usize,
        unverified: Option<usize>,
        result_only: bool,
        compactions: &Compactions,
    ) {
        let turn = self.turn(index);
        self.active_rejection = if let Some(summary) = self.nodes[index].summary {
            // An earlier verified summary restores that branch's retained context.
            unverified.filter(|&failed| failed <= summary)
        } else if unverified.is_some() || !result_only {
            unverified
        } else {
            // Late parallel results can arrive through different chunks of the same turn.
            self.active_rejection
                .filter(|&failed| self.rejected(failed, RejectionScope::Turn(turn), compactions))
        };
    }

    fn observe_tool_source(
        &mut self,
        node: usize,
        record: &Value,
        records: &HashMap<Uuid, ClaudeRecord>,
        duplicates: &HashSet<Uuid>,
    ) {
        if let Some(source) = record
            .get("sourceToolAssistantUUID")
            .filter(|value| !value.is_null())
        {
            // A later duplicate UUID cannot change the record this result referred to.
            let source = source
                .as_str()
                .and_then(|id| id.parse::<Uuid>().ok())
                .map_or(ToolSource::Unresolved, |id| {
                    if duplicates.contains(&id) {
                        return ToolSource::Duplicate {
                            uuid: id,
                            same_parent: super::record_uuid(record, "parentUuid") == Some(id),
                        };
                    }
                    let Some(record) = records.get(&id) else {
                        return ToolSource::Missing;
                    };
                    record
                        .branch
                        .source
                        .filter(|&source| {
                            let assistant = &self.nodes[source];
                            !record.branch.transparent
                                && !assistant.user
                                && assistant.events == record.events
                        })
                        .map_or(ToolSource::Unresolved, ToolSource::Record)
                });
            if let ToolSource::Duplicate { uuid, .. } = source {
                self.duplicate_tool_sources.insert(uuid);
            }
            self.tool_sources.insert(node, source);
        }
    }

    pub(super) fn mark_unverified_retention(&mut self, summary: usize) {
        self.unverified_nodes.insert(summary, summary);
        self.active_rejection = Some(summary);
    }

    pub(super) fn unverified_retention(&self) -> Option<usize> {
        self.tip
            .and_then(|i| self.unverified_nodes.get(&i).copied())
    }

    pub(super) fn missing_ancestor(&self) -> bool {
        self.tip.is_some_and(|i| self.nodes[i].missing_ancestor)
    }

    pub(super) fn may_have_missing_parent(&self, parent: Parent) -> bool {
        parent.missing_ancestor || self.external_parents.is_some()
    }

    pub(super) fn uncertain_cutoff(&self) -> bool {
        self.tip
            .is_some_and(|node| self.uncertain_cutoffs.contains(&node))
    }

    pub(super) fn uncertain_timestamps(&self) -> bool {
        let Some(summary) = self.selected_summary() else {
            return false;
        };
        if self.timestamp_parents.is_empty() || !self.clock.uncertain(self.nodes[summary].uuid) {
            return false;
        }
        let mut node = self.tip;
        while let Some(index) = node {
            if index <= summary {
                break;
            }
            if self.timestamp_parents.contains(&index) {
                return true;
            }
            node = self.nodes[index].parent;
        }
        false
    }

    pub(super) fn tip_uuid(&self) -> Option<Uuid> {
        self.nodes[self.tip?].uuid
    }

    pub(super) fn selected_summary(&self) -> Option<usize> {
        self.nodes[self.tip?].summary
    }

    pub(super) fn summary_for(&self, parent: Parent) -> Option<usize> {
        self.nodes[parent.node?].summary
    }

    fn retained_parent(
        &mut self,
        id: Uuid,
        parent: Parent,
        records: &HashMap<Uuid, ClaudeRecord>,
        duplicates: &HashSet<Uuid>,
        compactions: &Compactions,
    ) -> Option<(usize, Uuid)> {
        if self.ever_retained.is_empty() || duplicates.contains(&id) {
            return None;
        }
        let summary = self.selected_summary()?;
        if !parent.transparent || parent.source.is_none() || parent.missing_ancestor {
            return self
                .retained_summary(id, parent.node, records, compactions)
                .map(|owner| (owner, id));
        }
        if parent.node == Some(summary) || parent.source >= Some(summary) {
            return None;
        }
        if let Some(cached) = self.queried_owners.get(&(summary, id))
            && cached.revision == self.marker_revision
        {
            return cached.owner;
        }

        let source = parent.source?;
        let mut owner = self.nodes[source]
            .uuid
            .filter(|uuid| {
                !duplicates.contains(uuid)
                    && records.get(uuid).is_some_and(|record| {
                        record.branch.source == Some(source) && !record.branch.transparent
                    })
            })
            .and_then(|uuid| {
                self.retained_summary(uuid, Some(source), records, compactions)
                    .map(|owner| (owner, uuid))
            });
        // A visible parent already retained by a newer summary takes precedence
        // over every transparent record retained by an earlier one.
        let latest_marker = if self.replaced_retained_uuid {
            // A replacement can change whether a retained UUID is transparent.
            Some(summary)
        } else {
            self.latest_marker_summary.get(&summary).copied().flatten()
        };
        if latest_marker.is_none() || latest_marker < owner.map(|(s, _)| s) {
            return owner;
        }
        if let Ok(result) =
            self.indexed_marker(latest_marker, id, source, owner, records, compactions)
        {
            self.cache_marker(summary, id, result);
            return result;
        }
        let mut path = Vec::new();
        let mut cursor = Some(id);
        let mut complete = true;
        while let Some(uuid) = self.next_marker(cursor, source, records, duplicates) {
            #[cfg(test)]
            {
                self.transparent_links_read += 1;
            }
            let Some(record) = records.get(&uuid) else {
                // A sampled tail can supply an unresolved UUID later.
                complete = false;
                break;
            };
            // A repeated UUID can now point at a different conversation.
            if !record.branch.transparent || record.branch.source != parent.source {
                break;
            }
            if let Some(&cached) = self.transparent_owners.get(&uuid) {
                if cached.map(|(s, _)| s) >= owner.map(|(s, _)| s) {
                    owner = cached;
                }
                break;
            }
            // The provisional entry also bounds malformed cycles.
            self.transparent_owners.insert(uuid, None);
            let retained = self
                .retained_summary(uuid, record.branch.node, records, compactions)
                .map(|s| (s, uuid));
            path.push((uuid, retained));
            if retained.is_some_and(|(s, _)| Some(s) == latest_marker) || duplicates.contains(&uuid)
            {
                break;
            }
            cursor = record.parent;
        }
        // Prefer the newest applicable summary. Within it, the closest explicitly retained
        // transcript record marks the cutoff, even if its conversation parent is also retained.
        for (uuid, retained) in path.into_iter().rev() {
            if retained.map(|(s, _)| s) >= owner.map(|(s, _)| s) {
                owner = retained;
            }
            if complete {
                self.transparent_owners.insert(uuid, owner);
            } else {
                self.transparent_owners.remove(&uuid);
            }
        }
        if complete {
            self.cache_marker(summary, id, owner);
        }
        owner
    }

    fn cache_marker(&mut self, summary: usize, id: Uuid, owner: Option<(usize, Uuid)>) {
        self.queried_owners.insert(
            (summary, id),
            CachedMarker {
                revision: self.marker_revision,
                owner,
            },
        );
    }

    fn indexed_marker(
        &mut self,
        mut summary: Option<usize>,
        id: Uuid,
        source: usize,
        mut owner: Option<(usize, Uuid)>,
        records: &HashMap<Uuid, ClaudeRecord>,
        compactions: &Compactions,
    ) -> Result<Option<(usize, Uuid)>, super::markers::Unindexed> {
        let query = compactions.marker_paths.query(records[&id].order)?;
        if let Some(duplicate) = query.duplicate {
            let current = records[&duplicate].branch;
            if current.transparent
                && current.source == Some(source)
                && let Some(s) =
                    self.retained_summary(duplicate, current.node, records, compactions)
                && Some(s) >= owner.map(|(s, _)| s)
            {
                owner = Some((s, duplicate));
            }
        }
        while let Some(s) = summary.filter(|s| Some(*s) >= owner.map(|(o, _)| o)) {
            if let Some(marker) = compactions.nearest_marker(s, &query)? {
                return Ok(Some((s, marker)));
            }
            summary = self
                .previous_summary(s)
                .and_then(|p| self.latest_marker_summary.get(&p).copied().flatten());
        }
        Ok(owner)
    }

    fn next_marker(
        &mut self,
        mut cursor: Option<Uuid>,
        source: usize,
        records: &HashMap<Uuid, ClaudeRecord>,
        duplicates: &HashSet<Uuid>,
    ) -> Option<Uuid> {
        let mut skipped = Vec::new();
        while let Some(id) = cursor {
            let Some(record) = records.get(&id) else {
                break;
            };
            if !record.branch.transparent || record.branch.source != Some(source) {
                cursor = None;
                break;
            }
            if self.ever_retained.contains(&id) || duplicates.contains(&id) {
                break;
            }
            if let Some(&next) = self.next_markers.get(&id) {
                cursor = next;
                break;
            }
            // The provisional entry bounds cyclic records during the first walk.
            #[cfg(test)]
            {
                self.transparent_links_read += 1;
            }
            self.next_markers.insert(id, None);
            skipped.push(id);
            cursor = record.parent;
        }
        for id in skipped {
            self.next_markers.insert(id, cursor);
        }
        cursor
    }

    fn retained_summary(
        &mut self,
        id: Uuid,
        parent: Option<usize>,
        records: &HashMap<Uuid, ClaudeRecord>,
        compactions: &Compactions,
    ) -> Option<usize> {
        let summary = self.selected_summary()?;
        if parent >= Some(summary) || !self.ever_retained.contains(&id) {
            return None;
        }
        if compactions.contains(summary, id, records) {
            return Some(summary);
        }
        let owner = if let Some(&owner) = self.earlier_owners.get(&id) {
            owner
        } else {
            let mut earlier = self.previous_summary(summary);
            while let Some(index) = earlier {
                if compactions.contains(index, id, records) {
                    break;
                }
                earlier = self.previous_summary(index);
            }
            self.earlier_owners.insert(id, earlier);
            earlier
        };
        owner.filter(|&index| parent < Some(index))
    }

    fn previous_summary(&self, summary: usize) -> Option<usize> {
        self.nodes[summary]
            .parent
            .and_then(|parent| self.nodes[parent].summary)
    }

    pub(super) fn record_replaced(&mut self, id: Uuid) {
        // An indexed path can now stop at this UUID even if it was never retained.
        self.marker_revision += 1;
        let retained = self.ever_retained.contains(&id);
        self.replaced_retained_uuid |= retained;
        if retained || self.next_markers.contains_key(&id) {
            self.clear_transparent_paths();
        }
    }

    fn clear_transparent_paths(&mut self) {
        self.next_markers.clear();
        self.queried_owners.clear();
        self.transparent_owners.clear();
    }

    pub(super) fn remember_retention(
        &mut self,
        ids: &[Uuid],
        records: &HashMap<Uuid, ClaudeRecord>,
    ) {
        let mut new_marker = false;
        let mut has_marker = false;
        for &id in ids {
            new_marker |= self.ever_retained.insert(id) && self.next_markers.contains_key(&id);
            has_marker |= records[&id].branch.transparent;
        }
        if has_marker && let Some(summary) = self.selected_summary() {
            self.latest_marker_summary.insert(summary, Some(summary));
        }
        if new_marker {
            self.clear_transparent_paths();
        }
    }

    pub(super) fn select_retention(&mut self) {
        if let Some(summary) = self.selected_summary()
            && !self.latest_marker_summary.contains_key(&summary)
        {
            let previous = self
                .previous_summary(summary)
                .and_then(|p| self.latest_marker_summary.get(&p).copied().flatten());
            self.latest_marker_summary.insert(summary, previous);
        }
        self.transparent_owners.clear();
        self.earlier_owners.clear();
    }

    pub(super) fn selected_path(&self) -> Vec<usize> {
        let mut path = Vec::new();
        let mut next = self.tip;
        while let Some(index) = next {
            path.push(index);
            next = self.nodes[index].parent;
        }
        path.reverse();
        path
    }

    pub(super) fn trailing_path(
        &self,
        summary: usize,
        tip: RetainedParent,
    ) -> Option<TrailingPath> {
        let cutoff = tip.source.or(tip.node)?;
        let tail = self.nodes[summary].parent?;
        if cutoff == tail {
            return None;
        }
        let start = if self.nodes[cutoff].summary == self.nodes[tail].summary {
            cutoff
        } else {
            self.nodes[tail]
                .summary
                .filter(|&earlier| earlier > cutoff)?
        };
        Some(TrailingPath {
            cutoff,
            start,
            tail,
        })
    }

    pub(super) fn cutoff_at(&self, node: usize) -> Option<RetainedParent> {
        Some(RetainedParent {
            uuid: self.nodes[node].uuid?,
            node: Some(node),
            source: Some(node),
        })
    }

    pub(super) fn retained_segments(&self, path: &[usize]) -> Vec<(usize, Option<RetainedParent>)> {
        path.iter()
            .enumerate()
            .filter(|&(_, &index)| self.nodes[index].summary == Some(index))
            .map(|(position, &index)| {
                let tip = path
                    .get(position + 1)
                    .and_then(|child| self.suffix_parents.get(child))
                    .copied();
                (index, tip)
            })
            .collect()
    }

    pub(super) fn retained_call_queries<'a>(
        &'a self,
        events: &'a [ClaudeEvent],
    ) -> HashMap<usize, RetainedCallQueries<'a>> {
        let mut queries = HashMap::<_, RetainedCallQueries>::new();
        let Some(last) = self.nodes.iter().rposition(|node| {
            node.user
                && node.summary.is_some()
                && events[node.events.clone()].iter().any(|event| {
                    matches!(event.kind, EventKind::ToolCompleted | EventKind::ToolFailed)
                })
        }) else {
            return queries;
        };
        let mut calls = ToolCalls::default();
        let no_retention = Rewind::default();
        for (index, node) in self.nodes[..=last].iter().enumerate() {
            if !node.user {
                calls.observe(index, self, false, &no_retention, events);
                continue;
            }
            let Some(summary) = node.summary.filter(|_| node.user) else {
                continue;
            };
            let parent = self.issuing_parent(index);
            let source = self.tool_source(index);
            for event in &events[node.events.clone()] {
                if !matches!(event.kind, EventKind::ToolCompleted | EventKind::ToolFailed) {
                    continue;
                }
                let Some(id) = event.payload["tool_use_id"].as_str() else {
                    continue;
                };
                let query = queries.entry(summary).or_default();
                if let ToolSource::Duplicate { uuid, .. } = source {
                    query.by_id.insert(
                        (
                            node.writer.as_deref(),
                            CandidateOwner::Source(uuid, None),
                            id,
                        ),
                        0,
                    );
                }
                if let Some((call, ResultSource::Linked)) =
                    calls.resolve(self, parent, source, node, id, |(writer, owner, _, id)| {
                        query.by_id.insert((writer, owner, id), 0);
                    })
                {
                    query.exact.insert(call.position);
                } else if matches!(source, ToolSource::Absent)
                    && let Some(parent) = parent
                {
                    query.by_id.insert(
                        (
                            node.writer.as_deref(),
                            CandidateOwner::Turn(self.nodes[parent].turn),
                            id,
                        ),
                        0,
                    );
                }
            }
        }
        queries
    }

    pub(super) fn index_retention<'a>(
        &'a self,
        events: &'a [ClaudeEvent],
        duplicates: &'a HashSet<Uuid>,
    ) -> RetainedBranch<'a> {
        let mut children = vec![Vec::new(); self.nodes.len()];
        let mut siblings = HashMap::<_, Vec<_>>::new();
        let mut first_versions = HashSet::new();
        let mut repeated_at = HashMap::new();
        let mut record_calls = HashSet::new();
        let mut response_calls = HashMap::new();
        for (index, node) in self.nodes.iter().enumerate() {
            if let Some(parent) = node.parent {
                children[parent].push(index);
            }
            if let Some(key) = node.response_key(node.summary) {
                siblings.entry(key).or_default().push(index);
                if let Some(uuid) = node.uuid.filter(|uuid| duplicates.contains(uuid))
                    && !first_versions.insert((key, uuid))
                {
                    // Only versions observed by the summary can make its proof ambiguous.
                    repeated_at.entry((key, uuid)).or_insert(index);
                }
            }
            if let Some(unit) = self.response_unit(index) {
                for call in &events[node.events.clone()] {
                    if call.kind == EventKind::ToolCalled
                        && let Some(id) = call.payload["id"].as_str()
                    {
                        record_calls.insert((index, id));
                        response_calls.entry((unit, id)).or_insert(index);
                    }
                }
            }
        }
        RetainedBranch {
            branch: self,
            events,
            subtree: self.subtree_ranges(&children),
            children,
            siblings,
            duplicates,
            repeated_at,
            record_calls,
            response_calls,
        }
    }

    // Preorder intervals answer ancestry queries without walking every kept path for each
    // cutoff. Each compaction summary starts a new tree, regardless of its logical parent.
    fn subtree_ranges(&self, children: &[Vec<usize>]) -> Vec<Range<usize>> {
        let mut ranges = vec![0..0; self.nodes.len()];
        let mut visit = Vec::new();
        let mut position = 0;
        for (index, node) in self.nodes.iter().enumerate() {
            if node.parent.is_some() && node.summary != Some(index) {
                continue;
            }
            visit.push((index, false));
            while let Some((id, leaving)) = visit.pop() {
                if leaving {
                    ranges[id].end = position;
                } else {
                    ranges[id].start = position;
                    position += 1;
                    visit.push((id, true));
                    visit.extend(
                        children[id]
                            .iter()
                            .filter(|&&child| self.nodes[child].summary != Some(child))
                            .map(|&child| (child, false)),
                    );
                }
            }
        }
        ranges
    }

    fn response_unit(&self, index: usize) -> Option<ResponseUnit<'_>> {
        let node = &self.nodes[index];
        if node.user || node.summary == Some(index) {
            return None;
        }
        if let Some(key) = node.response_key(node.summary) {
            return Some(ResponseUnit::Named(key));
        }
        (!node.events.is_empty()).then_some(ResponseUnit::Record(index))
    }

    pub(super) fn fold_response_metadata(&self, events: &mut Vec<ClaudeEvent>) {
        let mut first = HashMap::<_, usize>::new();
        let mut kept = 0;
        for i in 0..events.len() {
            if events[i].kind == EventKind::CompactionCreated
                || events[i].raw_type.as_deref()
                    == Some(omnis_core::CLAUDE_MISSING_COMPACTION_SUMMARY)
            {
                first.clear();
            }
            if let Some(super::ClaudeEventSource::Response(node)) = events[i].source
                && let Some(key) = self.nodes[node].response_key(None)
            {
                // Fold usage in the selected timeline. Unselected chunks can repeat
                // an ID or contain usage that a compaction deliberately omitted.
                match first.entry(key) {
                    std::collections::hash_map::Entry::Occupied(prior) => {
                        let payload = std::mem::take(&mut events[i].payload);
                        super::merge_response_metadata(&mut events[*prior.get()].payload, &payload);
                        continue;
                    }
                    std::collections::hash_map::Entry::Vacant(slot) => {
                        slot.insert(kept);
                    }
                }
            }
            if kept != i {
                events.swap(kept, i);
            }
            kept += 1;
        }
        events.truncate(kept);
    }

    pub(super) fn needs_event_order(&self, events: &[ClaudeEvent]) -> bool {
        if self.forked {
            return true;
        }
        // In a linear transcript, complete groups with distinct IDs are already in native order.
        let mut seen = HashSet::new();
        let mut pending = HashMap::new();
        let mut receiving = false;
        for (index, node) in self.nodes.iter().enumerate() {
            for event in &events[node.events.clone()] {
                match event.kind {
                    EventKind::ToolCalled => {
                        let Some(id) = event.payload["id"].as_str() else {
                            return true;
                        };
                        if !seen.insert(id) || (receiving && !pending.is_empty()) {
                            return true;
                        }
                        pending.insert(id, index);
                        receiving = false;
                    }
                    EventKind::ToolCompleted | EventKind::ToolFailed => {
                        let Some(caller) = event.payload["tool_use_id"]
                            .as_str()
                            .and_then(|id| pending.remove(id))
                        else {
                            return true;
                        };
                        if node.parent != Some(caller) || self.nodes[caller].writer != node.writer {
                            return true;
                        }
                        if let Some(source) = self.tool_sources.get(&index)
                            && *source != ToolSource::Record(caller)
                        {
                            return true;
                        }
                        receiving = true;
                    }
                    EventKind::MessageUser | EventKind::MessageAssistant if !pending.is_empty() => {
                        return true;
                    }
                    _ => {}
                }
            }
        }
        !pending.is_empty()
    }

    pub(super) fn event_order(
        &self,
        events: &mut [ClaudeEvent],
        path: &[usize],
        rewind: &Rewind,
        duplicates: &HashSet<Uuid>,
        inspect: impl FnOnce(&[ClaudeEvent], &[(usize, usize)]),
    ) -> (Vec<usize>, Ambiguity) {
        let mut active = vec![false; self.nodes.len()];
        let mut epoch: Vec<_> = self.nodes.iter().map(|node| node.summary).collect();
        let mut summary = None;
        for &index in path {
            active[index] = true;
            if self.nodes[index].summary == Some(index) {
                summary = Some(index);
            }
            epoch[index] = summary;
        }
        // A recovered sibling must belong to the same compacted conversation. Only a
        // provider-declared retained record can cross the summary from its physical past.
        let can_recover = |index: usize, owner: usize| {
            (!rewind.excluded.contains(&index) && self.nodes[index].summary == epoch[owner])
                || rewind
                    .retained_results
                    .get(&index)
                    .is_some_and(|&summary| epoch[owner] == Some(summary))
        };
        let mut responses = HashMap::new();
        for &index in path {
            let node = &self.nodes[index];
            if let Some(key) = node.response_key(epoch[index]) {
                responses.insert(key, index);
            }
        }
        let mut anchors: Vec<_> = self
            .nodes
            .iter()
            .enumerate()
            .map(|(index, node)| {
                let scope = if active[index] {
                    epoch[index]
                } else {
                    node.summary
                };
                if let Some(key) = node.response_key(scope) {
                    responses
                        .get(&key)
                        .copied()
                        .filter(|&owner| active[index] || can_recover(index, owner))
                } else {
                    // A tool result can identify its assistant parent even without a message ID.
                    (!node.user && active[index]).then_some(index)
                }
            })
            .collect();

        let assistants = self.filter_record_versions(path, &active, duplicates, &mut anchors);
        let eligible = |index: usize, owner: usize| {
            if active[index] {
                epoch[index] == epoch[owner]
            } else {
                can_recover(index, owner)
            }
        };
        let results = self.parallel_results(events, &active, &anchors, &epoch, &eligible, rewind);
        inspect(events, &results.pairs);
        (
            self.merge_selected_events(events, path, &anchors, &results),
            Ambiguity {
                results: results.ambiguous_selected,
                missing_source: results.missing_source,
                assistants,
            },
        )
    }

    fn filter_record_versions(
        &self,
        path: &[usize],
        active: &[bool],
        duplicates: &HashSet<Uuid>,
        anchors: &mut [Option<usize>],
    ) -> bool {
        if duplicates.is_empty() {
            return false;
        }
        let selected: HashSet<_> = path
            .iter()
            .filter_map(|&i| self.nodes[i].uuid)
            .filter(|uuid| duplicates.contains(uuid))
            .collect();
        let mut recovered = HashMap::new();
        let mut ambiguous = false;
        for (i, node) in self.nodes.iter().enumerate() {
            if active[i] {
                continue;
            }
            let (Some(owner), Some(uuid)) = (anchors[i], node.uuid) else {
                continue;
            };
            if !duplicates.contains(&uuid) {
                continue;
            }
            // A selected version identifies the record. Its incompatible versions cannot
            // supply extra streamed blocks, even if their response metadata matches.
            if selected.contains(&uuid) {
                anchors[i] = None;
            } else if let Some(prior) = recovered.insert((owner, uuid), i) {
                anchors[prior] = None;
                anchors[i] = None;
                ambiguous = true;
            }
        }
        ambiguous
    }

    /// The selected path, including the provider's retained order, is the timeline.
    /// Insert missing blocks beside surviving events of their response in source order.
    fn merge_selected_events(
        &self,
        events: &mut [ClaudeEvent],
        path: &[usize],
        anchors: &[Option<usize>],
        results: &ToolResults,
    ) -> Vec<usize> {
        let mut owners = vec![None; events.len()];
        for (node, &owner) in self.nodes.iter().zip(anchors) {
            if owner.is_some() {
                owners[node.events.clone()].fill(owner);
            }
        }
        let call_for: HashMap<_, _> = results
            .pairs
            .iter()
            .map(|&(call, result)| {
                owners[result] = owners[call];
                (result, call)
            })
            .collect();
        results.prepare_native_tools(events, &owners);
        let mut base = Vec::new();
        let mut slot = vec![None; events.len()];
        let mut empty = HashMap::new();
        for &node in path {
            if let Some(owner) = anchors[node] {
                empty.insert(owner, base.len());
            }
            for pos in self.nodes[node].events.clone() {
                if !results.filtered.contains(&pos) {
                    slot[pos] = Some(base.len());
                    base.push(pos);
                }
            }
        }

        if owners
            .iter()
            .zip(&slot)
            .all(|(owner, slot)| owner.is_none() || slot.is_some())
        {
            return base;
        }

        // The nearest following event of the same response sets the insertion point.
        // For a trailing block, use the nearest preceding event instead.
        let mut next = HashMap::new();
        let mut insert_at = vec![None; events.len()];
        for (pos, &owner) in owners.iter().enumerate().rev() {
            if let Some(owner) = owner {
                if let Some(at) = slot[pos] {
                    next.insert(owner, at);
                } else {
                    insert_at[pos] = next.get(&owner).copied();
                }
            }
        }
        let mut previous = HashMap::new();
        let mut extra = HashMap::<usize, Vec<usize>>::new();
        for (pos, &owner) in owners.iter().enumerate() {
            let Some(owner) = owner else { continue };
            if let Some(at) = slot[pos] {
                previous.insert(owner, at + 1);
            } else {
                let Some(mut at) = insert_at[pos]
                    .or_else(|| previous.get(&owner).or_else(|| empty.get(&owner)).copied())
                else {
                    continue;
                };
                if let Some(&call) = call_for.get(&pos) {
                    // A reordered retained segment can move the call past the next selected block.
                    let issued = slot[call].map(|at| at + 1).or(insert_at[call]);
                    if let Some(issued) = issued {
                        at = at.max(issued);
                    }
                }
                insert_at[pos] = Some(at);
                extra.entry(at).or_default().push(pos);
            }
        }
        let mut order = Vec::with_capacity(base.len());
        for (at, event) in base
            .into_iter()
            .map(Some)
            .chain(std::iter::once(None))
            .enumerate()
        {
            if let Some(extra) = extra.remove(&at) {
                order.extend(extra);
            }
            order.extend(event);
        }
        order
    }

    fn tool_source(&self, index: usize) -> ToolSource {
        self.tool_sources
            .get(&index)
            .copied()
            .unwrap_or(ToolSource::Absent)
    }

    fn issuing_parent(&self, index: usize) -> Option<usize> {
        self.suffix_parents
            .get(&index)
            .map_or(self.nodes[index].parent, |parent| parent.source)
    }

    fn candidate_owners(&self, index: usize) -> impl Iterator<Item = CandidateOwner<'_>> {
        let node = &self.nodes[index];
        let response = self.response_unit(index);
        let duplicate = node
            .uuid
            .filter(|uuid| self.duplicate_tool_sources.contains(uuid));
        std::iter::once(CandidateOwner::Turn(node.turn)).chain(duplicate.into_iter().flat_map(
            move |uuid| {
                [None, Some(ResponseUnit::Record(index))]
                    .into_iter()
                    .chain(response.map(Some))
                    .map(move |parent| CandidateOwner::Source(uuid, parent))
            },
        ))
    }

    fn has_ambiguous_selected_result(
        &self,
        unresolved: &HashMap<(CandidateCall<'_>, Option<usize>), usize>,
        events: &[ClaudeEvent],
        anchors: &[Option<usize>],
        epoch: &[Option<usize>],
        rewind: &Rewind,
    ) -> bool {
        if unresolved.is_empty() {
            return false;
        }
        self.nodes.iter().enumerate().any(|(index, node)| {
            let Some(owner) = anchors[index] else {
                return false;
            };
            node.events.clone().any(|position| {
                let event = &events[position];
                if event.kind != EventKind::ToolCalled {
                    return false;
                }
                let Some(id) = event.payload["id"].as_str() else {
                    return false;
                };
                // A discarded retry cannot make an otherwise complete transfer look incomplete.
                rewind.epochs_for_call(position, node.summary).any(|scope| {
                    self.candidate_owners(index).any(|candidate| {
                        let key = (node.writer.as_deref(), candidate, scope, id);
                        unresolved
                            .get(&(key, epoch[owner]))
                            .is_some_and(|&result| index < result)
                    })
                })
            })
        })
    }

    fn missing_declared_call(
        &self,
        source: ToolSource,
        result: usize,
        carrier: Option<usize>,
        anchors: &[Option<usize>],
        calls: &ToolCalls<'_>,
        id: Option<&str>,
    ) -> bool {
        let result = &self.nodes[result];
        let issuer = match source {
            ToolSource::Missing => return true,
            ToolSource::Record(issuer) => issuer,
            ToolSource::Duplicate { uuid, .. } => {
                let writer = result.writer.as_deref();
                if id.is_some_and(|id| {
                    calls
                        .by_source
                        .contains_key(&(writer, uuid, result.summary, id))
                }) {
                    return false;
                }
                let selected = carrier
                    .filter(|&node| {
                        self.nodes[node].uuid == Some(uuid)
                            && self.nodes[node].writer == result.writer
                    })
                    .or_else(|| {
                        calls
                            .selected_sources
                            .get(&(writer, uuid, result.summary))
                            .copied()
                    });
                let Some(issuer) = selected else {
                    return false;
                };
                issuer
            }
            _ => return false,
        };
        if self.nodes[issuer].writer != result.writer
            || id.is_some_and(|id| calls.by_record.contains_key(&(issuer, id)))
        {
            return false;
        }
        let declared = self.response_unit(issuer);
        let parent = carrier.and_then(|node| self.response_unit(node));
        // A retained result can outlive its issuer. Its original response still
        // distinguishes a missing block from an abandoned retry.
        if anchors[issuer].is_none()
            && carrier != Some(issuer)
            && (parent.is_none() || parent != declared)
        {
            return false;
        }
        !id.is_some_and(|id| {
            [declared, parent]
                .into_iter()
                .flatten()
                .any(|unit| calls.by_response.contains_key(&(unit, id)))
        })
    }

    /// Resolve outcomes against the calls that already existed when Claude wrote each result.
    fn parallel_results(
        &self,
        events: &[ClaudeEvent],
        active: &[bool],
        anchors: &[Option<usize>],
        epoch: &[Option<usize>],
        eligible: &impl Fn(usize, usize) -> bool,
        rewind: &Rewind,
    ) -> ToolResults {
        let mut calls = ToolCalls::default();
        let mut results = ToolResults::default();
        let mut outcomes = HashMap::<_, Outcome>::new();
        let mut transports = HashMap::new();
        let mut unresolved = HashMap::new();
        for (index, node) in self.nodes.iter().enumerate() {
            if !node.user {
                calls.observe(index, self, anchors[index].is_some(), rewind, events);
                continue;
            }
            let partial = active[index] && rewind.results_only(index);
            let selected = active[index] && !partial;
            let parent = self.issuing_parent(index);
            let source = self.tool_source(index);
            let issuer = parent.map(|p| *transports.get(&p).unwrap_or(&p));
            let origin = issuer.and_then(|p| self.response_unit(p));
            // A verified outcome can link the next parallel result to its original response.
            // An ordinary user message supplies no such evidence.
            let mut transport = issuer.filter(|_| origin.is_some());
            for position in node.events.clone() {
                let event = &events[position];
                // A withdrawn row contributes only individually verified outcomes.
                if partial {
                    results.withhold(position);
                }
                if !is_tool_outcome(event) {
                    continue;
                }
                let id = event.payload["tool_use_id"].as_str();
                if selected {
                    let missing =
                        self.missing_declared_call(source, index, issuer, anchors, &calls, id);
                    results.observe_selected(
                        position,
                        source,
                        &calls,
                        node.writer.as_deref(),
                        id,
                        missing,
                    );
                }
                let Some(id) = id else {
                    transport = None;
                    continue;
                };
                let ambiguous = |key| {
                    if selected {
                        unresolved.insert((key, epoch[index]), index);
                    }
                };
                let Some((call, mut priority)) =
                    calls.resolve(self, parent, source, node, id, ambiguous)
                else {
                    transport = None;
                    continue;
                };
                let available = anchors[call.node].is_some_and(|owner| eligible(index, owner));
                let paired = available && !call.repeated;
                let original_parent = origin.is_some() && origin == self.response_unit(call.node);
                if !original_parent {
                    transport = None;
                }
                let separated = epoch[index] != node.summary
                    || (active[call.node] && epoch[call.node] != epoch[index]);
                let retained = active[index]
                    && separated
                    && (node
                        .summary
                        .is_some_and(|summary| rewind.kept_call(call.position, summary))
                        || (self.nodes[call.node].summary == node.summary
                            && (anchors[call.node].is_some() || (!partial && original_parent))));
                let repeated = available && call.repeated && selected && active[call.node];
                if !paired && !retained && !repeated {
                    if active[index] && priority == ResultSource::Linked {
                        results.withhold(position);
                    }
                    continue;
                }
                if selected {
                    results.withhold(position);
                    priority = ResultSource::Selected;
                }
                // A result retained without its call stays in that summary's documentary history.
                let documentary = !paired;
                let scope = if documentary { epoch[index] } else { None };
                outcomes
                    .entry((call, documentary, scope))
                    .and_modify(|outcome| outcome.consider(priority, position, events))
                    .or_insert(Outcome {
                        priority,
                        position: Some(position),
                    });
            }
            if let Some(issuer) = transport
                && events[node.events.clone()].iter().any(is_tool_outcome)
            {
                transports.insert(index, issuer);
            }
        }
        results.ambiguous_selected =
            self.has_ambiguous_selected_result(&unresolved, events, anchors, epoch, rewind);
        results.accept_outcomes(outcomes);
        results
    }
}

#[derive(Default)]
struct ToolResults {
    filtered: HashSet<usize>,
    documentary: HashSet<usize>,
    pairs: Vec<(usize, usize)>,
    ambiguous_selected: bool,
    missing_source: bool,
}

impl ToolResults {
    fn accept_outcomes(&mut self, outcomes: HashMap<(ToolCall, bool, Option<usize>), Outcome>) {
        for ((call, documentary, _), outcome) in outcomes {
            if let Some(position) = outcome.position {
                self.filtered.remove(&position);
                if documentary {
                    self.documentary.insert(position);
                } else {
                    self.pairs.push((call.position, position));
                }
            } else if outcome.priority == ResultSource::Selected {
                self.ambiguous_selected = true;
            }
        }
    }

    fn observe_selected(
        &mut self,
        position: usize,
        source: ToolSource,
        calls: &ToolCalls<'_>,
        writer: Option<&str>,
        id: Option<&str>,
        missing: bool,
    ) {
        if missing {
            self.documentary.insert(position);
            self.missing_source = true;
            return;
        }
        let selected_id = id.is_some_and(|id| calls.selected_ids.contains(&(writer, id)));
        // Preserve a selected orphan unless its own links identify a stale call.
        if source == ToolSource::Absent && !selected_id {
            self.documentary.insert(position);
        } else {
            self.filtered.insert(position);
        }
    }

    fn withhold(&mut self, position: usize) {
        self.documentary.remove(&position);
        self.filtered.insert(position);
    }

    fn prepare_native_tools(&self, events: &mut [ClaudeEvent], owners: &[Option<usize>]) {
        let paired: HashSet<_> = self.pairs.iter().map(|&(call, _)| call).collect();
        for &pos in &self.documentary {
            events[pos].payload["omnisession_unpaired_result"] = Value::Bool(true);
        }
        for (pos, event) in events.iter_mut().enumerate() {
            if owners[pos].is_some()
                && event.kind == EventKind::ToolCalled
                && !paired.contains(&pos)
            {
                event.payload["omnisession_unpaired_call"] = Value::Bool(true);
            }
        }
        // Native groups pair by ID. Different requests can reuse a tool ID while both
        // calls are pending. Keep Claude's original fields and name each verified pair.
        let mut seen = HashSet::new();
        if self
            .pairs
            .iter()
            .any(|&(call, _)| !seen.insert(events[call].payload["id"].as_str()))
        {
            for &(call, result) in &self.pairs {
                let id = Value::String(format!("omni-claude-call-{call}"));
                events[call].payload["call_id"] = id.clone();
                events[result].payload["call_id"] = id;
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ToolSource {
    Absent,
    Missing,
    Unresolved,
    Duplicate { uuid: Uuid, same_parent: bool },
    Record(usize),
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct ToolCall {
    node: usize,
    position: usize,
    repeated: bool,
}

#[derive(Clone, Copy)]
enum CallsWithId {
    One(ToolCall),
    Reused(ToolCall),
}

impl CallsWithId {
    fn last(self) -> ToolCall {
        match self {
            Self::One(call) | Self::Reused(call) => call,
        }
    }

    fn unique(self) -> Option<ToolCall> {
        match self {
            Self::One(call) => Some(call),
            Self::Reused(_) => None,
        }
    }
}

#[derive(Default)]
pub(super) struct RetainedCallQueries<'a> {
    exact: HashSet<usize>,
    by_id: HashMap<(Option<&'a str>, CandidateOwner<'a>, &'a str), u8>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum CandidateOwner<'a> {
    Turn(usize),
    Source(Uuid, Option<ResponseUnit<'a>>),
}

type ScopedCall<'a, Owner> = (Option<&'a str>, Owner, Option<usize>, &'a str);
type CandidateCall<'a> = ScopedCall<'a, CandidateOwner<'a>>;

#[derive(Default)]
struct ToolCalls<'a> {
    by_record: HashMap<(usize, &'a str), CallsWithId>,
    by_response: HashMap<(ResponseUnit<'a>, &'a str), CallsWithId>,
    by_scope: HashMap<ScopedCall<'a, usize>, CallsWithId>,
    by_source: HashMap<ScopedCall<'a, Uuid>, CallsWithId>,
    selected_ids: HashSet<(Option<&'a str>, &'a str)>,
    selected_sources: HashMap<(Option<&'a str>, Uuid, Option<usize>), usize>,
}

fn record_call<K: std::hash::Hash + Eq>(map: &mut HashMap<K, CallsWithId>, key: K, call: ToolCall) {
    map.entry(key)
        .and_modify(|prior| {
            if prior.last() != call {
                *prior = CallsWithId::Reused(call);
            }
        })
        .or_insert(CallsWithId::One(call));
}

impl<'a> ToolCalls<'a> {
    fn observe(
        &mut self,
        index: usize,
        branch: &'a Branch,
        selected: bool,
        rewind: &Rewind,
        events: &'a [ClaudeEvent],
    ) {
        let node = &branch.nodes[index];
        let duplicate = node
            .uuid
            .filter(|uuid| branch.duplicate_tool_sources.contains(uuid));
        if selected && let Some(uuid) = duplicate {
            self.selected_sources
                .insert((node.writer.as_deref(), uuid, node.summary), index);
        }
        let Some(unit) = branch.response_unit(index) else {
            return;
        };
        for position in node.events.clone() {
            let event = &events[position];
            if event.kind == EventKind::ToolCalled
                && let Some(id) = event.payload["id"].as_str()
            {
                let call = ToolCall {
                    node: index,
                    position,
                    repeated: self.by_response.contains_key(&(unit, id)),
                };
                if selected {
                    self.selected_ids.insert((node.writer.as_deref(), id));
                }
                record_call(&mut self.by_record, (index, id), call);
                record_call(&mut self.by_response, (unit, id), call);
                // Only a summary that kept this call can make it available in a later epoch.
                for scope in rewind.epochs_for_call(position, node.summary) {
                    record_call(
                        &mut self.by_scope,
                        (node.writer.as_deref(), node.turn, scope, id),
                        call,
                    );
                    if let Some(uuid) = duplicate {
                        record_call(
                            &mut self.by_source,
                            (node.writer.as_deref(), uuid, scope, id),
                            call,
                        );
                    }
                }
            }
        }
    }

    fn resolve(
        &self,
        branch: &'a Branch,
        parent: Option<usize>,
        source: ToolSource,
        result: &'a Node,
        id: &'a str,
        mut ambiguous: impl FnMut(CandidateCall<'a>),
    ) -> Option<(ToolCall, ResultSource)> {
        // The same repeated UUID in both fields provides only one piece of evidence.
        let parent = parent.filter(|_| {
            !matches!(
                source,
                ToolSource::Duplicate {
                    same_parent: true,
                    ..
                }
            )
        });
        let parent_unit = parent.and_then(|node| branch.response_unit(node));
        let parent_record = parent.and_then(|node| self.by_record.get(&(node, id)));
        let parent_response = parent_unit.and_then(|unit| self.by_response.get(&(unit, id)));
        let call = match source {
            ToolSource::Record(node) => self.by_record.get(&(node, id))?.last(),
            ToolSource::Duplicate { uuid, .. } => {
                let key = (result.writer.as_deref(), uuid, result.summary, id);
                let Some(call) = self.by_source.get(&key)?.unique() else {
                    let constraint = if parent_record.is_some() {
                        parent.map(ResponseUnit::Record)
                    } else {
                        parent_unit.filter(|_| parent_response.is_some())
                    };
                    ambiguous((key.0, CandidateOwner::Source(uuid, constraint), key.2, id));
                    return None;
                };
                call
            }
            ToolSource::Missing | ToolSource::Unresolved => return None,
            ToolSource::Absent => {
                if let Some(call) = parent_record.or(parent_response) {
                    call.last()
                } else {
                    let turn = branch.nodes[parent?].turn;
                    // A later compaction can move the result independently of its call.
                    // Match the turn and summary in which Claude actually wrote the result.
                    let key = (result.writer.as_deref(), turn, result.summary, id);
                    let Some(call) = self.by_scope.get(&key)?.unique() else {
                        ambiguous((key.0, CandidateOwner::Turn(turn), key.2, id));
                        return None;
                    };
                    return Some((call, ResultSource::CallId));
                }
            }
        };
        // A call in the actual parent response can contradict an explicit source.
        if !matches!(source, ToolSource::Absent)
            && ((parent_response.is_some() && parent_unit != branch.response_unit(call.node))
                || parent_record.is_some_and(|other| other.last() != call))
        {
            return None;
        }
        (branch.nodes[call.node].writer == result.writer).then_some((call, ResultSource::Linked))
    }
}

#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
enum ResultSource {
    CallId,
    Linked,
    Selected,
}

struct Outcome {
    priority: ResultSource,
    position: Option<usize>,
}

impl Outcome {
    fn consider(&mut self, priority: ResultSource, position: usize, events: &[ClaudeEvent]) {
        if priority > self.priority {
            self.priority = priority;
            self.position = Some(position);
        } else if priority == self.priority
            && self.position.is_some_and(|prior| {
                events[prior].kind != events[position].kind
                    || events[prior].payload != events[position].payload
            })
        {
            self.position = None;
        }
    }
}
