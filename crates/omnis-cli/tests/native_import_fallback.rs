#![cfg(any(unix, windows))]

//! Drives native import failures and interrupts end to end through synthetic `codex` and `pi`
//! providers: shell scripts on Unix, and Node scripts behind npm command shims on Windows.

#[cfg(unix)]
use std::os::unix::{fs::PermissionsExt, process::CommandExt};
#[cfg(windows)]
use std::{env, ffi::OsString, os::windows::process::CommandExt};
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::OnceLock,
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use serde_json::{Value, json};
use wait_timeout::ChildExt;

const SOURCE_ID: &str = "11111111-1111-4111-8111-111111111111";
const THREAD_ID: &str = "33333333-3333-4333-8333-333333333333";
const SOURCE_QUESTION: &str = "Synthetic opening question";
// Hang cases shorten the 20-second production RPC timeout. Healthy requests keep the default.
const HANG_RPC_TIMEOUT_MS: &str = "2000";
// Below the production RPC timeout, so a hang that ignores the shortened timeout fails.
const BOUNDED: Duration = Duration::from_secs(15);
const WATCHDOG: Duration = Duration::from_secs(90);
// Injects a Ctrl+C at one import checkpoint instead of racing a real signal.
const INTERRUPT: &str = "OMNI_TEST_IMPORT_INTERRUPT";
/// Whether `omni shim exec codex` imports natively. Windows leaves Codex cross-provider import
/// undeclared, so shim routing into Codex continues through semantic handoff there.
const SHIM_IMPORTS_CODEX: bool = cfg!(unix);

// Synthetic Codex CLI. It passes the version gate, answers just enough app-server JSON-RPC to import
// a thread whose visible turns echo the imported history, and records any provider launch. The
// rollout it writes diverges from that history, so OmniSession's store read-back fails, unless
// `FAKE_CODEX_ROLLOUT=faithful` writes the imported history instead. `FAKE_CODEX_IMPORT` breaks the
// import request, `FAKE_CODEX_READ=empty` breaks in-server turn verification, and
// `FAKE_CODEX_DELETE` breaks rollback. `FAKE_CODEX_IMPORT=wait` publishes the thread and then waits
// for `release` before answering. An app-server that receives SIGINT records `server-interrupted`
// and exits, like a provider killed by a terminal Ctrl+C.
#[cfg(unix)]
const FAKE_CODEX: &str = r#"#!/bin/sh
capture=$FAKE_CODEX_CAPTURE
if [ "$1" = "--version" ]; then
    printf 'codex-cli 0.146.0\n'
    exit 0
fi
if [ "$1" != "app-server" ]; then
    for prompt do printf '%s\000' "$prompt" >> "$capture/launch-args"; done
    pwd > "$capture/launch-cwd"
    handoff=${prompt#*\`}
    handoff=${handoff%%\`*}
    /bin/cp "$handoff" "$capture/handoff.md" 2>/dev/null || exit 92
    exit 0
fi
printf '%s\n' "$$" >> "$capture/server-pids"
trap ': > "$capture/server-interrupted"; exit 130' INT
rollout=$CODEX_HOME/sessions/2026/01/01/rollout-2026-01-01T00-00-00-$FAKE_CODEX_THREAD.jsonl
while IFS= read -r line; do
    id=${line#'{"id":'}
    id=${id%%,*}
    method=${line#*'"method":"'}
    method=${method%%'"'*}
    thread=
    case "$line" in *'"threadId":"'*) thread=${line#*'"threadId":"'}; thread=${thread%%'"'*} ;; esac
    printf '%s\n' "$method${thread:+ $thread}" >> "$capture/rpc.log"
    case "$method" in
    initialize)
        printf '{"id":%s,"result":{}}\n' "$id"
        ;;
    externalAgentConfig/import)
        case "$FAKE_CODEX_IMPORT" in
        hang) continue ;;
        disconnect) exit 1 ;;
        malformed) printf '{"id":%s,"result":{"importId":\n' "$id"; continue ;;
        esac
        source=${line#*'"path":"'}
        source=${source%%'"'*}
        items=
        history=
        while IFS= read -r record; do
            text=${record#*'"message":{"content":'}
            text=${text%%',"role":'*}
            case "$record" in
            *'"type":"assistant"}')
                item='{"type":"agentMessage","text":'"$text"'}'
                message='{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":'"$text"'}]}}'
                ;;
            *)
                item='{"type":"userMessage","content":[{"type":"text","text":'"$text"'}]}'
                message='{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":'"$text"'}]}}'
                ;;
            esac
            items=${items:+$items,}$item
            history="$history$message
"
        done < "$source"
        printf '%s' "$items" > "$capture/turn-items"
        if [ "$FAKE_CODEX_ROLLOUT" != faithful ]; then
            history='{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Synthetic divergent history"}]}}
'
        fi
        /bin/mkdir -p "${rollout%/*}"
        printf '{"type":"session_meta","timestamp":"2026-01-01T00:00:00Z","payload":{"id":"%s"}}\n%s' "$FAKE_CODEX_THREAD" "$history" > "$rollout"
        if [ "$FAKE_CODEX_IMPORT" = wait ]; then
            : > "$capture/importing"
            waited=0
            while [ ! -e "$capture/release" ] && [ "$waited" -lt 300 ]; do
                /bin/sleep 0.05
                waited=$((waited + 1))
            done
        fi
        printf '{"id":%s,"result":{"importId":"synthetic-import"}}\n' "$id"
        printf '{"method":"externalAgentConfig/import/completed","params":{"importId":"synthetic-import","itemTypeResults":[{"itemType":"SESSIONS","successes":[{"target":"%s"}]}]}}\n' "$FAKE_CODEX_THREAD"
        ;;
    thread/read)
        case "$FAKE_CODEX_READ" in
        empty) printf '{"id":%s,"result":{"thread":{"turns":[]}}}\n' "$id" ;;
        *) printf '{"id":%s,"result":{"thread":{"turns":[{"items":[%s]}]}}}\n' "$id" "$(/bin/cat "$capture/turn-items")" ;;
        esac
        ;;
    thread/delete)
        case "$FAKE_CODEX_DELETE" in
        hang) ;;
        reject) printf '{"id":%s,"error":{"code":-32000,"message":"synthetic delete failure"}}\n' "$id" ;;
        *)
            if [ "$thread" = "$FAKE_CODEX_THREAD" ]; then /bin/rm -f "$rollout"; fi
            printf '{"id":%s,"result":{}}\n' "$id"
            ;;
        esac
        ;;
    esac
done
"#;

// The Unix synthetic Codex CLI as a Node script. `FAKE_CODEX_IMPORT=break` publishes the thread,
// sends Ctrl+Break to omni's console process group, and answers after omni's handler has run. An
// app-server that receives Ctrl+C or Ctrl+Break records `server-interrupted` and exits.
#[cfg(windows)]
const FAKE_CODEX: &str = r#"#!/usr/bin/env node
"use strict";
const childProcess = require("child_process");
const fs = require("fs");
const path = require("path");
const readline = require("readline");

const env = process.env;
const capture = env.FAKE_CODEX_CAPTURE;
const args = process.argv.slice(2);
const append = (name, text) => fs.appendFileSync(path.join(capture, name), text);

if (args[0] === "--version") {
  fs.writeSync(1, "codex-cli 0.146.0\n");
  process.exit(0);
}
if (args[0] !== "app-server") {
  for (const argument of args) append("launch-args", `${argument}\0`);
  fs.writeFileSync(path.join(capture, "launch-cwd"), process.cwd());
  try {
    fs.copyFileSync(args[args.length - 1].split("`")[1], path.join(capture, "handoff.md"));
  } catch {
    process.exit(92);
  }
  process.exit(0);
}

