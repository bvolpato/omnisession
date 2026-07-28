# OmniSession

Move coding sessions between Claude Code, Codex, OpenCode, Grok, and Cursor.

[Website](https://bvolpato.github.io/omnisession/) · [Design notes](docs/rfcs/README.md) · [Compatibility](docs/COMPATIBILITY.md)

> OmniSession is alpha software. It never changes source sessions. Cross-provider transfers create new target sessions through provider APIs or exact-version native writers. Unsupported versions use a concise handoff. Failed writes stop after an exact rollback attempt.

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

Open the session picker, choose the source, then choose where to continue:

```sh
omnis resume
```

The picker opens immediately from its local index while provider scans refresh in background. It starts with sessions from current workspace. Search matches titles, full session IDs, directory paths, branches, providers, and trajectory text already read by OmniSession. Use arrow keys to choose a session, press `Tab` to include every workspace, and use left or right arrow to change source provider. Selected session shows full metadata, first and latest meaningful messages, and recorded source-to-target lineage. Visible-row previews build redacted trajectory index in background. OmniSession never bulk-reads every provider session to populate it.

After choosing a session, OmniSession lists runnable agents found on `PATH`. The original source is selected by default. Pass `--in` to skip that step, or start with a source filter:

```sh
omnis resume --in codex
omnis resume --in codex --from claude
omnis resume --all
```

If a saved workspace moved or was deleted, the picker asks where to open it. Current directory is prefilled; edit it or press `Ctrl-U`, type another path, and use `Tab` for directory completion.

Scripts can still resume by ID. Provider prefix is needed only when the same ID appears in more than one store.

```sh
SESSION_ID="<id-from-omnis-list>"
omnis resume "$SESSION_ID"
omnis resume "claude:$SESSION_ID"
```

Preview a transfer before starting another agent:

```sh
omnis resume "$SESSION_ID" --in codex --dry-run
```

Claude Code, Codex, OpenCode, Grok, and Cursor Agent can receive a converted trajectory in a new native session. Use `--materialize-only` to create and verify that session without opening its TUI.

```sh
omnis resume "$SESSION_ID" --in codex --materialize-only
omnis resume "$SESSION_ID" --in claude --materialize-only
omnis resume "$SESSION_ID" --in opencode
omnis resume "$SESSION_ID" --in opencode --materialize-only
omnis resume "$SESSION_ID" --in grok --materialize-only
omnis resume "$SESSION_ID" --in cursor --materialize-only
```

Native imports report each stage on stderr: preparation, provider import, read-back verification, and completion.

Export full visible conversation history to Markdown for manual handoffs:

```sh
omnis markdown "$SESSION_ID" > session.md
omnis markdown "$SESSION_ID" -o session.md
```

Markdown exports include redacted user and assistant messages, recorded workspace state, and bounded historical tool activity. Individual tool records and total tool history have size limits. Approvals, secret events, and hidden reasoning stay out.

Track work that moves through several agents:

```sh
omnis task start auth-refactor --from "claude:$SESSION_ID"
omnis checkout auth-refactor
omnis switch codex
omnis task bind codex:<new-session-id>
```

`omnis task bind` is explicit because OmniSession never picks a new session by modification time.

Export or import a machine-readable portable bundle:

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
2. Use target import or history-injection APIs when supported.
3. Read converted history back and verify it.
4. Otherwise, start a new target session with a redacted handoff.

OpenCode imports through its JSON CLI. Codex 0.145.0 uses app-server history injection. Grok 0.2.112 uses its ACP session import extension. Claude Code 2.1.220 receives a transactional JSONL transcript. Cursor Agent 2026.07.23-e383d2b receives a new SQLite and protobuf trajectory. Each path creates a new ID and reads history back before launch.

Converted sessions preserve ordered user and assistant messages plus bounded tool activity as documentary history. Tool records are never executed. Approvals, hidden reasoning, secrets, and provider permission state stay out.

## Provider support

| Provider | Session discovery | Read support | Native resume | Cross-provider transfer |
| --- | --- | --- | --- | --- |
| Claude Code | JSONL store | Messages and historical tool events | `--resume` | Accepts exact-version native imports |
| Codex | Rollout JSONL | Response items and historical tool events | `fork` and `resume` | Accepts version-gated app-server imports |
| OpenCode | Official CLI | Official JSON export | `--session --fork` | Official import into a new verified session |
| Grok | Local session store | ACP update stream | `--resume --fork-session` | Accepts version-gated ACP imports |
| Cursor CLI | SQLite and protobuf graph | Messages and historical tool events | `--resume` | Accepts exact-version native imports |
| Cursor IDE | SQLite metadata | Read-only metadata | Not supported | Not supported |

Provider versions and private formats change. Run `omnis doctor` to see installation and read errors. [`docs/COMPATIBILITY.md`](docs/COMPATIBILITY.md) lists verified versions.

Cursor Agent support is exact-build gated. OmniSession verifies Cursor's JavaScript bundle fingerprints before writing its private store. Updated or repackaged builds fall back to a concise semantic handoff until their format is verified.

## Safety

- Source provider stores are read-only. Target import always creates a new ID. Claude and Cursor target materializers are exact-version direct native-store writers.
- Tool calls and shell commands remain bounded documentary history. Approvals remain excluded. A transfer never executes historical tools.
- Authentication files and environment values are not collected.
- Local search index stores metadata plus up to 512 KiB of redacted trajectory text per session already read by OmniSession. Messages, historical tools, commands, plans, and file activity are searchable. Secret events, hidden reasoning, approvals, and recognized credentials stay out.
- Target sessions use target permission defaults.
- Cross-provider routing needs an explicit source or selected task. Modification time is never enough.
- Interactive resume uses an explicit picker selection. It never auto-selects the newest session.
- Workspace roots must match unless you pass `--allow-workspace-mismatch`.
- OpenCode uses its import CLI, Codex uses app-server RPC, Grok uses ACP, and Claude and Cursor use exact-version transactional writers. Every path reads results back and rolls back only its generated ID after failure.
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
