use std::{cell::RefCell, collections::VecDeque, rc::Rc};

use serde_json::Value;

use super::{WITNESS, shell};

#[derive(Clone, Default)]
pub(in crate::codex) struct ShellSources(Rc<RefCell<RecentSources>>);

#[derive(Default)]
struct RecentSources {
    displays: VecDeque<(String, String)>,
    bytes: usize,
}

impl ShellSources {
    pub(in crate::codex) fn remember(&self, display: &Value, item: &Value) {
        if item["type"] != "CommandExecution" {
            return;
        }
        let Some(source) = item["aggregated_output"].as_str() else {
            return;
        };
        let Some(projected) = display["output"].as_str() else {
            return;
        };
        if projected.len() == source.len() && projected == source {
            return;
        }
        let Some(key) = display[WITNESS]["body"].as_str() else {
            return;
        };
        let mut recent = self.0.borrow_mut();
        if recent.displays.iter().any(|(digest, _)| digest == key) {
            return;
        }
        let limit = usize::try_from(crate::support::MAX_STREAMED_TRANSCRIPT_LINE_SIZE)
            .unwrap_or(usize::MAX);
        if source.len() > limit {
            return;
        }
        while recent.bytes > limit - source.len() {
            let Some((_, old)) = recent.displays.pop_front() else {
                break;
            };
            recent.bytes -= old.len();
        }
        recent.bytes += source.len();
        recent
            .displays
            .push_back((key.to_owned(), source.to_owned()));
    }

    pub(super) fn agrees(&self, display: &Value, witness: &Value) -> bool {
        let recent = self.0.borrow();
        let source = witness.get("cut").and_then(|_| {
            let key = display[WITNESS]["body"].as_str()?;
            recent
                .displays
                .iter()
                .rev()
                .find(|(digest, _)| digest == key)
                .map(|(_, text)| text.as_str())
        });
        shell::agrees(display, witness, source)
    }
}