append("server-pids", `${process.pid}\n`);
for (const signal of ["SIGINT", "SIGBREAK"]) {
  process.on(signal, () => {
    fs.writeFileSync(path.join(capture, "server-interrupted"), "");
    process.exit(130);
  });
}
const rollout = path.join(env.CODEX_HOME, "sessions", "2026", "01", "01",
  `rollout-2026-01-01T00-00-00-${env.FAKE_CODEX_THREAD}.jsonl`);
const send = (message) => fs.writeSync(1, `${JSON.stringify(message)}\n`);

// GenerateConsoleCtrlEvent cannot limit Ctrl+C to a process group, so this sends Ctrl+Break to the
// group omni leads. The sender joins this app-server's group, so it sees the event only if the
// app-server shares omni's group.
function interruptOmni() {
  const temporary = path.join(capture, "sender-tmp");
  fs.mkdirSync(temporary, { recursive: true });
  const script = "$ErrorActionPreference = 'Stop'; $quote = [char]34; " +
    "Add-Type -Namespace OmniTest -Name Console -MemberDefinition ('[System.Runtime.InteropServices.DllImport(' + $quote + 'kernel32.dll' + $quote + ', SetLastError = true)] public static extern bool GenerateConsoleCtrlEvent(uint ctrlEvent, uint processGroupId);'); " +
    "if (-not [OmniTest.Console]::GenerateConsoleCtrlEvent(1, [uint32]$env:OMNI_TEST_BREAK_GROUP)) { throw ('GenerateConsoleCtrlEvent failed: ' + [System.Runtime.InteropServices.Marshal]::GetLastWin32Error()) }";
  const sender = childProcess.spawnSync(
    path.join(env.SystemRoot, "System32", "WindowsPowerShell", "v1.0", "powershell.exe"),
    ["-NoLogo", "-NoProfile", "-NonInteractive", "-Command", script],
    {
      // Add-Type compiles in TEMP, which omni's isolated temporary directory must not collect.
      env: { ...env, OMNI_TEST_BREAK_GROUP: String(process.ppid), TEMP: temporary, TMP: temporary },
      stdio: "ignore",
    },
  );
  append("break-sender", `${sender.status}\n`);
}

