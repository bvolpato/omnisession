//! Codex's visible assistant text in Plan mode.

use std::{borrow::Cow, collections::HashMap};

use omnis_ir::{EventKind, ReplayPolicy};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::{EventBuilder, MAX_CANONICAL_EVENTS};

type Key = [u8; 32];

/// Codex removes citations before rendering the assistant text that surrounds a plan.
pub(super) fn without_plan(text: &str) -> Option<String> {
    if !text.contains("<proposed_plan>") && !text.contains("<oai-mem-citation>") {
        return None;
    }
    let mut visible = String::new();
    last_plan(&uncited(text), |line| visible.push_str(line))?;
    Some(visible)
}

/// One model response can produce both a public assistant message and a separate Plan.
pub(super) fn model_plan(item: &Value) -> Option<(Option<Key>, Key)> {
    if item["type"] != "message" || item["role"] != "assistant" {
        return None;
    }
    let mut parts = item["content"]
        .as_array()?
        .iter()
        .filter(|part| part["type"] == "output_text");
    let mut text = Cow::Borrowed(parts.next()?["text"].as_str()?);
    for part in parts {
        text.to_mut().push_str(part["text"].as_str()?);
    }
    let plan = proposal_digest(&text)?;
    let mut outer = String::new();
    last_plan(&uncited(&text), |line| outer.push_str(line));
    let assistant = (!outer.trim().is_empty()).then(|| Sha256::digest(outer.as_bytes()).into());
    Some((assistant, plan))
}

/// The native Plan item contains the last block in a response, with its citations removed.
pub(super) fn proposal_digest(text: &str) -> Option<Key> {
    let body = last_plan(text, |_| {})?;
    Some(display_digest(&uncited(body)))
}

fn uncited(text: &str) -> Cow<'_, str> {
    let mut result = String::new();
    if visit_uncited(text, |part| result.push_str(part)) {
        Cow::Owned(result)
    } else {
        Cow::Borrowed(text)
    }
}

pub(super) fn display_digest(text: &str) -> Key {
    // Some restored Plan items omit the line ending before the closing tag.
    let text = text
        .strip_suffix("\r\n")
        .or_else(|| text.strip_suffix('\n'))
        .unwrap_or(text);
    Sha256::digest(text.as_bytes()).into()
}

fn last_plan(text: &str, mut visible: impl FnMut(&str)) -> Option<&str> {
    const OPEN: &str = "<proposed_plan>";
    const CLOSE: &str = "</proposed_plan>";
    if !text.contains(OPEN) {
        visible(text);
        return None;
    }
    let mut start = None;
    let mut last = None;
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        match (start, line.trim()) {
            (None, OPEN) => start = Some(offset + line.len()),
            (Some(from), CLOSE) => {
                last = Some(&text[from..offset]);
                start = None;
            }
            (None, _) => visible(line),
            (Some(_), _) => {}
        }
        offset += line.len();
    }
    start.map(|from| &text[from..]).or(last)
}

#[derive(Default)]
struct PlanPresence {
    proposal: Option<u64>,
    display: Option<u64>,
}

#[derive(Default)]
pub(super) struct PlanReceipts {
    plans: HashMap<(Option<Key>, Key), PlanPresence>,
    overflow: bool,
}

impl PlanReceipts {
    pub(super) fn proposal(&mut self, turn: Option<&str>, body: Key, owner: Option<u64>) {
        self.observe(turn, body, owner, false);
    }

    pub(super) fn completed(&mut self, turn: &str, text: &str, owner: Option<u64>) {
        self.observe(Some(turn), display_digest(text), owner, true);
    }

    fn observe(&mut self, turn: Option<&str>, body: Key, owner: Option<u64>, display: bool) {
        let key = (
            turn.map(|turn| Sha256::digest(turn.as_bytes()).into()),
            body,
        );
        if self.plans.len() >= MAX_CANONICAL_EVENTS && !self.plans.contains_key(&key) {
            self.overflow |= !display;
            return;
        }
        let plan = self.plans.entry(key).or_default();
        let first = if display {
            &mut plan.display
        } else {
            &mut plan.proposal
        };
        // Zero precedes the first instruction. Only the earliest receipt needs to survive rollback.
        let position = owner.map_or(0, |input| input + 1);
        *first = Some(first.map_or(position, |prior| prior.min(position)));
    }

    pub(super) fn rollback(&mut self, cut: u64) {
        self.plans.retain(|_, plan| {
            plan.proposal = plan.proposal.filter(|&position| position <= cut);
            plan.display = plan.display.filter(|&position| position <= cut);
            plan.proposal.is_some() || plan.display.is_some()
        });
    }

    pub(super) fn report(&self, builder: &mut EventBuilder) {
        let missing = self
            .plans
            .values()
            .filter(|plan| plan.proposal.is_some() && plan.display.is_none())
            .count();
        if missing > 0 || self.overflow {
            builder.push(EventKind::ProviderEvent, json!({"omitted_events":missing, "native_history_unverified":true,
                    "reason":"The retained Codex history could not confirm every proposed plan with a completed display."}),
                None, ReplayPolicy::HistoricalOnly, Some("omnisession.codex_plan_display_unverified".into()), None);
        }
    }
}

/// Visit the visible spans only when Codex citation markup is present.
pub(super) fn visit_uncited(text: &str, mut visit: impl FnMut(&str)) -> bool {
    const OPEN: &str = "<oai-mem-citation>";
    const CLOSE: &str = "</oai-mem-citation>";
    let Some((before, mut citation)) = text.split_once(OPEN) else {
        return false;
    };
    visit(before);
    while let Some((_, rest)) = citation.split_once(CLOSE) {
        if let Some((normal, next)) = rest.split_once(OPEN) {
            visit(normal);
            citation = next;
        } else {
            visit(rest);
            break;
        }
    }
    true
}
