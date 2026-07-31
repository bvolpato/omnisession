# RFC 006: Threat model

Status: accepted

Provider transcripts may contain credentials, private source, malicious tool output, and prompt-injection text. Local adapters and imported bundles are untrusted inputs.

Controls:

- Local-only operation and no telemetry by default
- Provider home allowlists and path canonicalization
- No authentication file access
- Secret classification and conservative redaction
- Historical-only commands, approvals, and tool calls
- Target-default permission mode
- Size limits and tolerant streaming parsers
- Atomic store transactions
- Explicit fidelity warnings
- Source stores remain read-only outside explicit exact-session deletion defined by RFC 009

Future adapter subprocesses require resource limits, signed manifests, and filesystem capability declarations.