(async () => {
  const lines = readline.createInterface({ input: process.stdin, crlfDelay: Infinity });
  for await (const line of lines) {
    const request = JSON.parse(line);
    const params = request.params || {};
    append("rpc.log", `${request.method}${params.threadId ? ` ${params.threadId}` : ""}\n`);
    switch (request.method) {
      case "initialize":
        send({ id: request.id, result: {} });
        break;
      case "externalAgentConfig/import": {
        const mode = env.FAKE_CODEX_IMPORT;
        if (mode === "hang") break;
        if (mode === "disconnect") process.exit(1);
        if (mode === "malformed") {
          fs.writeSync(1, `{"id":${request.id},"result":{"importId":\n`);
          break;
        }
        const items = [];
        const history = [];
        const source = params.migrationItems[0].details.sessions[0].path;
        for (const record of fs.readFileSync(source, "utf8").split("\n").filter(Boolean)) {
          const { message, type } = JSON.parse(record);
          const role = type === "assistant" ? "assistant" : "user";
          items.push(role === "assistant"
            ? { type: "agentMessage", text: message.content }
            : { type: "userMessage", content: [{ type: "text", text: message.content }] });
          history.push({
            type: "response_item",
            payload: {
              type: "message",
              role,
              content: [{ type: role === "assistant" ? "output_text" : "input_text", text: message.content }],
            },
          });
        }
        fs.writeFileSync(path.join(capture, "turn-items"), JSON.stringify(items));
        const records = env.FAKE_CODEX_ROLLOUT === "faithful" ? history : [{
          type: "response_item",
          payload: { type: "message", role: "assistant", content: [{ type: "output_text", text: "Synthetic divergent history" }] },
        }];
        fs.mkdirSync(path.dirname(rollout), { recursive: true });
        fs.writeFileSync(rollout, [
          { type: "session_meta", timestamp: "2026-01-01T00:00:00Z", payload: { id: env.FAKE_CODEX_THREAD } },
          ...records,
        ].map((record) => `${JSON.stringify(record)}\n`).join(""));
        if (mode === "break") {
          interruptOmni();
          // Console control events run on a new thread in each target; give omni's handler time.
          await new Promise((resolve) => setTimeout(resolve, 2000));
        }
        send({ id: request.id, result: { importId: "synthetic-import" } });
        send({
          method: "externalAgentConfig/import/completed",
          params: {
            importId: "synthetic-import",
            itemTypeResults: [{ itemType: "SESSIONS", successes: [{ target: env.FAKE_CODEX_THREAD }] }],
          },
        });
        break;
      }
      case "thread/read": {
        const turns = env.FAKE_CODEX_READ === "empty"
          ? []
          : [{ items: JSON.parse(fs.readFileSync(path.join(capture, "turn-items"), "utf8")) }];
        send({ id: request.id, result: { thread: { turns } } });
        break;
      }
      case "thread/delete":
        if (env.FAKE_CODEX_DELETE === "hang") break;
        if (env.FAKE_CODEX_DELETE === "reject") {
          send({ id: request.id, error: { code: -32000, message: "synthetic delete failure" } });
          break;
        }
        if (params.threadId === env.FAKE_CODEX_THREAD) fs.rmSync(rollout, { force: true });
        send({ id: request.id, result: {} });
        break;
    }
  }
  process.exit(0);
})();
"#;

// Synthetic Pi CLI. It passes the version gate and records any provider launch.
#[cfg(unix)]
const FAKE_PI: &str = r#"#!/bin/sh
if [ "$1" = "--version" ]; then
    printf '0.82.0\n'
    exit 0
fi
for argument do printf '%s\000' "$argument" >> "$FAKE_PI_CAPTURE/pi-launch-args"; done
exit 0
"#;

#[cfg(windows)]
const FAKE_PI: &str = r#"#!/usr/bin/env node
"use strict";
const fs = require("fs");
const path = require("path");

const args = process.argv.slice(2);
if (args[0] === "--version") {
  fs.writeSync(1, "0.82.0\n");
  process.exit(0);
}
for (const argument of args) {
  fs.appendFileSync(path.join(process.env.FAKE_PI_CAPTURE, "pi-launch-args"), `${argument}\0`);
}
process.exit(0);
"#;

/// Current npm `cmd-shim` output, up to the package script path.
#[cfg(windows)]
const NPM_COMMAND_SHIM_PREFIX: &str = "@ECHO off\r\nGOTO start\r\n:find_dp0\r\nSET dp0=%~dp0\r\nEXIT /b\r\n:start\r\nSETLOCAL\r\nCALL :find_dp0\r\n\r\nIF EXIST \"%dp0%\\node.exe\" (\r\n  SET \"_prog=%dp0%\\node.exe\"\r\n) ELSE (\r\n  SET \"_prog=node\"\r\n)\r\n\r\nendLocal & goto #_undefined_# 2>NUL || title %COMSPEC% & set PATHEXT=%PATHEXT:;.JS;=;% & \"%_prog%\"  \"%dp0%\\";
#[cfg(windows)]
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Clone, Copy, Debug)]
enum Route {
    Shim,
    Resume,
    Switch,
}

impl Route {
    const ALL: [Self; 3] = [Self::Shim, Self::Resume, Self::Switch];

    fn args(self) -> Vec<String> {
        match self {
            Self::Shim => strings(&["shim", "exec", "codex", "--", "resume", "--last"]),
            Self::Resume => strings(&["resume", &format!("claude:{SOURCE_ID}"), "--in", "codex"]),
            Self::Switch => strings(&["switch", "codex"]),
        }
    }
}

/// `routes` that import into Codex natively on this platform.
fn native_routes(routes: &[Route]) -> Vec<Route> {
    routes
        .iter()
        .copied()
        .filter(|route| SHIM_IMPORTS_CODEX || !matches!(route, Route::Shim))
        .collect()
}

#[test]
fn readback_failure_rolls_back_and_launches_semantic_handoff() {
    if !tools_available() {
        return;
    }
    for route in native_routes(&Route::ALL) {
        let label = format!("{route:?}");
        let fixture = Fixture::new();
        let run = fixture.run(&route.args(), &[]);
        assert!(
            run.status.success(),
            "{label} did not launch a handoff: {}",
            run.stderr
        );
        assert!(
            run.stderr
                .contains("Codex import failed read-back verification"),
            "{label}: {}",
            run.stderr
        );
        assert!(
            run.stderr.contains("using semantic handoff"),
            "{label}: {}",
            run.stderr
        );
        assert!(
            fixture
                .rpc_log()
                .contains(&format!("thread/delete {THREAD_ID}")),
            "{label} did not roll back the generated thread: {:?}",
            fixture.rpc_log()
        );
        assert!(
            !fixture.generated_rollout().exists(),
            "{label} left the generated thread behind"
        );

        let arguments = fixture
            .launch_arguments()
            .unwrap_or_else(|| panic!("{label} never launched Codex: {}", run.stderr));
        let [prompt] = arguments.as_slice() else {
            panic!("{label} launched Codex with {arguments:?}");
        };
        let Some(handoff) = prompt.split('`').nth(1) else {
            panic!("{label} prompt names no handoff: {prompt}");
        };
        assert!(
            PathBuf::from(handoff).starts_with(fixture.root.join("state").join("handoffs")),
            "{label}: {prompt}"
        );
        assert!(
            !prompt.contains(SOURCE_QUESTION),
            "{label} put history in argv"
        );
        let document = fs::read_to_string(fixture.capture.join("handoff.md"))
            .unwrap_or_else(|error| panic!("{label} launched Codex without a handoff: {error}"));
        assert!(
            document.contains(SOURCE_QUESTION),
            "{label} handoff omitted source history"
        );
        let cwd = fs::read_to_string(fixture.capture.join("launch-cwd")).expect("launch cwd");
        assert_eq!(Path::new(cwd.trim()), fixture.workspace, "{label}");
        assert_eq!(
            fixture.bound_session(),
            format!("claude:{SOURCE_ID}"),
            "{label} bound a rolled-back thread"
        );
        fixture.assert_nothing_left_behind(&label);
    }
}

