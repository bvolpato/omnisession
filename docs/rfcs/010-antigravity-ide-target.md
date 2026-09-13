# RFC 010: Antigravity IDE target

Status: draft

`antigravity-ide` names the Antigravity desktop app. OmniSession reads it as a read-only source. This RFC proposes how OmniSession could write a conversation into the app's store and open it there. None of this is implemented. `reject_unsupported_target` still rejects the IDE as a target.

## Verified facts

Surveyed on macOS on 2026-09-12 against Antigravity 2.2.1. The survey recorded structure and counts only. SQLite files were opened immutable. Neither the app nor its language server was launched. Protobuf message and field names come from descriptors embedded in `language_server`, read statically.

### Bundle

- `/Applications/Antigravity.app`: `CFBundleIdentifier` `com.google.antigravity`, `CFBundleShortVersionString` 2.2.1, `LSMinimumSystemVersion` 12.0. `CFBundleURLTypes` registers scheme `antigravity`. No `CFBundleDocumentTypes`.
- There is no `Contents/Resources/app/product.json`, so this is not the 1.x VS Code layout. App code lives in `Contents/Resources/app.asar`. Its `package.json` declares name `antigravity`, version 2.2.1, description "Antigravity - Agentic Desktop Application", and main `dist/main.js`.
- `Contents/Resources/bin` holds only `language_server` and `webm_encoder`. There is no CLI launcher. `~/.antigravity/antigravity/bin/{agy,antigravity}` are dangling symlinks into the missing 1.x path `Contents/Resources/app/bin/antigravity`.
- `dist/main.js` takes Electron's single-instance lock and registers the app as the default `antigravity` protocol client. It forwards `antigravity://` URLs from `open-url`, `second-instance`, and `process.argv` to the renderer as a `deep-link` IPC message. It has no `open-file` handler and ignores folder arguments. No `antigravity://` route strings appear in `dist/*.js` or in `language_server` strings, so the routes are unknown.
- `dist/languageServer.js` spawns `Contents/Resources/bin/language_server --standalone --override_ide_name antigravity --subclient_type hub --override_ide_version <app version> --override_user_agent_name antigravity --https_server_port 0 --csrf_token <random> --app_data_dir antigravity --api_server_url ... --cloud_code_endpoint ... --enable_sidecars` and restarts it after crashes. The data dir name is `app.getName()` lowercased with whitespace removed, or `antigravity-dev` when unpackaged.
- `dist/paths.js` has three data dirs:
  - `~/.gemini/antigravity`, the old IDE data dir;
  - `~/.gemini/antigravity-ide`, used by the separately installed IDE (`/Applications/Antigravity IDE.app`, Linux `~/.local/share/antigravity-ide`);
  - `~/.gemini/antigravity-backup`, a backup copy.
- `dist/ideInstall` offers to install that IDE and copy user data into it. On the surveyed machine `Antigravity IDE.app` is absent and `~/.gemini/antigravity-ide/conversations` holds no databases.

### Store `~/.gemini/antigravity/`

- **Conversation DBs.**
  - `conversations/<uuid>.db`: 9 files, 49 KB to 60 MB. No `-wal`, `-shm`, or `-journal` sidecars at rest.
  - All 9 use the same seven-table schema that the Antigravity CLI writer creates (`battle_mode_infos`, `executor_metadata`, `gen_metadata`, `parent_references`, `steps`, `trajectory_meta`, `trajectory_metadata_blob`), with `user_version` 1 and `step_format` 0.
- **`trajectory_meta`.** One row per DB.
  - The 7 populated DBs have `cascade_id` equal to the file UUID, `trajectory_type` 4 (`CASCADE`), and `source` 1 (`CASCADE_CLIENT`). The CLI writer uses source 17 (`CLI`).
  - The 2 empty DBs have type 0, source 0, and a different `cascade_id`.
- **`trajectory_metadata_blob`.** Row `main` holds `exa.cortex_pb.CortexTrajectoryMetadata`: 1 `workspaces`, 2 `created_at`, 3 `initialization_state_id`, 5 `parent_conversation_id`, 6 `root_conversation_id`, 7 `workspace_uris`, 8 `subagent_spec`, 17 `nesting_depth`, 18 `project_id`, and more.
  - The blob is empty in the 2 empty DBs.
  - 2 populated DBs are subagent trajectories: they set `parent_conversation_id`, `subagent_spec`, and `nesting_depth`.
- **Steps.** 4804 rows.
  - The existing Antigravity CLI `ProtoStep` decoder decoded all 4804 with 0 failures. The payload type matched the `step_type` column in every row, and every step had `metadata.created_at`.
  - Step types (`CortexStepType`): CODE_ACTION 150, GREP_SEARCH 230, VIEW_FILE 513, LIST_DIRECTORY 107, USER_INPUT 63, PLANNER_RESPONSE 2265, ERROR_MESSAGE 20, RUN_COMMAND 625, CHECKPOINT 24, SEARCH_WEB 3, CONVERSATION_HISTORY 20, SYSTEM_MESSAGE 410, GENERIC 374.
