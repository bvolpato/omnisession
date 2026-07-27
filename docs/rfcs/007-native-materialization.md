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