#[test]
fn failed_rollback_refuses_to_launch_while_generated_thread_may_remain() {
    if !tools_available() {
        return;
    }
    for route in native_routes(&Route::ALL) {
        for (delete, reason) in [
            (
                "reject",
                "Codex app-server rejected thread/delete: synthetic delete failure",
            ),
            ("hang", "Codex app-server timed out or disconnected"),
        ] {
            let label = format!("{route:?} with {delete} rollback");
            let fixture = Fixture::new();
            let run = fixture.run(&route.args(), &broken("FAKE_CODEX_DELETE", delete));
            assert!(!run.status.success(), "{label} succeeded: {}", run.stderr);
            for expected in ["native import failed", "rollback also failed", reason] {
                assert!(run.stderr.contains(expected), "{label}: {}", run.stderr);
            }
            assert!(
                !run.stderr.contains("using semantic handoff"),
                "{label}: {}",
                run.stderr
            );
            assert!(run.elapsed < BOUNDED, "{label} took {:?}", run.elapsed);
            assert!(
                fixture
                    .rpc_log()
                    .contains(&format!("thread/delete {THREAD_ID}")),
                "{label}: {:?}",
                fixture.rpc_log()
            );
            assert!(
                fixture.generated_rollout().exists(),
                "{label}: failed rollback must strand the generated thread"
            );
            assert_eq!(
                fixture.launch_arguments(),
                None,
                "{label} launched Codex while the generated thread remains"
            );
            assert_eq!(
                fixture.bound_session(),
                format!("claude:{SOURCE_ID}"),
                "{label}"
            );
            fixture.assert_nothing_left_behind(&label);
        }
    }
}

#[test]
fn broken_app_server_ends_within_bound_without_partial_state() {
    if !tools_available() {
        return;
    }
    for (mode, reason) in [
        ("hang", "Codex app-server timed out or disconnected"),
        ("disconnect", "Codex app-server timed out or disconnected"),
        ("malformed", "invalid Codex app-server response"),
    ] {
        if SHIM_IMPORTS_CODEX {
            let label = format!("routed shim with {mode} import");
            let fixture = Fixture::new();
            let run = fixture.run(&Route::Shim.args(), &broken("FAKE_CODEX_IMPORT", mode));
            assert!(run.status.success(), "{label}: {}", run.stderr);
            for expected in [reason, "using semantic handoff"] {
                assert!(run.stderr.contains(expected), "{label}: {}", run.stderr);
            }
            assert!(
                fixture.launch_arguments().is_some(),
                "{label} did not launch a handoff"
            );
            fixture.assert_import_abandoned(&label, &run);
        }

        let label = format!("materialize-only resume with {mode} import");
        let fixture = Fixture::new();
        let run = fixture.run(
            &strings(&[
                "resume",
                &format!("claude:{SOURCE_ID}"),
                "--in",
                "codex",
                "--materialize-only",
            ]),
            &broken("FAKE_CODEX_IMPORT", mode),
        );
        assert!(!run.status.success(), "{label} succeeded: {}", run.stderr);
        for expected in ["Codex native import failed", reason] {
            assert!(run.stderr.contains(expected), "{label}: {}", run.stderr);
        }
        assert_eq!(fixture.launch_arguments(), None, "{label} launched Codex");
        fixture.assert_import_abandoned(&label, &run);
    }
}

#[test]
fn failed_in_server_verification_rollback_refuses_to_launch() {
    if !tools_available() {
        return;
    }
    for route in native_routes(&Route::ALL) {
        let label = format!("{route:?} with empty thread/read and rejected rollback");
        let fixture = Fixture::new();
        let run = fixture.run(
            &route.args(),
            &[
                ("FAKE_CODEX_READ", "empty"),
                ("FAKE_CODEX_DELETE", "reject"),
            ],
        );
        assert!(!run.status.success(), "{label} succeeded: {}", run.stderr);
        for expected in [
            "native import failed",
            "rollback also failed",
            "synthetic delete failure",
            "verifying visible Codex turns",
        ] {
            assert!(run.stderr.contains(expected), "{label}: {}", run.stderr);
        }
        assert!(
            !run.stderr.contains("using semantic handoff"),
            "{label}: {}",
            run.stderr
        );
        assert!(
            fixture
                .rpc_log()
                .contains(&format!("thread/delete {THREAD_ID}")),
            "{label}: {:?}",
            fixture.rpc_log()
        );
        assert!(
            fixture.generated_rollout().exists(),
            "{label}: failed rollback must strand the generated thread"
        );
        assert_eq!(
            fixture.launch_arguments(),
            None,
            "{label} launched Codex while the generated thread remains"
        );
        assert_eq!(
            fixture.bound_session(),
            format!("claude:{SOURCE_ID}"),
            "{label}"
        );
        fixture.assert_nothing_left_behind(&label);
    }
}

