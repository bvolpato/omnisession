# Changelog

## Unreleased

### Added

- Add `omni search` to find sessions by title, folder, branch, ID, or conversation text without opening the picker. Each run first indexes only sessions changed since the last index; `--all-projects`, `--provider`, `--show-text`, `--no-index`, and `--json` shape scope and output. ([#115](https://github.com/bvolpato/omnisession/pull/115))
- Index every discovered session in the background for full-text picker search, current workspace and newest sessions first, and add `omni index` to build the same index without the picker. Conversation-derived titles fill untitled rows, and search documents built with an older format or older redaction rebuild once. ([#112](https://github.com/bvolpato/omnisession/pull/112))
- Stop indexing after the current session on the first `Ctrl+C` in `omni index` or `omni search`, continue on the next run, and index imported bundles and the current workspace first. ([#115](https://github.com/bvolpato/omnisession/pull/115))
- Roll back native imports on `Ctrl+C` for all nine native targets, including shim-routed imports: finish the in-flight write, roll back the generated target, and exit without launching the provider. ([#120](https://github.com/bvolpato/omnisession/pull/120), [#128](https://github.com/bvolpato/omnisession/pull/128))
- Delete Claude Code sessions through guarded private-store deletion, and enable guarded private-store deletion on macOS for Claude Code, Pi, Cursor Agent, Antigravity CLI, and Cursor IDE. ([#116](https://github.com/bvolpato/omnisession/pull/116))
- Read Antigravity desktop app conversations as a read-only `antigravity-ide` source on Linux and macOS, and draft RFC 010 for a future native target. ([#117](https://github.com/bvolpato/omnisession/pull/117))
- Import Cursor IDE chats into never-opened macOS folders, and stop with a clear error when a Cursor IDE import fails instead of announcing a handoff Cursor IDE cannot deliver. ([#114](https://github.com/bvolpato/omnisession/pull/114))
- Declare Codex and Grok read/index, clean start, and same-provider resume on Windows. ([#123](https://github.com/bvolpato/omnisession/pull/123))
- On Windows, stop `omni index` and `omni search` after the current session and roll back native imports on the first `Ctrl+C` or `Ctrl+Break`, as on Linux and macOS. Import helpers start on a hidden console of their own, and Codex and Grok now declare cross-provider import on Windows. ([#128](https://github.com/bvolpato/omnisession/pull/128))
- Add fuzzy session picker search over titles, folders, branches, and IDs with highlighted matches, a `?`/`F1` help overlay, mouse support (`OMNI_NO_MOUSE=1` keeps native text selection), and `Ctrl+P`/`Ctrl+N`, `Home`/`End`, `Ctrl+W`, and confirmed `Ctrl+D` deletion. ([#108](https://github.com/bvolpato/omnisession/pull/108))
- Add dark, light, and mono session picker palettes, chosen from `NO_COLOR`, `OMNI_THEME`, `COLORFGBG`, or a terminal background query on Unix, with dark as the fallback. ([#111](https://github.com/bvolpato/omnisession/pull/111))
- Title Claude Code sessions at discovery from custom titles, `ai-title`, or `summary` records, else the first history prompt. ([#109](https://github.com/bvolpato/omnisession/pull/109))
- Report omitted events in `omni verify`, including `omitted_events` in JSON output. ([#96](https://github.com/bvolpato/omnisession/pull/96))

### Changed

- Import complete tool call/result pairs into Claude Code, Pi, Hermes, OpenCode, and Grok as native tool records instead of documentary assistant text, and verify them natively on read-back. Orphaned or incomplete tool records stay documentary. ([#82](https://github.com/bvolpato/omnisession/pull/82), [#84](https://github.com/bvolpato/omnisession/pull/84), [#85](https://github.com/bvolpato/omnisession/pull/85), [#87](https://github.com/bvolpato/omnisession/pull/87), [#88](https://github.com/bvolpato/omnisession/pull/88), [#90](https://github.com/bvolpato/omnisession/pull/90))
- Name Pi, Antigravity CLI, and Cursor Agent imports after the redacted source title instead of `Imported from <source>`. ([#92](https://github.com/bvolpato/omnisession/pull/92))
- In the session picker, `Esc` clears the query before quitting, the newest session is selected on open, provider warnings collapse into a badge so key hints stay visible, previews show the conversation before details and scroll with `Shift+Up`/`Shift+Down` or the wheel, and `Ctrl+U` clears the query while updates install from the help overlay. ([#108](https://github.com/bvolpato/omnisession/pull/108))
- Exit `omni resume` and picker launches with the provider's exit code, or 128 plus the signal number, instead of 1. ([#129](https://github.com/bvolpato/omnisession/pull/129))
- Stop warning about providers that are not installed (missing Cursor IDE, Antigravity, or Hermes stores, or a missing OpenCode binary); corrupt databases and non-executable binaries still warn. ([#109](https://github.com/bvolpato/omnisession/pull/109))
- Fully parse Codex rollouts for previews only when they may contain a rollback marker, so escaped control characters no longer force a whole-transcript parse. ([#101](https://github.com/bvolpato/omnisession/pull/101))
- Rewrite the README and add a documentation index, an architecture guide, expanded contributing and security guides, and issue templates that route vulnerabilities to private reporting. ([#126](https://github.com/bvolpato/omnisession/pull/126))
- Rebuild the website as a static landing page with a generated supported-agents matrix, and refresh the README screenshot with synthetic sessions. ([#127](https://github.com/bvolpato/omnisession/pull/127), [#132](https://github.com/bvolpato/omnisession/pull/132))

### Fixed

- Stop one oversized record from failing a whole session read: Codex, Antigravity CLI, Claude Code, and Pi full reads skip it with an omission notice, and session previews skip it so large sessions stay available in the picker. ([#78](https://github.com/bvolpato/omnisession/pull/78), [#96](https://github.com/bvolpato/omnisession/pull/96), [#99](https://github.com/bvolpato/omnisession/pull/99))
- Stream Claude Code and Pi transcripts over 32 MiB, raise the streamed file limit from 512 MiB to 4 GiB, and keep the newest 100,000 events of Codex rollouts over 100,000 records instead of failing. ([#99](https://github.com/bvolpato/omnisession/pull/99), [#130](https://github.com/bvolpato/omnisession/pull/130))
- Load provider SQLite stores up to 4 GiB of database plus WAL instead of failing above 256 MiB, so the largest Cursor Agent, Antigravity, Hermes, and Grok catalog stores index. `OMNI_SNAPSHOT_MAX_BYTES` changes the limit and larger stores fail closed. A free-space check stops a snapshot copy that would leave less than 256 MiB free on the temporary volume, and index runs retry that session instead of marking it unreadable. ([#136](https://github.com/bvolpato/omnisession/pull/136))
- Stream Claude Code `history.jsonl` and Codex `session_index.jsonl` past the strict reader's limits so large stores keep workspace and title mappings, and read Grok sessions with large `updates.jsonl` files. ([#109](https://github.com/bvolpato/omnisession/pull/109))
- Stop `omni list` and the picker from hanging on stores with thousands of sessions: Git only resolves recorded paths inside the requested workspace, and folders with no `.git` marker skip Git spawns. ([#79](https://github.com/bvolpato/omnisession/pull/79), [#93](https://github.com/bvolpato/omnisession/pull/93))
- Stop Claude Code listings from missing sessions on large stores, which also sent native imports to handoff: only transcripts count toward the discovery cap, `subagents` and `tool-results` folders are skipped, and exact reads probe the transcript path first. ([#80](https://github.com/bvolpato/omnisession/pull/80), [#95](https://github.com/bvolpato/omnisession/pull/95))
- Hide Codex guardian subagent threads from listings, the picker, and search indexing, and stop Codex scans from panicking on rollout filenames that end mid-character. ([#102](https://github.com/bvolpato/omnisession/pull/102), [#104](https://github.com/bvolpato/omnisession/pull/104))
- Stop native read-back from failing and falling back to handoff on large tool outputs or messages, which were re-checked against source import limits, and on empty tool results in Grok and Hermes. ([#89](https://github.com/bvolpato/omnisession/pull/89), [#94](https://github.com/bvolpato/omnisession/pull/94))
- Keep assistant-first history (truncated sources, compaction anchors, sessions that open with an assistant or tool record) in Cursor Agent imports instead of failing verification. ([#91](https://github.com/bvolpato/omnisession/pull/91))
- Resolve duplicate Cursor IDE workspace records on macOS with Cursor's own workspace key instead of falling back to semantic handoff. ([#81](https://github.com/bvolpato/omnisession/pull/81))
- Give imported OpenCode sessions, messages, and parts OpenCode's own time-ordered IDs, so imported history keeps document order and sorts before records OpenCode creates later. ([#83](https://github.com/bvolpato/omnisession/pull/83))
- Show imported history in OpenCode's TUI, web app, share pages, and transcripts: imported user and assistant text is no longer marked `synthetic`, which OpenCode hid there while still sending it to the model, and every OpenCode import now opens with a visible OmniSession history notice. ([#134](https://github.com/bvolpato/omnisession/pull/134))
- Stop marking imported Hermes messages as `observed`, a flag Hermes reserves for gateway group-chat context. ([#86](https://github.com/bvolpato/omnisession/pull/86))
- Detect native Claude Code installs (`claude/versions/<version>` binaries) as running Claude, so an active session blocks private Claude store writes. ([#98](https://github.com/bvolpato/omnisession/pull/98))
- Fall back to a semantic handoff when a routed shim import fails to materialize, verify, or plan its launch, and never launch a fallback after a failed rollback. ([#105](https://github.com/bvolpato/omnisession/pull/105), [#113](https://github.com/bvolpato/omnisession/pull/113))
- Stop concurrent routed shim imports from orphaning a generated session (a route whose task head moved rolls back and asks for a rerun), and stop wrapper scripts that re-enter the shim through `PATH` after 16 nested runs. ([#129](https://github.com/bvolpato/omnisession/pull/129))
- Let `omni task bind` replace a branch head whose prior session was deleted, warning and skipping lineage instead of aborting. ([#106](https://github.com/bvolpato/omnisession/pull/106))
- Include Pi branch summaries and displayed extension messages in transcripts, search, and handoffs as contextual messages. ([#130](https://github.com/bvolpato/omnisession/pull/130))
- List OpenCode sessions from every project when listing across workspaces, through `opencode db` with a fallback to the current project, and stop showing synthetic OpenCode user text (attached file contents, plan reminders) as user-authored when the message also has a typed prompt. ([#130](https://github.com/bvolpato/omnisession/pull/130))
- Decode Hermes structured message content, so titles and transcripts no longer leak its internal JSON marker or base64 image data. ([#130](https://github.com/bvolpato/omnisession/pull/130))
- Stop over-counting Claude Code token usage repeated across per-content-block records, and take Claude Code session creation time from the first history record. ([#130](https://github.com/bvolpato/omnisession/pull/130))
- Keep image-only user turns as `[N images omitted]` placeholders for Claude Code, Codex, Pi, OpenCode, and Hermes instead of dropping them. ([#130](https://github.com/bvolpato/omnisession/pull/130))
- Stop re-indexing sessions whose full reads report omitted events, so repeat index runs settle instead of re-reading them every time. ([#125](https://github.com/bvolpato/omnisession/pull/125))
- Skip sessions that failed to index until their source changes instead of re-reading them every run; `omni index --retry-failed` reads them anyway, `omni index` and `omni search` JSON report `failed_skipped`, and the picker reports new failures in one aggregated line. ([#131](https://github.com/bvolpato/omnisession/pull/131))
- Stop listing archived Antigravity desktop app conversations, whose databases the app empties, and replace a Cursor IDE message over the record size limit with an `oversized_bubble` marker instead of failing the whole conversation. ([#131](https://github.com/bvolpato/omnisession/pull/131))
- Honor `OMNI_OPENCODE_BIN` during OpenCode discovery; an invalid provider binary override now means not installed instead of falling back to `PATH`. ([#119](https://github.com/bvolpato/omnisession/pull/119))
- Keep Windows shims alive through `Ctrl+C` and `Ctrl+Break` while a provider runs, and relink provider aliases left on an older build during upgrade. ([#118](https://github.com/bvolpato/omnisession/pull/118))
- Normalize `\\?\` Windows workspace roots, allow slow Windows process listings, and retry busy SQLite writers so concurrent store access stops failing intermittently on Windows. ([#121](https://github.com/bvolpato/omnisession/pull/121), [#122](https://github.com/bvolpato/omnisession/pull/122), [#123](https://github.com/bvolpato/omnisession/pull/123))

### Security

- Redact more common credential formats: prefixed or unseparated environment names such as `DB_PASSWORD`, `AWS_SECRET_ACCESS_KEY`, and `PGPASSWORD`, quoted JSON and YAML keys, Ruby `=>` hashes, `--password` flags, `curl -u` credentials, `Cookie` and `Set-Cookie` headers, `Basic`, `Token`, and `ApiKey` authorization schemes, and URL passwords, including ones containing `@`. ([#100](https://github.com/bvolpato/omnisession/pull/100), [#107](https://github.com/bvolpato/omnisession/pull/107))
- Redact secrets in session picker titles, which can come straight from a first user prompt. ([#103](https://github.com/bvolpato/omnisession/pull/103))
- Never resolve recorded workspace paths that could contact another host (UNC and device paths, `/net`, `/Network`, and `/afs` automounts, and paths containing `..`) during workspace matching for the picker, `omni list`, and search indexing, unless they sit on the current workspace's own share, and reject bundles with such workspace roots on import, export, and load. ([#129](https://github.com/bvolpato/omnisession/pull/129))
- Require Grok session IDs to be lowercase hyphenated UUIDs before `grok sessions delete` runs, so other spellings never reach the command or falsely verify a deletion. ([#129](https://github.com/bvolpato/omnisession/pull/129))
- On Windows, never run `.cmd` or `.bat` provider launchers through `cmd.exe`: provider discovery and the OpenCode adapter refuse them, and validated npm command shims run through `node.exe`. ([#119](https://github.com/bvolpato/omnisession/pull/119), [#123](https://github.com/bvolpato/omnisession/pull/123))

### Internal

- Make private-store lock tests deterministic, back compatibility tests with the provider manifest, add Unicode path coverage, format-drift fixtures, property tests, end-to-end native import fallback tests, and installed Hermes and OpenCode tool-pair conformance, and run the live Windows process listing test alone in CI. ([#97](https://github.com/bvolpato/omnisession/pull/97), [#109](https://github.com/bvolpato/omnisession/pull/109), [#110](https://github.com/bvolpato/omnisession/pull/110), [#113](https://github.com/bvolpato/omnisession/pull/113), [#124](https://github.com/bvolpato/omnisession/pull/124))

## 0.8.51 - 2026-09-09

- Show provider discovery warning text in the session picker footer instead of only a count.
- Print `omni doctor` discovery errors and notes, including this-workspace vs all-workspace session counts.
- Surface Codex scan limits, unreadable session files, and empty listings when jsonl files exist.

## 0.8.50 - 2026-09-08

- Keep exact bound sessions on same-provider switch, write long semantic handoffs to private files, and forward Unix signals through shims.
- Match relocated imported workspaces by repository fingerprint without inferring identity from a reused path.
- Discard Codex rolled-back turns, aborted events, and contextual harness markers; discover Claude transcripts from a bounded metadata prefix; keep Pi compaction summaries as contextual turns.
- Disambiguate Cursor and Antigravity CLI vs IDE targets, fall back to semantic handoff when native materialization fails unless `--materialize-only`, and reject Antigravity IDE as a native target.
- Roll back Hermes provider imports before failing closed, and keep readable imported sessions when one stored bundle is unreadable.
- Pin Next 16.3.4 and sharp 0.35.4 to clear the high libheif advisory in website image processing.
- Align CodeQL Action pins, bump setup-uv to 10.0.1, and refresh website type packages, uuid, and capability docs.

## 0.8.49 - 2026-08-30

- Separate declared provider capabilities from runtime readiness so diagnostics, target pickers, and compatibility docs remain evidence-based across platforms.
- Pin the official Pi package and exercise its installed token-free Linux path in provider conformance.
- Add guarded Antigravity native materialization on macOS with exact CLI writer detection while keeping private deletion Linux-only.
- Discover Cursor Agent and Cursor IDE through canonical paths and bounded static metadata without launching desktop binaries; keep older or unversioned IDE builds limited to explicit same-provider workspace opens.
- Add an explicit Windows installer option to the website while preserving the Linux/macOS command as the default.

## 0.8.48 - 2026-08-29

- Add Windows x86-64 preview packaging, a checksum-verified PowerShell 5.1 installer, and compiled provider shims with safe npm launcher handling.
- Exercise all nine providers through credential-free Windows fixtures and run installed token-free Codex, OpenCode, and Grok checks while keeping provider capabilities evidence-based.
- Make portable bundles exact resumable `imported:<bundle-uuid>` sources, migrate legacy indexes, and bind relocated repositories only through matching fingerprints.
- Serialize private provider writers, harden Windows lock and file URI handling, and keep unverified Windows private writes fail-closed.
- Generate provider compatibility signals from one reviewed manifest and distinguish expected, observed, failed, and not-run states.
- Align portable schema provider values, update Next.js to 16.3.3, and improve project metadata, navigation, and release discovery.

## 0.8.47 - 2026-08-29

- Fail closed when applicable extended validation is skipped, pin required provider checks to release-tested versions, and keep latest-provider drift checks scheduled.
- Draft and verify complete release assets before publication, serialize and queue release runs, and test public installers on native x86_64 and aarch64 Linux runners.
- Require full-SHA GitHub Actions and enable private vulnerability reporting.
- Wait for lazy-loaded website images in browser smoke coverage without hiding broken assets.

## 0.8.46 - 2026-08-26

- Build static Linux release binaries without a host glibc dependency and support manual artifact preflights before tagging.

## 0.8.45 - 2026-08-26

- Accept release checksums on older `awk` implementations used by supported Linux installers.
- Migrate website dependency management and CI from npm to pnpm 11.
- Require website packages, including transitive dependencies, to be at least three days old.
- Redesign the product site and add mobile browser error coverage.
- Split CLI routing, transfer orchestration, picker discovery, and terminal rendering into focused modules.
- Isolate Cursor IDE conformance from unrelated workstation processes on Linux and stop isolated children on cancellation.
- Refresh security-model and roadmap documentation for current provider safeguards.

## 0.8.44 - 2026-08-23

- Serialize Claude native writes across OmniSession processes, reject active Claude writers, and preserve exact rollback and read-back guarantees.
- Read Hermes versions from isolated installed-package metadata without launching network-aware provider commands.
- Audit Rust and website dependencies on changes, weekly schedules, and manual runs.
- Add scheduled search benchmarks for 10 MiB indexing, ranked 10k-session queries, bounded result pages, unchanged refreshes, and stale-row pruning.

## 0.8.43 - 2026-08-16

- Index redacted trajectory edges up to 10 MiB in bounded overlapping chunks, with explicit source and coverage metadata for head-tail retention.
- Bound ranked search delivery, report additional matches, and keep stable best-chunk ordering.
- Surface local indexing failures without blocking safe session reads or imports.
- Prune stale native search rows only after successful provider refreshes while preserving imported bundles and failed-refresh data.
- Pin GitHub Actions to reviewed commit SHAs across CI, CodeQL, release, provider-conformance, Pages, and website workflows.

## 0.8.42 - 2026-08-13

- Match nested Git workspaces by repository identity and invalidate cached parent matches when a nested repository appears.
- Delete only the exact selected Cursor IDE composer and its descendants while preserving unrelated native records.
- Use fast Hermes `--version` path for reliable installed-version checks.
- Pin provider versions used by release validation for reproducible artifacts.
- Patch Nano ID to 3.3.18 and update Next.js to 16.3, React to 19.2.8, TypeScript to 7.0, and Rust dependencies.
- Enforce Rust 1.85.1 compatibility in CI and replace newer language and library syntax.

## 0.8.41 - 2026-08-06

- Add Hermes to installed token-free cross-provider conformance and release validation.
- Verify 72 cross-provider paths without credentials and run provider conformance daily.
- Add Rust coverage thresholds, installer smoke tests, website type checks, static builds, and browser smoke tests to CI.
- Harden Hermes title allocation, native fork lineage, read-back, and rollback.
- Update `md-5` to 0.11 and `base64` to 0.23.
- Make installer smoke tests portable across CI environments.

## 0.8.40 - 2026-07-31

- Add Hermes discovery, complete visible-history reads, native resume, and parent-linked forks.
- Import into Hermes through provider-owned session APIs with independent read-back and exact rollback.
- Add Hermes shims, deletion, compatibility docs, synthetic fixtures, and conversion coverage.

## 0.8.39 - 2026-07-31

- Replace exact private-writer version checks with minimum-version gates plus structural read-back validation.
- Expand Cursor IDE launch and native continuity support across Linux and macOS.
- Refine compatibility signals and provider presentation on website.
- Fix Windows conditional imports.

## 0.8.38 - 2026-07-31

- Import complete visible Codex turns through provider-owned external-session interface.
- Verify persisted Codex messages, roles, ordering, and token estimates before launch.
- Strengthen installed Codex conformance for large synthetic histories.

## 0.8.37 - 2026-07-31

- Add guarded native deletion for Antigravity, Pi, Cursor Agent, and Cursor IDE.
- Require exact source selection, active-writer checks, rollback, and read-back for private-store deletion.
- Preserve newest visible context when large trajectories exceed transfer limits.
- Add RFC 009 for native source deletion.

## 0.8.36 - 2026-07-30

- Check latest release in picker background without delaying session discovery.
- Show installed version at footer right and offer `Ctrl+U` only when newer release exists.
- Confirm exact executable path, then verify release checksum, archive layout, and staged binary before atomic self-update.

## 0.8.35 - 2026-07-30

- Delete Codex, OpenCode, and Grok sessions from their native source through documented provider commands and explicit picker confirmation.
- Show model, reasoning mode, recorded token usage, trajectory size, and matched full-text context in session details.
- Replace generic search-result titles with ranked trajectory snippets and highlight matching terms without moving selection.
- Accept compatible Grok patch releases and strengthen Claude-to-Codex read-back verification.
- Refresh and compress product screenshot, expose `$ omni` above browser view, and distinguish full, version-gated, and exact-build support signals.

## 0.8.34 - 2026-07-30

- Put `NEW SESSION` first in interactive picker and launch selected installed agent cleanly.
- Keep new-session placeholder visible while searches select matching history.
- Add product screenshot to README and website.
- Add `omni fork SESSION`, with interactive target selection or direct `--in PROVIDER` routing.
- Accept `claude` and `claude-code` as interchangeable provider names.

## 0.8.33 - 2026-07-29

- Verify Codex 0.146.0 and Grok 0.2.114 native imports against installed binaries.
- Require all 56 installed cross-provider conversions to match original trajectory oracle.
- Prove imported context reaches Pi and OpenCode models through opt-in noninteractive probes.
- Preserve Cursor IDE workspace identity inside imported headers across later transfers.
- Make Cursor IDE rollback safe across workspace-selection failures and shared rewind anchors.
- Reject symlinked Cursor Agent workspace metadata before native writes.
- Serialize Pi native imports while validating encoded workspace directory identity.

## 0.8.32 - 2026-07-29

- Store Cursor IDE prompt history, turns, assistant steps, and rewind anchors as native trajectory records.
- Restore exact imported composer when opening an existing Cursor workspace.
- Verify Cursor IDE 3.12.17 materialization against installed AppImage.

## 0.8.31 - 2026-07-29

- Create Cursor IDE continuations before a workspace has been opened in Cursor.
- Resolve Linux workspace identity using Cursor's path-and-inode contract.
- Show Cursor IDE as a target only when its launcher matches a verified build.
- Recognize the standard `~/Applications/Cursor.AppImage` desktop alias.

## 0.8.30 - 2026-07-29

- Keep visible metadata matches stable while asynchronous full-text results arrive.
- Append and label sessions matched through indexed trajectory content.
- Keep lineage context in selected-session details instead of adding orphan tree rows to filtered lists.

## 0.8.29 - 2026-07-29

- Link same-provider Codex forks into session trees automatically.
- Match new fork IDs through launch time and workspace without touching Codex state.
- Leave concurrent matching sessions unlinked instead of guessing.

## 0.8.28 - 2026-07-29

- Exclude harness-generated instruction and environment envelopes from transfers.
- Extract Antigravity user requests without runtime metadata wrappers.
- Preserve HTML and XML that belongs to real conversation messages.

## 0.8.27 - 2026-07-29

- Print transfer stages before source parsing and workspace checks begin.
- Avoid recursive home-directory scans when continuing non-Git sessions.

## 0.8.26 - 2026-07-29

- Rename public CLI from `omnis` to `omni`.
- Publish `omni-*` release archives and migrate prior OmniSession installs safely.
- Update commands across CLI guidance, docs, and website.

## 0.8.25 - 2026-07-29

- Offer native fork beside in-place continuation when target agent matches source.
- Keep in-place continuation selected by default.

## 0.8.24 - 2026-07-28

- Name continued sessions from first user message recorded after handoff.
- Replace imported placeholder titles across picker list, details, and lineage tree.
- Resolve continuation titles from complete visible-lineage reads in background.

## 0.8.23 - 2026-07-28

- Group related sessions together in main picker list.
- Render root-to-leaf tree indentation in agent column.
- Preserve selected session when lineage arrives asynchronously.

## 0.8.22 - 2026-07-28

- Show complete cross-agent session trees in picker detail pane.
- Keep filtered or unavailable ancestors visible as lineage context.
- Load titles for known tree nodes in background without moving selection.

## 0.8.21 - 2026-07-28

- Use `opencode` as canonical provider name in JSON and portable bundles.
- Continue accepting legacy `open-code` input.

## 0.8.20 - 2026-07-28

- Open session picker when `omni` runs without a subcommand.
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

- Add `omni resume SESSION --fork` for explicit copy-on-resume.
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

- Open an interactive session picker when `omni resume` has no source ID.
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

- Add `omni markdown SESSION` for redacted manual session handoffs with bounded tool history.
- Resolve bare session IDs across provider stores for Markdown exports.
- Support stdout output or atomic, non-overwriting file output with `-o`.

## 0.3.1 - 2026-07-27

- Report progress while preparing, importing, and verifying native OpenCode sessions.
- Replace generated README and website copy with direct usage documentation.
- Redesign the website around session routing and fidelity reports.

## 0.3.0 - 2026-07-27

- Resolve bare session IDs by exact match across provider stores.
- Default `omni resume ID` to an in-place native-provider resume.
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
- Add explicit `omni` CLI for discovery, inspection, transfer, export, import, and diagnostics.
