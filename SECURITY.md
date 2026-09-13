# Security policy

OmniSession reads local coding-agent transcripts. These files may contain source code, command output, tokens, credentials, or personal data, so security reports get priority.

## Reporting a vulnerability

Report vulnerabilities privately through [GitHub private vulnerability reporting](https://github.com/bvolpato/omnisession/security/advisories/new). Do not open a public issue or pull request containing secrets, transcript data, or exploit details.

Include:

- OmniSession version (`omni --version`) and operating system.
- Agents and versions involved.
- Impact, and a minimal reproduction using synthetic data.

Never attach real transcripts, credentials, or tokens. Describe structure instead, and redact personal paths. Fixes ship in new releases, so reproduce on the [latest release](https://github.com/bvolpato/omnisession/releases/latest) when you can.

## Scope

In scope:

- Writes to provider session stores outside confirmed deletion of the exact selected session, or deletion that removes unrelated records.
- Reading provider credential or authentication files.
- Redaction bypasses for supported credential patterns in the search index, handoffs, imports, exports, bundles, or CLI output.
- Transcript text in CLI output the user did not request.
- Replay of historical tool calls, shell commands, approvals, or imported transcript instructions.
- Native imports that corrupt target stores, skip read-back verification, or fail to roll back exactly.
- Untrusted input handling: malformed provider records or portable bundles that escape their allowed roots or trigger unintended execution.
- Shim routing that runs the wrong binary, recurses through `PATH`, or runs `.cmd` or `.bat` providers through `cmd.exe`.
- Installer or self-update integrity, such as checksum verification bypasses.

Out of scope:

- Vulnerabilities in the coding agents themselves. Report those to their owners.
- Arbitrary secrets that match no credential pattern. Redaction is conservative and cannot prove every secret is absent, though reports of common credential formats it misses are welcome.

## Security model

### Local by default

- OmniSession runs locally without daemon, telemetry, or hosted session service. Background update checks contact GitHub; launched provider commands retain their own network behavior.
- Known provider authentication files are excluded. Environment values are never collected directly.
- Imported tool calls are historical context, never replay instructions.
- CLI output stays free of transcript content unless the user requests show, export, search text, or transfer.

### Source stores

- Transfers never edit source provider stores. Cross-provider imports create new target IDs. Source deletion is separate, explicit, exact-ID scoped, and verified.
- Snapshot-based adapters copy provider SQLite databases and available WAL files into private temporary directories before SQLite opens them. Snapshot queries run with `query_only` enabled, so SQLite sidecar activity stays in the private copy.
- Cursor IDE metadata opens read-only in place, with `query_only` enabled and no busy wait.

### Redaction and handoffs

- Export applies conservative pattern and structured-field redaction. Redaction reduces exposure but cannot prove that every arbitrary secret is absent.
- Handoff documents are private temporary files, secret events are excluded, and untrusted transcript content is conservatively redacted and quoted.
- Official import documents are private temporary files deleted after target read-back.
- Cross-workspace transfer fails closed unless caller supplies `--allow-workspace-mismatch`.
- Target permissions always use target defaults.

### Native imports

Native target-store writes remain disabled below minimum versions accepted in [RFC 007](docs/rfcs/007-native-materialization.md). Newer versions stay enabled only while schema validation and independent read-back pass.

Provider-owned imports handle Codex, OpenCode, Grok, and Hermes. Codex, Grok, and Hermes imports also require minimum versions, while OpenCode's official import has no version gate and relies on read-back and exact rollback. Minimum-version private writers handle Claude Code, Antigravity CLI, Pi, Cursor Agent, and Cursor IDE. Every native target import creates a new ID, validates generated records, reads visible history back, and removes only exact generated records after failure before lineage commits. Launch failure after lineage commit preserves verified target and binding. Unsupported or malformed formats fail before private-store mutation and may use semantic handoff.

On Linux and macOS, `Ctrl+C` during a native import started from the picker, `omni resume`, or `omni fork` lets in-flight writes and read-back finish, then rolls back the generated target and exits without launching. Shim-routed imports are not covered yet. Windows keeps the default `Ctrl+C` action, and cross-provider import stays undeclared there until it can roll back.

### Locks and deletion

Claude Code, Antigravity CLI, and Cursor IDE private mutations use owner-private system-temporary locks keyed by canonical provider root. Locks stay outside provider storage and configured OmniSession state. Locks cover mutation, verification, exact rollback, launch planning, lineage recording, and provider process creation. Lock releases before waiting for provider exit.

Guarded private-store deletion runs on Linux and macOS, refuses while that agent runs, and verifies that provider discovery and direct reads no longer find the session. Claude Code, Antigravity CLI, and Cursor IDE deletion hold their provider-root locks through confirmed absence. Windows keeps provider-owned exact-ID deletion only. See [RFC 009](docs/rfcs/009-native-deletion.md).
