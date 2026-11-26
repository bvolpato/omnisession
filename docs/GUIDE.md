# Guide

The complete manual for `omni`. The [README](../README.md) has the short version. [COMPATIBILITY.md](COMPATIBILITY.md) has versions, platforms, and transfer details for each agent.

- [Install](#install)
- [The picker](#the-picker)
- [Search](#search)
- [The search index](#the-search-index)
- [Continue or fork](#continue-or-fork)
- [Permission modes](#permission-modes)
- [Inspect a transfer](#inspect-a-transfer)
- [Markdown and chat handoff](#markdown-and-chat-handoff)
- [Tasks](#tasks)
- [Launch profiles](#launch-profiles)
- [Portable bundles](#portable-bundles)
- [Delete a session](#delete-a-session)
- [Configuration](#configuration)
- [Notes for each agent](#notes-for-each-agent)
- [Troubleshooting](#troubleshooting)

## Install

### Linux and macOS

```sh
curl -fsSL https://raw.githubusercontent.com/bvolpato/omnisession/main/install.sh | sh
```

The installer verifies the release checksum and installs `omni` into `~/.local/bin`. Fresh installs leave shell profiles and provider commands unchanged. Add `~/.local/bin` to `PATH` if needed.

Set `OMNI_INSTALL_DIR` to an absolute path to install elsewhere. Set `OMNI_NO_MODIFY_PATH=1` to leave shell profiles alone. If the path of `omni` in that directory is itself a directory, the installer stops.

```sh
curl -fsSL https://raw.githubusercontent.com/bvolpato/omnisession/main/install.sh | OMNI_INSTALL_DIR="$HOME/bin" sh
```

Linux archives contain static musl binaries that do not depend on the host glibc. WSL is a separate Linux environment. Use this installer inside WSL.

### Windows x86-64 (preview)

From PowerShell:

```powershell
irm https://raw.githubusercontent.com/bvolpato/omnisession/main/install.ps1 | iex
```

The installer verifies the release checksum, installs `omni` into `%LOCALAPPDATA%\OmniSession\bin`, and adds that directory to `PATH`. It does not change provider commands. Restart your shell after a PATH change. `OMNI_INSTALL_DIR` and `OMNI_NO_MODIFY_PATH` work here too. The installer does not replace an `omni.exe` that it did not install.

Limits of the preview:

- Windows packaging, installer, CLI, and synthetic-store checks run in native Windows CI. Codex and Grok declare read/index, clean start, same-agent resume, and cross-agent import on Windows. Installed Codex, OpenCode, and Grok checks run without credentials. Broader provider fidelity remains provisional.
- Other agents do not declare cross-agent import on Windows. Picker targets without declared import continue through a handoff. Explicit `omni resume --in` still attempts a native import.
- `Ctrl+Break` counts as `Ctrl+C` during `omni index`, `omni search`, native imports, and OpenCode 2 forks. Provider probes and native import helpers run on a hidden console of their own.
- Native Windows and WSL provider stores are not interchangeable.

### From source

Building requires Rust 1.89 or newer.

```sh
git clone https://github.com/bvolpato/omnisession.git
cd omnisession
cargo build --release --locked -p omnisession-cli
# then copy target/release/omni onto your PATH
```

### Upgrade from 0.8.x

Earlier versions installed aliases for provider commands. Run the installer once to remove them. This also applies if OmniSession updated itself from 0.8.x, because the in-app updater replaces only the binary. On Linux and macOS, the installer also removes the exact PATH block that earlier versions generated. With `OMNI_NO_MODIFY_PATH=1`, the aliases are removed, but an old PATH block can remain.

### Check this machine

```sh
omni doctor                     # installations, stores, state, and import version gates
omni adapters                   # declared capabilities, without running anything
omni adapters --check-imports   # runs version probes: import=ready or import=blocked
```

Each command accepts `--json`.

`omni doctor` runs the version command of each installed CLI that has a version gate, for 15 seconds at most. It reports old or unrecognized versions and failed probes as native-import blockers. Cursor IDE is read from static product metadata and is never started. A version newer than the tested one is an advisory, not a limit. A passed version gate does not certify the store format: schema, active-writer, rollback, and read-back checks still run at transfer time. Doctor never writes provider stores.

## The picker

```sh
omni
```

Sessions from every installed agent appear in one list, with the current workspace first. Related sessions stay grouped as a tree across agents.

- `NEW SESSION` starts a clean session in any installed agent that supports it.
- Type to filter titles, folders, branches, and IDs fuzzily. Conversation text matches come from the local search index, with matching context and highlighted terms. Quoted text matches exactly, as in [search](#search).
- If conversation text gives the first match, the picker selects it when it arrives, unless you chose `NEW SESSION` during that search.
- `Tab` includes every workspace. The left and right arrows cycle source agents.
- The details pane shows workspace, branch, trajectory size, model, reasoning mode, token usage, and conversation lineage when the agent records them.
- Discovery warnings show as a footer badge. `F1` opens help with every key and the full warning text.

| Key | Action |
| --- | --- |
| `↑` `↓` or `Ctrl-P` `Ctrl-N` | Move selection |
| `PgUp` `PgDn` `Home` `End` | Jump through the list |
| `Enter` | Continue selected session |
| `Esc` | Clear search, then quit |
| `Tab` | Toggle current or all workspaces |
| `←` `→` | Change source agent |
| `Delete` or `Ctrl-D` | Delete selected session (`y` deletes, `n` cancels) |
| `Ctrl-U` or `Ctrl-W` | Clear search or last word |
| `Shift-↑` `Shift-↓` | Scroll preview |
| `F1` | Show or hide help (`?` also works while the search is empty) |

The mouse works too: click selects, double-click opens, and the wheel scrolls. On Linux and macOS, pasted text only edits the search. A line break in it does not open a session.

### The target page

After you select a session, choose where it opens. The picker offers only runnable targets that it found on this machine. When the target matches the source, you can resume in place or fork.

- Type to filter agents by name or by any name that `--in` accepts, such as `grok`, `agy`, or `cur`. `Esc` clears the filter.
- `←` and `→` change the [permission mode](#permission-modes). The page shows the mode and the exact flags that it adds.
- **Clipboard handoff (Grok Bot)** is the last target for an existing session. It saves the full context and instructions to a private file and copies a compact excerpt to the clipboard. It reports the saved path and the copy result. It is available even when no agent is installed.

### The workspace page

When a session needs a different folder, the picker asks for one. `↑` and `↓` select the saved or current folder, `Ctrl-S` uses the saved folder, `Tab` completes a path, and `Ctrl-U` clears it.

### Updates

The picker checks for releases in the background. The footer shows the installed version and an update badge when an update is available. Press `F1` and then `u` to update. The confirmation shows the executable path.

On Linux and macOS, the update replaces that `omni` file in place and follows a symlink to the linked binary. If a package manager owns the file, update through the package manager instead. Windows has no in-app updater, so run the installer again.

## Search

```sh
omni search "rate limiter"
omni search '"qwen3.8"'   # exact phrase
omni search pagination --all-projects --provider codex --limit 50
omni --json search pagination
omni search pagination --cached
```

- Every word must match. Plain words match titles, folders, branches, and IDs fuzzily, and conversation text by prefix.
- Words with inner punctuation, such as `qwen3.8`, `feat/rate-limiter`, or `api_key`, match without gaps: as a substring of a title, folder, branch, or ID, and as adjacent words in conversation text. `qwen3.8` finds `qwen3-8`, but not `qwen3` and `8` far apart.
- Double quotes match exactly, without regard to case but with spaces and punctuation: `"qwen3.8"` finds `Qwen3.8-Coder`, but not `qwen3 8` or `qwen3-8`. An unterminated quote runs to the end of the query. Empty quotes are ignored. A query needs at least one letter or digit. In conversation text, a quoted phrase must start at the beginning of a word.
- Title, folder, branch, and ID matches rank first. Conversation matches follow.
- By default, output shows session reference, age, folder, and match kind. Titles often quote prompts, so titles, index coverage (`complete`, `head-tail`, or `preview`), and redacted conversation text appear only with `--show-text`. JSON always reports coverage for conversation matches.
- Each run first indexes changed sessions in the current project. A live project search also checks recent Codex sessions that may have moved from another directory, and it reports incomplete coverage if it reaches the scan limit. Use `--all-projects` to search every workspace.
- `--no-index` uses the existing search index.
- `--cached` does not open provider stores. It searches the cached session list and the existing index, so results can include sessions that changed or were deleted since the last refresh. Run `omni index` to refresh.
- The first `Ctrl+C` during indexing stops after the current session and searches what is indexed so far. A second `Ctrl+C` exits immediately.

## The search index

The index is local and redacted. The first run indexes every discovered session. Later runs index only sessions that changed.

While the picker is open, it indexes conversation text in the background and shows progress in the header. It usually reads transcripts up to 16 MiB in full. It samples the beginning and the end when a transcript exceeds the byte budget or the full-read limit of the provider. `omni index` builds the same index without the picker. `Ctrl+C` stops it after the current session, and the next run continues from there.

- **Unreadable sessions.** Later index runs skip an unreadable session until its source changes. Use `omni index --retry-failed` to try again.
- **Titles.** The picker and search redact session titles before they cache them. After an upgrade, the CLI also redacts titles that older pickers saved when it first opens the index.
- **Partial refreshes.** A partial provider refresh keeps unobserved sessions available in the picker and cached search. Before the picker resumes a cached native session, it checks the current workspace of that session by ID. If the provider reports that the session moved, you can select it again in its new location. For Claude, the picker also checks the selected transcript after a live refresh, because the history index can still name a previous workspace.
- **Restored files.** The next index refresh picks up a restored Codex file even if its modification time stays the same or moves backward. If an in-place restore preserves the file identity, the size, and all available timestamps, use `omni show codex:<id>` to read the contents and refresh the cached text and workspace. This can apply to same-size restores on Windows that preserve the write time.
- **After an upgrade.** Cached search hides native Codex text and derived titles from older readers until the source is read again. Run `omni index --provider codex` to rebuild them, including messages that unchanged forks inherit. Provider-assigned names and imported bundles remain searchable.

## Continue or fork

```sh
omni resume <session> --in codex            # continue in another agent
omni resume claude:<session-id> --in codex  # qualify the provider when needed
omni fork <session>                         # fork, and choose the target interactively
omni fork <session> --in codex
omni resume <session> --in claude-gateway   # a launch profile that you defined
```

Session references use `provider:id`. A bare ID works when it is unique. Some agents accept several names, such as `claude` and `claude-code`, `agy` and `antigravity`, or `cursor`, `cursor-agent`, and `cursor-cli`.

| Option | Effect |
| --- | --- |
| `--in <agent>` | Target agent or [launch profile](#launch-profiles) |
| `--from <provider>`, `--all` | Starting filters of the picker when `omni resume` has no session |
| `--dry-run` | Show what would happen without launching |
| `--materialize-only` | Create and verify the native target session without launching it |
| `--mode <mode>` | [Permission mode](#permission-modes) that the agent starts in |
| `--fork`, `--no-fork` | Fork a same-agent session, or resume it in place |
| `--allow-workspace-mismatch` | Allow a transfer across different workspace roots |
| `omni fork --model <model>` | Pass `--model <model>` to the target agent |

`--mode` and `--model` start an agent, so they conflict with `--materialize-only`. `omni --json resume` requires `--dry-run`.

### How a transfer chooses its route

1. **Same agent.** The session resumes in place or forks through the agent's own commands.
2. **Native import.** OmniSession writes a new session in the target agent's own format, through the agent's import interface or a native writer. It reads each new session back before launch. If it can verify what a failed import created, it can roll back that session.
3. **Handoff.** A fresh target session starts from a private file that quotes redacted history as untrusted context.

Agents with a version gate stay enabled on newer versions unless structural validation or read-back fails. Older versions cannot import natively. Cursor IDE builds below the gate are excluded from target choices. OpenCode imports through its official API without a version gate.

### When an import cannot run

On a terminal, OmniSession says why and asks what to do: retry (for example after you close the agent that blocked it), fork in the source agent, continue with a handoff file, or cancel. It does not silently replace the import that you asked for.

Scripts and pipes use the handoff automatically when the native import can safely fall back. The warning prints the full cause. Cursor IDE declares no clean start, so failed Cursor IDE imports stop with an error and do not use a handoff.

If a submitted import can still publish a session, OmniSession prints the path of a private recovery file and any known target ID. Check that session before you retry.

### Cancel an import

During a native import, `Ctrl+C` stops the launch and attempts to roll back the generated target. If an OpenCode worker can still publish it, OmniSession reports the target ID and keeps the recovery file. If you interrupt an OpenCode 2 native fork, OmniSession removes the child ID that the provider returned and verifies its removal. If the provider did not return a child ID, check `opencode session list` before you retry.

### History that does not fit

If an import leaves out available history, the receiving agent sees the path of a private Markdown file in its imported conversation. This also applies to same-agent forks that OmniSession creates with an import. The file includes earlier messages that the provider summarized and text that exceeded the native import limits, when the source still has them. For Pi and Oh My Pi, it includes earlier turns on the selected branch.

A handoff gives the path to the agent in its launch prompt. The file stays in the local temporary directory after a successful transfer. You can remove it when you finish, or let the operating system clean it up.

If a native cross-agent import or a Codex archive restore fails and its target may still exist or appear later, OmniSession keeps the private handoff and prints its path. A surviving session can still open the file that its imported history links. You can also give the saved path to the next agent when you continue manually.

### Workspaces

Transfers across different workspace roots require `--allow-workspace-mismatch`. On a terminal, `omni resume` and `omni fork` ask whether to use the source workspace, continue in the current folder, or cancel. If the old worktree has moved, the flag also lets an explicit native fork or resume use the current workspace.

If a Codex source has no verified working directory, run the fork again from the directory where you want to continue and pass `--allow-workspace-mismatch`. This also applies when the new session opens in another agent.

### Models

`omni fork --model <model>` stops before any import if the target agent does not list that option, or if the launch profile already sets a model. If you choose to fork in the source agent after a failed import, OmniSession does not pass the model to that agent.

### Native commands

OmniSession installs only `omni`. Commands such as `omp update`, `claude --continue`, and `codex resume` keep the normal behavior of their providers and do not read OmniSession task state.

## Permission modes

By default, OmniSession passes no permission flags, so the target agent starts in its own default mode. To start it in a stronger mode, press `→` on the target page or pass `--mode` to `omni resume`, `omni fork`, or `omni switch`. Modes run from least to most permissive, and the arrows stop at either end.

| Mode | What the agent does | Claude Code | Codex | Grok | Cursor Agent | Antigravity CLI | OpenCode | Hermes |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `default` | No flags: the agent's own settings decide | no flag | no flag | no flag | no flag | no flag | no flag | no flag |
| `accept-edits` | Edits apply without asking, commands still ask | `--permission-mode acceptEdits` | | `--permission-mode acceptEdits` | | `--mode accept-edits` | | |
| `auto` | Safe actions run on their own, and the agent's own review still stops risky ones | `--permission-mode auto` | `--approve-for-me` | `--permission-mode auto` | `--auto-review` | `--mode auto` | | |
| `yolo` | Nothing asks, every command runs | `--dangerously-skip-permissions` | `--dangerously-bypass-approvals-and-sandbox` | `--always-approve` | `--force` | `--dangerously-skip-permissions` | `--auto` | `--yolo` |

- The built-in default is always `default`. `auto` and `yolo` are choices that you make each time, and the page draws `yolo` in the danger color. `OMNI_MODE=auto` (or any mode) sets your own standing default where the agent has that mode. An unknown `OMNI_MODE` value is ignored.
- `--mode` preselects the mode on the target page and outranks `OMNI_MODE`. On a short terminal, the page scrolls so that the selected agent and its mode stay on screen.
- Pi has no permission prompts to configure, and the IDEs take no launch flags, so they show no mode.
- Oh My Pi offers `default` (no flags), `always-ask` (`--approval-mode always-ask`), `accept-edits` (`--approval-mode write`), and `yolo` (`--approval-mode yolo`). `always-ask` fails closed if the installed help cannot validate it.
- OmniSession uses a mode only when the agent's `--help` lists its flag and, where the help prints accepted values, the requested value. An older agent steps down to the nearest mode that it has, with a warning. For launchers that it can identify, OmniSession reuses complete help results for the same program, arguments, workspace, and environment. An update of the executable, of OmniSession, or of its mode definitions invalidates the result. A profile that replaces the launcher gets a fresh check each time.
- The launch line says which mode runs, for example `Codex permission mode: auto (--approve-for-me)`. Transferred history is still untrusted context, so review a session before you continue it in `yolo`.

## Inspect a transfer

```sh
omni inspect <session> --target grok
omni inspect <session> --target codex --mode auto
```

Inspection reports what a specific route preserves, summarizes, or omits. For a portable bundle, it checks whether the target can import the bundle or receive a handoff, even when the bundle came from the same agent.

Inspection checks the permission mode that `resume` would use, including your `OMNI_MODE` preference. Use `--mode` to check an explicit override.

Before it reports a native CLI resume or a handoff, `inspect` checks that the launcher can run `--help` or `--version`. When a Codex session selects a launch profile, the check uses its arguments and environment. If several profiles match, use `--target <profile>` to inspect one of them. If both probes fail, inspection reports the launcher error. You can still preview a planned handoff with `omni resume <session> --in <agent> --dry-run`.

## Markdown and chat handoff

Export the visible history for manual use:

```sh
omni markdown <session> -o session.md
```

The command does not overwrite an existing file. The size limit is 64 MiB.

Prepare context to paste into Grok Bot or another chat app:

```sh
omni handoff <session>
```

This command saves the full redacted Markdown export to a private file under `~/.omnisession/handoffs` (or `$OMNISESSION_HOME/handoffs`) and copies a compact message to the clipboard. The file remains available after the command exits.

The message includes continuation instructions, source harness and session details, the title and recorded folders when available, and the first three and last two available user or assistant messages. It marks omitted middle messages and shortened excerpts. Tool activity stays in the saved file. The message also gives the saved file path so that you can attach the full context manually. A chat app cannot open a local path from the pasted text by itself.

OmniSession caps the message at 4,000 bytes of UTF-8 as a conservative bound. It is not a documented Grok Bot limit. The handoff moves context. It does not restore native Grok Bot messages or transfer permissions.

Clipboard copying uses `pbcopy` on macOS, `wl-copy`, `xclip`, or `xsel` on Linux, and PowerShell on Windows. If copying fails or no helper is available, the command keeps the file and reports a warning. With `--json`, the fields are:

| Field | Meaning |
| --- | --- |
| `output` | Path of the saved file |
| `clipboard_copied` | Whether the copy succeeded |
| `clipboard_bytes` | Size of the compact message in UTF-8 bytes |
| `clipboard_truncated` | Whether history sampling, source limits, metadata, or an excerpt caused content to be shortened |

## Tasks

`omni task` keeps an exact session selected for a workspace branch. Workspace selection and exact session IDs decide the binding. Recency does not.

```sh
omni task start <name>             # create and select a task for this workspace
omni task bind <session>           # bind an exact session to the task branch
omni task status
omni checkout <name>               # select an existing task
omni switch <agent-or-profile>     # continue the selected session in another agent
```

If another command rebinds the same task branch while `omni switch` is running, the newer binding stays in place. A verified transfer still records the source and target sessions in its handoff history. A manual `omni task bind` only changes which existing session the branch selects. Earlier selections stay in the binding history.

## Launch profiles

A launch profile starts an agent that OmniSession already supports with your own arguments, environment, or wrapper program. It keeps setups such as "Claude Code through a local gateway" out of shell aliases and launcher scripts. The setup appears as a row in `omni` and `omni fork`.

```toml
# ~/.omnisession/profiles.toml (chmod 600)
[profiles.claude-gateway]
label = "Claude Code (local gateway)"
agent = "claude"
args = ["--settings", "${HOME}/.local/share/gateway/claude-settings.json"]
unset = ["ANTHROPIC_API_KEY"]

[profiles.claude-gateway.env]
ANTHROPIC_BASE_URL = "${GATEWAY_URL:-http://127.0.0.1:11435}"
ANTHROPIC_AUTH_TOKEN = "${GATEWAY_TOKEN:-local}"
```

```sh
omni profiles                                   # list profiles and check that each can start
omni resume <session> --in claude-gateway
omni switch claude-gateway
```

A profile changes how an agent starts. Sessions still belong to the agent that the profile names, so a profile cannot change the variables that decide where sessions are. The file is local to your machine. A launch command hides values that look like credentials and values that a profile read from a credential-like variable. [PROFILES.md](PROFILES.md) lists every field, more examples, and the limits.

## Portable bundles

```sh
omni export <session> -o session.omnisession
omni import session.omnisession
omni resume imported:<bundle-uuid> --in codex
```

Bundles are redacted, versioned JSON ([RFC 008](rfcs/008-portable-bundle.md), [schema](../schemas/omnisession-bundle-v1.schema.json)). Imported bundles become durable local sources with exact `imported:<bundle-uuid>` locators. They stay searchable and resumable after the original native store is unavailable, and they keep the original provider and session provenance. Import never writes a provider store. `omni export` does not overwrite an existing file, and a bundle can have at most 64 MiB.

## Delete a session

`Delete` or `Ctrl-D` in the picker removes a supported session from its native source store. Every delete asks for confirmation: `y` deletes, `n` cancels.

- Codex, OpenCode, Grok, and Hermes delete through their own commands.
- On Linux and macOS, Claude Code, Pi, Cursor Agent, Cursor IDE, and Antigravity CLI use guarded private-store deletion. It validates exact paths and schemas, refuses while that agent runs, and verifies that the session is gone. Windows keeps command-based deletion only.
- Oh My Pi, Antigravity Desktop, and Antigravity IDE sessions cannot be deleted from the picker.
- Claude Code deletion removes the transcript and the sidecars named by the session ID. Shared prompt history in `history.jsonl` keeps its lines.

[RFC 009](rfcs/009-native-deletion.md) has the details.

## Configuration

| Variable | Effect |
| --- | --- |
| `OMNISESSION_HOME` | Moves OmniSession state (default `~/.omnisession`) |
| `OMNI_THEME` | `light`, `dark`, or `mono` picker palette |
| `NO_COLOR` | Any non-empty value selects the mono palette |
| `OMNI_NO_MOUSE` | `1` turns off picker mouse capture |
| `OMNI_NO_UPDATE_CHECK` | `1` turns off background release checks |
| `OMNI_MODE` | Standing default [permission mode](#permission-modes): `always-ask` (Oh My Pi), `accept-edits`, `auto`, or `yolo`, used where the agent has that mode. The target page and `--mode` still override it. |
| `OMNI_SNAPSHOT_MAX_BYTES` | Largest provider SQLite database plus WAL copied into a private temporary snapshot, as a positive whole number of bytes (default 4 GiB). Larger stores fail closed. The snapshot also keeps 256 MiB of disk space free. |
| `OMNI_CLAUDE_BIN`, `OMNI_CODEX_BIN`, `OMNI_OPENCODE_BIN`, `OMNI_GROK_BIN`, `OMNI_HERMES_BIN`, `OMNI_ANTIGRAVITY_BIN`, `OMNI_PI_BIN`, `OMNI_OMP_BIN`, `OMNI_CURSOR_AGENT_BIN`, `OMNI_CURSOR_IDE_BIN` | Absolute path of a provider binary. An invalid override means not installed, never a `PATH` fallback. |
| `OMNI_INSTALL_DIR`, `OMNI_NO_MODIFY_PATH` | Installers: install directory, and `1` to skip PATH changes |

Launch profiles are read from `profiles.toml` in the OmniSession state directory.

Picker colors follow the terminal background. The picker reads `COLORFGBG`, then asks the terminal for its background color (OSC 11, for about 100 ms at most), and falls back to the dark palette. Windows uses `COLORFGBG` only. `OMNI_THEME` overrides detection. A non-empty `NO_COLOR` selects mono (bold, dim, underline, and reverse only).

## Notes for each agent

### Antigravity

Antigravity CLI (`antigravity` or `agy`), Desktop (`antigravity-desktop`), and the standalone IDE (`antigravity-ide`) have separate stores. Desktop and IDE history can continue in a supported target with `--in`. Native import into Desktop and the IDE is not available.

### Codex

If a Codex session changed directories, a full read records the workspace of its latest surviving turn. The picker, project listings, and search use that workspace until the rollout changes. When a rollback removes the request that supplied a search title, indexing can retire the title even when the parent of the fork is unavailable. If indexing cannot verify the previous search text for a large rollout, it keeps the usual head and tail. Use `omni show codex:<id>` to index the full rollout.

If you name a thread in the Codex app, the picker shows that name above the conversation preview. `omni list --provider codex` also shows the name, and `omni --json list --provider codex` includes it in the `title` field.

### Cursor

Cursor Agent resumes the chat in its recorded folder, including a subdirectory of the current repository.

For a Cursor IDE chat with readable history, open the recorded folder or saved workspace, then select the chat from Show Chat History. You can find a saved workspace from any of the local folders that its `.code-workspace` file lists. OmniSession opens the saved file from any member, including when the file is a symlink. If the file loses its `folders` array, Cursor opens it as a document, and OmniSession directs you to export the chat or continue in another agent. An exported Cursor bundle remains discoverable from its member folders after you import it.

If you fork a chat from a saved `.code-workspace` while the picker is outside its member folders, the picker asks for a destination. Cursor IDE uses the exact folder that you select, including a repository subdirectory or a symlink.

Cursor IDE can assign a new workspace ID if you delete and recreate a folder at the same path. If the recorded ID no longer matches, use `omni export <session> --output chat.json` to recover the original chat history. If an older record has no verifiable workspace ID, OmniSession reports that limitation. Inspection and resume also flag missing or unreadable conversation records. If only a chat header survives or the composer contains unreadable JSON, use the same export command to save the remaining records.

OmniSession v0.9.0 wrote some Cursor IDE conversations as SQLite BLOBs. Cursor cannot display those records. OmniSession can still export their messages or transfer them to another agent.

### Oh My Pi

Oh My Pi is a separate agent, not a Pi alias. Use `omp` with `--in` and `omp:ID` for session references. An Oh My Pi `/clear` starts a new task and keeps the preceding task out of its handoff. [COMPATIBILITY.md](COMPATIBILITY.md#oh-my-pi) covers its session roots and modes.

## Troubleshooting

**The picker is empty.** Run `omni doctor`. It reports session counts for this workspace and for all workspaces, with any discovery notes. Press `Tab` for every workspace, or list without the project filter:

```sh
omni list --provider codex
omni list --all-projects --provider codex
```

**An import is blocked.** Run `omni adapters --check-imports`. It names the agents that are older than the minimum version.

**OmniSession reports that its state was written by a newer version.** Keep the existing state. Upgrade the executable, or return to the compatible source build that created the state. A release and a newer source build can report the same package version while they support different state formats. Native imports check state compatibility before they create a target session. Native-import previews and direct in-place native resume do not require writable local state.

**A search misses recent text.** Run `omni index`. If a session failed to index before, run `omni index --retry-failed`.

**Something else.** `omni --help` lists diagnostics and advanced commands. Open a [bug report](https://github.com/bvolpato/omnisession/issues/new?template=bug_report.yml), and leave transcript data and secrets out of it.
