# RFC 007: Native materialization

Status: accepted

Direct private-format target-store writes are disabled below each accepted minimum provider version. Writers and provider import interfaces create new target IDs, receive read-back verification, and roll back exact generated sessions on failure before lineage commits. After lineage commits, launch failure preserves verified target and valid binding.

A future private writer is eligible only when:

- Provider has a tested minimum version. Newer versions remain enabled unless structural validation or read-back fails.
- New session ID is generated.
- Source and existing target sessions remain unchanged.
- Write is transactional with backup and rollback.
- Multi-resource private writes are serialized by canonical provider root outside provider storage.
- Target adapter reads result back and matches expected fingerprint.
- Target adapter or provider interface reads full materialized history back.
- Capability and loss report identifies synthesized fields.

Older or malformed versions fail closed and use semantic handoff.

Claude Code, Antigravity, and Cursor IDE keep provider-root writer lock through launch planning, lineage recording, and successful provider process creation. Lock releases before waiting for provider exit so blocked OmniSession writers can recheck provider activity without waiting for long-lived session.

Windows reserves user-scoped lock namespaces under `%LOCALAPPDATA%\OmniSession\provider-locks`, outside provider storage and independent of `OMNISESSION_HOME`. Lock setup rejects reparse-point directory chains, non-disk or multi-link lock files, and opened-path identity changes before taking a kernel file lock. This namespace is not independently ACL-attested as owner-private. Claude Code, Antigravity, and Cursor IDE Windows writers remain disabled until each provider has accepted version, schema, active-writer, rollback, and read-back evidence.

Pi 0.82.0 and newer are accepted for documented session format v3. OmniSession writes a new UUID session header and parent-linked message entries into a private same-directory temporary file, syncs it, publishes without replacement, and reads it through Pi adapter before launch. `pi --session ID` resumes generated session and `pi --fork ID` handles same-provider forks. Unknown session-format versions fail closed.

Antigravity CLI 1.1.8 and newer are accepted as a private Linux SQLite/protobuf writer. OmniSession refuses writes while Antigravity is active and serializes its own materialization, read-back, and rollback with an owner-private system-temporary lock keyed by canonical Antigravity data root. Lock location is independent of configured OmniSession state and leaves no provider-store metadata. Writer validates target schemas, creates a new conversation database before publishing one new summary row, then reads visible history through independent adapter. Failure and same-provider fork rollback compare and remove only exact generated records under same lock. Other platforms and older versions fail closed.

Codex 0.146.0 and newer are accepted. OmniSession gives its provider-owned app-server importer a private temporary, redacted transcript, waits for the generated thread ID, verifies completed visible turns through `thread/read`, and verifies the result again through the read-only Codex adapter. No rollout or catalog file is written directly.

Grok 0.2.114 and newer are accepted through its ACP session import extension. OmniSession submits native update envelopes, reads state and complete updates back through ACP, then verifies the session through its read-only adapter. Failure deletes only the generated session ID. The minimum version protects the import contract without rejecting compatible patch releases; read-back verification and exact-ID rollback remain mandatory.

Hermes 0.19.1 and newer are accepted through provider-owned `SessionDB.import_sessions`. OmniSession invokes installed Hermes Python runtime with JSON over stdin, creates one generated session ID, and independently reads full visible history from documented `state.db`. Imported titles are redacted, terminal-safe, bounded, and allocated through Hermes's native title lineage (`title`, `title #2`, ...). Same-provider forks must preserve native parent ID plus source and branch lineage markers. Failed post-import verification calls documented exact-ID deletion. Launcher without discoverable Python runtime, unsupported database shape, and older releases fail closed before target write.

Claude Code releases 2.1.220 and newer are accepted for text-only private writing on Linux and macOS. OmniSession requires Claude to be closed and serializes its own writers with an owner-scoped private lock in the system temporary directory, keyed by the canonical Claude projects root and independent of configured OmniSession state. No lock artifact is added to provider storage. It builds current user and synthetic-assistant records in a new UUID transcript, writes through a same-directory private temporary file, syncs, publishes without replacement, and verifies full history through its read-only adapter. Rollback first compares every generated record. Native writes fail closed on other platforms. Tool events remain documentary assistant text. File history, plans, permissions, subagents, attachments, and private reasoning are not synthesized.

Cursor Agent 2026.07.23-e383d2b and newer are accepted as a private SQLite/protobuf writer. OmniSession creates a new UUID under the MD5 workspace directory and writes schema-version 1 `blobs` and `meta` tables. Blob IDs are SHA-256 digests. Root state contains model-visible JSON prompt references, turn references, mode, and start time. Each turn contains a user message, assistant steps, and a rewind anchor pointing to history before that turn.

Cursor files are built in a private same-filesystem staging directory, synced, and published into a new target directory with `meta.json` last as commit marker. Read-back uses an independent adapter over a private SQLite snapshot and verifies content-addresses, protobuf structure, message identity, and rewind anchors. Failure rollback compares exact metadata and every generated blob before removing only that UUID. Tool activity remains bounded documentary assistant text. Older Cursor builds use semantic handoff before any write.

Cursor IDE 3.12.17 and newer are accepted as Linux and macOS private SQLite writers. Support requires accepted `composerHeaders`, `cursorDiskKV`, and workspace `ItemTable` schemas. Linux workspace identity follows Cursor's path-and-inode MD5 contract, so a target does not need an existing workspace-storage record. macOS requires an existing workspace-storage record whose `workspace.json` resolves to target path.

OmniSession requires Cursor to be closed, verifies this through platform process state, and serializes its own materialization, read-back, and rollback with an owner-private system-temporary lock keyed by canonical Cursor User metadata root. Lock location is independent of configured OmniSession state and leaves no provider-store metadata. Under lock, writer uses immediate transactions with no busy wait to insert a new composer UUID, visible bubbles, content-addressed prompt history, native turn structures, assistant steps, and per-turn rewind anchors. It reads every visible message through independent Cursor IDE adapter. Content-addressed blobs already present with identical bytes are reused; mismatches fail closed.

When an existing workspace database is available, OmniSession reads and replaces only its `composer.composerData` value to select the imported composer on startup. It does not query any other `ItemTable` key. Rollback validates the generated composer, restores the exact previous selection value and SQLite type, then deletes only generated rows. Without existing workspace state, Cursor opens the workspace and the imported composer remains available in History.
