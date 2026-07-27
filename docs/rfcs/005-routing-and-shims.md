# RFC 005: Routing and shims

Status: accepted

Transparent routing is opt-in. `omnis shim install --bin-dir DIR` installs `claude`, `codex`, `opencode`, `grok`, and `cursor-agent` symlinks in `${OMNISESSION_HOME:-$HOME/.omnisession}/shims`. `DIR/omnis` must already exist and be executable. Install never overwrites an unowned path. It prints shell-specific PATH guidance; shim directory must precede real provider directories. `omnis shim uninstall --bin-dir DIR` removes only symlinks targeting that exact OmniSession binary.

Ordinary commands pass unchanged to real provider binaries. Interception grammar is intentionally narrow:

| Provider | Routed forms |
| --- | --- |
| Claude Code | `claude --continue`, `claude -c` |
| Codex | `codex resume`, `codex resume --last` |
| OpenCode | `opencode --continue`, `opencode -c` |
| Grok | `grok --continue`, `grok -c`, `grok --resume`, `grok -r` |
| Cursor Agent | `cursor-agent --continue`, `cursor-agent --resume`, `cursor-agent resume` |

Extra arguments, explicit native session IDs, and every other command pass through. This avoids rewriting provider-specific commands or overriding explicit user intent.

Resolution order:

1. No selected OmniSession task for exact workspace: pass through.
2. Selected task has no exact `main` binding: stop and require `omnis task bind PROVIDER:ID`.
3. Binding workspace differs from canonical current workspace: stop.
4. Current branch head already targets invoked provider: resume exact bound native session in place.
5. Current branch head targets another provider: create semantic handoff and start invoked provider. Never guess resulting native session ID; require explicit bind afterward.

`OMNI_BYPASS=1` always bypasses routing. Real binary resolution excludes shim directory and current `omnis` executable, preventing PATH recursion. Absolute override variables are `OMNI_CLAUDE_BIN`, `OMNI_CODEX_BIN`, `OMNI_OPENCODE_BIN`, `OMNI_GROK_BIN`, and `OMNI_CURSOR_AGENT_BIN`. Final launch uses process replacement on Unix, preserving stdin, stdout, stderr, signals, and TTY behavior.

`omnis shim exec PROVIDER -- ARGS...` exposes wrapper behavior for diagnostics and non-symlink launchers. Provider session stores remain read-only. Routing reads exact selected task and binding only; timestamps and provider recency never select OmniSession state.
