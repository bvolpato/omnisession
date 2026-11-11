//! Select the conversation that Claude would continue after a rewind.

use std::{
    collections::{HashMap, HashSet},
    ops::Range,
};

use omnis_ir::EventKind;
use serde_json::Value;
use uuid::Uuid;

use super::{
    ClaudeEvent, ClaudeRecord, Compactions, MissingRetainedRecord, ResponseId, response_id,
    string_at,
};

#[derive(Clone, Copy, Default)]
pub(super) struct Parent {
    pub(super) node: Option<usize>,
    // Transparent records keep the physical conversation parent across summary rewrites.
    source: Option<usize>,
    transparent: bool,
    unverified: Option<usize>,
    suffix: Option<Uuid>,
    missing_ancestor: bool,
}

#[derive(Default)]
struct Node {
    uuid: Option<Uuid>,
    parent: Option<usize>,
    summary: Option<usize>,
    events: Range<usize>,
    response: Option<ResponseId>,
    writer: Option<String>,
    user: bool,
    missing_ancestor: bool,
}

#[derive(Default)]
struct EarlierRetention {
    next_summary: Option<usize>,
    owners: HashMap<Uuid, usize>,
    missing: HashSet<Uuid>,
    incomplete: bool,
}

type ResponseKey<'a> = (&'a ResponseId, Option<&'a str>, Option<usize>);

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum ResponseUnit<'a> {
    Named(ResponseKey<'a>),
    Record(usize),
}

impl Node {
    fn response_key(&self, summary: Option<usize>) -> Option<ResponseKey<'_>> {
        Some((self.response.as_ref()?, self.writer.as_deref(), summary))
    }
}

#[derive(Default)]
pub(super) struct Rewind {
    pub(super) excluded: HashSet<usize>,
    excluded_paths: HashMap<usize, usize>,
    retained_results: HashMap<usize, usize>,
}

impl Rewind {
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
}

