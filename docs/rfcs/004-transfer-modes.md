# RFC 004: Transfer modes and fidelity

Status: accepted and partially implemented

Planner chooses safest available mode:

1. Native resume for same provider.
2. Official documented import.
3. Verified native materialization for exact tested versions.
4. Semantic handoff into a fresh target session.
5. Portable export into a redacted canonical bundle.

v0.3 implements native resume, OpenCode official import, semantic handoff, and portable export. OpenCode imports visible user and assistant history with synthesized target metadata. Tool calls remain omitted instead of becoming replayable target tool calls. Later conformance reports expand to files, subagents, reasoning, permissions, checkpoints, plans, and attachments. Status is `preserved`, `summarized`, `historical_only`, `redacted`, `omitted`, or `unsupported`.

No transfer silently upgrades to a riskier mode. Failed official imports roll back exact generated IDs and fall back to semantic handoff. Private-format native materialization remains disabled.
