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

    pub(super) fn forked(&self) -> bool {
        self.forked
    }

    pub(super) fn event_order<'a>(
        &self,
        events: &[ClaudeEvent],
        path: &[usize],
        excluded: &HashSet<usize>,
        record_for: impl Fn(&Uuid) -> Option<&'a ClaudeRecord>,
    ) -> Vec<usize> {
        let mut active = vec![false; self.nodes.len()];
        let mut epoch = vec![None; self.nodes.len()];
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
            !excluded.contains(&index) && self.nodes[index].summary == epoch[owner]
        };
        let mut responses = HashMap::new();
        for &index in path {
            let node = &self.nodes[index];
            if let Some(response) = &node.response {
                responses.insert((response, node.writer.as_deref(), epoch[index]), index);
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
        let results = self.parallel_results(events, &active, &anchors, &can_recover, record_for);
        let mut order = Vec::new();
        for &index in path {
            let node = &self.nodes[index];
            if let Some(group) = blocks.get(&index) {
                for &block in group.iter().rev() {
                    order.extend(self.nodes[block].events.clone());
                }
            } else {
                order.extend(node.events.clone());
            }
            if let Some(extra) = results.get(&index) {
                order.extend(extra);
            }
        }
        order
    }

    /// Recover individual result blocks. Other content in an abandoned user turn stays off the branch.
    fn parallel_results<'a>(
        &self,
        events: &[ClaudeEvent],
        active: &[bool],
        anchors: &[Option<usize>],
        can_recover: &impl Fn(usize, usize) -> bool,
        record_for: impl Fn(&Uuid) -> Option<&'a ClaudeRecord>,
    ) -> HashMap<usize, Vec<usize>> {
        let calls = ToolCalls::new(&self.nodes, events, anchors);
        let mut outcomes = HashMap::new();
        let mut unplaced_selected = HashSet::new();
        for (index, node) in self.nodes.iter().enumerate().filter(|(_, node)| node.user) {
            let source = self
                .tool_sources
                .get(&index)
                .map_or(ToolSource::Absent, |id| {
                    let resolved = id.as_ref().and_then(&record_for).and_then(|record| {
                        let source = record.branch.node?;
                        // A progress record can point at the assistant but cannot identify its call.
                        (source < index
                            && !self.nodes[source].user
                            && self.nodes[source].events == record.events)
                            .then_some(source)
                    });
                    resolved.map_or(ToolSource::Unresolved, ToolSource::Record)
                });
            for position in node.events.clone() {
                let event = &events[position];
                if !matches!(event.kind, EventKind::ToolCompleted | EventKind::ToolFailed) {
                    continue;
                }
                let Some(id) = event.payload["tool_use_id"].as_str() else {
                    continue;
                };
                let key = (node.writer.as_deref(), id);
                let Some((owner, mut priority)) =
                    calls.owner(index, node, id, source, &self.nodes, anchors)
                else {
                    if active[index] {
                        unplaced_selected.insert(key);
                    }
                    continue;
                };
                if !active[index] && !can_recover(index, owner) {
                    continue;
                }
                if active[index] {
                    priority = ResultSource::Selected;
                }
                outcomes
                    .entry((owner, id))
                    .and_modify(|(best, kept)| {
                        match priority.cmp(best) {
                            std::cmp::Ordering::Greater => {
                                *best = priority;
                                *kept = Some(position);
                            }
                            // Two equally plausible orphaned outcomes cannot establish which one survived.
                            std::cmp::Ordering::Equal => *kept = None,
                            std::cmp::Ordering::Less => {}
                        }
                    })
                    .or_insert((priority, Some(position)));
            }
        }
        let mut extras = HashMap::<usize, Vec<usize>>::new();
        for ((owner, id), (source, position)) in outcomes {
            if source != ResultSource::Selected
                && let Some(position) = position
                && !unplaced_selected.contains(&(self.nodes[owner].writer.as_deref(), id))
            {
                extras.entry(owner).or_default().push(position);
            }
        }
        for positions in extras.values_mut() {
            positions.sort_unstable();
        }
        extras
    }
}

#[derive(Clone, Copy)]
enum ToolSource {
    Absent,
    Unresolved,
    Record(usize),
}

struct ToolCalls<'a> {
    by_record: HashSet<(usize, &'a str)>,
    first_in_response: HashMap<(usize, &'a str), usize>,
    unique_owner: HashMap<(Option<&'a str>, &'a str), Option<usize>>,
}

impl<'a> ToolCalls<'a> {
    fn new(nodes: &'a [Node], events: &'a [ClaudeEvent], anchors: &[Option<usize>]) -> Self {
        let mut calls = Self {
            by_record: HashSet::new(),
            first_in_response: HashMap::new(),
            unique_owner: HashMap::new(),
        };
        for (index, node) in nodes.iter().enumerate() {
            for event in &events[node.events.clone()] {
                if event.kind == EventKind::ToolCalled
                    && let Some(id) = event.payload["id"].as_str()
                {
                    calls.by_record.insert((index, id));
                    let owner = anchors[index];
                    if let Some(owner) = owner {
                        calls.first_in_response.entry((owner, id)).or_insert(index);
                    }
                    calls
                        .unique_owner
                        .entry((node.writer.as_deref(), id))
                        .and_modify(|prior| {
                            if *prior != owner {
                                *prior = None;
                            }
                        })
                        .or_insert(owner);
                }
            }
        }
        calls
    }

    fn owner(
        &self,
        index: usize,
        node: &Node,
        id: &str,
        source: ToolSource,
        nodes: &[Node],
        anchors: &[Option<usize>],
    ) -> Option<(usize, ResultSource)> {
        let precedes_result = |owner: &usize| {
            self.first_in_response
                .get(&(*owner, id))
                .is_some_and(|&caller| caller < index)
        };
        let parent = node
            .parent
            .and_then(|p| anchors[p])
            .filter(|&owner| nodes[owner].writer == node.writer)
            .filter(precedes_result);
        if matches!(source, ToolSource::Unresolved) {
            return None;
        }
        if let ToolSource::Record(source) = source {
            let owner = anchors[source]?;
            if nodes[source].writer != node.writer || !self.by_record.contains(&(source, id)) {
                return None;
            }
            // Two assistants that claim the same call ID cannot identify each other's result.
            if parent.is_some_and(|other| other != owner)
                || (parent.is_none()
                    && node
                        .parent
                        .is_some_and(|p| self.by_record.contains(&(p, id))))
            {
                return None;
            }
            return Some((owner, ResultSource::Linked));
        }
        if let Some(owner) = parent {
            return Some((owner, ResultSource::Linked));
        }
        let owner = self
            .unique_owner
            .get(&(node.writer.as_deref(), id))
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
