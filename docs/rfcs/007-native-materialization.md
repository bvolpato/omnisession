# RFC 007: Native materialization

Status: accepted

Native target-store writes are disabled in v0.1. A future writer is eligible only when:

- Provider version range is exact and tested.
- New session ID is generated.
- Source and existing target sessions remain unchanged.
- Write is transactional with backup and rollback.
- Target adapter reads result back and matches expected fingerprint.
- Target CLI opens result in verification mode.
- Capability and loss report identifies synthesized fields.

Unknown versions fail closed and use semantic handoff.
