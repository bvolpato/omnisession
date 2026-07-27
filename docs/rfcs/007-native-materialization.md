# RFC 007: Native materialization

Status: accepted

Direct private-format target-store writes are disabled. Documented provider import commands are separate: they create new target IDs, validate provider schema, receive read-back verification, and roll back exact generated sessions on failure.

A future private writer is eligible only when:

- Provider version range is exact and tested.
- New session ID is generated.
- Source and existing target sessions remain unchanged.
- Write is transactional with backup and rollback.
- Target adapter reads result back and matches expected fingerprint.
- Target CLI opens result in verification mode.
- Capability and loss report identifies synthesized fields.

Unknown versions fail closed and use semantic handoff.

Codex 0.145.0 is the first accepted implementation. OmniSession creates a thread through documented app-server RPC, injects bounded Responses API history, shuts down app-server to flush its rollout, and verifies the new thread through the read-only Codex adapter. No rollout or catalog file is written directly. Support is exact-version gated because `thread/inject_items` is experimental.
