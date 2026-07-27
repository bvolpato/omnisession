# OmniSession

**Switch agents. Keep the thread.**

OmniSession is a local-first session fabric for moving coding work between Claude Code, Codex, OpenCode, Grok, and Cursor without losing task lineage or repository context.

> Alpha software. OmniSession reads provider stores but never rewrites them. Cross-provider moves use a new semantic handoff. Native session materialization stays disabled until each format has version-gated writers and read-back verification.

## Why `omnis`, not `omni`?

Project name is OmniSession. CLI is `omnis`. `omni` already belongs to several active developer CLIs, including [omnicli.dev](https://omnicli.dev/) and [Omni Analytics](https://omni.co/blog/introducing-the-omni-cli). Avoiding that collision makes installation and shell shims predictable.

## Install

```sh
cargo install --git https://github.com/bvolpato/omnisession omnisession-cli
```

Or build locally:

```sh
cargo build --release
./target/release/omnis --help
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

# Preview transfer plan and generated handoff
omnis resume "claude:$CLAUDE_SESSION_ID" --in codex --dry-run

# Create handoff and launch target agent
omnis resume "claude:$CLAUDE_SESSION_ID" --in codex

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

Native provider IDs remain provider bindings. OmniSession keeps task branches and explicit handoff lineage. v0.1 transfers bounded recent visible context and captures current repository fingerprints for comparison. Delta checkpoints are planned for v0.2.

## Support

| Provider | Discovery | Canonical read | Same-provider resume | Cross-provider |
| --- | --- | --- | --- | --- |
| Claude Code | JSONL store | Visible messages and historical tool events | Official `--resume`, forked by default | Semantic handoff |
| Codex | Rollout JSONL | Visible response items and historical tool events | Official `fork`/`resume` | Semantic handoff |
| OpenCode | Official CLI | Official JSON export | Official `--session --fork` | Semantic handoff; native import intentionally gated |
| Grok | Local session store | ACP update stream, best effort | Official `--resume --fork-session` | Semantic handoff |
| Cursor CLI | Local metadata | Metadata; opaque blobs reported as unsupported | Official in-place `--resume` with explicit `--no-fork` | Metadata-only handoff |
| Cursor IDE | SQLite metadata | Read-only metadata | Not supported | Not supported |

Provider versions and private formats change. `omnis doctor` reports installation, store access, and degraded reads. Verified versions live in compatibility docs.

## Safety guarantees

- OmniSession never edits provider stores directly. Explicit `--no-fork` lets provider CLI append to native session.
- Fidelity reports describe canonicalized categories. Unrecognized private records are omitted in v0.1.
- Tool calls, commands, and approvals remain historical. They are never replayed.
- Known authentication files are never read. Environment values are never collected directly.
- Target permissions remain target defaults.
- Cross-provider routing requires an explicit source or selected task binding. Recency never selects a task.
- Source and current workspace roots must match unless explicit `--allow-workspace-mismatch` is supplied.
- Provider-native writers remain disabled in v0.1.
- Portable bundles always omit explicitly secret events and redact common credential patterns and sensitive fields.
- Live handoffs use private temporary files, not transcript-sized command arguments.

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
