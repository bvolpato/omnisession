# RFC 003: Adapter protocol

Status: accepted

Adapters expose provider identity, installation probe, capabilities, session listing, canonical read, transfer planning, verification, and launch plan.

Rules:

- Prefer documented APIs and import/export commands.
- Treat provider files as read-only unless a version-gated writer is enabled.
- Ignore credentials and authentication stores.
- Tolerate unknown record kinds and truncated tails.
- Match projects using normalized real paths.
- Return metadata from listing. Read transcript content only for explicit show, export, or transfer operations.

Long term, adapters run out of process over JSON-RPC/stdio. v0.1 keeps built-in adapters in process while preserving a narrow trait boundary.
