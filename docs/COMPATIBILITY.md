# Compatibility

<!-- provider-compatibility:start -->
Last verified: 2026-08-30

| Provider | Version signal | Session source | Resume interface | Read/index | Clean start | Same-provider resume | Cross-provider import | Notes | Validation evidence |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Codex | >= 0.146.0 | `~/.codex/sessions/**/*.jsonl` | `codex fork ID` | Linux, macOS | Linux, macOS | Linux, macOS | Linux, macOS | Minimum-version app-server session import | Linux: source-ci + installed-token-free<br>macOS: source-ci + synthetic-store<br>Windows: source-ci + synthetic-store |
| Claude Code | >= 2.1.220 | `~/.claude/projects/*/*.jsonl` | `claude --resume ID --fork-session` | Linux, macOS | Linux, macOS | Linux, macOS | Linux, macOS | Minimum-version transactional target writer | Linux: source-ci + installed-token-free<br>macOS: source-ci + synthetic-store<br>Windows: source-ci + synthetic-store |
| OpenCode | Official API (tested 1.18.18) | Official list/export CLI | `opencode --session ID --fork` | Linux, macOS | Linux, macOS | Linux, macOS | Linux, macOS | Public release import verified with read-back and rollback | Linux: source-ci + installed-token-free<br>macOS: source-ci + synthetic-store<br>Windows: source-ci + synthetic-store |
| Pi | >= 0.82.0 | `~/.pi/agent/sessions/*/*.jsonl` | `pi --session ID`, `pi --fork ID` | Linux, macOS | Linux, macOS | Linux, macOS | Linux, macOS | v3 JSONL target with read-back and rollback | Linux: source-ci + installed-token-free<br>macOS: source-ci + synthetic-store<br>Windows: source-ci + synthetic-store |
| Grok | >= 0.2.114 | `~/.grok/sessions/*/*/` | `grok --resume ID --fork-session` | Linux, macOS | Linux, macOS | Linux, macOS | Linux, macOS | Minimum-version ACP import and read-back | Linux: source-ci + installed-token-free<br>macOS: source-ci + synthetic-store<br>Windows: source-ci + synthetic-store |
| Cursor IDE | >= 3.12.17 | Cursor User `globalStorage/state.vscdb` | Restored composer selection | Linux, macOS | Not guaranteed | Linux, macOS | Linux, macOS | Linux and macOS native continuation; no guaranteed direct clean-session launcher | Linux: source-ci + synthetic-store<br>macOS: source-ci + synthetic-store<br>Windows: source-ci + synthetic-store |
| Cursor Agent | >= 2026.07.23-e383d2b | `~/.cursor/chats/*/*/store.db` | `cursor-agent --resume ID` | Linux, macOS | Linux, macOS | Linux, macOS | Linux, macOS | Minimum-version SQLite/protobuf target writer | Linux: source-ci + synthetic-store<br>macOS: source-ci + synthetic-store<br>Windows: source-ci + synthetic-store |
| Antigravity CLI | >= 1.1.8 | `~/.gemini/antigravity-cli/` summary SQLite plus local transcripts | `agy --conversation ID` | Linux, macOS | Linux, macOS | Linux, macOS | Linux, macOS | Minimum-version Linux and macOS SQLite/protobuf target writer | Linux: source-ci + synthetic-store<br>macOS: source-ci + synthetic-store<br>Windows: source-ci + synthetic-store |
| Hermes | >= 0.19.1 | `~/.hermes/state.db` | `hermes --resume ID` | Linux, macOS | Linux, macOS | Linux, macOS | Linux, macOS | Provider-owned session import with read-back and rollback | Linux: source-ci + installed-token-free<br>macOS: source-ci + synthetic-store<br>Windows: source-ci + synthetic-store |
<!-- provider-compatibility:end -->

`crates/omnis-cli/provider-compatibility.json` is source of truth for native version gates, release-tested provider versions, capability platforms, authenticated marker canary, website signals, and this table. `node scripts/provider-compatibility.mjs check` fails when generated Rust, website, or documentation output drifts from manifest.

Validation evidence is platform-specific. `source-ci` compiles and tests source, `synthetic-store` exercises isolated synthetic provider stores, `installed-token-free` invokes installed provider code without credentials, and `authenticated-canary` is an opt-in model-backed probe. Reports keep overall run, cross-provider matrix, adapter/store, and per-provider installed outcomes separate. Evidence does not expand declared platform capabilities. `not-run` means workflow produced no evidence for that provider in that run.

## Platforms

Prebuilt OmniSession releases support Linux and macOS on x86-64 and ARM64. Native Windows x86-64 release and checksum-verifying PowerShell installer are preview. Linux archives contain static musl binaries without a host glibc dependency. Canonical trajectories are OS-neutral: source path remains provenance while target materialization uses target machine's current workspace and native provider store.

