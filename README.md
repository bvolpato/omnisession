<p align="center">
  <img src="website/public/logo.svg" width="112" alt="OmniSession logo">
</p>

<h1 align="center">OmniSession</h1>

<p align="center">Search local coding-agent sessions and continue them in another harness.</p>

<p align="center">
  <a href="https://bvolpato.github.io/omnisession/">Website</a> ·
  <a href="docs/COMPATIBILITY.md">Compatibility</a> ·
  <a href="docs/rfcs/README.md">Design</a>
</p>

OmniSession is alpha. Transfers leave source sessions unchanged, create a separate target session, and verify imported history before launch. Unsupported versions use a short handoff when available or stay out of target picker. Deletion is separate, explicit, and limited to selected native session.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/bvolpato/omnisession/main/install.sh | sh
```

Installer verifies release checksum, installs `omni`, and adds shims for supported agent commands. Restart shell after install.
Prebuilt binaries support Linux and macOS on x86-64 and ARM64.

## Pick a session

```sh
omni
```

`NEW SESSION` starts a clean session in any installed agent. Type to filter by title, message, ID, directory, branch, or provider. Current workspace appears first; `Tab` includes every workspace. Select a session, then choose where it should open.

`Delete` removes supported sessions from native source store. Confirm with `y`, cancel with `n`, or press `a` to skip later confirmations during current browser run.

Related sessions stay grouped across agents. Selection panel shows workspace, branch, trajectory size, model, reasoning mode, token usage, and conversation edges when recorded. Full-text results show matching context and highlight search terms.

Picker checks for releases in background. Footer shows installed version and offers `Ctrl+U` when an update is available. Confirmation shows executable path. Package-manager installs still update through their manager. Set `OMNI_NO_UPDATE_CHECK=1` to turn check off.

<p align="center">
  <img src="website/public/session-browser.png" width="1200" alt="OmniSession session browser showing related sessions across Codex, Grok, and Claude">
</p>

## Resume directly

```sh
omni resume <session> --in codex
```

Bare session IDs work when unique. Add provider when needed:

```sh
omni resume claude:<session-id> --in codex
```

Fork without changing source session. Omit `--in` to choose target interactively:

```sh
omni fork <session>
omni fork <session> --in codex
```

Export visible history for manual use:

```sh
omni markdown <session> -o session.md
```

Run `omni --help` for diagnostics, shims, bundles, and advanced commands.

## Supported agents

- Claude Code
- Codex
- OpenCode
- Grok
- Hermes
- Antigravity
- Pi
- Cursor Agent
- Cursor IDE

Picker shows runnable targets found on current machine. [`docs/COMPATIBILITY.md`](docs/COMPATIBILITY.md) lists minimum supported versions and transfer paths. Newer versions remain enabled unless structural validation or read-back fails.
`claude` and `claude-code` are interchangeable in session references and provider flags.

## What moves

OmniSession preserves ordered user and assistant messages plus bounded tool activity. Same-provider sessions use native resume and fork paths when available. Cross-provider transfers prefer documented imports, then verified native writers for supported private formats.

Tool calls and shell commands remain historical text. They are never replayed. Approvals, credentials, hidden reasoning, and provider permission state stay out.

## Safety

- Transfers do not write source provider stores. Deletion requires `Delete` plus confirmation and removes only selected native ID.
- Cross-agent transfers create a new target session ID.
- OmniSession reads target back before launch.
- Failed target writes roll back only records OmniSession created.
- Workspace selection or exact session ID decides routing. Recency does not.
- Local index stores bounded, redacted content from sessions OmniSession already read.

Set `OMNI_BYPASS=1` to bypass installed shims for one provider command. OmniSession data lives in `~/.omnisession/`; set `OMNISESSION_HOME` to move it.

## Development

```sh
cargo fmt --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

MIT licensed.
