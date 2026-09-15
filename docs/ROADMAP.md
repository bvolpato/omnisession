# Roadmap

What OmniSession ships today and what comes next. Ordering is not a commitment. [CHANGELOG.md](../CHANGELOG.md) records released changes, and [COMPATIBILITY.md](COMPATIBILITY.md) is the source of truth for provider capabilities.

## Next

### Antigravity IDE target

[RFC 010](rfcs/010-antigravity-ide-target.md) drafts a native writer for the Antigravity desktop app, which is a read-only source today.

- Run the RFC's one-time manual validation and resolve its open questions.
- Land a macOS-first writer behind version, schema, active-writer, rollback, and read-back gates.
- Survey the Linux layout and process names before enabling Linux.

### Delta continuity

- Synchronization checkpoints for incremental transfers.
- Content-addressed storage for large artifacts.
- Smaller follow-up imports after a session crosses providers more than once.

### Fidelity evaluation

- Deterministic continuity questions over synthetic fixture corpus.
- Retain compatibility dashboard history for trend analysis.
- Add authenticated semantic canaries as provider automation becomes stable.

### Adapter boundaries

- Keep provider codecs and native writers isolated by provider.
- Move adapters out of process over JSON-RPC or stdio.
- Publish conformance runner and adapter SDK for third-party integrations.
- Add generic ACP adapter where protocol exposes required session lifecycle.

### Platform hardening

- Add installed Windows provider canaries before promoting read/index, clean-start, or continuation fidelity.
- Add guarded Windows private writers and private-store deletion only after native schema and active-writer validation.
- Share validated npm launcher routing with adapters, so npm-only OpenCode installs are discoverable on Windows.
- Broader active-writer detection and serialization for remaining private writers.
- Encrypted machine-to-machine bundles.
- Signed third-party adapter manifests.

## Shipped

### v0.8.52

- `omni search` with delta indexing, plus background full-text indexing of every discovered session while the picker runs.
- Exact quoted-phrase search, plus faster multi-word search and Codex session listing.
- Large-store hardening: provider SQLite stores and streamed transcripts up to 4 GiB, and failed sessions skipped until they change.
- Fuzzy picker search, help overlay, mouse support, and an adaptive color palette.
- Complete tool call and result pairs written as native historical tool records for Claude Code, Pi, Hermes, OpenCode, and Grok.
- Broader secret redaction: env-style credential names, URL passwords, Basic auth, quoted keys, flag and cookie credentials, and picker titles.
- Guarded Claude Code deletion, and guarded private-store deletion on macOS.
- Antigravity desktop app conversations as a read-only `antigravity-ide` source on Linux and macOS.
- `Ctrl+C` rollback for native imports on Linux, macOS, and Windows, including shim-routed imports.
- Cursor IDE imports into never-opened macOS folders.
- Windows preview hardening: `Ctrl+C`-safe shims, alias relinking on upgrade, and Codex and Grok read/index, clean start, same-provider resume, and cross-provider import.

### Through v0.8.51

- Match nested workspaces only within the same Git repository.
- Delete exact selected Cursor IDE records with active-writer exclusion and read-back verification.
- Run reproducible release validation against pinned provider versions.
- Enforce the declared Rust 1.85 minimum in CI.
- Retain searchable first and last context for oversized redacted trajectories.
- Rank bounded full-text results after workspace and provider eligibility filtering.
- Preserve imported bundle trajectories during successful native-index pruning.
- Pin GitHub Actions to reviewed commit SHAs across release and validation workflows.
- Serialize Claude native writes with active-writer exclusion and owner-private cross-process locking.
- Serialize Antigravity CLI and Cursor IDE multi-resource writes, verification, rollback, and deletion by canonical provider root.
- Probe Hermes versions from isolated package metadata without provider network checks.
- Audit dependencies continuously and benchmark large-index search and refresh paths.
- Searchable session picker from `omni`, with workspace and provider filters.
- Full-text trajectory search over visible messages, tool activity, commands, plans, and file events.
- Native resume and fork paths across Claude Code, Codex, OpenCode, Grok, Hermes, Antigravity CLI, Pi, Cursor Agent, and Cursor IDE.
- New target IDs, fidelity reports, independent read-back, exact rollback, and recorded lineage.
- Guarded native deletion for supported providers.
- Linux and macOS release binaries, checksum-verified installer, shims, and background self-update checks.
- Windows x86-64 preview archive, checksum-verified PowerShell installer, source-tested compiled shims, provider evidence, and hardened lock/path substrate.
- Credential-free 72-path provider conformance, daily compatibility runs, coverage thresholds, installer smoke, and website browser smoke.
- Portable bundles as exact resumable sources with redacted legacy-index migration and repository-safe task binding.
- Evidence-labeled provider capabilities and runtime readiness, ordered by supported-provider priority.
- Pinned Pi conformance, guarded Antigravity macOS imports, and static Cursor launcher discovery without desktop execution.
- Preserve harness and task continuity across providers and relocated workspaces.
- Disambiguate CLI vs IDE continuation targets with fail-closed native import fallback.
- Surface provider discovery failures and Codex scan limits in doctor and the session picker.