#[test]
fn interrupted_codex_import_rolls_back_and_exits_without_launching() {
    if !tools_available() {
        return;
    }
    let faithful = ("FAKE_CODEX_ROLLOUT", "faithful");
    let mut cases = vec![
        (Route::Resume, "materialized", vec![faithful]),
        (Route::Switch, "recorded", vec![faithful]),
        // Read-back fails and rolls back on its own; the interrupt must still block the handoff.
        (Route::Resume, "materialized", vec![]),
    ];
    if SHIM_IMPORTS_CODEX {
        cases.extend([
            (Route::Shim, "materialized", vec![faithful]),
            (Route::Shim, "recorded", vec![faithful]),
            (Route::Shim, "materialized", vec![]),
        ]);
    }
    for (route, checkpoint, environment) in cases {
        let label = format!("{route:?} interrupted at {checkpoint} with {environment:?}");
        let fixture = Fixture::new();
        let mut environment = environment;
        environment.push((INTERRUPT, checkpoint));
        let run = fixture.run(&route.args(), &environment);
        assert_eq!(run.status.code(), Some(130), "{label}: {}", run.stderr);
        if environment.contains(&faithful) {
            assert_eq!(
                rolled_back_target(&label, &run),
                format!("codex:{THREAD_ID}")
            );
            assert!(
                run.stderr
                    .contains(&format!("Imported and verified `codex:{THREAD_ID}`.")),
                "{label}: {}",
                run.stderr
            );
        } else {
            assert!(
                run.stderr
                    .contains("Interrupted; Codex native import did not complete"),
                "{label}: {}",
                run.stderr
            );
        }
        assert!(
            !run.stderr.contains("using semantic handoff"),
            "{label}: {}",
            run.stderr
        );
        assert!(
            fixture
                .rpc_log()
                .contains(&format!("thread/delete {THREAD_ID}")),
            "{label} did not roll back the generated thread: {:?}",
            fixture.rpc_log()
        );
        assert!(
            !fixture.generated_rollout().exists(),
            "{label} left the generated thread behind"
        );
        assert_eq!(fixture.launch_arguments(), None, "{label} launched Codex");
        assert_eq!(
            fixture.bound_session(),
            format!("claude:{SOURCE_ID}"),
            "{label}"
        );
        fixture.assert_nothing_left_behind(&label);
    }

    for route in native_routes(&[Route::Resume, Route::Shim]) {
        let label = format!("{route:?} interrupted with rejected rollback");
        let fixture = Fixture::new();
        let run = fixture.run(
            &route.args(),
            &[
                faithful,
                (INTERRUPT, "materialized"),
                ("FAKE_CODEX_DELETE", "reject"),
            ],
        );
        assert!(!run.status.success(), "{label} succeeded: {}", run.stderr);
        assert_ne!(run.status.code(), Some(130), "{label}: {}", run.stderr);
        for expected in ["rollback also failed", "synthetic delete failure"] {
            assert!(run.stderr.contains(expected), "{label}: {}", run.stderr);
        }
        assert!(
            fixture.generated_rollout().exists(),
            "{label}: failed rollback must strand the generated thread"
        );
        assert_eq!(fixture.launch_arguments(), None, "{label} launched Codex");
        fixture.assert_nothing_left_behind(&label);
    }
}

#[test]
fn console_ctrl_c_during_codex_import_rolls_back_the_surviving_thread() {
    if !tools_available() {
        return;
    }
    for route in native_routes(&[Route::Resume, Route::Shim]) {
        let label = format!("{route:?} interrupted from the console during the import request");
        let fixture = Fixture::new();
        let run = fixture.run_interrupted_import(&route.args());
        let diagnostics = format!(
            "{}{}",
            run.stderr,
            fs::read_to_string(fixture.capture.join("break-sender"))
                .map(|status| format!("\nconsole break sender exited with {status}"))
                .unwrap_or_default()
        );
        assert_eq!(run.status.code(), Some(130), "{label}: {diagnostics}");
        assert!(
            !fixture.capture.join("server-interrupted").exists(),
            "{label}: the app-server shared omni's process group and received the interrupt: {diagnostics}"
        );
        assert!(
            fixture
                .rpc_log()
                .contains(&format!("thread/read {THREAD_ID}")),
            "{label}: the app-server did not survive to verify the import: {:?}",
            fixture.rpc_log()
        );
        assert_eq!(
            rolled_back_target(&label, &run),
            format!("codex:{THREAD_ID}")
        );
        assert!(
            fixture
                .rpc_log()
                .contains(&format!("thread/delete {THREAD_ID}")),
            "{label} did not roll back the generated thread: {:?}",
            fixture.rpc_log()
        );
        assert!(
            !fixture.generated_rollout().exists(),
            "{label} left the generated thread behind"
        );
        assert_eq!(fixture.launch_arguments(), None, "{label} launched Codex");
        assert_eq!(
            fixture.bound_session(),
            format!("claude:{SOURCE_ID}"),
            "{label}"
        );
        fixture.assert_nothing_left_behind(&label);
    }
}

