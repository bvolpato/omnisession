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

Long term, adapters run out of process over JSON-RPC/stdio. Current implementation keeps built-in adapters in process while preserving a narrow trait boundary.

## Discovered paths for search indexing

The in-process `read_session_at` and `preview_session_at` methods accept an optional path from session discovery. Their default implementations delegate to the existing read and preview methods, so adapters can adopt path hints independently.

Adapters that use a hint must validate it against their canonical provider root and the exact requested session identity. Claude validates the UUID filename; Pi validates the session header ID. Invalid or stale hints fall back to the existing session lookup. Hints do not authorize provider-store writes or change full-read and preview limits.

The indexer uses these methods to avoid repeated directory walks for discovered sessions. It writes redacted search documents to OmniSession's SQLite store in atomic batches of at most 16 documents or 8 MiB of indexed text. A failed batch rolls back before individual retries, preserving successful neighboring documents. Existing single-document writes remain available for larger documents. Pending documents are flushed at progress updates, completion, and graceful cancellation.
