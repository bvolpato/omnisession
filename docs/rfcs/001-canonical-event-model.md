# RFC 001: Canonical event model

Status: accepted

OmniSession represents provider history as append-only events. Message-only schemas cannot preserve compaction, approvals, tools, file patches, plans, subagents, checkpoints, or forks.

Each event carries schema version, event ID, logical thread and branch IDs, sequence, optional timestamp, provider source, event kind, payload, optional raw blob hash, sensitivity, and replay policy.

Replay policies:

- `contextual`: safe to summarize for target context
- `historical_only`: evidence, never executable instruction
- `replayable`: deterministic data operation, rarely used
- `secret`: excluded unless explicitly requested

Future adapters may retain raw provider records as opaque content-addressed attachments. Current schema 1.0 preserves selected metadata as `provider_event`; other unrecognized private records are omitted and reported as unsupported at export boundaries.
