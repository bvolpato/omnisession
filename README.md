# OmniSession

**Switch agents. Keep the thread.**

OmniSession is a local-first session fabric for moving coding work between Claude Code, Codex, OpenCode, Grok, and Cursor without losing task lineage or repository context.

[Website](https://bvolpato.github.io/omnisession/) · [Design](docs/rfcs/README.md) · [Compatibility](docs/COMPATIBILITY.md)

> Alpha software. OmniSession never modifies source provider stores. Cross-provider moves use documented native import where available, then fall back to a semantic handoff. Private-format target writers stay disabled.

## Install

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://raw.githubusercontent.com/bvolpato/omnisession/main/install.sh | sh
```

Installer verifies release checksum, installs `omnis`, and configures transparent shims for supported agent commands. Open a new shell after installation.

Bypass routing for one command with `OMNI_BYPASS=1 claude --continue`. Remove shims with `omnis shim uninstall --bin-dir "$HOME/.local/bin"`.

Build from source instead:

```sh
cargo install --git https://github.com/bvolpato/omnisession omnisession-cli
```

## Start

```sh
# Check local provider installations and stores
omnis doctor

# Discover sessions for current repository
omnis list --project .
omnis list --provider cursor-ide --all-projects

# Copy one ID from `omnis list`
CLAUDE_SESSION_ID="<uuid-from-list>"
omnis show "claude:$CLAUDE_SESSION_ID"

# Resume a unique ID in its native provider
omnis resume "$CLAUDE_SESSION_ID"

# Preview safest available transfer
omnis resume "$CLAUDE_SESSION_ID" --in codex --dry-run

# Create handoff and launch target agent
omnis resume "$CLAUDE_SESSION_ID" --in codex

# Create verified, natively resumable OpenCode history
omnis resume "$CLAUDE_SESSION_ID" --in opencode

# Convert without launching target TUI
omnis resume "$CLAUDE_SESSION_ID" --in opencode --materialize-only

# Track one logical task across providers
omnis task start auth-refactor --from "claude:$CLAUDE_SESSION_ID"
omnis checkout auth-refactor
omnis switch codex
# After target exits, bind exact ID. OmniSession never guesses by recency.
CODEX_SESSION_ID="<uuid-from-list>"
omnis task bind "codex:$CODEX_SESSION_ID"

# Portable local bundle
omnis export "claude:$CLAUDE_SESSION_ID" -o auth-refactor.omnisession
omnis import auth-refactor.omnisession
```

## Model

```text
logical task
  -> branch
    -> provider binding
      -> native session
        -> canonical append-only events
```

Native provider IDs remain provider bindings. OmniSession keeps task branches and explicit handoff lineage. v0.3 resolves exact bare IDs, imports full bounded visible history into OpenCode, captures repository fingerprints, and routes supported continue commands through exact task bindings. Delta checkpoints remain planned work.

## Support

| Provider | Discovery | Canonical read | Same-provider resume | Cross-provider |
| --- | --- | --- | --- | --- |
| Claude Code | JSONL store | Visible messages and historical tool events | Official `--resume`; explicit same-provider target forks by default | Native visible-history import into OpenCode; semantic handoff elsewhere |
| Codex | Rollout JSONL | Visible response items and historical tool events | Official `fork`/`resume` | Native visible-history import into OpenCode; semantic handoff elsewhere |
| OpenCode | Official CLI | Official JSON export | Official `--session --fork` | Official import creates new verified native session |
| Grok | Local session store | ACP update stream, best effort | Official `--resume --fork-session` | Native visible-history import into OpenCode; semantic handoff elsewhere |
| Cursor CLI | Local metadata | Metadata; opaque blobs reported as unsupported | Official in-place `--resume` | Metadata-only handoff |
| Cursor IDE | SQLite metadata | Read-only metadata | Not supported | Not supported |

Provider versions and private formats change. `omnis doctor` reports installation, store access, and degraded reads. Verified versions live in compatibility docs.

## Safety guarantees

- OmniSession never edits provider stores directly. Official provider commands may create new target sessions; explicit `--no-fork` lets provider CLI append to native session.
- Fidelity reports describe canonicalized categories. Unrecognized private records are omitted.
- Tool calls, commands, and approvals remain historical. They are never replayed.
- Known authentication files are never read. Environment values are never collected directly.
- Target permissions remain target defaults.
- Cross-provider routing requires an explicit source or selected task binding. Recency never selects a task.
- Source and current workspace roots must match unless explicit `--allow-workspace-mismatch` is supplied.
- Private-format provider writers remain disabled. Official imports use new IDs, read-back verification, and exact rollback.
- Portable bundles always omit explicitly secret events and redact common credential patterns and sensitive fields.
- Native imports use private temporary JSON deleted after verification. Explicit semantic handoffs use private temporary files; transparent semantic shims pass bounded redacted context at launch.

Data lives under `~/.omnisession/` by default. Set `OMNISESSION_HOME` for an isolated store.

## Design

Architecture decisions live in [`docs/rfcs`](docs/rfcs/README.md). Current roadmap lives in [`docs/ROADMAP.md`](docs/ROADMAP.md).

## Development

```sh
cargo fmt --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

MIT licensed.