impl RetainedBranch<'_> {
    pub(super) fn kept_before<'a>(
        &self,
        summary: usize,
        retained: &[usize],
        end: usize,
        parent: Option<usize>,
        rewind: &mut Rewind,
        record_for: impl Fn(&Uuid) -> Option<&'a ClaudeRecord>,
    ) -> Vec<usize> {
        let mut kept = retained[..end].to_vec();
        let issuing = |node| self.response_node(node, &record_for);
        let kept_responses: HashSet<_> = kept
            .iter()
            .copied()
            .chain(parent)
            .filter_map(issuing)
            .filter_map(|node| self.branch.response_unit(node))
            .collect();
        let mut discarded = Vec::new();
        for &node in &retained[end..] {
            if self.branch.nodes[node].user {
                // Validate each result block against the retained calls. The rest of the
                // user record and its descendants stay behind the cutoff.
                rewind.retained_results.entry(node).or_insert(summary);
            } else if self
                .branch
                .response_unit(node)
                .is_some_and(|unit| kept_responses.contains(&unit))
            {
                kept.push(node);
                continue;
            }
            discarded.push(node);
        }
        self.exclude_before(summary, &discarded, &kept, parent, rewind);
        kept
    }

    // A tool result can link to any block in a response. Ordinary user text starts another turn.
    fn response_node<'a>(
        &self,
        index: usize,
        record_for: &impl Fn(&Uuid) -> Option<&'a ClaudeRecord>,
    ) -> Option<usize> {
        let node = &self.branch.nodes[index];
        if !node.user {
            return self.branch.response_unit(index).map(|_| index);
        }
        let parent = self.branch.issuing_parent(index, record_for);
        let explicit = self.branch.tool_source(index, record_for);
        let source = match explicit {
            ToolSource::Record(source) => source,
            ToolSource::Absent => parent?,
            ToolSource::Unresolved => return None,
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

/// A progress or attachment record points to its nearest conversation ancestor in the UUID map.
#[derive(Default)]
pub(super) struct Branch {
    nodes: Vec<Node>,
    tip: Option<usize>,
    ever_retained: HashSet<Uuid>,
    earlier: EarlierRetention,
    transparent_owners: HashMap<Uuid, Option<(usize, Uuid)>>,
    unverified_nodes: HashMap<usize, usize>,
    unverified_sources: HashMap<usize, usize>,
    suffix_parents: HashMap<usize, Uuid>,
    tool_sources: HashMap<usize, Option<Uuid>>,
    retained: HashSet<Uuid>,
    forked: bool,
    external_parents: Option<HashMap<Uuid, usize>>,
}

impl Branch {
    pub(super) fn start_sampled_tail(&mut self, connected: bool) {
        self.external_parents = Some(HashMap::new());
        if !connected {
            self.tip = None;
            self.forked = true;
            self.retained.clear();
            self.transparent_owners.clear();
        }
    }

    fn unresolved_parent(&mut self, value: &Value) -> Parent {
        let id = value.as_str().and_then(|s| Uuid::parse_str(s).ok());
        if let (Some(id), Some(parents)) = (id, &mut self.external_parents) {
            // Siblings can share an unobserved parent. Its ancestors remain unknown.
            let index = *parents.entry(id).or_insert_with(|| {
                let index = self.nodes.len();
                self.nodes.push(Node::default());
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
        records: &HashMap<Uuid, ClaudeRecord>,
        duplicates: &HashSet<Uuid>,
        compactions: &Compactions,
    ) -> Parent {
        let Some((id, mut parent)) = value.as_str().and_then(|s| {
            let id = s.parse::<Uuid>().ok()?;
            Some((id, records.get(&id)?.branch))
        }) else {
            return self.unresolved_parent(value);
        };
        if let Some((summary, tip)) =
            self.retained_parent(id, parent, records, duplicates, compactions)
        {
            parent.node = Some(summary);
            parent.suffix = Some(tip);
            parent.missing_ancestor = self.nodes[summary].missing_ancestor;
        } else if let Some(unsupported) = parent
            .source
            .and_then(|node| self.unverified_sources.get(&node))
            .copied()
            && self.selected_summary() <= Some(unsupported)
        {
            parent.unverified = Some(unsupported);
        }
        if parent.unverified.is_none() {
            parent.unverified = parent
                .node
                .and_then(|i| self.unverified_nodes.get(&i).copied());
        }
        parent
    }

    pub(super) fn observe(
        &mut self,
        record: &Value,
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
        let mut resolve =
            |value: &Value| self.resolve_parent(value, records, duplicates, compactions);
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
        let parent = match record.get("parentUuid") {
            None => previous, // Older records have no parent field.
            Some(Value::Null) => Parent::default(),
            Some(value) => resolve(value),
        };
        let role = string_at(record, &[&["message", "role"], &["role"], &["type"]]);
        if record["type"] == "progress"
            || !matches!(role, Some("user" | "assistant"))
            || record["isMeta"] == true
        {
            return Parent {
                transparent: true,
                ..parent
            };
        }
        self.forked |= parent.node != previous.node;
        let index = self.nodes.len();
        if let Some(summary) = parent.unverified {
            self.unverified_nodes.insert(index, summary);
        }
        if let Some(id) = parent.suffix {
            self.suffix_parents.insert(index, id);
        }
        if role == Some("user")
            && let Some(source) = record
                .get("sourceToolAssistantUUID")
                .filter(|v| !v.is_null())
        {
            self.tool_sources
                .insert(index, source.as_str().and_then(|s| s.parse().ok()));
        }
        self.nodes.push(Node {
            uuid: super::record_uuid(record, "uuid"),
            parent: parent.node,
            summary: if record["isCompactSummary"] == true {
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
        self.tip = Some(index);
        Parent {
            node: Some(index),
            source: Some(index),
            transparent: false,
            unverified: parent.unverified,
            suffix: None,
            missing_ancestor: parent.missing_ancestor,
        }
    }

    pub(super) fn mark_unverified_retention(
        &mut self,
        summary: usize,
        ids: &[Uuid],
        records: &HashMap<Uuid, ClaudeRecord>,
    ) {
        // A failed relink affects the summary and continuations through its proposed members.
        self.unverified_nodes.insert(summary, summary);
        self.unverified_sources.extend(
            ids.iter()
                .filter_map(|id| Some((records.get(id)?.branch.source?, summary))),
        );
    }

    pub(super) fn unverified_retention(&self) -> Option<usize> {
        self.tip
            .and_then(|i| self.unverified_nodes.get(&i).copied())
    }

    pub(super) fn missing_ancestor(&self) -> bool {
        self.tip.is_some_and(|i| self.nodes[i].missing_ancestor)
    }

    pub(super) fn selected_summary(&self) -> Option<usize> {
        self.nodes[self.tip?].summary
    }

    fn retained_parent(
        &mut self,
        id: Uuid,
        parent: Parent,
        records: &HashMap<Uuid, ClaudeRecord>,
        duplicates: &HashSet<Uuid>,
        compactions: &Compactions,
    ) -> Option<(usize, Uuid)> {
        if self.ever_retained.is_empty() {
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
        if let Some(&owner) = self.transparent_owners.get(&id) {
            return owner;
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
        let mut path = Vec::new();
        let mut cursor = Some(id);
        while let Some(uuid) = cursor {
            let Some(record) = records.get(&uuid) else {
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
            if retained.is_some_and(|(s, _)| s == summary) || duplicates.contains(&uuid) {
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
            self.transparent_owners.insert(uuid, owner);
        }
        owner
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
        if self.retained.contains(&id) {
            return Some(summary);
        }
        if let Some(&owner) = self.earlier.owners.get(&id) {
            return (parent < Some(owner)).then_some(owner);
        }
        // Walk this branch newest first. Overlapping segments need only one owner per UUID.
        while let Some(earlier) = self.earlier.next_summary {
            if parent >= Some(earlier) {
                break;
            }
            self.earlier.next_summary = self.previous_summary(earlier);
            match compactions.context(Some(earlier), records) {
                Ok(Some(context)) => {
                    for member in context.records {
                        self.earlier.owners.entry(member).or_insert(earlier);
                    }
                    if let Some(&owner) = self.earlier.owners.get(&id) {
                        return Some(owner);
                    }
                }
                Err(error) => {
                    self.earlier.incomplete = true;
                    if let Some(missing) = error.downcast_ref::<MissingRetainedRecord>() {
                        self.earlier.missing.insert(missing.0);
                    }
                }
                Ok(None) => {}
            }
        }
        None
    }

    fn previous_summary(&self, summary: usize) -> Option<usize> {
        self.nodes[summary]
            .parent
            .and_then(|parent| self.nodes[parent].summary)
    }

    fn reset_earlier_retention(&mut self) {
        self.earlier.next_summary = self
            .selected_summary()
            .and_then(|summary| self.previous_summary(summary));
        self.transparent_owners.clear();
        self.earlier.owners.clear();
        self.earlier.missing.clear();
        self.earlier.incomplete = false;
    }

    pub(super) fn record_inserted(&mut self, id: Uuid, changed_link: bool, replaced: bool) {
        if replaced
            && (self.transparent_owners.contains_key(&id) || self.ever_retained.contains(&id))
        {
            self.transparent_owners.clear();
        }
        // A replaced parent or transcript type can invalidate a cached segment.
        if (changed_link && (self.earlier.incomplete || self.earlier.owners.contains_key(&id)))
            || self.earlier.missing.contains(&id)
        {
            self.reset_earlier_retention();
        }
    }

    pub(super) fn select_retention(&mut self, ids: &[Uuid]) {
        self.retained.clear();
        self.retained.extend(ids);
        self.ever_retained.extend(ids);
        self.reset_earlier_retention();
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

    pub(super) fn retained_segments(&self, path: &[usize]) -> Vec<(usize, Option<Uuid>)> {
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

    pub(super) fn index_retention<'a>(&'a self, events: &'a [ClaudeEvent]) -> RetainedBranch<'a> {
        let mut children = vec![Vec::new(); self.nodes.len()];
        let mut siblings = HashMap::<_, Vec<_>>::new();
        let mut record_calls = HashSet::new();
        let mut response_calls = HashMap::new();
        for (index, node) in self.nodes.iter().enumerate() {
            if let Some(parent) = node.parent {
                children[parent].push(index);
            }
            if let Some(key) = node.response_key(node.summary) {
                siblings.entry(key).or_default().push(index);
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

    pub(super) fn needs_event_order<'a>(
        &self,
        events: &[ClaudeEvent],
        record_for: impl Fn(&Uuid) -> Option<&'a ClaudeRecord>,
    ) -> bool {
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
                            && !source.as_ref().and_then(&record_for).is_some_and(|r| {
                                r.branch.node == Some(caller)
                                    && r.events == self.nodes[caller].events
                            })
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

    pub(super) fn event_order<'a>(
        &self,
        events: &mut [ClaudeEvent],
        path: &[usize],
        rewind: &Rewind,
        record_for: impl Fn(&Uuid) -> Option<&'a ClaudeRecord>,
    ) -> Vec<usize> {
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
        let anchors: Vec<_> = self
            .nodes
            .iter()
            .enumerate()
            .map(|(index, node)| {
                if let Some(response) = &node.response {
                    let scope = if active[index] {
                        epoch[index]
                    } else {
                        node.summary
                    };
                    responses
                        .get(&(response, node.writer.as_deref(), scope))
                        .copied()
                        .filter(|&owner| active[index] || can_recover(index, owner))
                } else {
                    // A tool result can identify its assistant parent even without a message ID.
                    (!node.user && active[index]).then_some(index)
                }
            })
            .collect();

        let eligible = |index: usize, owner: usize| {
            if active[index] {
                epoch[index] == epoch[owner]
            } else {
                can_recover(index, owner)
            }
        };
        let results =
            self.parallel_results(events, &active, &anchors, &epoch, &eligible, record_for);
        self.merge_selected_events(events, path, &anchors, &results)
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

    fn tool_source<'a>(
        &self,
        index: usize,
        record_for: &impl Fn(&Uuid) -> Option<&'a ClaudeRecord>,
    ) -> ToolSource {
        let Some(id) = self.tool_sources.get(&index) else {
            return ToolSource::Absent;
        };
        id.as_ref()
            .and_then(record_for)
            .and_then(|record| {
                let source = record.branch.node?;
                // A progress record can point at the assistant but cannot identify its call.
                (source < index
                    && !self.nodes[source].user
                    && self.nodes[source].events == record.events)
                    .then_some(source)
            })
            .map_or(ToolSource::Unresolved, ToolSource::Record)
    }

    fn issuing_parent<'a>(
        &self,
        index: usize,
        record_for: &impl Fn(&Uuid) -> Option<&'a ClaudeRecord>,
    ) -> Option<usize> {
        match self.suffix_parents.get(&index) {
            Some(id) => record_for(id)?
                .branch
                .source
                .filter(|&parent| parent < index),
            None => self.nodes[index].parent,
        }
    }

    /// Resolve outcomes against the calls that already existed when Claude wrote each result.
    fn parallel_results<'a>(
        &self,
        events: &[ClaudeEvent],
        active: &[bool],
        anchors: &[Option<usize>],
        epoch: &[Option<usize>],
        eligible: &impl Fn(usize, usize) -> bool,
        record_for: impl Fn(&Uuid) -> Option<&'a ClaudeRecord>,
    ) -> ToolResults {
        let mut calls = ToolCalls::default();
        let mut results = ToolResults::default();
        let mut outcomes = HashMap::new();
        for (index, node) in self.nodes.iter().enumerate() {
            if !node.user {
                if let Some(unit) = self.response_unit(index) {
                    calls.observe(index, node, unit, epoch[index], events);
                }
                continue;
            }
            let parent = self.issuing_parent(index, &record_for);
            let source = self.tool_source(index, &record_for);
            for position in node.events.clone() {
                let event = &events[position];
                if !matches!(event.kind, EventKind::ToolCompleted | EventKind::ToolFailed) {
                    continue;
                }
                let id = event.payload["tool_use_id"].as_str();
                if active[index] {
                    if matches!(source, ToolSource::Absent)
                        && !id.is_some_and(|id| calls.known.contains(id))
                    {
                        // No observed call owns this historical result. Retain its text without pairing it.
                        results.documentary.insert(position);
                        continue;
                    }
                    results.filtered.insert(position);
                }
                let Some(id) = id else { continue };
                let Some((call, mut priority)) =
                    calls.resolve(self, parent, source, node, epoch[index], id)
                else {
                    continue;
                };
                let available = anchors[call.node].is_some_and(|owner| eligible(index, owner));
                let paired = available && !call.repeated;
                let original_parent = parent
                    .and_then(|p| self.response_unit(p))
                    .is_some_and(|unit| Some(unit) == self.response_unit(call.node));
                let retained = active[index]
                    && epoch[index] != node.summary
                    && self.nodes[call.node].summary == node.summary
                    && (anchors[call.node].is_some() || original_parent);
                let repeated = available && call.repeated && active[index] && active[call.node];
                if !paired && !retained && !repeated {
                    continue;
                }
                if active[index] {
                    priority = ResultSource::Selected;
                }
                // A result retained without its call stays in that summary's documentary history.
                let documentary = !paired;
                outcomes
                    .entry((
                        call,
                        documentary,
                        if documentary { epoch[index] } else { None },
                    ))
                    .and_modify(|(best, kept)| match priority.cmp(best) {
                        std::cmp::Ordering::Greater => {
                            *best = priority;
                            *kept = Some(position);
                        }
                        std::cmp::Ordering::Equal => {
                            if kept.is_some_and(|prior: usize| {
                                events[prior].kind != event.kind
                                    || events[prior].payload != event.payload
                            }) {
                                *kept = None;
                            }
                        }
                        std::cmp::Ordering::Less => {}
                    })
                    .or_insert((priority, Some(position)));
            }
        }
        for ((call, documentary, _), (_, position)) in outcomes {
            if let Some(position) = position {
                results.filtered.remove(&position);
                if documentary {
                    results.documentary.insert(position);
                } else {
                    results.pairs.push((call.position, position));
                }
            }
        }
        results
    }
}

#[derive(Default)]
struct ToolResults {
    filtered: HashSet<usize>,
    documentary: HashSet<usize>,
    pairs: Vec<(usize, usize)>,
}

impl ToolResults {
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

#[derive(Clone, Copy)]
enum ToolSource {
    Absent,
    Unresolved,
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

type ScopedCallId<'a> = (Option<&'a str>, Option<usize>, &'a str);

#[derive(Default)]
struct ToolCalls<'a> {
    by_record: HashMap<(usize, &'a str), CallsWithId>,
    by_response: HashMap<(ResponseUnit<'a>, &'a str), CallsWithId>,
    by_scope: HashMap<ScopedCallId<'a>, CallsWithId>,
    known: HashSet<&'a str>,
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
        node: &'a Node,
        unit: ResponseUnit<'a>,
        epoch: Option<usize>,
        events: &'a [ClaudeEvent],
    ) {
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
                self.known.insert(id);
                record_call(&mut self.by_record, (index, id), call);
                record_call(&mut self.by_response, (unit, id), call);
                for scope in [epoch, node.summary] {
                    record_call(
                        &mut self.by_scope,
                        (node.writer.as_deref(), scope, id),
                        call,
                    );
                }
            }
        }
    }

    fn resolve(
        &self,
        branch: &Branch,
        parent: Option<usize>,
        source: ToolSource,
        result: &Node,
        epoch: Option<usize>,
        id: &str,
    ) -> Option<(ToolCall, ResultSource)> {
        let parent_unit = parent.and_then(|node| branch.response_unit(node));
        let parent_record = parent.and_then(|node| self.by_record.get(&(node, id)));
        let parent_response = parent_unit.and_then(|unit| self.by_response.get(&(unit, id)));
        let call = match source {
            ToolSource::Record(node) => {
                let call = self.by_record.get(&(node, id))?.last();
                // Any earlier call in the parent response can contradict an explicit source.
                if (parent_response.is_some() && parent_unit != branch.response_unit(node))
                    || parent_record.is_some_and(|other| other.last() != call)
                {
                    return None;
                }
                call
            }
            ToolSource::Unresolved => return None,
            ToolSource::Absent => {
                if let Some(call) = parent_record.or(parent_response) {
                    call.last()
                } else {
                    let in_scope = |scope| {
                        self.by_scope
                            .get(&(result.writer.as_deref(), scope, id))
                            .map(|calls| calls.unique())
                    };
                    let physical = in_scope(result.summary);
                    let logical = in_scope(epoch);
                    if physical.is_some() && logical.is_some() && physical != logical {
                        return None;
                    }
                    return Some((physical.or(logical).flatten()?, ResultSource::CallId));
                }
            }
        };
        (branch.nodes[call.node].writer == result.writer).then_some((call, ResultSource::Linked))
    }
}

#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
enum ResultSource {
    CallId,
    Linked,
    Selected,
}
