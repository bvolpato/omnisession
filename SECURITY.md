# Security

OmniSession reads local coding-agent transcripts. These files may contain source code, command output, tokens, credentials, or personal data.

## Reporting

Report vulnerabilities privately through GitHub Security Advisories. Do not open a public issue containing secrets or transcript data.

## Security model

- OmniSession runs locally without daemon, telemetry, or hosted session service. Background update checks contact GitHub; launched provider commands retain their own network behavior.
- Transfers never edit source provider stores. Cross-provider imports create new target IDs. Source deletion is separate, explicit, exact-ID scoped, and verified.
- Imported tool calls are historical context, never replay instructions.
- Known provider authentication files are excluded. Environment values are never collected directly.
- Export applies conservative pattern and structured-field redaction. Redaction reduces exposure but cannot prove that every arbitrary secret is absent.
- Handoff documents are private temporary files and untrusted transcript content is quoted.
- Official import documents are private temporary files deleted after target read-back.
- Cross-workspace transfer fails closed unless caller supplies `--allow-workspace-mismatch`.
- Target permissions always use target defaults.

Native target-store writes remain disabled below minimum versions accepted in RFC 007. Newer versions stay enabled only while schema validation and independent read-back pass.

Provider-owned imports handle Codex, OpenCode, Grok, and Hermes. Minimum-version private writers handle Claude Code, Antigravity CLI, Pi, Cursor Agent, and Cursor IDE. Every native target import creates a new ID, validates generated records, reads visible history back, and removes only exact generated records after failure before lineage commits. Launch failure after lineage commit preserves verified target and binding. Unsupported or malformed formats fail before private-store mutation and may use semantic handoff.

Claude Code, Antigravity CLI, and Cursor IDE private mutations use owner-private system-temporary locks keyed by canonical provider root. Locks stay outside provider storage and configured OmniSession state. Locks cover mutation, verification, exact rollback, launch planning, lineage recording, and provider process creation. Antigravity CLI and Cursor IDE deletion also retain locks through confirmed absence on Linux. Lock releases before waiting for provider exit.

Provider SQLite databases and available WAL files are copied to private temporary directories before SQLite opens them. Queries run with `query_only` enabled, so SQLite sidecar activity remains outside provider stores.
