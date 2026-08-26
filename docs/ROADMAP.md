# Roadmap

## Shipped through v0.8.45

- Match nested workspaces only within the same Git repository.
- Delete exact selected Cursor IDE records with active-writer exclusion and read-back verification.
- Run reproducible release validation against pinned provider versions.
- Enforce the declared Rust 1.85 minimum in CI.
- Retain searchable first and last context for oversized redacted trajectories.
- Rank bounded full-text results after workspace and provider eligibility filtering.
- Preserve imported bundle trajectories during successful native-index pruning.
- Pin GitHub Actions to reviewed commit SHAs across release and validation workflows.
- Serialize Claude native writes with active-writer exclusion and owner-private cross-process locking.
- Probe Hermes versions from isolated package metadata without provider network checks.
- Audit dependencies continuously and benchmark large-index search and refresh paths.
- Searchable session picker from `omni`, with workspace and provider filters.
- Full-text trajectory search over visible messages, tool activity, commands, plans, and file events.
- Native resume and fork paths across Claude Code, Codex, OpenCode, Grok, Hermes, Antigravity, Pi, Cursor Agent, and Cursor IDE.
- New target IDs, fidelity reports, independent read-back, exact rollback, and recorded lineage.
- Guarded native deletion for supported providers.
- Linux and macOS release binaries, checksum-verified installer, shims, and background self-update checks.
- Credential-free 72-path provider conformance, daily compatibility runs, coverage thresholds, installer smoke, and website browser smoke.

## Next

### Delta continuity

- Synchronization checkpoints for incremental transfers.
- Content-addressed storage for large artifacts.
- Smaller follow-up imports after a session crosses providers more than once.

### Fidelity evaluation

- Deterministic continuity questions over synthetic fixture corpus.
- Per-provider compatibility dashboard generated from conformance results.
- Optional authenticated semantic canaries outside normal CI.

### Adapter boundaries

- Keep provider codecs and native writers isolated by provider.
- Move adapters out of process over JSON-RPC or stdio.
- Publish conformance runner and adapter SDK for third-party integrations.
- Add generic ACP adapter where protocol exposes required session lifecycle.

### Platform hardening

- Windows installer, shims, and guarded private writers.
- Broader active-writer detection and advisory locking.
- Encrypted machine-to-machine bundles.
- Signed third-party adapter manifests.