Windows packaging, installer, CLI, shims, and synthetic stores are source-tested in Windows CI. Installed Codex, OpenCode, and Grok explicit native imports also run token-free Windows conformance, but remain unadvertised while Windows provider support is provisional. Explicit targets use provider-specific binary, version, schema, rollback, and read-back gates; manifest capability absence means not declared, not necessarily impossible. Source CI is not evidence of installed-provider compatibility. WSL is a separate Linux environment and uses the Linux installer; native Windows and WSL provider stores are not treated as interchangeable.

Cursor IDE discovery and native continuation support Linux and macOS. macOS app bundles, Linux AppImages, Linux deb/rpm installs, and PATH-installed launchers are detected. Claude Code, Cursor IDE, and Antigravity CLI native target writing remain unavailable on Windows; Antigravity CLI native target writing supports Linux and macOS. Undeclared targets stay hidden from automatic selection; explicit targets attempt only provider-specific runtime-validated paths.

Compatibility is capability-based. OmniSession canonicalizes recognized visible records and omits unrecognized private records. Malformed records are skipped. Discovery and transfers open source provider files read-only. Target imports always use new IDs.

Portable imports are durable local sources addressed as `imported:<bundle-uuid>`. Exact UUID lookup works after original native store is unavailable; canonical snapshot retains original provider/session provenance. Imported sources participate in picker metadata and redacted full-text search, and continuation defaults to original provider unless `--in` selects another target. Import does not mutate provider stores. Identical repeated imports repair local indexes safely; UUID collisions with different content fail closed. Existing valid bundles migrate to exact imported locators before discovery and search; malformed stored bundles remain unavailable. Task binding maps an unavailable exported workspace to the current canonical repository only when its stored remote fingerprint matches; an existing different workspace or fingerprint mismatch fails closed.

Claude Code, Antigravity CLI, and Cursor IDE hold canonical-provider-root writer lock through launch planning, lineage recording, and successful provider process creation, then release it before waiting for provider exit. Launch failure after lineage commit preserves verified target and binding.

Session browser deletion supports Codex, OpenCode, Grok, and Hermes on Linux and macOS through provider commands. On Linux, guarded private-store deletion also supports Antigravity CLI, Pi, Cursor Agent, and Cursor IDE. Grok runs provider-owned search reconciliation to clear stale catalog rows. Pi and Cursor Agent mirror their native picker deletion over exact validated paths. Antigravity CLI and Cursor IDE use verified SQLite schemas, immediate transactions, active-writer exclusion, exact-ID rows, and canonical-provider-root cross-process locks held through post-delete read-back. Locks live in owner-private system-temporary namespaces, not provider stores or `OMNISESSION_HOME`. Claude Code remains read-only.

OpenCode target imports synthesize required message metadata, preserve bounded tools as documentary assistant history, redact credential-like text, create a new session ID, and verify history through official export before launch.

Codex 0.146.0 and newer target imports use its provider-owned external-session importer against a private temporary, redacted transcript. OmniSession verifies completed native turns through `thread/read`, confirms persisted estimated token usage in installed conformance, then reads the new session through the independent Codex adapter. Older Codex versions fall back to semantic handoff. Failed imports delete only the exact newly generated target ID. Same-provider forks are linked into the OmniSession tree when exactly one new user session appears in the launch workspace and time window; ambiguous matches are never guessed.

Grok 0.2.114 and newer target imports use `_x.ai/session/import`, then verify state and full update history through ACP before adapter read-back. Failed imports call exact-ID session deletion.

Hermes 0.19.1 and newer target imports use Hermes's own `SessionDB.import_sessions` implementation through installed Python runtime. OmniSession supplies bounded visible messages and documentary tool history, then reads new session through independent SQLite adapter. Imported titles are redacted, terminal-safe, bounded, and allocated through Hermes's native title lineage (`title`, `title #2`, ...) to satisfy provider-wide uniqueness. Same-provider forks retain native parent ID and OmniSession lineage markers. Failure removes only generated ID through `hermes sessions delete ID --yes`. Non-Python launchers and older releases fall back before any target write.

Claude Code 2.1.220 and newer target imports write a new text-only JSONL chain with current native constructors on Linux and macOS. Claude must be closed; OmniSession serializes its own writers with an owner-scoped system-temporary lock keyed by canonical projects root and independent of `OMNISESSION_HOME`, without adding provider-store metadata. Writes use a private same-directory temporary file, `fsync`, no-clobber publication, independent adapter read-back, and exact-record rollback validation. Older versions use semantic handoff; native writes fail closed on unsupported platforms.

Antigravity CLI versions 1.1.8 and newer read conversation summaries from SQLite and visible history from transcript JSONL or conversation SQLite store. Linux and macOS writers create a new conversation database and summary row, then verify visible history through independent adapter before `agy --conversation ID`. OmniSession refuses writes while exact `agy` or Antigravity CLI language-server processes are active, excluding IDE and unrelated processes. Owner-private cross-process lock keyed by canonical data root covers publication, summary mutation, read-back, and exact rollback. Private-store deletion remains Linux-only. Automated compatibility evidence uses synthetic stores and version stubs; it does not establish authenticated or full-fidelity provider behavior.

