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
    suffix: Option<Uuid>,
    missing_ancestor: bool,
}

#[derive(Default)]
struct Node {
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
    retained_results: HashMap<usize, usize>,
}

/// Index the logical response and child links only if a rewind discarded retained turns.
pub(super) struct RetainedBranch<'a> {
    branch: &'a Branch,
    events: &'a [ClaudeEvent],
    record_calls: HashSet<(usize, &'a str)>,
    response_calls: HashSet<(ResponseUnit<'a>, &'a str)>,
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
        self.exclude_before(summary, discarded, &kept, &mut rewind.excluded);
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
                        if matches!(explicit, ToolSource::Record(_)) {
                            self.record_calls.contains(&(source, id))
                                && !parent_unit.is_some_and(|other| {
                                    other != unit && self.response_calls.contains(&(other, id))
                                })
                        } else {
                            self.response_calls.contains(&(unit, id))
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
        discarded: Vec<usize>,
        kept: &[usize],
        excluded: &mut HashSet<usize>,
    ) {
        let protected: HashSet<_> = kept.iter().copied().collect();
        let mut pending = discarded;
        let mut responses = HashSet::new();
        while let Some(index) = pending.pop() {
            let node = &self.branch.nodes[index];
            if index >= summary
                || node.summary == Some(index)
                || protected.contains(&index)
                || !excluded.insert(index)
            {
                continue;
            }
            pending.extend(self.children[index].iter().copied());
            if let Some(key) = node.response_key(node.summary)
                && responses.insert(key)
                && let Some(group) = self.siblings.get(&key)
            {
                pending.extend(group.iter().copied());
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

    pub(super) fn observe(
        &mut self,
        record: &Value,
        records: &HashMap<Uuid, ClaudeRecord>,
        compactions: &Compactions,
        events: Range<usize>,
    ) -> Parent {
        let previous = Parent {
            node: self.tip,
            missing_ancestor: self.missing_ancestor(),
            ..Parent::default()
        };
        let lookup = |value: &Value| {
            let id = value.as_str()?.parse::<Uuid>().ok()?;
            Some((id, records.get(&id)?.branch))
        };
        let mut resolve = |value: &Value| {
            let Some((id, parent)) = lookup(value) else {
                return self.unresolved_parent(value);
            };
            if let Some(summary) = self.retained_summary(id, parent, records, compactions) {
                Parent {
                    node: Some(summary),
                    suffix: Some(id),
                    missing_ancestor: self.nodes[summary].missing_ancestor,
                }
            } else {
                parent
            }
        };
        if record["type"] == "system" && record["subtype"] == "compact_boundary" {
            // The logical parent connects earlier conversation for the recovery file.
            return record
                .get("logicalParentUuid")
                .filter(|id| !id.is_null())
                .map_or(previous, resolve);
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
            return parent;
        }
        self.forked |= parent.node != previous.node;
        let index = self.nodes.len();
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
            suffix: None,
            missing_ancestor: parent.missing_ancestor,
        }
    }

    pub(super) fn missing_ancestor(&self) -> bool {
        self.tip.is_some_and(|i| self.nodes[i].missing_ancestor)
    }

    pub(super) fn selected_summary(&self) -> Option<usize> {
        self.nodes[self.tip?].summary
    }

    fn retained_summary(
        &mut self,
        id: Uuid,
        parent: Parent,
        records: &HashMap<Uuid, ClaudeRecord>,
        compactions: &Compactions,
    ) -> Option<usize> {
        let summary = self.selected_summary()?;
        if parent.node >= Some(summary) || !self.ever_retained.contains(&id) {
            return None;
        }
        if self.retained.contains(&id) {
            return Some(summary);
        }
        if let Some(&owner) = self.earlier.owners.get(&id) {
            return (parent.node < Some(owner)).then_some(owner);
        }
        // Walk this branch newest first. Overlapping segments need only one owner per UUID.
        while let Some(earlier) = self.earlier.next_summary {
            if parent.node >= Some(earlier) {
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
        self.earlier.owners.clear();
        self.earlier.missing.clear();
        self.earlier.incomplete = false;
    }

    pub(super) fn record_inserted(&mut self, id: Uuid, changed_parent: bool) {
        // Only a changed link inside a cached segment can alter a successful walk.
        if (changed_parent && (self.earlier.incomplete || self.earlier.owners.contains_key(&id)))
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
        let mut response_calls = HashSet::new();
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
                        response_calls.insert((unit, id));
                    }
                }
            }
        }
        RetainedBranch {
            branch: self,
            events,
            children,
            siblings,
            record_calls,
            response_calls,
        }
    }

    fn response_unit(&self, index: usize) -> Option<ResponseUnit<'_>> {
        let node = &self.nodes[index];
        if node.user || node.summary == Some(index) || node.events.is_empty() {
            return None;
        }
        Some(
            node.response_key(node.summary)
                .map_or(ResponseUnit::Record(index), ResponseUnit::Named),
        )
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

        // Keep each off-branch block before the next selected block of its response, or
        // after the last one. Selected records of other turns stay in their original slots.
        let mut next_selected = HashMap::new();
        let mut blocks = HashMap::<usize, Vec<usize>>::new();
        for (index, &anchor) in anchors.iter().enumerate().rev() {
            if let Some(anchor) = anchor {
                let slot = next_selected.entry(anchor).or_insert(anchor);
                if active[index] {
                    *slot = index;
                }
                blocks.entry(*slot).or_default().push(index);
            }
        }
        let eligible = |index: usize, owner: usize| {
            if active[index] {
                epoch[index] == epoch[owner]
            } else {
                can_recover(index, owner)
            }
        };
        let results =
            self.parallel_results(events, &active, &anchors, &epoch, &eligible, record_for);
        // An unmatched call is documentary. It cannot invalidate a later, complete tool group.
        for (node, anchor) in self.nodes.iter().zip(&anchors) {
            if anchor.is_some() {
                for pos in node.events.clone() {
                    if events[pos].kind == EventKind::ToolCalled
                        && !results.paired_calls.contains(&pos)
                    {
                        events[pos].payload["omnisession_unpaired_call"] = Value::Bool(true);
                    }
                }
            }
        }
        let mut order = Vec::new();
        for &index in path {
            let node = &self.nodes[index];
            if let Some(group) = blocks.get(&index) {
                for &block in group.iter().rev() {
                    order.extend(self.nodes[block].events.clone());
                }
            } else {
                order.extend(
                    node.events
                        .clone()
                        .filter(|position| !results.filtered.contains(position)),
                );
            }
            if let Some(extra) = results.after.get(&index) {
                order.extend(extra);
            }
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
            Some(id) => record_for(id)?.branch.node.filter(|&parent| parent < index),
            None => self.nodes[index].parent,
        }
    }

    /// Validate selected and recovered result blocks against the calls in the final conversation.
    fn parallel_results<'a>(
        &self,
        events: &[ClaudeEvent],
        active: &[bool],
        anchors: &[Option<usize>],
        epoch: &[Option<usize>],
        eligible: &impl Fn(usize, usize) -> bool,
        record_for: impl Fn(&Uuid) -> Option<&'a ClaudeRecord>,
    ) -> ToolResults {
        let calls = ToolCalls::new(&self.nodes, events, anchors, epoch);
        let mut results = ToolResults::default();
        let mut outcomes = HashMap::new();
        for (index, node) in self.nodes.iter().enumerate().filter(|(_, node)| node.user) {
            let parent = self.issuing_parent(index, &record_for);
            let source = self.tool_source(index, &record_for);
            for position in node.events.clone() {
                let event = &events[position];
                if !matches!(event.kind, EventKind::ToolCompleted | EventKind::ToolFailed) {
                    continue;
                }
                let id = event.payload["tool_use_id"].as_str();
                if active[index]
                    && (!matches!(source, ToolSource::Absent)
                        || id.is_some_and(|id| calls.known.contains(id)))
                {
                    results.filtered.insert(position);
                }
                let Some(id) = id else { continue };
                let Some((owner, mut priority)) =
                    calls.owner(index, parent, id, source, &self.nodes, anchors)
                else {
                    continue;
                };
                if !eligible(index, owner) {
                    continue;
                }
                if active[index] {
                    priority = ResultSource::Selected;
                }
                outcomes
                    .entry((owner, id))
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
        let mut confirmed = HashMap::<usize, Vec<(bool, usize)>>::new();
        for ((owner, id), (priority, position)) in outcomes {
            if let Some(position) = position {
                confirmed
                    .entry(owner)
                    .or_default()
                    .push((priority == ResultSource::Selected, position));
                if let Some(Some((_, pos))) = calls.first_in_response.get(&(owner, id)) {
                    results.paired_calls.insert(*pos);
                }
            }
        }
        for (owner, mut positions) in confirmed {
            // Recovered siblings precede the selected result, which originally followed the calls.
            positions.sort_unstable();
            results
                .after
                .insert(owner, positions.into_iter().map(|(_, p)| p).collect());
        }
        results
    }
}

#[derive(Default)]
struct ToolResults {
    filtered: HashSet<usize>,
    after: HashMap<usize, Vec<usize>>,
    paired_calls: HashSet<usize>,
}

#[derive(Clone, Copy)]
enum ToolSource {
    Absent,
    Unresolved,
    Record(usize),
}

type ScopedCallId<'a> = (Option<&'a str>, Option<usize>, &'a str);

struct ToolCalls<'a> {
    by_record: HashSet<(usize, &'a str)>,
    first_in_response: HashMap<(usize, &'a str), Option<(usize, usize)>>,
    unique_owner: HashMap<ScopedCallId<'a>, Option<usize>>,
    known: HashSet<&'a str>,
    epoch: &'a [Option<usize>],
}

impl<'a> ToolCalls<'a> {
    fn new(
        nodes: &'a [Node],
        events: &'a [ClaudeEvent],
        anchors: &[Option<usize>],
        epoch: &'a [Option<usize>],
    ) -> Self {
        let mut calls = Self {
            by_record: HashSet::new(),
            first_in_response: HashMap::new(),
            unique_owner: HashMap::new(),
            known: HashSet::new(),
            epoch,
        };
        for (index, node) in nodes.iter().enumerate() {
            for position in node.events.clone() {
                let event = &events[position];
                if event.kind == EventKind::ToolCalled
                    && let Some(id) = event.payload["id"].as_str()
                {
                    calls.known.insert(id);
                    calls.by_record.insert((index, id));
                    let owner = anchors[index];
                    if let Some(owner) = owner {
                        calls
                            .first_in_response
                            .entry((owner, id))
                            .and_modify(|call| *call = None)
                            .or_insert(Some((index, position)));
                    }
                    // Results kept across a summary can have a user parent and no explicit
                    // source. Require one issuer in their original conversation as well.
                    for scope in [epoch[index], node.summary] {
                        calls
                            .unique_owner
                            .entry((node.writer.as_deref(), scope, id))
                            .and_modify(|prior| {
                                if *prior != owner {
                                    *prior = None;
                                }
                            })
                            .or_insert(owner);
                    }
                }
            }
        }
        calls
    }

    fn owner(
        &self,
        index: usize,
        parent_node: Option<usize>,
        id: &str,
        source: ToolSource,
        nodes: &[Node],
        anchors: &[Option<usize>],
    ) -> Option<(usize, ResultSource)> {
        let node = &nodes[index];
        let precedes_result = |owner: &usize| {
            self.first_in_response
                .get(&(*owner, id))
                .is_some_and(|caller| caller.is_some_and(|(caller, _)| caller < index))
        };
        let parent = parent_node
            .and_then(|p| anchors[p])
            .filter(|&owner| nodes[owner].writer == node.writer)
            .filter(precedes_result);
        if matches!(source, ToolSource::Unresolved) {
            return None;
        }
        if let ToolSource::Record(source) = source {
            let owner = anchors[source]?;
            if nodes[source].writer != node.writer
                || !self.by_record.contains(&(source, id))
                || !precedes_result(&owner)
            {
                return None;
            }
            // Two assistants that claim the same call ID cannot identify each other's result.
            if parent.is_some_and(|other| other != owner)
                || (parent.is_none()
                    && parent_node.is_some_and(|p| self.by_record.contains(&(p, id))))
            {
                return None;
            }
            return Some((owner, ResultSource::Linked));
        }
        if let Some(owner) = parent {
            return Some((owner, ResultSource::Linked));
        }
        if parent_node.is_some_and(|p| self.by_record.contains(&(p, id))) {
            return None;
        }
        let owner = self
            .unique_owner
            .get(&(node.writer.as_deref(), self.epoch[index], id))
            .copied()
            .flatten()
            .filter(precedes_result)?;
        Some((owner, ResultSource::CallId))
    }
}

#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
enum ResultSource {
    CallId,
    Linked,
    Selected,
}
