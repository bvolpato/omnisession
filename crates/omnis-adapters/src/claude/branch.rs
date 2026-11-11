//! Select the conversation that Claude would continue after a rewind.

use std::{
    collections::{HashMap, HashSet},
    ops::Range,
};

use omnis_ir::EventKind;
use serde_json::Value;
use uuid::Uuid;

use super::{ClaudeEvent, ClaudeRecord, PreservedContext, string_at};

#[derive(Clone, Copy, Default)]
pub(super) struct Parent {
    node: Option<usize>,
    suffix: Option<Uuid>,
    missing_ancestor: bool,
}

struct Node {
    parent: Option<usize>,
    events: Range<usize>,
    response: Option<String>,
    writer: Option<String>,
    user: bool,
    missing_ancestor: bool,
}

/// Only messages need a node. A progress or attachment record can point to its nearest
/// conversation ancestor in the UUID map.
#[derive(Default)]
pub(super) struct Branch {
    nodes: Vec<Node>,
    summary: Option<usize>,
    suffix_parents: HashMap<usize, Uuid>,
    retained: HashSet<Uuid>,
    forked: bool,
    pub(super) sampled_tail: bool,
}

impl Branch {
    pub(super) fn observe(
        &mut self,
        record: &Value,
        records: &HashMap<Uuid, ClaudeRecord>,
        preserved: Option<&PreservedContext>,
        events: Range<usize>,
    ) -> Parent {
        let previous = Parent {
            node: self.nodes.len().checked_sub(1),
            missing_ancestor: self.missing_ancestor(),
            ..Parent::default()
        };
        // The preview intentionally joins two known byte windows. A full read must never
        // join an unresolved parent to whichever message preceded it in the file.
        let unresolved = if self.sampled_tail {
            previous
        } else {
            Parent {
                missing_ancestor: true,
                ..Parent::default()
            }
        };
        let lookup = |value: &Value| {
            let id = value.as_str()?.parse::<Uuid>().ok()?;
            Some((id, records.get(&id)?.branch))
        };
        if record["type"] == "system" && record["subtype"] == "compact_boundary" {
            self.summary = None;
            self.suffix_parents.clear();
            self.retained.clear();
            if let Some(context) = preserved {
                self.retained.extend(&context.records);
            }
            // Earlier conversation remains available to the recovery file.
            return match record.get("logicalParentUuid").filter(|id| !id.is_null()) {
                Some(id) => lookup(id).map_or(unresolved, |(_, branch)| branch),
                None => previous,
            };
        }
        let parent = match record.get("parentUuid") {
            None => previous, // Older records have no parent field.
            Some(Value::Null) => Parent::default(),
            Some(value) => lookup(value).map_or(unresolved, |(id, parent)| {
                // A reply can point back to the physically earlier suffix that Claude moved
                // after its summary. Rewinds to that suffix still belong to the compacted turn.
                if let Some(summary) = self.summary
                    && parent.node < Some(summary)
                    && self.retained.contains(&id)
                {
                    Parent {
                        node: Some(summary),
                        suffix: Some(id),
                        missing_ancestor: self.nodes[summary].missing_ancestor,
                    }
                } else {
                    parent
                }
            }),
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
            events,
            response: (role == Some("assistant"))
                .then(|| record["message"]["id"].as_str())
                .flatten()
                .map(str::to_owned),
            writer: record["agentId"].as_str().map(str::to_owned),
            user: role == Some("user"),
            missing_ancestor: parent.missing_ancestor,
        });
        if record["isCompactSummary"] == true {
            self.summary = Some(index);
        }
        Parent {
            node: Some(index),
            suffix: None,
            missing_ancestor: parent.missing_ancestor,
        }
    }

    pub(super) fn missing_ancestor(&self) -> bool {
        self.nodes.last().is_some_and(|node| node.missing_ancestor)
    }

    /// A rewind into the retained context can abandon its last messages.
    pub(super) fn retained_tip(&self) -> Option<Uuid> {
        if !self.suffix_parents.is_empty() {
            let mut next = self.nodes.len().checked_sub(1);
            while let Some(index) = next {
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
        let mut next = self.nodes.len().checked_sub(1);
        while let Some(index) = next {
            active[index] = true;
            next = self.nodes[index].parent;
        }
        let mut responses = HashMap::new();
        for (index, node) in self.nodes.iter().enumerate().filter(|(i, _)| active[*i]) {
            if let Some(response) = node.response.as_deref() {
                responses.insert((response, node.writer.as_deref()), index);
            }
        }
        let anchors: Vec<_> = self
            .nodes
            .iter()
            .map(|node| {
                responses
                    .get(&(node.response.as_deref()?, node.writer.as_deref()))
                    .copied()
            })
            .collect();
        let mut extra_calls = HashMap::<usize, Vec<usize>>::new();
        for (index, anchor) in anchors.iter().enumerate() {
            if let Some(anchor) = anchor
                && !active[index]
            {
                extra_calls.entry(*anchor).or_default().push(index);
            }
        }
        let extra_results = self.parallel_results(events, &active, &anchors);
        let mut order = Vec::new();
        for (index, node) in self.nodes.iter().enumerate().filter(|(i, _)| active[*i]) {
            order.extend(node.events.clone());
            for extra in extra_calls
                .get(&index)
                .into_iter()
                .flatten()
                .chain(extra_results.get(&index).into_iter().flatten())
            {
                order.extend(self.nodes[*extra].events.clone());
            }
        }
        order
    }

    fn parallel_results(
        &self,
        events: &[ClaudeEvent],
        active: &[bool],
        anchors: &[Option<usize>],
    ) -> HashMap<usize, Vec<usize>> {
        let mut owners = HashMap::new();
        let mut answered = HashSet::new();
        let mut extras = HashMap::<usize, Vec<usize>>::new();
        let mut strays = Vec::new();
        for (index, node) in self.nodes.iter().enumerate() {
            let parent_anchor = node.parent.and_then(|parent| anchors[parent]);
            let mut result_ids = Vec::new();
            for event in &events[node.events.clone()] {
                if event.kind == EventKind::ToolCalled
                    && let Some(id) = event.payload["id"].as_str()
                {
                    let owner = anchors[index];
                    owners
                        .entry(id)
                        .and_modify(|previous| {
                            if *previous != owner {
                                *previous = None;
                            }
                        })
                        .or_insert(owner);
                } else if node.user
                    && matches!(event.kind, EventKind::ToolCompleted | EventKind::ToolFailed)
                    && let Some(id) = event.payload["tool_use_id"].as_str()
                {
                    result_ids.push(id);
                }
            }
            if !result_ids.is_empty() {
                if active[index] {
                    answered.extend(result_ids);
                } else if let Some(anchor) = parent_anchor {
                    extras.entry(anchor).or_default().push(index);
                    answered.extend(result_ids);
                } else {
                    strays.push((index, result_ids));
                }
            }
        }
        // Some results attach to a progress entry on another path. Recover one only when
        // its tool ID identifies an unanswered call from the same writer.
        for (index, ids) in strays {
            if let Some(anchor) = ids
                .iter()
                .filter(|id| !answered.contains(**id))
                .filter_map(|id| owners.get(id).copied().flatten())
                .find(|&owner| self.nodes[index].writer == self.nodes[owner].writer)
            {
                extras.entry(anchor).or_default().push(index);
                answered.extend(ids);
            }
        }
        extras
    }
}
