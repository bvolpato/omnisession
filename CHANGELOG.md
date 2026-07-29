# Changelog

## 0.8.22 - 2026-07-28

- Show complete cross-agent session trees in picker detail pane.
- Keep filtered or unavailable ancestors visible as lineage context.
- Load titles for known tree nodes in background without moving selection.

## 0.8.21 - 2026-07-28

- Use `opencode` as canonical provider name in JSON and portable bundles.
- Continue accepting legacy `open-code` input.

## 0.8.20 - 2026-07-28

- Open session picker when `omnis` runs without a subcommand.
- Center README, website, and help on direct session portability.
- Keep diagnostics, bundles, and routing bindings as advanced commands.

## 0.8.19 - 2026-07-28

- Ignore OmniSession shims while detecting installed provider binaries.

## 0.8.18 - 2026-07-28

- Add read, resume, fork-clone, and exact-version native imports for Antigravity.
- Add read, resume, fork, and documented v3 JSONL imports for Pi.
- Read complete Cursor IDE conversations and materialize exact-build native targets.
- Cover all eight sources and targets with a 64-cell synthetic conversion matrix.

## 0.8.17 - 2026-07-28

- Accept Codex's single provider-owned environment context before injected history.
- Run all 20 cross-provider paths against installed binaries in isolated homes.
- Materialize and read back every synthetic Claude and Cursor matrix cell.

## 0.8.16 - 2026-07-28

- Add `omnis resume SESSION --fork` for explicit copy-on-resume.
- Use native forks for Claude Code, Codex, OpenCode, and Grok.
- Clone Cursor trajectories into verified new sessions without changing source.

## 0.8.15 - 2026-07-28

- Accept provider-managed Grok summary fields while verifying every imported field exactly.
- Add isolated installed-Grok conformance for synthetic 101-item Codex trajectories.

## 0.8.14 - 2026-07-28

- Keep secret redaction stable across import and read-back verification.
- Verify OpenCode imports from exact target workspace and report safe mismatch counts.
- Import assistant-first trajectories through a filtered structural parent message.
- Add synthetic 20-path conversion coverage and isolated installed-OpenCode conformance testing.

## 0.8.13 - 2026-07-28

- Stream large Codex rollouts instead of loading complete JSONL files into memory.
- Preserve visible conversation and latest 256 documentary tool events while reporting older tool omissions.
- Strip embedded image data and bound oversized tool values before indexing or transfer.
- Keep exact provider limit errors instead of reducing them to generic parse failures.

## 0.8.12 - 2026-07-28

- Render browser updates as synchronized terminal frames.
- Stop clearing full screen during search, scrolling, discovery, and title loading.
- Erase stale list and detail rows without moving current selection.

## 0.8.11 - 2026-07-28

- Load titles for every session visible in the browser instead of a fixed nine-row window.
- Preserve selected session while background trajectory search results arrive.

## 0.8.10 - 2026-07-28

- Show project, working directory, repository root, recorded branch, current branch, HEAD, timestamps, and workspace state in expanded session details.
- Recover missing workspace and Git metadata from bounded session previews.
- Keep long paths and session IDs recognizable by preserving both ends when space is limited.
- Update rusqlite to 0.40 and sha2 to 0.11.

## 0.8.9 - 2026-07-28

- Resume same-provider sessions from metadata without parsing full trajectories.
- Bound Claude, Codex, and Grok previews to head and latest records.
- Preserve later messages after tool limits and keep documentary tools historical across hops.
- Require exact read-back histories and exact Grok rollback state.
- Reuse unchanged indexes and skip current trajectory reindexing.
- Keep provider discovery caches fast while finding newly imported sessions.

## 0.8.8 - 2026-07-28

- Keep complete trajectory indexes when newer bounded previews arrive.

## 0.8.7 - 2026-07-28

- Index bounded, redacted session trajectories whenever OmniSession reads them.
- Search messages, historical tools, commands, plans, and file activity from session browser.
- Persist full-text index locally and refresh visible-row previews in background.

