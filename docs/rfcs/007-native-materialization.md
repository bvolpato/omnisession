# RFC 007: Native materialization

Status: accepted

Direct private-format target-store writes are disabled by default. Accepted exact-version writers and provider import interfaces create new target IDs, receive read-back verification, and roll back exact generated sessions on failure.

A future private writer is eligible only when:

- Provider version range is exact and tested.
- New session ID is generated.
- Source and existing target sessions remain unchanged.
- Write is transactional with backup and rollback.
- Target adapter reads result back and matches expected fingerprint.
- Target adapter or provider interface reads full materialized history back.
- Capability and loss report identifies synthesized fields.

Unknown versions fail closed and use semantic handoff.

Codex 0.145.0 is the first accepted implementation. OmniSession creates a thread through documented app-server RPC, injects bounded Responses API history, shuts down app-server to flush its rollout, and verifies the new thread through the read-only Codex adapter. No rollout or catalog file is written directly. Support is exact-version gated because `thread/inject_items` is experimental.

Grok 0.2.112 is accepted through its ACP session import extension. OmniSession submits native update envelopes, reads state and complete updates back through ACP, then verifies the session through its read-only adapter. Failure deletes only the generated session ID.

Claude Code 2.1.220 is accepted as a text-only private writer. OmniSession builds current user and synthetic-assistant records in a new UUID transcript, writes through a same-directory private temporary file, syncs, publishes without replacement, and verifies full history through its read-only adapter. Rollback first compares every generated record. Tool events remain documentary assistant text. File history, plans, permissions, subagents, attachments, and private reasoning are not synthesized.

Cursor Agent 2026.07.09-a3815c0 is not accepted. Its hidden `--conversation-history-file` option supplies history to one headless action, but a second native `--resume` does not receive that history. The CLI has no transcript export/read-back or exact-session delete command. CLI and ACP stores are separate, so ACP cannot verify or roll back that result. A future private SQLite/protobuf writer needs an exact schema implementation and independent reader before activation.