Pi versions 0.82.0 and newer accept documented v3 JSONL targets. OmniSession serializes native imports while checking encoded workspace-directory identity, writes a new UUID chain through a private same-directory temporary file, syncs and publishes without replacement, then reads it through Pi adapter before `pi --session ID`. `pi --fork ID` handles same-provider forks. Older versions fall back to semantic handoff.

Cursor Agent 2026.07.23-e383d2b and newer target imports build a new content-addressed SQLite/protobuf conversation graph under the exact workspace key. Model-visible prompt history, turn structures, assistant steps, and rewind anchors are written into a new UUID session. OmniSession stages and syncs both files, publishes metadata last, reads complete trajectory through its independent adapter, and validates every generated blob before rollback. Automated conformance does not launch Cursor Agent against model backend. Older builds fall back to semantic handoff.

Cursor IDE versions 3.12.17 and newer read composer conversation, historical tool, checkpoint, and diff records from `state.vscdb` through bounded query-only access on Linux and macOS. OmniSession discovers AppImage, deb/rpm, PATH, and macOS app-bundle installations. Private target writer validates required SQLite schemas and refuses writes while Cursor is running. Owner-private cross-process lock keyed by canonical Cursor User metadata root covers global and workspace database mutation, read-back, exact rollback, and deletion. Linux derives Cursor's workspace key from canonical path and inode, including unopened workspaces. macOS matches an existing `workspaceStorage` record, so target workspace must have been opened in Cursor once. Writer creates model-visible prompt blobs, native user turns, assistant steps, and rewind anchors under a fresh composer ID, then reads visible history back through independent adapter. For an existing workspace, it updates only `composer.composerData` in workspace state so Cursor restores imported composer, preserving previous typed value for rollback. Older builds are excluded from target choices.

SQLite adapters use query-only reads during discovery and transfer. Snapshot-based adapters copy available WAL files into private temporary storage. Explicit confirmed deletion is sole source-store mutation and follows [RFC 009](rfcs/009-native-deletion.md).

## Conformance tests

Provider conformance emits machine-readable JSON and Markdown dashboards as workflow artifacts, including failed or skipped scheduled/manual runs when report setup is available. Dashboard separates expected pins from observed installed versions; missing observations say `not recorded`, and skipped conformance says `not_run`. It also separates read/index, clean-start, and continuation capability by platform. A missing capability is reported explicitly instead of inferred from another passing capability.

Hermes expected release tag and commit identify pinned source checkout. Dashboard observed version comes from installed `hermes-agent` package metadata read through isolated selected Python interpreter without launching provider; observed tag and checked-out commit are recorded separately, including latest-version scheduled runs.

Full workspace tests run nine provider-labelled canonical snapshots through all nine target builders. All 81 cells must match one visible-history oracle, including Unicode, multiline messages, documentary tools, redaction, repeated roles, secret omission, and unknown records. Claude, Hermes, Antigravity CLI, Cursor Agent, Cursor IDE, and Pi targets are materialized, read through independent adapters, verified, and rolled back. OpenCode is parsed back through its export adapter. Codex and Grok RPC writers are verified through installed-provider conformance.

Token-free provider conformance runs all 72 off-diagonal paths across Claude, Codex, OpenCode, Grok, Hermes, Antigravity CLI, Pi, Cursor Agent, and Cursor IDE in isolated homes. Claude, Codex, OpenCode, Grok, and latest stable Hermes run through installed provider code. Remaining private-format writers use isolated synthetic stores and version stubs without launching model backends. Non-Codex source rows come from first-generation native imports. Every cell must pass target read-back and match original canonical trajectory, preventing loss from accumulating across hops. Scheduled and release workflows run this matrix without credentials.

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

Fixed marker question and expected answer live in compatibility manifest. Opt-in model-backed probes continue imported synthetic sessions through Pi and OpenCode noninteractive modes and ask that question. Session storage stays temporary, but normal provider authentication and network access are required:

```sh
OMNI_TEST_LIVE_PROMPTS=1 OMNI_TEST_PI_BIN=/path/to/pi \
  cargo test -p omnisession-cli --test native_conformance \
  live_pi_consumes_imported_context -- --ignored --nocapture

OMNI_TEST_LIVE_PROMPTS=1 OMNI_TEST_OPENCODE_BIN=/path/to/opencode \
  cargo test -p omnisession-cli --test native_conformance \
  live_opencode_consumes_imported_context -- --ignored --nocapture
```

Model-backed probes remain separate because they consume external service capacity and depend on authentication and model availability. Conversion conformance never uses personal target stores, credentials, MCP configuration, or real transcripts.
