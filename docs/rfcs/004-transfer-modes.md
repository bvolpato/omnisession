# RFC 004: Transfer modes and fidelity

Status: accepted and partially implemented

Planner chooses safest available mode:

1. Native resume for same provider.
2. Official documented import.
3. Verified native materialization for exact tested versions.
4. Semantic handoff into a fresh target session.
5. Portable export into a redacted canonical bundle.

OmniSession implements native resume, OpenCode CLI import, version-gated Codex app-server injection, Grok ACP import, exact-version Claude JSONL materialization, exact-build Cursor SQLite/protobuf materialization, semantic handoff, and portable export. Native target imports preserve visible user and assistant history plus bounded tool records as documentary messages. Later conformance reports expand to files, subagents, permissions, checkpoints, plans, and attachments. Status is `preserved`, `summarized`, `historical_only`, `redacted`, `omitted`, or `unsupported`.

No transfer silently upgrades to a riskier mode. Unsupported native imports use semantic handoff before writing. Failed writes stop after an exact rollback attempt, instead of launching a second transfer with uncertain state. Private-format writing remains disabled except accepted, exact-version implementations in RFC 007.
