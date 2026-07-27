# Security

OmniSession reads local coding-agent transcripts. These files may contain source code, command output, tokens, credentials, or personal data.

## Reporting

Report vulnerabilities privately through GitHub Security Advisories. Do not open a public issue containing secrets or transcript data.

## Security model

- OmniSession runs locally and has no telemetry or network service.
- OmniSession never edits source provider stores. Cross-provider imports create new target IDs.
- Imported tool calls are historical context, never replay instructions.
- Known provider authentication files are excluded. Environment values are never collected directly.
- Export applies conservative pattern and structured-field redaction. Redaction reduces exposure but cannot prove that every arbitrary secret is absent.
- Handoff documents are private temporary files and untrusted transcript content is quoted.
- Official import documents are private temporary files deleted after target read-back.
- Cross-workspace transfer fails closed unless caller supplies `--allow-workspace-mismatch`.
- Target permissions always use target defaults.

Private native target-store writes remain disabled except exact-version implementations accepted in RFC 007. Claude Code 2.1.220 has a text-only JSONL writer. Cursor Agent 2026.07.23-e383d2b has a bundle-fingerprinted SQLite/protobuf writer. Both publish new IDs without replacement, read results back independently, and remove only exact generated record sets on failure. Codex, OpenCode, and Grok use provider interfaces. Unknown versions use semantic handoff before any write. Failed writes stop after an exact rollback attempt.

Provider SQLite databases and available WAL files are copied to private temporary directories before SQLite opens them. Queries run with `query_only` enabled, so SQLite sidecar activity remains outside provider stores.