#[test]
fn interrupted_pi_import_rolls_back_and_exits_without_launching() {
    if !tools_available() {
        return;
    }
    let source = format!("claude:{SOURCE_ID}");
    let mut cases = vec![
        (strings(&["resume", &source, "--in", "pi"]), "materialized"),
        (
            strings(&["resume", &source, "--in", "pi", "--materialize-only"]),
            "planned",
        ),
        (strings(&["switch", "pi"]), "recorded"),
    ];
    // Windows leaves Pi cross-provider import undeclared, so shim routing into Pi uses semantic
    // handoff there.
    if cfg!(unix) {
        cases.push((
            strings(&["shim", "exec", "pi", "--", "--continue"]),
            "recorded",
        ));
    }
    for (args, checkpoint) in cases {
        let label = format!("`omni {}` interrupted at {checkpoint}", args.join(" "));
        let fixture = Fixture::new();
        let pi_root = fixture.root.join("pi");
        let run = fixture.run(
            &args,
            &[
                ("OMNI_PI_BIN", path_str(synthetic_pi())),
                ("PI_CODING_AGENT_DIR", path_str(&pi_root)),
                (INTERRUPT, checkpoint),
            ],
        );
        assert_eq!(run.status.code(), Some(130), "{label}: {}", run.stderr);
        let target = rolled_back_target(&label, &run);
        assert!(target.starts_with("pi:"), "{label}: {target}");
        assert!(
            run.stderr
                .contains(&format!("Imported and verified `{target}`.")),
            "{label}: {}",
            run.stderr
        );
        // On Windows, omni locks the Pi session root through its own lock file there.
        let remaining = walkdir::WalkDir::new(pi_root.join("sessions"))
            .into_iter()
            .map(|entry| entry.expect("Pi session entry"))
            .filter(|entry| {
                entry.file_type().is_file()
                    && entry.file_name() != std::ffi::OsStr::new(".omnisession.lock")
            })
            .map(walkdir::DirEntry::into_path)
            .collect::<Vec<_>>();
        assert!(
            remaining.is_empty(),
            "{label} left Pi sessions behind: {remaining:?}"
        );
        assert!(
            !fixture.capture.join("pi-launch-args").exists(),
            "{label} launched Pi"
        );
        assert_eq!(fixture.bound_session(), source, "{label}");
        fixture.assert_nothing_left_behind(&label);
    }
}

struct Run {
    status: ExitStatus,
    elapsed: Duration,
    stderr: String,
}

struct Fixture {
    _temporary: tempfile::TempDir,
    root: PathBuf,
    workspace: PathBuf,
    capture: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temporary = tempfile::tempdir().expect("temporary fixture");
        // Ordinary canonical paths compare equal to the workspace omni resolves on every platform.
        let root = omnis_core::canonicalize_path(temporary.path()).expect("canonical fixture");
        let fixture = Self {
            workspace: root.join("workspace"),
            capture: root.join("capture"),
            root,
            _temporary: temporary,
        };
        for directory in [
            "home",
            "tmp",
            "capture",
            "codex",
            "workspace",
            "claude/projects/synthetic",
        ] {
            fs::create_dir_all(fixture.root.join(directory)).expect("isolated fixture directory");
        }

        let records = [
            json!({
                "type": "user",
                "uuid": "44444444-4444-4444-8444-444444444444",
                "sessionId": SOURCE_ID,
                "timestamp": "2026-01-01T00:00:00Z",
                "cwd": fixture.workspace,
                "message": {"role": "user", "content": SOURCE_QUESTION}
            }),
            json!({
                "type": "assistant",
                "uuid": "55555555-5555-4555-8555-555555555555",
                "parentUuid": "44444444-4444-4444-8444-444444444444",
                "sessionId": SOURCE_ID,
                "timestamp": "2026-01-01T00:00:01Z",
                "cwd": fixture.workspace,
                "message": {
                    "role": "assistant",
                    "content": [{"type": "text", "text": "Synthetic opening answer"}]
                }
            }),
        ];
        fs::write(
            fixture
                .root
                .join(format!("claude/projects/synthetic/{SOURCE_ID}.jsonl")),
            records.map(|record| record.to_string() + "\n").concat(),
        )
        .expect("write synthetic Claude history");
        fs::write(
            fixture.root.join("claude/history.jsonl"),
            json!({"sessionId": SOURCE_ID, "project": fixture.workspace, "timestamp": 1_767_225_600_000_i64})
                .to_string(),
        )
        .expect("write synthetic Claude index");

        let output = fixture
            .command()
            .args(["task", "start", "fallback", "--from"])
            .arg(format!("claude:{SOURCE_ID}"))
            .output()
            .expect("bind synthetic source");
        assert!(
            output.status.success(),
            "binding failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        fixture
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_omni"));
        #[cfg(unix)]
        command.env_clear().env("PATH", "/usr/bin:/bin");
        // Windows programs need their system environment, so only OmniSession overrides go.
        #[cfg(windows)]
        {
            for (name, _) in env::vars_os() {
                if name
                    .to_string_lossy()
                    .to_ascii_uppercase()
                    .starts_with("OMNI_")
                {
                    command.env_remove(name);
                }
            }
            command.env("PATH", tool_path().expect("Windows fixture tools"));
        }
        let temporary = self.root.join("tmp");
        command
            .current_dir(&self.workspace)
            .env("HOME", self.root.join("home"))
            .env("USERPROFILE", self.root.join("home"))
            .env("TMPDIR", &temporary)
            .env("TEMP", &temporary)
            .env("TMP", &temporary)
            .env("XDG_CONFIG_HOME", self.root.join("xdg/config"))
            .env("XDG_DATA_HOME", self.root.join("xdg/data"))
            .env("XDG_STATE_HOME", self.root.join("xdg/state"))
            .env("XDG_CACHE_HOME", self.root.join("xdg/cache"))
            .env("OMNISESSION_HOME", self.root.join("state"))
            .env("CLAUDE_CONFIG_DIR", self.root.join("claude"))
            .env("CODEX_HOME", self.root.join("codex"))
            .env("GROK_HOME", self.root.join("grok"))
            .env("HERMES_HOME", self.root.join("hermes"))
            .env("PI_CODING_AGENT_DIR", self.root.join("pi"))
            .env("CURSOR_AGENT_HOME", self.root.join("cursor"))
            .env("ANTIGRAVITY_CLI_HOME", self.root.join("antigravity-cli"))
            .env("ANTIGRAVITY_IDE_HOME", self.root.join("antigravity-ide"))
            .env("OMNI_NO_UPDATE_CHECK", "1")
            .env("OMNI_CODEX_BIN", synthetic_codex())
            .env("FAKE_CODEX_CAPTURE", &self.capture)
            .env("FAKE_CODEX_THREAD", THREAD_ID)
            .env("FAKE_PI_CAPTURE", &self.capture);
        for variable in [
            "OMNI_CLAUDE_BIN",
            "OMNI_OPENCODE_BIN",
            "OMNI_GROK_BIN",
            "OMNI_HERMES_BIN",
            "OMNI_ANTIGRAVITY_BIN",
            "OMNI_PI_BIN",
            "OMNI_CURSOR_AGENT_BIN",
            "OMNI_CURSOR_IDE_BIN",
        ] {
            command.env(variable, self.root.join("missing").join(variable));
        }
        command
    }

