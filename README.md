# OmniSession

Move coding sessions between Claude Code, Codex, OpenCode, Grok, and Cursor.

[Website](https://bvolpato.github.io/omnisession/) · [Design notes](docs/rfcs/README.md) · [Compatibility](docs/COMPATIBILITY.md)

> OmniSession is alpha software. It reads source sessions without changing them. Cross-provider transfers use a documented import command when one exists. Other transfers start a new session with a concise handoff. OmniSession does not write private provider formats.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/bvolpato/omnisession/main/install.sh | sh
```

The installer verifies the release checksum, installs `omnis`, and adds shims for supported agent commands. Open a new shell when it finishes.

Use `OMNI_BYPASS=1 claude --continue` to skip OmniSession for one command. Remove the shims with `omnis shim uninstall --bin-dir "$HOME/.local/bin"`.

To build from source:

```sh
cargo install --git https://github.com/bvolpato/omnisession omnisession-cli
```

## Use it

Check which agents and session stores are available:

```sh
omnis doctor
omnis list --project .
```

Resume a session by ID. You only need the provider prefix when the same ID appears in more than one store.

```sh
SESSION_ID="<id-from-omnis-list>"
omnis resume "$SESSION_ID"
omnis resume "claude:$SESSION_ID"
```

Preview a transfer before starting another agent:

```sh
omnis resume "$SESSION_ID" --in codex --dry-run
```

OpenCode can import visible conversation history into a new native session. Use `--materialize-only` to create and verify that session without opening its TUI.

```sh
omnis resume "$SESSION_ID" --in opencode
omnis resume "$SESSION_ID" --in opencode --materialize-only
```

Track work that moves through several agents:

```sh
omnis task start auth-refactor --from "claude:$SESSION_ID"
omnis checkout auth-refactor
omnis switch codex
omnis task bind codex:<new-session-id>
```

`omnis task bind` is explicit because OmniSession never picks a new session by modification time.

Export or import a portable local bundle:

```sh
omnis export "claude:$SESSION_ID" -o auth-refactor.omnisession
omnis import auth-refactor.omnisession
```

## How transfers work

A task can have branches, and each branch points to an exact provider session. OmniSession also keeps normalized events and repository fingerprints so it can explain what will survive a transfer.

```text
task
  -> branch
    -> provider session
      -> normalized events
```

Transfer order:

1. Resume the original provider session when source and target match.
2. Use the target's documented import command when supported.
3. Otherwise, start a new target session with a redacted handoff.

OpenCode currently provides the cross-provider import used by OmniSession. The import includes bounded visible user and assistant history, creates a new session ID, and verifies the result by exporting it again. Tool calls and approvals are recorded as history but never replayed.

## Provider support

| Provider | Session discovery | Read support | Native resume | Cross-provider transfer |
| --- | --- | --- | --- | --- |
| Claude Code | JSONL store | Messages and historical tool events | `--resume` | Visible history into OpenCode; handoff elsewhere |
| Codex | Rollout JSONL | Response items and historical tool events | `fork` and `resume` | Visible history into OpenCode; handoff elsewhere |
| OpenCode | Official CLI | Official JSON export | `--session --fork` | Official import into a new verified session |
| Grok | Local session store | ACP updates when available | `--resume --fork-session` | Visible history into OpenCode; handoff elsewhere |
| Cursor CLI | Local metadata | Metadata only for opaque records | `--resume` | Metadata handoff |
| Cursor IDE | SQLite metadata | Read-only metadata | Not supported | Not supported |

Provider versions and private formats change. Run `omnis doctor` to see installation and read errors. [`docs/COMPATIBILITY.md`](docs/COMPATIBILITY.md) lists verified versions.

## Safety

- Source provider stores are read-only. Provider commands can create a new target session, but OmniSession does not edit those stores itself.
- Tool calls, shell commands, and approvals remain history. A transfer never executes them.
- Authentication files and environment values are not collected.
- Target sessions use target permission defaults.
- Cross-provider routing needs an explicit source or selected task. Modification time is never enough.
- Workspace roots must match unless you pass `--allow-workspace-mismatch`.
- OpenCode imports use a private temporary file, read-back verification, and rollback of the exact new ID after failure.
- Portable bundles omit secret events and redact common credential patterns.

OmniSession stores its own data in `~/.omnisession/`. Set `OMNISESSION_HOME` to use another location.

## Development

Architecture decisions are in [`docs/rfcs`](docs/rfcs/README.md). Planned work is in [`docs/ROADMAP.md`](docs/ROADMAP.md).

```sh
cargo fmt --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

MIT licensed.