- **User text.** In all 63 user steps, `CortexStepUserInput.query` (1) is empty. `user_response` (2) is set and equals the concatenated `items[].text` (3). The CLI writer sets only `query`.
- **Summary cache.** `agyhub_summaries_proto.pb` (116 KB) is `jetbox_summaries_pb.SummariesState`: 1 `summaries` map<string, `exa.jetski_cortex_pb.CascadeTrajectorySummary`>, 2 `file_mod_times` map (absent).
  - Summary fields: 1 `summary` (title), 2 `step_count`, 3 `last_modified_time`, 4 `trajectory_id`, 5 `status`, 7 `created_time`, 8 `waiting_steps`, 9 `workspaces`, 10 `last_user_input_time`, 15 `annotations`, 16 `last_user_input_step_index`, 17 `trajectory_metadata`, 20 `source`, 21 `not_fully_idle`, 22 `trajectory_type`, 23 `killed`.
  - `CortexWorkspaceMetadata` fields: 1 `workspace_folder_absolute_uri`, 2 `git_root_absolute_uri`, 3 `repository`, 4 `branch_name`.
- **Discovery.** The cache has 107 entries against 9 DBs. Every DB has an entry, but 98 entries have neither a DB nor a brain dir. The cache outlives databases, so the app likely lists conversations from summaries. OmniSession lists only DBs.
- **Summary vs DB.**
  - In all 7 populated DBs, `last_modified_time` is at or after the newest step `created_at`, and `step_count` equals the row count.
  - Both empty DBs declare a nonzero `step_count`.
  - 2 DB files were modified after their summary's `last_modified_time`.
  - `status` is IDLE (1) in 106 entries and RUNNING (2) in 1. `trajectory_type` is 4 in 106 entries. `source` is never set.
- **Titles.** `summary` is present in 106 of 107 entries. `annotations/<uuid>.pbtxt` (104 files) holds text-format `ConversationAnnotations` (`archived` 99, `title` 4). Those 4 titles equal the embedded summary annotations and the summary text.
- **Workspaces.** The first `workspaces[].workspace_folder_absolute_uri` is a `file://` URI in 105 entries. It differs from `trajectory_metadata.workspace_uris[0]` only in 7 multi-root entries, which have the same URI set in a different order. 5 entries have no metadata URIs.
- **Brain logs.** `brain/<uuid>/.system_generated/logs/transcript{,_full}.jsonl` exists for the 9 DBs. Row counts differ from `steps` in 4 of 9, so the logs are not authoritative.
- **Other files.** `antigravity_state.pbtxt` is `jetbox_state_pb.JetboxAppState` (`migrate_convos_into_projects`, `migrate_retroactive_projects`, `sidebar_workspaces`, ...). `implicit/` (100 `.pb` files) is not keyed by conversation ID.

## Current support

Read-only source adapter on Linux and macOS:

- Root is `~/.gemini/antigravity`, overridable with `ANTIGRAVITY_IDE_HOME`.
- Listing scans `conversations/*.db` and joins cache entries for title, `last_modified_time`, `created_time`, first workspace folder, and branch.
- Cache entries without a DB are skipped. DBs without an entry are listed with an approximate update time from the file's mtime.
- Reads decode steps with the shared Antigravity CLI decoder. User text falls back from `query` to `user_response` to text `items`.
- A read fails when the summary declares steps but the DB has none.
- `omni list` and `omni resume antigravity-ide:ID --in PROVIDER` accept the IDE as a source. Targeting the IDE stays rejected.

## Proposed target flow

Every gate fails closed to semantic handoff.

1. **Platform.** macOS first. Linux only after a separate survey. Windows stays disabled.
2. **Version.** The resolved bundle's `CFBundleShortVersionString` must be at least the accepted minimum (see below).
3. **Schema.** The target root must contain `conversations/`, and sampled DBs must match the accepted schema with `user_version` 1. The summary file must be absent or decode as `SummariesState`.
4. **Closed app.** The app and its language server must not be running (see active-writer detection). Take an owner-private system-temp lock keyed by the canonical data root, as the CLI writer does, and recheck processes under the lock.
5. **New IDs.** Generate UUID v4 conversation and trajectory IDs.
6. **DB image.** Build with the CLI builder, plus these app values:
   - `trajectory_meta`: `cascade_id` equal to the UUID, source 1, type 4;
   - metadata blob with `workspaces[0]` (folder URI, plus git root and branch when known), `created_at`, and `workspace_uris`;
   - user steps set `query`, `user_response`, and one text item.