    fn run(&self, args: &[String], environment: &[(&str, &str)]) -> Run {
        self.run_during(args, environment, |_| {})
    }

    // Runs omni as a process group leader, so a signal or console event can reach its group like a
    // terminal Ctrl+C.
    fn run_during(
        &self,
        args: &[String],
        environment: &[(&str, &str)],
        during: impl FnOnce(&Child),
    ) -> Run {
        let started = Instant::now();
        let mut command = self.command();
        command
            .args(args)
            .envs(environment.iter().copied())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        command.process_group(0);
        // A hidden console of its own keeps console events away from this test.
        #[cfg(windows)]
        command.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
        let mut child = command.spawn().expect("launch omni");
        let stdout = drain(child.stdout.take().expect("omni stdout"));
        let stderr = drain(child.stderr.take().expect("omni stderr"));
        during(&child);
        let Some(status) = child.wait_timeout(WATCHDOG).expect("wait for omni") else {
            let _ = child.kill();
            let _ = child.wait();
            panic!("`omni {}` exceeded {WATCHDOG:?}", args.join(" "));
        };
        let elapsed = started.elapsed();
        stdout.join().expect("omni stdout reader");
        Run {
            status,
            elapsed,
            stderr: String::from_utf8_lossy(&stderr.join().expect("omni stderr reader"))
                .into_owned(),
        }
    }

    // A terminal Ctrl+C signals the whole foreground process group, not just omni.
    #[cfg(unix)]
    fn run_interrupted_import(&self, args: &[String]) -> Run {
        self.run_during(
            args,
            &[
                ("FAKE_CODEX_ROLLOUT", "faithful"),
                ("FAKE_CODEX_IMPORT", "wait"),
            ],
            |omni| {
                wait_for_file(&self.capture.join("importing"));
                rustix::process::kill_process_group(
                    rustix::process::Pid::from_child(omni),
                    rustix::process::Signal::INT,
                )
                .expect("signal omni's process group");
                fs::write(self.capture.join("release"), "").expect("release synthetic import");
            },
        )
    }

    // The synthetic app-server sends Ctrl+Break to omni's console process group mid-import.
    #[cfg(windows)]
    fn run_interrupted_import(&self, args: &[String]) -> Run {
        self.run(
            args,
            &[
                ("FAKE_CODEX_ROLLOUT", "faithful"),
                ("FAKE_CODEX_IMPORT", "break"),
            ],
        )
    }

    fn generated_rollout(&self) -> PathBuf {
        self.root.join(format!(
            "codex/sessions/2026/01/01/rollout-2026-01-01T00-00-00-{THREAD_ID}.jsonl"
        ))
    }

    fn rpc_log(&self) -> Vec<String> {
        fs::read_to_string(self.capture.join("rpc.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn launch_arguments(&self) -> Option<Vec<String>> {
        let bytes = fs::read(self.capture.join("launch-args")).ok()?;
        Some(
            bytes
                .split(|byte| *byte == 0)
                .filter(|argument| !argument.is_empty())
                .map(|argument| String::from_utf8_lossy(argument).into_owned())
                .collect(),
        )
    }

    fn bound_session(&self) -> String {
        let output = self
            .command()
            .args(["--json", "task", "status"])
            .output()
            .expect("read task status");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let status: Value = serde_json::from_slice(&output.stdout).expect("task status JSON");
        format!(
            "{}:{}",
            status["session"]["provider"].as_str().unwrap_or_default(),
            status["session"]["id"].as_str().unwrap_or_default()
        )
    }

    fn assert_import_abandoned(&self, label: &str, run: &Run) {
        assert!(run.elapsed < BOUNDED, "{label} took {:?}", run.elapsed);
        assert_eq!(
            self.rpc_log().last().map(String::as_str),
            Some("externalAgentConfig/import"),
            "{label}: app-server must break during the import request"
        );
        assert!(
            !self.root.join("codex/sessions").exists(),
            "{label} left a Codex session behind"
        );
        assert_eq!(
            self.bound_session(),
            format!("claude:{SOURCE_ID}"),
            "{label}"
        );
        self.assert_nothing_left_behind(label);
    }

    fn assert_nothing_left_behind(&self, label: &str) {
        let handoffs = self.root.join("state/handoffs");
        if handoffs.exists() {
            let remaining = fs::read_dir(&handoffs).expect("handoff directory").count();
            assert_eq!(remaining, 0, "{label} left a private handoff behind");
        }
        let temporary = fs::read_dir(self.root.join("tmp"))
            .expect("isolated temporary directory")
            .map(|entry| entry.expect("temporary directory entry").file_name())
            .collect::<Vec<_>>();
        assert!(
            temporary.is_empty(),
            "{label} left isolated import sources behind: {temporary:?}"
        );
        let pids = fs::read_to_string(self.capture.join("server-pids")).unwrap_or_default();
        for pid in pids.lines() {
            assert!(
                !process_alive(pid),
                "{label}: synthetic app-server {pid} outlived omni"
            );
        }
    }
}

#[cfg(unix)]
fn process_alive(pid: &str) -> bool {
    Command::new("kill")
        .args(["-0", pid])
        .stderr(Stdio::null())
        .status()
        .expect("probe synthetic app-server")
        .success()
}

#[cfg(windows)]
fn process_alive(pid: &str) -> bool {
    let system = PathBuf::from(env::var_os("SystemRoot").expect("SystemRoot")).join("System32");
    let output = Command::new(system.join("tasklist.exe"))
        .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
        .stdin(Stdio::null())
        .output()
        .expect("probe synthetic app-server");
    String::from_utf8_lossy(&output.stdout).contains(&format!("\"{pid}\""))
}

/// Whether the tools the synthetic providers need are available.
#[cfg(unix)]
const fn tools_available() -> bool {
    true
}

#[cfg(windows)]
fn tools_available() -> bool {
    tool_path().is_some()
}

/// `PATH` holding Node for the synthetic providers, Git for workspace capture, and System32.
///
/// CI images ship Node and Git, so a missing tool fails there and skips elsewhere.
#[cfg(windows)]
fn tool_path() -> Option<&'static OsString> {
    static PATH: OnceLock<Option<OsString>> = OnceLock::new();
    PATH.get_or_init(|| {
        let directory = |executable: &str| {
            env::var_os("PATH").and_then(|path| {
                env::split_paths(&path).find(|directory| directory.join(executable).is_file())
            })
        };
        let (Some(node), Some(git)) = (directory("node.exe"), directory("git.exe")) else {
            assert!(
                env::var_os("CI").is_none(),
                "Windows native import tests require node.exe and git.exe on PATH"
            );
            eprintln!("skipping Windows native import test: node.exe or git.exe is not on PATH");
            return None;
        };
        let system = PathBuf::from(env::var_os("SystemRoot").expect("SystemRoot")).join("System32");
        Some(env::join_paths([node, git, system]).expect("fixture PATH"))
    })
    .as_ref()
}

fn synthetic_codex() -> &'static Path {
    static CODEX: OnceLock<PathBuf> = OnceLock::new();
    CODEX.get_or_init(|| install_synthetic("codex", FAKE_CODEX))
}

