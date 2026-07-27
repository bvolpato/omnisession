# RFC 004: Transfer modes and fidelity

Status: accepted design; v0.1 implements native resume, semantic handoff, and portable export

Planner chooses safest available mode:

1. Native resume for same provider.
2. Official documented import.
3. Verified native materialization for exact tested versions.
4. Semantic handoff into a fresh target session.
5. Portable export into a redacted canonical bundle.

v0.1 reports implemented categories. Later conformance reports expand to files, subagents, reasoning, permissions, checkpoints, plans, and attachments. Status is `preserved`, `summarized`, `historical_only`, `redacted`, `omitted`, or `unsupported`.

No transfer silently upgrades to a riskier mode. Native materialization remains disabled for all versions in v0.1.
