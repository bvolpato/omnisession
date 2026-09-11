# RFC 004: Transfer modes and fidelity

Status: accepted and implemented for visible-history continuity; richer event fidelity remains partial

Planner chooses safest available mode:

1. Native resume or fork for same provider.
2. Official documented import.
3. Verified native materialization for supported provider versions.
4. Semantic handoff into a fresh target session.
5. Portable export/import through a redacted canonical bundle.

OmniSession implements native resume and fork, provider-owned imports for Codex, OpenCode, Grok, and Hermes, version-gated private writers for Claude Code, Pi, Antigravity CLI, Cursor Agent, and Cursor IDE, semantic handoff, and portable export/import. Native target imports preserve visible user and assistant history. Complete tool call/result pairs become historical native tool records in writers that declare them (Claude Code, Pi, Hermes, OpenCode, and Grok), named `hist_<provider>_<tool>` and never re-run; other bounded tool records stay documentary messages. OpenCode receives each pair as an assistant message with a `tool` part in `completed` or `error` state. That v2 part schema predates `opencode import`, which stores parts as exported, so no separate version gate applies. Later conformance reports expand to files, subagents, permissions, checkpoints, plans, and attachments. Status is `preserved`, `summarized`, `historical_only`, `redacted`, `omitted`, or `unsupported`.

No transfer silently upgrades to a riskier mode. Unsupported native imports use semantic handoff before writing. Failed writes stop after an exact rollback attempt, instead of launching a second transfer with uncertain state. Private-format writing remains disabled below accepted minimum versions in RFC 007. Structural validation and read-back catch incompatible newer formats.
