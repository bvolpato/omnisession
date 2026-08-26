# Compatibility

Last verified: 2026-08-26

| Provider | Minimum version | Session source | Resume interface | Notes |
| --- | --- | --- | --- | --- |
| Claude Code | >= 2.1.220 | `~/.claude/projects/*/*.jsonl` | `claude --resume ID --fork-session` | Minimum-version transactional target writer |
| Codex | >= 0.146.0 | `~/.codex/sessions/**/*.jsonl` | `codex fork ID` | Minimum-version app-server session import |
| OpenCode | 1.18.18 | Official list/export CLI | `opencode --session ID --fork` | Public release import verified with read-back and rollback |
| Grok | >= 0.2.114 | `~/.grok/sessions/*/*/` | `grok --resume ID --fork-session` | Minimum-version ACP import and read-back |
| Hermes | >= 0.19.1 | `~/.hermes/state.db` | `hermes --resume ID` | Provider-owned session import with read-back and rollback |
| Antigravity | >= 1.1.8 | `~/.gemini/antigravity-cli/` summary SQLite plus local transcripts | `agy --conversation ID` | Minimum-version Linux SQLite/protobuf target writer |
| Pi | >= 0.82.0 | `~/.pi/agent/sessions/*.jsonl` | `pi --session ID`, `pi --fork ID` | v3 JSONL target with read-back and rollback |
| Cursor Agent | >= 2026.07.23-e383d2b | `~/.cursor/chats/*/*/store.db` | `cursor-agent --resume ID` | Minimum-version SQLite/protobuf target writer |
| Cursor IDE | >= 3.12.17 | Cursor User `globalStorage/state.vscdb` | Restored composer selection | Linux and macOS native trajectory and workspace selection |

## Platforms

Prebuilt OmniSession releases support Linux and macOS on x86-64 and ARM64. Canonical trajectories are OS-neutral: source path remains provenance while target materialization uses target machine's current workspace and native provider store.

Cursor IDE discovery and native continuation support Linux and macOS. macOS app bundles, Linux AppImages, Linux deb/rpm installs, and PATH-installed launchers are detected. Windows remains CI-tested from source but has no supported installer, provider shims, or Cursor IDE writer yet. Antigravity native target writing remains Linux-only; unsupported platform targets fall back or stay hidden.

Compatibility is capability-based. OmniSession canonicalizes recognized visible records and omits unrecognized private records. Malformed records are skipped. Discovery and transfers open source provider files read-only. Target imports always use new IDs.

Session browser deletion supports Codex, OpenCode, Grok, and Hermes on Linux and macOS through provider commands. On Linux, guarded private-store deletion also supports Antigravity, Pi, Cursor Agent, and Cursor IDE. Grok runs provider-owned search reconciliation to clear stale catalog rows. Pi and Cursor Agent mirror their native picker deletion over exact validated paths. Antigravity and Cursor IDE use verified SQLite schemas, immediate transactions, active-writer exclusion, exact-ID rows, and post-delete read-back. Claude Code remains read-only.

OpenCode target imports synthesize required message metadata, preserve bounded tools as documentary assistant history, redact credential-like text, create a new session ID, and verify history through official export before launch.

Codex 0.146.0 and newer target imports use its provider-owned external-session importer against a private temporary, redacted transcript. OmniSession verifies completed native turns through `thread/read`, confirms persisted estimated token usage in installed conformance, then reads the new session through the independent Codex adapter. Older Codex versions fall back to semantic handoff. Failed imports delete only the exact newly generated target ID. Same-provider forks are linked into the OmniSession tree when exactly one new user session appears in the launch workspace and time window; ambiguous matches are never guessed.

Grok 0.2.114 and newer target imports use `_x.ai/session/import`, then verify state and full update history through ACP before adapter read-back. Failed imports call exact-ID session deletion.

Hermes 0.19.1 and newer target imports use Hermes's own `SessionDB.import_sessions` implementation through installed Python runtime. OmniSession supplies bounded visible messages and documentary tool history, then reads new session through independent SQLite adapter. Imported titles are redacted, terminal-safe, bounded, and allocated through Hermes's native title lineage (`title`, `title #2`, ...) to satisfy provider-wide uniqueness. Same-provider forks retain native parent ID and OmniSession lineage markers. Failure removes only generated ID through `hermes sessions delete ID --yes`. Non-Python launchers and older releases fall back before any target write.

Claude Code 2.1.220 and newer target imports write a new text-only JSONL chain with current native constructors on Linux and macOS. Claude must be closed; OmniSession serializes its own writers with an owner-scoped system-temporary lock keyed by canonical projects root and independent of `OMNISESSION_HOME`, without adding provider-store metadata. Writes use a private same-directory temporary file, `fsync`, no-clobber publication, independent adapter read-back, and exact-record rollback validation. Older versions use semantic handoff; native writes fail closed on unsupported platforms.

Antigravity versions 1.1.8 and newer read conversation summaries from SQLite and visible history from transcript JSONL or conversation SQLite store. Linux writer creates a new conversation database and summary row, then verifies visible history through independent adapter before `agy --conversation ID`. It refuses writes while Antigravity is active and rolls back only exact generated records.

Pi versions 0.82.0 and newer accept documented v3 JSONL targets. OmniSession serializes native imports while checking encoded workspace-directory identity, writes a new UUID chain through a private same-directory temporary file, syncs and publishes without replacement, then reads it through Pi adapter before `pi --session ID`. `pi --fork ID` handles same-provider forks. Older versions fall back to semantic handoff.

