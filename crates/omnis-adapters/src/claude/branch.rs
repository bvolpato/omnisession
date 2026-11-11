//! Select the conversation that Claude would continue after a rewind.

use std::{
    collections::{HashMap, HashSet},
    ops::Range,
};

use omnis_ir::EventKind;
use serde_json::Value;
use uuid::Uuid;

use super::{ClaudeEvent, ClaudeRecord, Compactions, ResponseId, response_id, string_at};

#[derive(Clone, Copy, Default)]
pub(super) struct Parent {
    node: Option<usize>,
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

struct SampledTail {
    head: Option<usize>,
    external_parents: HashMap<Uuid, usize>,
}

/// A progress or attachment record points to its nearest conversation ancestor in the UUID map.
#[derive(Default)]
pub(super) struct Branch {
    nodes: Vec<Node>,
    tip: Option<usize>,
    ever_retained: HashSet<Uuid>,
    suffix_parents: HashMap<usize, Uuid>,
    retained: HashSet<Uuid>,
    forked: bool,
    sampled_tail: Option<SampledTail>,
}

impl Branch {
    pub(super) fn start_sampled_tail(&mut self) {
        self.sampled_tail = Some(SampledTail {
            head: self.tip,
            external_parents: HashMap::new(),
        });
    }

    fn unresolved_parent(&mut self, value: &Value) -> Parent {
        let id = value.as_str().and_then(|s| Uuid::parse_str(s).ok());
        if let (Some(id), Some(tail)) = (id, &mut self.sampled_tail) {
            // Give each parent in the unread middle its own node. Siblings then meet at
            // that node, and the preview still includes the sampled head.
            let index = *tail.external_parents.entry(id).or_insert_with(|| {
                let index = self.nodes.len();
                self.nodes.push(Node {
                    parent: tail.head,
                    summary: tail.head.and_then(|index| self.nodes[index].summary),
                    missing_ancestor: tail.head.is_some_and(|i| self.nodes[i].missing_ancestor),
                    ..Node::default()
                });
                index
            });
            return Parent {
                node: Some(index),
                missing_ancestor: self.nodes[index].missing_ancestor,
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
        &self,
        id: Uuid,
        parent: Parent,
        records: &HashMap<Uuid, ClaudeRecord>,
        compactions: &Compactions,
    ) -> Option<usize> {
        let mut summary = self.selected_summary()?;
        if parent.node >= Some(summary) || !self.ever_retained.contains(&id) {
            return None;
        }
        if self.retained.contains(&id) {
            return Some(summary);
        }
        // A UUID retained by another branch cannot reconnect that branch's discarded summary.
        while let Some(earlier) = self.nodes[summary]
            .parent
            .and_then(|i| self.nodes[i].summary)
        {
            if parent.node >= Some(earlier) {
                break;
            }
            if compactions
                .context(Some(earlier), records)
                .ok()
                .flatten()
                .is_some_and(|context| context.records.contains(&id))
            {
                return Some(earlier);
            }
            summary = earlier;
        }
        None
    }

    pub(super) fn select_retention(&mut self, ids: &[Uuid]) {
        self.retained.clear();
        self.retained.extend(ids);
        self.ever_retained.extend(ids);
    }

    /// A rewind into the retained context can abandon its last messages.
    pub(super) fn retained_tip(&self) -> Option<Uuid> {
        if !self.suffix_parents.is_empty() {
            let mut next = self.tip;
            let summary = self.selected_summary()?;
            while let Some(index) = next.filter(|&index| index > summary) {
                if let Some(id) = self.suffix_parents.get(&index) {
                    return Some(*id);
                }
                next = self.nodes[index].parent;
            }
        }
        None
    }

    pub(super) fn forked(&self) -> bool {
        self.forked
    }

    pub(super) fn event_order(&self, events: &[ClaudeEvent]) -> Vec<usize> {
        if !self.forked {
            return (0..events.len()).collect();
        }
        let mut active = vec![false; self.nodes.len()];
        let mut next = self.tip;
        while let Some(index) = next {
            active[index] = true;
            next = self.nodes[index].parent;
        }
        let mut responses = HashMap::new();
        for (index, node) in self.nodes.iter().enumerate().filter(|(i, _)| active[*i]) {
            if let Some(response) = &node.response {
                responses.insert((response, node.writer.as_deref()), index);
            }
        }
        let anchors: Vec<_> = self
            .nodes
            .iter()
            .enumerate()
            .map(|(index, node)| {
                if let Some(response) = &node.response {
                    responses.get(&(response, node.writer.as_deref())).copied()
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
        let results = self.parallel_results(events, &active, &anchors);
        let mut order = Vec::new();
        for (index, node) in self.nodes.iter().enumerate().filter(|(i, _)| active[*i]) {
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
    fn parallel_results(
        &self,
        events: &[ClaudeEvent],
        active: &[bool],
        anchors: &[Option<usize>],
    ) -> HashMap<usize, Vec<usize>> {
        let mut calls = HashMap::new();
        let mut owners = HashMap::new();
        for (index, node) in self.nodes.iter().enumerate() {
            for event in &events[node.events.clone()] {
                if event.kind == EventKind::ToolCalled
                    && let Some(id) = event.payload["id"].as_str()
                {
                    let owner = anchors[index];
                    if let Some(owner) = owner {
                        calls.entry((owner, id)).or_insert(index);
                    }
                    owners
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
        let mut outcomes = HashMap::new();
        let mut unplaced_selected = HashSet::new();
        for (index, node) in self.nodes.iter().enumerate().filter(|(_, node)| node.user) {
            for position in node.events.clone() {
                let event = &events[position];
                if !matches!(event.kind, EventKind::ToolCompleted | EventKind::ToolFailed) {
                    continue;
                }
                let Some(id) = event.payload["tool_use_id"].as_str() else {
                    continue;
                };
                let key = (node.writer.as_deref(), id);
                let precedes_result = |owner: &usize| {
                    calls
                        .get(&(*owner, id))
                        .is_some_and(|&caller| caller < index)
                };
                let parent = node
                    .parent
                    .and_then(|p| anchors[p])
                    .filter(|&owner| self.nodes[owner].writer == node.writer)
                    .filter(precedes_result);
                let Some(owner) =
                    parent.or_else(|| owners.get(&key).copied().flatten().filter(precedes_result))
                else {
                    if active[index] {
                        unplaced_selected.insert(key);
                    }
                    continue;
                };
                let source = if active[index] {
                    ResultSource::Selected
                } else if parent.is_some() {
                    ResultSource::Parent
                } else {
                    ResultSource::CallId
                };
                outcomes
                    .entry((owner, id))
                    .and_modify(|(best, kept)| {
                        match source.cmp(best) {
                            std::cmp::Ordering::Greater => {
                                *best = source;
                                *kept = Some(position);
                            }
                            // Two equally plausible orphaned outcomes cannot establish which one survived.
                            std::cmp::Ordering::Equal => *kept = None,
                            std::cmp::Ordering::Less => {}
                        }
                    })
                    .or_insert((source, Some(position)));
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

#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
enum ResultSource {
    CallId,
    Parent,
    Selected,
}
