# Compatibility

Last verified: 2026-07-27

| Provider | Verified version | Session source | Resume interface | Notes |
| --- | --- | --- | --- | --- |
| Claude Code | 2.1.220 | `~/.claude/projects/*/*.jsonl` | `claude --resume ID --fork-session` | Internal JSONL read-only |
| Codex | 0.145.0 | `~/.codex/sessions/**/*.jsonl` | `codex fork ID` | Rollout JSONL read-only |
| OpenCode | local `0.0.0-bv/opencode-queue-202607230403` | Official list/export CLI | `opencode --session ID --fork` | Official import verified with read-back and rollback |
| Grok | 0.2.112 | `~/.grok/sessions/*/*/` | `grok --resume ID --fork-session` | ACP update stream read-only |
| Cursor Agent | 2026.07.09 | `~/.cursor/chats/*/*/` | `cursor-agent --resume ID` | Transcript blobs may be opaque |
| Cursor IDE | current local install | `state.vscdb` metadata | none | Separate provider, read-only metadata |

Compatibility is capability-based. OmniSession canonicalizes recognized visible records and omits unrecognized private records. Malformed records are skipped. Native provider files are opened read-only where possible and never rewritten directly by OmniSession.

OpenCode target imports synthesize required message metadata, omit native tool calls, redact credential-like text, create a new session ID, and verify visible history through official export before launch. Failed verification triggers deletion of exact newly generated ID.

SQLite adapters copy databases and available WAL files to private temporary snapshots, then issue query-only reads. Provider SQLite directories remain untouched.