7. **Publish DB.** Write a private temp file in the same directory, fsync it, then publish without replacement (`link` plus unlink, or `renameat2(RENAME_NOREPLACE)` on Linux).
8. **Summary entry.**
   - Read the summary file bytes and hash them.
   - Append one encoded field-1 map entry to the raw bytes instead of decoding and re-encoding, because re-encoding drops unknown fields.
   - Write a same-directory temp file, fsync, and rename it over the original only if the original's hash is unchanged. Fsync the directory. Keep the original bytes for rollback.
   - Set `summary`, `step_count`, `created_time`, `last_modified_time`, `last_user_input_time`, `last_user_input_step_index`, `trajectory_id`, `status` IDLE, `workspaces`, `trajectory_metadata`, and `trajectory_type` 4.
9. **Rollback.** On any failure before the lineage commit:
   - restore the original summary bytes if the current hash equals the generated one;
   - remove the generated DB if its bytes equal the image;
   - otherwise report `RollbackFailed`.
10. **Verify.** Read back through `AntigravityIdeAdapter`. The ID must be listed, title, workspace, and `updated_at` must match, and full visible history must equal the expected history (RFC 007).
11. **Open.** Release the lock, then run `open -b com.google.antigravity` (equivalent to `open -a Antigravity`).
    - Passing a folder (`open -a Antigravity <folder>`) is unverified, since `main.js` has no `open-file` handler.
    - `antigravity://` deep links reach the renderer, but the routes are unknown. Use a deep link only after an "open conversation" route is confirmed.
    - Until then, the launch plan opens the app and tells the user which conversation to pick.

## Active-writer detection

- **macOS.** Use `/bin/ps -ww -x -o pid=,ucomm=,args=`, as the CLI writer does. Refuse to write when any of these match:
  - an executable at `<bundle>.app/Contents/MacOS/Antigravity` whose `Info.plist` declares `com.google.antigravity`, at any install location;
  - a `language_server` or `language_server_*` process inside such a bundle;
  - a `language_server` whose `--app_data_dir` value (space- or `=`-separated) has the exact component `antigravity`;
  - a `language_server` with `--subclient_type hub` and no `--app_data_dir`, treated conservatively.
- **Race.** The language server restarts after a crash while the app runs, so check the app process first. Recheck under the lock immediately before publishing the DB and before renaming the summary file.
- **Linux.** Apply the same predicates over `/proc/<pid>/{comm,cmdline,status}`, skipping zombies.
- **CLI detection.** The CLI writer's check still matches only the exact `antigravity-cli` component and keeps ignoring app processes. The two stores do not overlap.

## Minimum version gate

- The accepted minimum starts at the first version that passes manual validation, proposed 2.2.1. Newer versions stay enabled while schema and summary checks pass. Older versions fail closed.
- **Version source.** Read `CFBundleShortVersionString` from the resolved bundle: `/Applications/Antigravity.app`, `~/Applications/Antigravity.app`, or a new `OMNI_ANTIGRAVITY_IDE_APP` override. Never launch the app or `language_server` to learn the version.
- **Build stamp.** `dist/languageServer.js` runs `language_server --stamp` to read the build CL. Do not call it until it is confirmed to exit without starting a server.
- **Manifest.** Record the minimum as `minimum_version`. Declare `cross_provider_import` (and `clean_start` if a launcher is verified) only with matching evidence.

## Open questions

1. Does the app list conversations from summaries only, from a DB scan, or both? Does it need `file_mod_times`?
2. Does the language server rewrite `agyhub_summaries_proto.pb` from memory on start or exit, dropping an entry written while the app was closed? Does it rebuild entries from DBs?
3. What state must exist for a conversation to render and continue: `brain/<uuid>/`, `annotations/<uuid>.pbtxt`, `gen_metadata` or `executor_metadata` rows, `initialization_state_id`, `project_id`?
4. Does project migration (`migrate_convos_into_projects`, `project_id`, `sidebar_workspaces`) gate visibility per workspace?
5. Which deep-link routes exist, and is opening a folder supported at all?
6. What are the Linux and Windows layout, process names, version source, and data dir?
7. Does the separately installed Antigravity IDE (`~/.gemini/antigravity-ide`) use the same store format? Should it get its own provider ID or a root override?
8. Will the model backend accept continuing a trajectory whose steps lack generator metadata?
9. What roles do the file UUID, `trajectory_id`, and `cascade_id` play when resuming?
10. Is source `CASCADE_CLIENT` required, or is CLI source 17 tolerated?

## One-time manual validation

This needs the user's explicit approval. It has not been performed.

1. Quit Antigravity and confirm no app or language server process remains.
2. Back up `agyhub_summaries_proto.pb` and record the `conversations/` listing. Do not use `~/.gemini/antigravity-backup`, because the app uses that name.
3. Run a prototype writer against the real store for one synthetic conversation, using a unique marker title and a synthetic workspace folder.
4. Read it back with `omni list --provider antigravity-ide --all-projects`.
5. Launch Antigravity. Confirm the conversation appears with its title, workspace, and visible messages. Do not send a prompt.
6. Quit. Confirm the summary entry and DB are still present, with only app-owned fields changed.
7. With the app closed, restore the summary backup and remove the generated DB. Relaunch and confirm the conversation is gone.
8. Record the app version, required fields, and any summary rewrite.