Cursor Agent 2026.07.23-e383d2b and newer target imports build a new content-addressed SQLite/protobuf conversation graph under the exact workspace key. Model-visible prompt history, turn structures, assistant steps, and rewind anchors are written into a new UUID session. OmniSession stages and syncs both files, publishes metadata last, reads complete trajectory through its independent adapter, and validates every generated blob before rollback. Automated conformance does not launch Cursor Agent against model backend. Older builds fall back to semantic handoff.

Cursor IDE versions 3.12.17 and newer read composer conversation, historical tool, checkpoint, and diff records from `state.vscdb` through bounded query-only access on Linux and macOS. OmniSession discovers AppImage, deb/rpm, PATH, and macOS app-bundle installations. Private target writer validates required SQLite schemas and refuses writes while Cursor is running. Linux derives Cursor's workspace key from canonical path and inode, including unopened workspaces. macOS matches an existing `workspaceStorage` record, so target workspace must have been opened in Cursor once. Writer creates model-visible prompt blobs, native user turns, assistant steps, and rewind anchors under a fresh composer ID, then reads visible history back through independent adapter. For an existing workspace, it updates only `composer.composerData` in workspace state so Cursor restores imported composer, preserving previous typed value for rollback. Older builds are excluded from target choices.

SQLite adapters use query-only reads during discovery and transfer. Snapshot-based adapters copy available WAL files into private temporary storage. Explicit confirmed deletion is sole source-store mutation and follows [RFC 009](rfcs/009-native-deletion.md).

## Conformance tests

Full workspace tests run nine provider-labelled canonical snapshots through all nine target builders. All 81 cells must match one visible-history oracle, including Unicode, multiline messages, documentary tools, redaction, repeated roles, secret omission, and unknown records. Claude, Hermes, Antigravity, Cursor Agent, Cursor IDE, and Pi targets are materialized, read through independent adapters, verified, and rolled back. OpenCode is parsed back through its export adapter. Codex and Grok RPC writers are verified through installed-provider conformance.

Token-free provider conformance runs all 72 off-diagonal paths across Claude, Codex, OpenCode, Grok, Hermes, Antigravity, Pi, Cursor Agent, and Cursor IDE in isolated homes. Claude, Codex, OpenCode, Grok, and latest stable Hermes run through installed provider code. Remaining private-format writers use isolated synthetic stores and version stubs without launching model backends. Non-Codex source rows come from first-generation native imports. Every cell must pass target read-back and match original canonical trajectory, preventing loss from accumulating across hops. Scheduled and release workflows run this matrix without credentials.

Same-provider fork commands have focused launch-plan and lineage tests. They are not part of 72-cell cross-provider matrix.

```sh
OMNI_TEST_CLAUDE_BIN=/path/to/claude \
OMNI_TEST_CODEX_BIN=/path/to/codex \
OMNI_TEST_OPENCODE_BIN=/path/to/opencode \
OMNI_TEST_GROK_BIN=/path/to/grok \
OMNI_TEST_HERMES_BIN=/path/to/hermes \
OMNI_TEST_ANTIGRAVITY_BIN=/path/to/agy \
OMNI_TEST_CURSOR_BIN=/path/to/cursor-agent \
OMNI_TEST_CURSOR_IDE_BIN=/path/to/Cursor-or-Cursor.AppImage \
OMNI_TEST_PI_BIN=/path/to/pi \
  cargo test -p omnisession-cli --test native_conformance \
  installed_nine_by_nine_cross_provider_matrix -- --ignored --nocapture
```

Codex verification requires every imported message and role in completed visible turns, ignores only Codex's own external-import marker, and independently checks the persisted canonical trajectory. Missing, reordered, duplicated, or additional trajectory messages fail closed and trigger exact target rollback.

Installed OpenCode conformance runs its real import/export commands against generated 304-item history inside a temporary home and database:

```sh
OMNI_TEST_OPENCODE_BIN=/path/to/opencode \
  cargo test -p omnisession-cli \
  installed_opencode_round_trips_isolated_bounded_history \
  -- --ignored --nocapture
```

Installed Grok conformance runs its real ACP import, state verification, and filesystem read-back against generated 101-item Codex history inside temporary homes:

```sh
OMNI_TEST_GROK_BIN=/path/to/grok \
  cargo test -p omnisession-cli --test grok_conformance \
  installed_grok_round_trips_isolated_synthetic_history \
  -- --ignored --nocapture
```

Opt-in model-backed probes continue imported synthetic sessions through Pi and OpenCode noninteractive modes. They assert model recovers a marker stored only in imported history. Session storage stays temporary, but normal provider authentication and network access are required:

```sh
OMNI_TEST_LIVE_PROMPTS=1 OMNI_TEST_PI_BIN=/path/to/pi \
  cargo test -p omnisession-cli --test native_conformance \
  live_pi_consumes_imported_context -- --ignored --nocapture

OMNI_TEST_LIVE_PROMPTS=1 OMNI_TEST_OPENCODE_BIN=/path/to/opencode \
  cargo test -p omnisession-cli --test native_conformance \
  live_opencode_consumes_imported_context -- --ignored --nocapture
```

Model-backed probes remain separate because they consume external service capacity and depend on authentication and model availability. Conversion conformance never uses personal target stores, credentials, MCP configuration, or real transcripts.
