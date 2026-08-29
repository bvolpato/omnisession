# RFC 009: Native session deletion

Status: accepted

Deletion is explicit source-store mutation. Session browser requires `Delete` plus `y`; `a` suppresses later prompts only for current browser process. OmniSession deletes exact selected native session and then verifies provider discovery and direct read no longer find ID.

Common requirements:

- Provider and session ID come from selected discovered row.
- Discovery and transfer paths remain read-only.
- Documented provider delete command is preferred.
- Missing workspace never redirects deletion to current directory.
- Private paths are canonicalized and checked for symlinks before removal.
- SQLite mutations require accepted schema, immediate transaction, zero busy wait, exact-key predicates, and read-back.
- Providers with active local writers require process to exit first.
- Multi-resource private deletion, rollback, and absence verification share an owner-private cross-process lock keyed by canonical provider root and stored outside provider data.
- Guarded private-store deletion is enabled only where active-writer detection is verified. Initial support is Linux-only.
- Shared content-addressed records remain untouched.
- OmniSession cache entry is forgotten only after native absence is verified.

Codex and OpenCode use documented exact-ID delete commands. Grok uses documented delete, then documented exact-ID search to reconcile provider-owned catalog left stale by Grok 0.2.117.

Pi mirrors native resume picker: remove exact v3 JSONL file after bounded header ID validation and session-root lock. Cursor Agent mirrors native picker: recursively remove exact selected UUID directory after metadata/path validation; native sidecars and WAL files belong to that directory.

Antigravity serializes deletion by canonical data root. It deletes exact `conversation_summaries` row inside immediate transaction. Matching conversation DB and brain directory are atomically staged before commit, restored on transaction failure, and removed after commit. Lock remains held through provider discovery and direct-read absence verification.

Cursor IDE serializes deletion by canonical Cursor User metadata root. It deletes exact composer header and ID-namespaced `cursorDiskKV` records, including explicit subcomposer descendants. Workspace selection arrays drop deleted IDs when valid JSON. Content-addressed `agentKv:blob:*` records remain because they may be shared. Lock remains held through provider discovery and direct-read absence verification. Cursor must be closed.
