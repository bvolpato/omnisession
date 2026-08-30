# RFC 005: Routing and shims

Status: accepted

Transparent routing is opt-in. `omni shim install --bin-dir DIR` installs provider aliases in `${OMNISESSION_HOME:-$HOME/.omnisession}/shims`. Unix aliases are `claude`, `codex`, `opencode`, `grok`, `hermes`, `agy`, `pi`, and `cursor-agent` symlinks. Windows aliases use the same command names with `.exe` and are hard links to compiled `omni.exe`; `.cmd` wrappers are never created. `agent` is Cursor's primary launcher, but OmniSession never installs that collision-prone alias. Cursor Agent discovery prefers an `agent` path whose canonical target has Cursor Agent identity and falls back to `cursor-agent`. Cursor IDE discovery and diagnostics use executable names and static product metadata only. They never execute desktop binary; only explicit resume or import run path launches it. Verified older desktop builds support only explicit same-provider workspace open; cross-provider import readiness and target pickers require minimum supported version. `DIR/omni` or `DIR/omni.exe` must already exist and be executable. Install never overwrites an unowned path. It prints platform-specific PATH guidance; shim directory must precede real provider directories. `omni shim uninstall --bin-dir DIR` removes only symlinks or hard links owned by that exact OmniSession binary. Windows binary upgrades must uninstall aliases before replacing `omni.exe`, then reinstall them; hard links cannot cross volumes.

Ordinary commands pass unchanged to real provider binaries. Interception grammar is intentionally narrow:

| Provider | Routed forms |
| --- | --- |
| Claude Code | `claude --continue`, `claude -c` |
| Codex | `codex resume`, `codex resume --last` |
| OpenCode | `opencode --continue`, `opencode -c` |
| Grok | `grok --continue`, `grok -c`, `grok --resume`, `grok -r` |
| Antigravity CLI | `agy --continue`, `agy -c` |
| Pi | `pi --continue`, `pi -c`, `pi --resume`, `pi -r` |
| Cursor Agent | `cursor-agent --continue`, `cursor-agent --resume`, `cursor-agent resume` |

Extra arguments, explicit native session IDs, and every other command pass through. This avoids rewriting provider-specific commands or overriding explicit user intent.

Two common shell-alias prefixes are also recognized and preserved: Claude's `--dangerously-skip-permissions` and Codex's `--yolo`. No other extra option is inferred or copied into a routed launch.

Resolution order:

1. No selected OmniSession task for exact workspace: pass through.
2. Selected task has no exact `main` binding: stop and require `omni task bind PROVIDER:ID`.
3. Binding workspace differs from canonical current workspace: stop.
4. Current branch head already targets invoked provider: resume exact bound native session in place.
5. Current branch head targets another provider: use documented target import when supported; otherwise create semantic handoff and start invoked provider. Verified imports bind exact generated target ID. Semantic fallbacks never guess resulting IDs and require explicit bind afterward.

`OMNI_BYPASS=1` always bypasses routing. Real binary resolution excludes shim directory and current `omni` executable, preventing PATH recursion. Windows resolution follows `PATHEXT` and falls back to `.COM`, `.EXE`, `.BAT`, and `.CMD` when it is unset. Native executables receive provider arguments as separate OS strings. OmniSession never executes `.cmd` or `.bat` providers through `cmd.exe`: recognized npm command shims are read with a fixed size limit, must match known npm scaffolding without extra commands or environment assignments, and must target a contained script with an exact no-argument Node shebang. OmniSession launches that script through an adjacent or PATH-resolved `node.exe`. Other batch files fail closed. Absolute override variables are `OMNI_CLAUDE_BIN`, `OMNI_CODEX_BIN`, `OMNI_OPENCODE_BIN`, `OMNI_GROK_BIN`, `OMNI_HERMES_BIN`, `OMNI_ANTIGRAVITY_BIN`, `OMNI_PI_BIN`, and `OMNI_CURSOR_AGENT_BIN`. Final launch uses process replacement on Unix, preserving stdin, stdout, stderr, signals, and TTY behavior. Windows inherits standard streams, waits for provider process, and returns its exit code.

`omni shim exec PROVIDER -- ARGS...` exposes wrapper behavior for diagnostics and non-symlink launchers. Provider session stores remain read-only. Routing reads exact selected task and binding only; timestamps and provider recency never select OmniSession state.
