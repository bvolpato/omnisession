# Roadmap

## v0.1: safe explicit CLI

- Canonical event model and portable bundle
- Repository-aware discovery for Claude, Codex, OpenCode, Grok, and Cursor
- Official same-provider resume/fork
- Cross-provider semantic handoff with fidelity report
- Internal session lineage and exact routing bindings
- Local SQLite lineage store
- Doctor, list, show, inspect, resume, switch, export, and import commands

## v0.2: transparent routing and distribution

- Opt-in PATH shims
- Exact bound-session routing for recognized continue/resume flags
- Wrapper bypass and deterministic real-binary resolution
- Checksum-verified installer and native release artifacts
- GitHub Pages product site

## v0.3: native continuity

- Exact bare-ID resolution across provider stores
- Official OpenCode import of bounded, redacted visible history
- New target IDs with read-back verification and exact rollback
- Conversion-only mode without target launch

## v0.4: manual portability

- Redacted Markdown export with bounded tool history
- Streamed native-import progress

## v0.5: native trajectory imports

- Version-gated Codex native trajectory injection through app-server
- Bounded documentary tool history in Codex and OpenCode imports

## v0.6: broader native continuity

- Exact-version Claude Code JSONL materialization
- Grok ACP session import with state and update read-back
- Native Claude and Grok routing through transparent shims
- Bare-ID resolution despite unrelated provider discovery failures

## v0.8: interactive resume

- Searchable terminal session picker when source ID is omitted
- Current-workspace default with explicit all-workspaces toggle
- Source-provider filters and same-provider in-place resume
- Explicit cross-workspace selection using recorded session workspace

## v0.8.1: responsive discovery

- Target-agent chooser when `--in` is omitted
- Runnable-target detection through real binaries on `PATH`
- Immediate cached results with provider-by-provider refresh
- Trigram inverted search across session metadata
- Interactive workspace recovery with directory completion
- Clear directory context in all-workspace results

## v0.8.20: session-first interface

- Open searchable session picker from bare `omnis`
- Lead help and product documentation with direct session portability
- Keep diagnostics, bundles, and persistent routing controls as advanced commands

## v0.8.22: cross-agent session trees

- Render full recorded ancestry, sibling branches, and descendants for selected sessions
- Keep missing or filtered ancestors visible without changing list selection
- Resolve known tree-node titles from cached metadata and background previews

## v0.8.23: grouped session lineage

- Group visible lineage components in root-to-leaf order
- Show recursive tree connectors directly in agent column
- Preserve selected session during asynchronous lineage refresh

## v0.8.24: continuation-aware titles

- Resolve first user message recorded after each handoff boundary
- Replace imported placeholders in list, details, and session trees
- Read complete visible lineage trajectories off UI thread

## v0.8.25: source fork action

- Offer both continue and fork when selected source agent is runnable
- Keep original-session continuation as default target action

## Next: Hermes adapter

- High-priority adapter using documented SQLite reads
- Native resume and export
- ACP fork with independent read-back, never a direct SQLite writer

## Later: delta continuity

- Synchronization checkpoints and incremental handoffs
- Content-addressed blobs for large artifacts
- Continuity evaluation fixture corpus
- Generic ACP adapter
- Adapter subprocess protocol and conformance runner

- More provider import APIs as they become available
- Additional exact-version private writers after independent read-back support
- Active-writer detection and advisory locks
- Signed third-party adapter manifests
- Encrypted machine-to-machine bundles
- IDE integrations and generated compatibility dashboard