## 0.8.6 - 2026-07-28

- Replace session IDs in browser rows with provider titles or conversation-derived titles.
- Load titles for visible rows in background batches while keeping full IDs in selected-session details.
- Preview large Codex sessions through bounded head and tail reads without weakening import safety limits.
- Exclude injected agent instructions and plugin recommendations from conversation titles.

## 0.8.5 - 2026-07-28

- Open the session browser after one lightweight Git root lookup instead of capturing and hashing full repository state.
- Load cached sessions in one pass and reuse recent provider checks across repeated launches.
- Back off repeated failed provider scans without discarding the last valid session index.
- Prepare bulk SQLite writes once per provider refresh.

## 0.8.4 - 2026-07-28

- Redesign the session browser with balanced responsive list and detail panes.
- Show redacted first and latest meaningful messages for the selected session.
- Derive missing selected titles from conversation previews and show transfer lineage beside them.
- Keep large provider updates, workspace matching, preview reads, and search indexing off the UI thread.
- Align columns by terminal display width across narrow, wide, Unicode, and resized terminals.

## 0.8.3 - 2026-07-27

- Stop reading or snapshotting Codex state databases during session discovery.
- Record every verified native transfer as source-to-target lineage.
- Show selected session ancestry and descendants in the interactive picker.
- Retry interrupted installer downloads over HTTP/1.1.

## 0.8.2 - 2026-07-27

- Verify Codex imports against persisted native messages without reapplying source import limits.
- Match Codex adapter duplicate-message normalization during read-back verification.
- Report safe message counts when Codex read-back verification fails.

## 0.8.1 - 2026-07-27

- Ask which runnable agent should open an interactively selected session when `--in` is omitted.
- Default target selection to the source agent when its executable is available.
- Render the picker before provider discovery finishes and add results as each store responds.
- Stream cached and live results provider by provider, then use a trigram inverted index for fast substring search across IDs and paths.
- Recover moved or deleted workspaces through an editable, Tab-completing folder prompt.
- Redesign source and target screens around clearer search fields, directory labels, counts, and actions.

## 0.8.0 - 2026-07-27

- Open an interactive session picker when `omnis resume` has no source ID.
- Search session titles, IDs, workspaces, branches, and source providers without reading transcripts.
- Toggle between current and all workspaces and cycle source providers from the picker.
- Resume same-provider selections in place and launch cross-workspace selections from their recorded workspace.

## 0.7.0 - 2026-07-27

- Read Cursor Agent model-visible prompt history and native turn graphs.
- Materialize cross-provider trajectories into Cursor Agent 2026.07.23-e383d2b SQLite/protobuf sessions.
- Verify content-addressed blobs, rewind anchors, full adapter read-back, exact rollback, and native Cursor resume.
- Route selected tasks into verified Cursor sessions without a handoff file.

## 0.6.0 - 2026-07-27

- Resolve bare IDs even when another provider store cannot be scanned.
- Materialize verified native trajectories into Claude Code 2.1.220 and Grok 0.2.112.
- Route Claude and Grok task continuations through native imports and skip installed shims during provider execution.
- Accept bare IDs in show, inspect, verify, Markdown, bundle export, and resume commands.

## 0.5.0 - 2026-07-27

- Materialize cross-provider history as native Codex threads through version-gated app-server injection.
- Preserve bounded historical tool activity in Codex and OpenCode trajectory imports.
- Verify imported native history before launch and roll back exact failed target IDs.
- Reopen Grok catalog sessions by exact ID even when local history exceeds scan limits.

## 0.4.1 - 2026-07-27

- Flush native-import progress and transfer output before long-running provider commands.

## 0.4.0 - 2026-07-27

- Add `omnis markdown SESSION` for redacted manual session handoffs with bounded tool history.
- Resolve bare session IDs across provider stores for Markdown exports.
- Support stdout output or atomic, non-overwriting file output with `-o`.

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
