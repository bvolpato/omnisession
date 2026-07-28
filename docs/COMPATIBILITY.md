# Compatibility

Last verified: 2026-07-28

| Provider | Verified version | Session source | Resume interface | Notes |
| --- | --- | --- | --- | --- |
| Claude Code | 2.1.220 | `~/.claude/projects/*/*.jsonl` | `claude --resume ID --fork-session` | Exact-version transactional target writer |
| Codex | 0.145.0 | `~/.codex/sessions/**/*.jsonl` | `codex fork ID` | Version-gated app-server trajectory injection |
| OpenCode | local `0.0.0-bv/opencode-queue-202607280328` | Official list/export CLI | `opencode --session ID --fork` | Official import verified with read-back and rollback |
| Grok | 0.2.112 | `~/.grok/sessions/*/*/` | `grok --resume ID --fork-session` | Version-gated ACP import and read-back |
| Cursor Agent | 2026.07.23-e383d2b | `~/.cursor/chats/*/*/store.db` | `cursor-agent --resume ID` | Exact-build SQLite/protobuf target writer |
| Cursor IDE | current local install | `state.vscdb` metadata | none | Separate provider, read-only metadata |

Compatibility is capability-based. OmniSession canonicalizes recognized visible records and omits unrecognized private records. Malformed records are skipped. Source provider files are always opened read-only. Target imports always use new IDs.

OpenCode target imports synthesize required message metadata, preserve bounded tools as documentary assistant history, redact credential-like text, create a new session ID, and verify history through official export before launch.

Codex 0.145.0 target imports create a thread through app-server, inject model-visible Responses API history, close the server to flush its rollout, then read the new session through the Codex adapter. Other Codex versions fail closed to semantic handoff until verified. Failed imports delete only the exact newly generated target ID.

Grok 0.2.112 target imports use `_x.ai/session/import`, then verify state and full update history through ACP before adapter read-back. Failed imports call exact-ID session deletion.

Claude Code 2.1.220 target imports write a new text-only JSONL chain with current native constructors. Writes use a private same-directory temporary file, `fsync`, no-clobber publication, independent adapter read-back, and exact-record rollback validation. Other versions fail closed.

Cursor Agent 2026.07.23-e383d2b target imports build a new content-addressed SQLite/protobuf conversation graph under the exact workspace key. Model-visible prompt history, turn structures, assistant steps, and rewind anchors are written into a new UUID session. OmniSession fingerprints the installed Cursor bundle, stages and syncs both files, publishes metadata last, reads the complete trajectory through its independent adapter, and validates every generated blob before rollback. The verified Cursor TUI renders imported messages and documentary tool records on native `--resume`. Other builds fail closed.

SQLite adapters copy databases and available WAL files to private temporary snapshots, then issue query-only reads. Provider SQLite directories remain untouched.

## Conformance tests

Full workspace tests run a synthetic five-source by five-target builder matrix. All 25 cells must match one independent visible-history oracle, including Unicode, multiline messages, documentary tools, redaction, repeated roles, secret omission, and unknown records. Every Claude and Cursor cell is materialized into a native store, read through its independent adapter, verified, and rolled back. Every OpenCode cell is parsed back through its export adapter. Codex and Grok RPC writers are verified through installed-provider conformance.

Installed conformance runs all 20 cross-provider paths against real local binaries inside isolated homes. It first creates synthetic native sources, then requires every target import to pass independent read-back verification. Same-provider Claude, Codex, OpenCode, and Grok cells use native fork commands instead of conversion; their exact launch arguments are covered separately. Cursor same-provider forks use its verified native materializer.

```sh
OMNI_TEST_CLAUDE_BIN=/path/to/claude \
OMNI_TEST_CODEX_BIN=/path/to/codex \
OMNI_TEST_OPENCODE_BIN=/path/to/opencode \
OMNI_TEST_GROK_BIN=/path/to/grok \
OMNI_TEST_CURSOR_BIN=/path/to/cursor-agent \
  cargo test -p omnisession-cli --test native_conformance \
  installed_five_by_five_cross_provider_matrix -- --ignored --nocapture
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

Conformance tests never use personal target stores, credentials, MCP configuration, or real transcripts.
