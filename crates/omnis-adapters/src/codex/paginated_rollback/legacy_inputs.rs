use std::collections::HashMap;

use super::{IdKind, PaginatedRollback, Withdrawal};
use crate::codex::paginated_checkpoint::{LegacyInput, LegacySource};

impl PaginatedRollback {
    /// Include unpublished inputs in the native budget without copying their text.
    pub(in super::super) fn legacy_inputs(&self, after: u64, through: u64) -> Vec<LegacyInput> {
        let start = self.model.partition_point(|raw| raw.ordinal <= after);
        let end = self.model.partition_point(|raw| raw.ordinal <= through);
        if start >= end {
            return Vec::new();
        }
        let aliases = self.resolved_turns();
        let mut marked = HashMap::new();
        let mut witnesses = HashMap::new();
        self.match_model_inputs(0, &aliases, &mut marked, |seq, raw, shown| {
            if raw.text.is_some()
                && raw.text == shown.text
                && (shown.plain_text() || shown.retains_caption(raw.media))
            {
                witnesses.insert(raw.ordinal, seq);
            }
        });
        self.model
            .range(start..end)
            .filter(|raw| raw.kind != IdKind::InterAgent)
            .map(|raw| LegacyInput {
                ordinal: raw.ordinal,
                tokens: raw.legacy_tokens,
                source: witnesses
                    .get(&raw.ordinal)
                    .copied()
                    .filter(|seq| marked.get(seq) == Some(&Withdrawal::Confirmed))
                    .map(LegacySource::User),
                order: raw.order,
                partial: false,
            })
            .collect()
    }
}
