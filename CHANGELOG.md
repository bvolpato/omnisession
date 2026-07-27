# Changelog

## 0.3.1 - 2026-07-27

- Report progress while preparing, importing, and verifying native OpenCode sessions.
- Replace generated README and website copy with direct usage documentation.
- Redesign the website around session routing and fidelity reports.

## 0.3.0 - 2026-07-27

- Resolve bare session IDs by exact match across provider stores.
- Default `omnis resume ID` to an in-place native-provider resume.
- Materialize visible cross-provider history through OpenCode's official import command.
- Verify imported sessions by reading them back and roll back exact new IDs on failure.
- Add `--materialize-only` for conversion without launching target TUI.

## 0.2.1 - 2026-07-27

- Preserve common Claude and Codex alias flags when transparent shims route a continuation.
- Treat an empty OpenCode session listing as an empty store instead of malformed JSON.

## 0.2.0 - 2026-07-27

- Add transparent provider shims with fail-closed task routing and `OMNI_BYPASS` support.
- Add checksum-verified one-command installer and native release artifacts.
- Add OmniSession product site and GitHub Pages deployment.

## 0.1.0 - 2026-07-27

- Add canonical append-only event model and portable bundle schema.
- Add read-only discovery for Claude Code, Codex, OpenCode, Grok, Cursor CLI, and Cursor IDE.
- Add repository fingerprints, secret redaction, semantic handoffs, and fidelity reports.
- Add SQLite task selection, branch heads, provider bindings, and handoff lineage.
- Add explicit `omnis` CLI for discovery, inspection, transfer, export, import, and diagnostics.