fn synthetic_pi() -> &'static Path {
    static PI: OnceLock<PathBuf> = OnceLock::new();
    PI.get_or_init(|| install_synthetic("pi", FAKE_PI))
}

fn synthetic_directory() -> PathBuf {
    let directory = Path::new(env!("CARGO_TARGET_TMPDIR")).join("native_import_fallback");
    fs::create_dir_all(&directory).expect("synthetic provider directory");
    directory
}

// Replaces `path` only when its content changed, so an unchanged script is never rewritten.
fn install_file(path: &Path, content: &str) {
    if fs::read_to_string(path).ok().as_deref() == Some(content) {
        return;
    }
    let directory = path.parent().expect("synthetic provider parent");
    let staged = tempfile::NamedTempFile::new_in(directory).expect("stage synthetic provider");
    fs::write(staged.path(), content).expect("write synthetic provider");
    #[cfg(unix)]
    fs::set_permissions(staged.path(), fs::Permissions::from_mode(0o700))
        .expect("make synthetic provider executable");
    staged.persist(path).expect("install synthetic provider");
}

// Reuses one unchanged script. Endpoint security can hold a new executable's first exec for
// seconds, which would trip omni's 5-second version probe.
#[cfg(unix)]
fn install_synthetic(name: &str, script: &str) -> PathBuf {
    let path = synthetic_directory().join(name);
    install_file(&path, script);
    // Absorb any first-exec scan before omni's timeouts start.
    let output = Command::new(&path)
        .arg("--version")
        .output()
        .expect("prime synthetic provider");
    assert!(
        output.status.success(),
        "synthetic {name} version probe failed"
    );
    path
}

/// Writes a Node package script behind the command shim npm generates for it.
#[cfg(windows)]
fn install_synthetic(name: &str, script: &str) -> PathBuf {
    let npm = synthetic_directory().join("npm");
    let package = npm
        .join("node_modules")
        .join("@omnisession-test")
        .join(name);
    fs::create_dir_all(&package).expect("synthetic provider package");
    install_file(&package.join("cli.js"), script);
    let shim = npm.join(format!("{name}.cmd"));
    install_file(
        &shim,
        &format!(
            "{NPM_COMMAND_SHIM_PREFIX}node_modules\\@omnisession-test\\{name}\\cli.js\" %*\r\n"
        ),
    );
    shim
}

// Returns the session named by the single interrupt report.
fn rolled_back_target(label: &str, run: &Run) -> String {
    let reports = run
        .stderr
        .lines()
        .filter_map(|line| {
            line.strip_prefix("Interrupted; rolled back ")?
                .strip_suffix(". Source session was not changed.")
        })
        .collect::<Vec<_>>();
    let [target] = reports.as_slice() else {
        panic!("{label} did not report one rollback: {}", run.stderr);
    };
    (*target).to_owned()
}

fn path_str(path: &Path) -> &str {
    path.to_str().expect("UTF-8 fixture path")
}

#[cfg(unix)]
fn wait_for_file(path: &Path) {
    let deadline = Instant::now() + BOUNDED;
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "{} never appeared",
            path.display()
        );
        thread::sleep(Duration::from_millis(10));
    }
}

// Breaks one app-server step. Hangs also shorten the RPC timeout so the command ends quickly.
fn broken(step: &'static str, mode: &'static str) -> Vec<(&'static str, &'static str)> {
    let mut environment = vec![(step, mode)];
    if mode == "hang" {
        environment.push(("OMNI_TEST_CODEX_RPC_TIMEOUT_MS", HANG_RPC_TIMEOUT_MS));
    }
    environment
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

fn drain(mut pipe: impl Read + Send + 'static) -> JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        pipe.read_to_end(&mut bytes).expect("read omni output");
        bytes
    })
}
