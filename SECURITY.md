# Security

OmniSession reads local coding-agent transcripts. These files may contain source code, command output, tokens, credentials, or personal data.

## Reporting

Report vulnerabilities privately through GitHub Security Advisories. Do not open a public issue containing secrets or transcript data.

## Security model

- Local-only by default. No telemetry or network service.
- OmniSession never edits provider stores directly. Official provider commands may create target sessions; explicit in-place resume may append.
- Imported tool calls are historical context, never replay instructions.
- Known provider authentication files are excluded. Environment values are never collected directly.
- Export always performs conservative pattern and structured-field redaction. It is defense in depth, not proof arbitrary secrets are absent.
- Handoff documents are private temporary files and untrusted transcript content is quoted.
- Official import documents are private temporary files deleted after target read-back.
- Cross-workspace transfer fails closed unless caller supplies `--allow-workspace-mismatch`.
- Target permissions always use target defaults.

Private native target-store writes remain disabled. OmniSession uses provider-supported resume/import paths or a new semantic handoff. Official imports receive new IDs, read-back verification, and exact rollback on failure.

Provider SQLite databases and available WAL files are copied to private temporary directories before SQLite opens them. Queries run with `query_only` enabled, so SQLite sidecar activity remains outside provider stores.
