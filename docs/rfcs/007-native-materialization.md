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

Pi session format v3 is accepted as a documented JSONL target. OmniSession writes a new UUID session header and parent-linked message entries into a private same-directory temporary file, syncs it, publishes without replacement, and reads it through the Pi adapter before launch. `pi --session ID` resumes the generated session and `pi --fork ID` handles same-provider forks. Unknown session-format versions fail closed.

Antigravity CLI 1.1.8 with the accepted Linux executable fingerprint is accepted as a private SQLite/protobuf writer. OmniSession refuses writes while Antigravity is active, creates a new conversation database before publishing one new summary row, then reads visible history through the independent adapter. Failure and same-provider fork rollback compare and remove only exact generated records. Other platforms and builds fail closed.

Codex 0.145.0 is the first accepted implementation. OmniSession creates a thread through documented app-server RPC, injects bounded Responses API history, shuts down app-server to flush its rollout, and verifies the new thread through the read-only Codex adapter. No rollout or catalog file is written directly. Support is exact-version gated because `thread/inject_items` is experimental.

Grok 0.2.112 is accepted through its ACP session import extension. OmniSession submits native update envelopes, reads state and complete updates back through ACP, then verifies the session through its read-only adapter. Failure deletes only the generated session ID.

Claude Code 2.1.220 is accepted as a text-only private writer. OmniSession builds current user and synthetic-assistant records in a new UUID transcript, writes through a same-directory private temporary file, syncs, publishes without replacement, and verifies full history through its read-only adapter. Rollback first compares every generated record. Tool events remain documentary assistant text. File history, plans, permissions, subagents, attachments, and private reasoning are not synthesized.

Cursor Agent 2026.07.23-e383d2b is accepted as a private SQLite/protobuf writer. Support requires exact version and SHA-256 fingerprints for `index.js`, `8176.index.js`, and `1931.index.js`. OmniSession creates a new UUID under the MD5 workspace directory and writes schema-version 1 `blobs` and `meta` tables. Blob IDs are SHA-256 digests. Root state contains model-visible JSON prompt references, turn references, mode, and start time. Each turn contains a user message, assistant steps, and a rewind anchor pointing to history before that turn.

Cursor files are built in a private same-filesystem staging directory, synced, and published into a new target directory with `meta.json` last as commit marker. Read-back uses an independent adapter over a private SQLite snapshot and verifies content-addresses, protobuf structure, message identity, and rewind anchors. Failure rollback compares exact metadata and every generated blob before removing only that UUID. Tool activity remains bounded documentary assistant text. Unknown or modified Cursor builds use semantic handoff before any write.

Cursor IDE 3.12.17 commit `0fb762053c34788bb7760d5673f8a6d4c8589d50` is accepted as a Linux-only private SQLite writer. Support requires an exact AppImage or installed-bundle fingerprint plus accepted `composerHeaders`, `cursorDiskKV`, and workspace `ItemTable` schemas. Linux workspace identity follows Cursor's path-and-inode MD5 contract, so a target does not need an existing workspace-storage record.

OmniSession requires Cursor to be closed and uses immediate transactions with no busy wait. It inserts a new composer UUID, visible bubbles, content-addressed prompt history, native turn structures, assistant steps, and per-turn rewind anchors. It reads every visible message through the independent Cursor IDE adapter. Content-addressed blobs already present with identical bytes are reused; mismatches fail closed.

When an existing workspace database is available, OmniSession reads and replaces only its `composer.composerData` value to select the imported composer on startup. It does not query any other `ItemTable` key. Rollback validates the generated composer, restores the exact previous selection value and SQLite type, then deletes only generated rows. Without existing workspace state, Cursor opens the workspace and the imported composer remains available in History.
