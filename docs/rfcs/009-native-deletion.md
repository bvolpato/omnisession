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
- Guarded private-store deletion is enabled only where active-writer detection is verified: Linux reads `/proc`; macOS parses `/bin/ps -ww -x -o pid=,command=` for current-user processes. Windows keeps provider-owned exact-ID deletion only.
- Shared content-addressed records remain untouched.
- OmniSession cache entry is forgotten only after native absence is verified.

Codex and OpenCode use documented exact-ID delete commands. Grok uses documented delete, then documented exact-ID search to reconcile provider-owned catalog left stale by Grok 0.2.117.

Claude Code has no documented delete command. OmniSession requires Claude to be closed and holds canonical projects-root lock through staging, read-back, cleanup, and caller absence verification. Discovered transcript must sit directly at `<projects>/<project>/<uuid>.jsonl` with no symlink in its directory chain, and sampled head records carrying `sessionId` must all name selected UUID. Deletion removes only entries named by that UUID: transcript, sibling `<project>/<uuid>/` directory (subagent transcripts, tool results, workflow and session memory), `file-history/<uuid>/`, `session-env/<uuid>/`, and `debug/<uuid>.txt`. Entries move into owner-private staging beside `projects/`; independent Claude adapter must no longer list or read ID before staged data is removed. Failed staging or read-back moves entries back without replacing recreated paths; failed restore is tagged as rollback failure and keeps staged data. Shared `history.jsonl` prompt lines, `sessions/`, `shell-snapshots/`, `tasks/` (task lists can be shared through `CLAUDE_CODE_TASK_LIST_ID`), project memory and `sessions-index.json`, and other sessions stay untouched.

Pi mirrors native resume picker: remove exact v3 JSONL file after bounded header ID validation and session-root lock. On macOS, active-writer detection matches `pi` process title, `node <prefix>/bin/pi`, and `pi-coding-agent` package paths. Cursor Agent mirrors native picker: recursively remove exact selected UUID directory after metadata/path validation; native sidecars and WAL files belong to that directory. On macOS, detection matches invoked `cursor-agent` name or any script under `cursor-agent` install directory, which covers launchers named `agent`.

Antigravity CLI private-store deletion supports Linux and macOS. It serializes deletion by canonical data root and deletes exact `conversation_summaries` row inside immediate transaction. Matching conversation DB and brain directory are atomically staged before commit, restored on transaction failure, and removed after commit. Lock remains held through provider discovery and direct-read absence verification.

Cursor IDE serializes deletion by canonical Cursor User metadata root. It deletes exact composer header and ID-namespaced `cursorDiskKV` records, including explicit subcomposer descendants. Workspace selection arrays drop deleted IDs when valid JSON. Content-addressed `agentKv:blob:*` records remain because they may be shared. Lock remains held through provider discovery and direct-read absence verification. Cursor must be closed.
