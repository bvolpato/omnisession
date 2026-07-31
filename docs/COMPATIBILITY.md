# Compatibility

Last verified: 2026-07-29

| Provider | Verified version | Session source | Resume interface | Notes |
| --- | --- | --- | --- | --- |
| Claude Code | 2.1.220 | `~/.claude/projects/*/*.jsonl` | `claude --resume ID --fork-session` | Exact-version transactional target writer |
| Codex | 0.146.0 | `~/.codex/sessions/**/*.jsonl` | `codex fork ID` | Version-gated app-server trajectory injection |
| OpenCode | local `0.0.0-bv/opencode-queue-202607280328` | Official list/export CLI | `opencode --session ID --fork` | Official import verified with read-back and rollback |
| Grok | >= 0.2.114 | `~/.grok/sessions/*/*/` | `grok --resume ID --fork-session` | Minimum-version ACP import and read-back |
| Antigravity | 1.1.8 | `~/.gemini/antigravity-cli/` summary SQLite plus local transcripts | `agy --conversation ID` | Exact-version Linux SQLite/protobuf target writer |
| Pi | 0.82.x | `~/.pi/agent/sessions/*.jsonl` | `pi --session ID`, `pi --fork ID` | Exact v3 JSONL target with read-back and rollback |
| Cursor Agent | 2026.07.23-e383d2b | `~/.cursor/chats/*/*/store.db` | `cursor-agent --resume ID` | Exact-build SQLite/protobuf target writer |
| Cursor IDE | AppImage 3.12.17 (`0fb7620`) | Cursor User `globalStorage/state.vscdb` | Restored composer selection | Exact-AppImage native trajectory, exact workspace selection, or materialize-only |

Compatibility is capability-based. OmniSession canonicalizes recognized visible records and omits unrecognized private records. Malformed records are skipped. Source provider files are always opened read-only. Target imports always use new IDs.

Session browser deletion uses documented native commands only. Codex, OpenCode, and Grok support confirmed deletion. Claude Code, Antigravity, Pi, Cursor Agent, and Cursor IDE remain read-only because they do not expose a supported non-interactive delete command.

OpenCode target imports synthesize required message metadata, preserve bounded tools as documentary assistant history, redact credential-like text, create a new session ID, and verify history through official export before launch.

Codex 0.146.0 target imports create a thread through app-server, inject model-visible Responses API history, close the server to flush its rollout, then read the new session through the Codex adapter. Other Codex versions fail closed to semantic handoff until verified. Failed imports delete only the exact newly generated target ID. Same-provider forks are linked into the OmniSession tree when exactly one new user session appears in the launch workspace and time window; ambiguous matches are never guessed.

Grok 0.2.114 and newer target imports use `_x.ai/session/import`, then verify state and full update history through ACP before adapter read-back. Failed imports call exact-ID session deletion.

Claude Code 2.1.220 target imports write a new text-only JSONL chain with current native constructors. Writes use a private same-directory temporary file, `fsync`, no-clobber publication, independent adapter read-back, and exact-record rollback validation. Other versions fail closed.

Antigravity 1.1.8 reads conversation summaries from SQLite and visible history from its transcript JSONL or conversation SQLite store. Its Linux exact-binary writer creates a new conversation database and summary row, then verifies visible history through the independent adapter before `agy --conversation ID`. It refuses writes while Antigravity is active and rolls back only exact generated records.

Pi 0.82.x accepts documented v3 JSONL targets. OmniSession serializes native imports while checking encoded workspace-directory identity, writes a new UUID chain through a private same-directory temporary file, syncs and publishes without replacement, then reads it through the Pi adapter before `pi --session ID`. `pi --fork ID` handles same-provider forks. Other release lines fail closed to semantic handoff.

Cursor Agent 2026.07.23-e383d2b target imports build a new content-addressed SQLite/protobuf conversation graph under the exact workspace key. Model-visible prompt history, turn structures, assistant steps, and rewind anchors are written into a new UUID session. OmniSession fingerprints the installed Cursor bundle, stages and syncs both files, publishes metadata last, reads complete trajectory through its independent adapter, and validates every generated blob before rollback. Automated conformance does not launch Cursor Agent against model backend. Other builds fail closed.

Cursor IDE reads composer conversation, historical tool, checkpoint, and diff records from `state.vscdb` through bounded query-only access. Its private target writer accepts only the exact AppImage 3.12.17 fingerprint, workbench bundle, and SQLite schemas. On Linux it derives Cursor's workspace key from canonical path and inode. It writes model-visible prompt blobs, native user turns, assistant steps, and rewind anchors under a fresh composer ID, then reads visible history back through the independent adapter. For an existing workspace, it updates only `composer.composerData` in that workspace's state database so Cursor restores the imported composer, preserving the previous typed value for rollback. New workspaces open normally with the imported chat available in History. Other builds are excluded from target choices.

SQLite adapters use query-only reads. Snapshot-based adapters copy available WAL files into private temporary storage. Provider SQLite directories remain untouched.

## Conformance tests

Full workspace tests run eight provider-labelled canonical snapshots through all eight target builders. All 64 cells must match one visible-history oracle, including Unicode, multiline messages, documentary tools, redaction, repeated roles, secret omission, and unknown records. Claude, Antigravity, Cursor Agent, Cursor IDE, and Pi targets are materialized, read through independent adapters, verified, and rolled back. OpenCode is parsed back through its export adapter. Codex and Grok RPC writers are verified through installed-provider conformance.

Installed conformance runs all 56 off-diagonal paths across Claude, Codex, OpenCode, Grok, Antigravity, Pi, Cursor Agent, and Cursor IDE in isolated homes. Non-Codex source rows come from first-generation native imports. Every cell must pass target read-back and match original canonical trajectory, preventing loss from accumulating across hops. This opt-in matrix is not part of normal CI because it requires exact provider binaries.

Same-provider fork commands have focused launch-plan and lineage tests. They are not part of 56-cell cross-provider matrix.

```sh
OMNI_TEST_CLAUDE_BIN=/path/to/claude \
OMNI_TEST_CODEX_BIN=/path/to/codex \
OMNI_TEST_OPENCODE_BIN=/path/to/opencode \
OMNI_TEST_GROK_BIN=/path/to/grok \
OMNI_TEST_ANTIGRAVITY_BIN=/path/to/agy \
OMNI_TEST_CURSOR_BIN=/path/to/cursor-agent \
OMNI_TEST_CURSOR_IDE_BIN=/path/to/Cursor.AppImage \
OMNI_TEST_PI_BIN=/path/to/pi \
  cargo test -p omnisession-cli --test native_conformance \
  installed_eight_by_eight_cross_provider_matrix -- --ignored --nocapture
```

Codex app-server persists one provider-owned `<environment_context>` message before injected history. Verification accepts exactly one such prefix, then still requires every imported message and role to match exactly. Missing, reordered, duplicated, or additional trajectory messages fail closed and trigger exact target rollback.

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
