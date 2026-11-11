#![cfg(any(unix, windows))]

//! Drives native import failures and interrupts end to end through synthetic `codex` and `pi`
//! providers: shell scripts on Unix, and Node scripts behind npm command shims on Windows.

#[cfg(unix)]
use std::os::unix::{fs::PermissionsExt, process::CommandExt};
#[cfg(windows)]
use std::{env, ffi::OsString, os::windows::process::CommandExt};
use std::{
    fs,
    io::{Read, Write},
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
// A cold `node` start on a Windows runner can take over two seconds to read its first request.
const HANG_RPC_TIMEOUT_MS: &str = "4000";
// Below the production RPC timeout, so a hang that ignores the shortened timeout fails.
const BOUNDED: Duration = Duration::from_secs(15);
const WATCHDOG: Duration = Duration::from_secs(90);
// Injects a Ctrl+C at one import checkpoint instead of racing a real signal.
const INTERRUPT: &str = "OMNI_TEST_IMPORT_INTERRUPT";
// Bounds each step of a console interrupt test. Omni waits up to 60 seconds for a held import.
const INTERRUPT_STEP: Duration = Duration::from_secs(40);

// Synthetic Codex CLI. It passes the version gate, answers just enough app-server JSON-RPC to import
// a thread whose visible turns echo the imported history, and records any provider launch. The
// rollout it writes diverges from that history, so OmniSession's store read-back fails, unless
// `FAKE_CODEX_ROLLOUT=faithful` writes the imported history instead. `FAKE_CODEX_IMPORT` breaks the
// import request, `FAKE_CODEX_READ=empty` breaks in-server turn verification, and
// `FAKE_CODEX_DELETE` breaks rollback. `FAKE_CODEX_IMPORT=hold` publishes the thread, answers the
// import request, and holds its completion notice until `release`, so omni stays mid-import while a
// test interrupts it. An app-server that receives SIGINT records `server-interrupted` and exits,
// like a provider killed by a terminal Ctrl+C.
#[cfg(unix)]
const FAKE_CODEX: &str = r#"#!/bin/sh
capture=$FAKE_CODEX_CAPTURE
if [ "$1" = "--version" ]; then
    printf 'codex-cli 0.146.0\n'
    exit 0
fi
if [ "$1" = "--help" ]; then
    printf 'Usage: codex [OPTIONS] [PROMPT]\n%s\n' "$FAKE_CODEX_HELP"
    exit 0
fi
if [ "$1" != "app-server" ]; then
    if [ "$FAKE_CODEX_LOG_LAUNCH" = 1 ]; then printf 'launch\n' >> "$capture/rpc.log"; fi
    for prompt do printf '%s\000' "$prompt" >> "$capture/launch-args"; done
    pwd > "$capture/launch-cwd"
    handoff=${prompt#*\`}
    handoff=${handoff%%\`*}
    /bin/cp "$handoff" "$capture/handoff.md" 2>/dev/null || exit 92
    exit 0
fi
[ -d "$CODEX_HOME" ] || exit 1
printf '%s\n' "$$" >> "$capture/server-pids"
trap ': > "$capture/server-interrupted"; exit 130' INT
rollout=$CODEX_HOME/sessions/2026/01/01/rollout-2026-01-01T00-00-00-$FAKE_CODEX_THREAD.jsonl
notify_sessions() {
    printf '{"method":"externalAgentConfig/import/%s","params":{"importId":"%s","itemTypeResults":[{"itemType":"SESSIONS","successes":[%s],"failures":[%s]}]}}\n' "$1" "$2" "$3" "$4"
}
accept_import() {
    printf '{"id":%s,"result":{"importId":"synthetic-import"}}\n' "$id"
    notify_sessions progress synthetic-import '' ''
}
complete_sessions() {
    notify_sessions progress "${FAKE_CODEX_PROGRESS_ID:-synthetic-import}" "$1" "$2"
    case "$FAKE_CODEX_COMPLETION" in
    timeout) return ;;
    disconnect) exit 1 ;;
    conflict-progress|conflict-completed)
        conflicting='{"itemType":"SESSIONS","source":"'"$source"'","target":"44444444-4444-4444-8444-444444444444"}'
        notify_sessions "${FAKE_CODEX_COMPLETION#conflict-}" synthetic-import "$conflicting" ''
        if [ "$FAKE_CODEX_COMPLETION" = conflict-progress ]; then exit 1; fi
        return
        ;;
    esac
    notify_sessions completed synthetic-import "$1" "$2"
}
send_progress() {
    : > "$capture/progress-started"
    for tick in 1 2 3 4 5 6; do
        printf '{"method":"synthetic/progress","params":{}}\n'
        /bin/sleep 1
    done
    : > "$capture/progress-finished"
}
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
        reject) printf '{"id":%s,"error":{"code":-32601,"message":"import unavailable"}}\n' "$id"; continue ;;
        hang) continue ;;
        disconnect) exit 1 ;;
        malformed) printf '{"id":%s,"result":{"importId":\n' "$id"; continue ;;
        esac
        source=${line#*'"path":"'}
        source=${source%%'"'*}
        case "$FAKE_CODEX_IMPORT" in
        prepare|prepare-wrong-source)
            if [ "$FAKE_CODEX_IMPORT" = prepare-wrong-source ]; then source=$source.other; fi
            stage=${FAKE_CODEX_FAILURE_STAGE:-session_prepare}
            subtype=${FAKE_CODEX_FAILURE_SUBTYPE:-failed_to_prepare_session}
            failure_source='"'"$source"'"'
            if [ "$stage" = session_permit ]; then failure_source=null; fi
            accept_import
            complete_sessions '' '{"itemType":"SESSIONS","source":'"$failure_source"',"failureStage":"'"$stage"'","subErrorType":"'"$subtype"'","message":"synthetic preparation failure"}'
            continue
            ;;
        esac
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
        if [ "$FAKE_CODEX_IMPORT" = late-reply ]; then send_progress; fi
        accept_import
        if [ "$FAKE_CODEX_IMPORT" = late-completion ]; then send_progress; fi
        if [ "$FAKE_CODEX_IMPORT" = lost-completion ]; then exit 1; fi
        if [ "$FAKE_CODEX_IMPORT" = hold ]; then
            : > "$capture/importing"
            waited=0
            while [ ! -e "$capture/release" ] && [ "$waited" -lt 800 ]; do
                /bin/sleep 0.05
                waited=$((waited + 1))
            done
        fi
        case "$FAKE_CODEX_IMPORT" in ledger-wrong-source|success-wrong-source) source=$source.other ;; esac
        success='{"itemType":"SESSIONS","source":"'"$source"'","target":"'"$FAKE_CODEX_THREAD"'"}'
        case "$FAKE_CODEX_IMPORT" in
        failed-completion) complete_sessions '' '{"itemType":"SESSIONS","source":"'"$source"'","failureStage":"session_persist","subErrorType":"failed_to_shutdown_thread","message":"failed to shutdown imported session"}' ;;
        ledger|ledger-wrong-source)
            complete_sessions "$success" '{"itemType":"SESSIONS","source":null,"failureStage":"session_ledger_update","subErrorType":"failed_to_update_session_ledger","message":"synthetic ledger failure"}'
            ;;
        *) complete_sessions "$success" '' ;;
        esac
        ;;
    thread/read)
        case "$FAKE_CODEX_READ" in
        defer) pending_read=$id ;;
        empty) printf '{"id":%s,"result":{"thread":{"turns":[]}}}\n' "$id" ;;
        *) printf '{"id":%s,"result":{"thread":{"modelProvider":"openai","turns":[{"items":[%s]}]}}}\n' "$id" "$(/bin/cat "$capture/turn-items")" ;;
        esac
        ;;
    thread/delete)
        if [ -n "$pending_read" ]; then printf '{"id":%s,"result":{"thread":{"turns":[]}}}\n' "$pending_read"; fi
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

// The Unix synthetic Codex CLI as a Node script. An app-server that receives Ctrl+C or Ctrl+Break
// records `server-interrupted` and exits, and a running app-server holds a named pipe named after
// `FAKE_CODEX_LIVENESS` and its PID.
#[cfg(windows)]
const FAKE_CODEX: &str = r#"#!/usr/bin/env node
"use strict";
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
if (args[0] === "--help") {
  fs.writeSync(1, `Usage: codex [OPTIONS] [PROMPT]\n${env.FAKE_CODEX_HELP || ""}\n`);
  process.exit(0);
}
if (args[0] !== "app-server") {
  if (env.FAKE_CODEX_LOG_LAUNCH === "1") append("rpc.log", "launch\n");
  for (const argument of args) append("launch-args", `${argument}\0`);
  fs.writeFileSync(path.join(capture, "launch-cwd"), process.cwd());
  try {
    fs.copyFileSync(args[args.length - 1].split("`")[1], path.join(capture, "handoff.md"));
  } catch {
    process.exit(92);
  }
  process.exit(0);
}

if (!fs.existsSync(env.CODEX_HOME)) process.exit(1);
append("server-pids", `${process.pid}\n`);
// Windows closes this pipe while terminating the process, before a wait on the process returns.
const liveness = require("net").createServer();
liveness.on("error", () => {});
liveness.listen(`\\\\.\\pipe\\${env.FAKE_CODEX_LIVENESS}-${process.pid}`);
liveness.unref();
for (const signal of ["SIGINT", "SIGBREAK"]) {
  process.on(signal, () => {
    fs.writeFileSync(path.join(capture, "server-interrupted"), "");
    process.exit(130);
  });
}
const rollout = path.join(env.CODEX_HOME, "sessions", "2026", "01", "01",
  `rollout-2026-01-01T00-00-00-${env.FAKE_CODEX_THREAD}.jsonl`);
const send = (message) => fs.writeSync(1, `${JSON.stringify(message)}\n`);
const notifySessions = (event, importId, successes, failures) => send({
  method: `externalAgentConfig/import/${event}`,
  params: { importId, itemTypeResults: [{ itemType: "SESSIONS", successes, failures }] },
});
function acceptImport(id) {
  send({ id, result: { importId: "synthetic-import" } });
  notifySessions("progress", "synthetic-import", [], []);
}
function completeSessions(successes, failures) {
  notifySessions("progress", env.FAKE_CODEX_PROGRESS_ID || "synthetic-import", successes, failures);
  const ending = env.FAKE_CODEX_COMPLETION;
  if (ending === "timeout") return;
  if (ending === "disconnect") process.exit(1);
  if (ending === "conflict-progress" || ending === "conflict-completed") {
    notifySessions(ending.split("-")[1], "synthetic-import",
      [{ ...successes[0], target: "44444444-4444-4444-8444-444444444444" }], []);
    if (ending === "conflict-progress") process.exit(1);
    return;
  }
  notifySessions("completed", "synthetic-import", successes, failures);
}
let pendingRead;
async function sendProgress() {
  fs.writeFileSync(path.join(capture, "progress-started"), "");
  for (let tick = 0; tick < 6; tick += 1) {
    send({ method: "synthetic/progress", params: {} });
    await new Promise((resolve) => setTimeout(resolve, 1000));
  }
  fs.writeFileSync(path.join(capture, "progress-finished"), "");
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
        if (mode === "reject") {
          send({ id: request.id, error: { code: -32601, message: "import unavailable" } });
          break;
        }
        if (mode === "hang") break;
        if (mode === "disconnect") process.exit(1);
        if (mode === "malformed") {
          fs.writeSync(1, `{"id":${request.id},"result":{"importId":\n`);
          break;
        }
        const items = [];
        const history = [];
        const source = params.migrationItems[0].details.sessions[0].path;
        if (mode === "prepare" || mode === "prepare-wrong-source") {
          acceptImport(request.id);
          const stage = env.FAKE_CODEX_FAILURE_STAGE || "session_prepare";
          const failureSource = mode === "prepare" ? source : source + ".other";
          completeSessions([], [{ itemType: "SESSIONS", source: stage === "session_permit" ? null : failureSource,
            failureStage: stage, subErrorType: env.FAKE_CODEX_FAILURE_SUBTYPE || "failed_to_prepare_session",
            message: "synthetic preparation failure" }]);
          break;
        }
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
        if (mode === "late-reply") await sendProgress();
        acceptImport(request.id);
        if (mode === "late-completion") await sendProgress();
        if (mode === "lost-completion") process.exit(1);
        if (mode === "hold") {
          fs.writeFileSync(path.join(capture, "importing"), "");
          const release = path.join(capture, "release");
          for (let waited = 0; !fs.existsSync(release) && waited < 2000; waited += 1) {
            await new Promise((resolve) => setTimeout(resolve, 20));
          }
        }
        const reportedSource = ["ledger-wrong-source", "success-wrong-source"].includes(mode) ? source + ".other" : source;
        const success = { itemType: "SESSIONS", source: reportedSource, target: env.FAKE_CODEX_THREAD };
        if (mode === "failed-completion") {
          completeSessions([], [{ itemType: "SESSIONS", source, failureStage: "session_persist",
            subErrorType: "failed_to_shutdown_thread", message: "failed to shutdown imported session" }]);
        } else if (mode === "ledger" || mode === "ledger-wrong-source") {
          completeSessions([success], [{ itemType: "SESSIONS", source: null, failureStage: "session_ledger_update",
            subErrorType: "failed_to_update_session_ledger", message: "synthetic ledger failure" }]);
        } else {
          completeSessions([success], []);
        }
        break;
      }
      case "thread/read": {
        if (env.FAKE_CODEX_READ === "defer") {
          pendingRead = request.id;
          break;
        }
        const turns = env.FAKE_CODEX_READ === "empty"
          ? []
          : [{ items: JSON.parse(fs.readFileSync(path.join(capture, "turn-items"), "utf8")) }];
        send({ id: request.id, result: { thread: { modelProvider: "openai", turns } } });
        break;
      }
      case "thread/delete":
        if (pendingRead !== undefined) send({ id: pendingRead, result: { thread: { turns: [] } } });
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
    printf '%s\n' "${FAKE_PI_VERSION:-0.82.0}"
    exit 0
fi
if [ "$1" = "--help" ]; then
    printf '%s\n' '      --auto-approve                    Auto-approve all tool calls (skip approval prompts)' \
                  '      --approval-mode=<value>           Override tools.approvalMode for this session (always-ask|write|yolo)'
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

/// PowerShell sender that types Ctrl+Break at omni's console, driven through marker files.
///
/// It compiles its console API bindings first, because that takes seconds, then attaches to omni's
/// console once `omni-pid` appears. On `send` it lists the processes on that console, sends
/// Ctrl+Break to all of them, and reports `sent`.
#[cfg(windows)]
const CONSOLE_BREAK_SCRIPT: &str = r#"param([Parameter(Mandatory = $true)][string]$Directory)
$ErrorActionPreference = 'Stop'

Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
using System.Text;

public static class OmniConsoleBreak
{
    public delegate bool HandlerRoutine(uint controlType);

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool FreeConsole();

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool AttachConsole(uint processId);

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool SetConsoleCtrlHandler(HandlerRoutine handler, bool add);

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool GenerateConsoleCtrlEvent(uint controlEvent, uint processGroupId);

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern uint GetConsoleProcessList(uint[] processIds, uint count);

    // Attached to omni's console, the sender receives its own Ctrl+Break and must survive it.
    private static readonly HandlerRoutine Handler = OnControl;

    private static bool OnControl(uint controlType)
    {
        return true;
    }

    public static void Attach(uint processId)
    {
        FreeConsole();
        if (!AttachConsole(processId))
        {
            throw new InvalidOperationException("AttachConsole failed: " + Marshal.GetLastWin32Error());
        }
        if (!SetConsoleCtrlHandler(Handler, true))
        {
            throw new InvalidOperationException("SetConsoleCtrlHandler failed: " + Marshal.GetLastWin32Error());
        }
    }

    public static string SendBreak()
    {
        uint[] processIds = new uint[64];
        uint count = GetConsoleProcessList(processIds, (uint)processIds.Length);
        if (count == 0 || count > processIds.Length)
        {
            throw new InvalidOperationException("GetConsoleProcessList failed: " + Marshal.GetLastWin32Error());
        }
        StringBuilder console = new StringBuilder();
        for (uint index = 0; index < count; index++)
        {
            console.Append(processIds[index]).Append('\n');
        }
        if (!GenerateConsoleCtrlEvent(1, 0))
        {
            throw new InvalidOperationException("GenerateConsoleCtrlEvent failed: " + Marshal.GetLastWin32Error());
        }
        return console.ToString();
    }
}
'@

function Wait-Marker([string]$Name) {
    $path = Join-Path $Directory $Name
    $deadline = [DateTime]::UtcNow.AddSeconds(120)
    while (-not (Test-Path -LiteralPath $path)) {
        if ([DateTime]::UtcNow -gt $deadline) { throw "$Name never appeared" }
        Start-Sleep -Milliseconds 10
    }
    return $path
}

function Write-Marker([string]$Name, [string]$Content) {
    $staged = Join-Path $Directory ($Name + '.staged')
    [System.IO.File]::WriteAllText($staged, $Content)
    Move-Item -LiteralPath $staged -Destination (Join-Path $Directory $Name)
}

Write-Marker 'ready' ''
$omni = [uint32]([System.IO.File]::ReadAllText((Wait-Marker 'omni-pid')).Trim())
[OmniConsoleBreak]::Attach($omni)
Write-Marker 'attached' ''
[void](Wait-Marker 'send')
Write-Marker 'sent' ([OmniConsoleBreak]::SendBreak())
"#;

#[derive(Clone, Copy, Debug)]
enum Route {
    Resume,
    Switch,
}

impl Route {
    const ALL: [Self; 2] = [Self::Resume, Self::Switch];

    fn args(self) -> Vec<String> {
        match self {
            Self::Resume => strings(&["resume", &format!("claude:{SOURCE_ID}"), "--in", "codex"]),
            Self::Switch => strings(&["switch", "codex"]),
        }
    }
}

#[test]
fn partial_codex_fork_keeps_a_private_file_with_the_omitted_text() {
    if !tools_available() {
        return;
    }
    let fixture = Fixture::new();
    let args = strings(&[
        "fork",
        &format!("claude:{SOURCE_ID}"),
        "--in",
        "codex",
        "--materialize-only",
    ]);
    let faithful = [("FAKE_CODEX_ROLLOUT", "faithful")];
    let clean = fixture.run(&args, &faithful);
    assert!(clean.status.success(), "{}", clean.stderr);
    fixture.assert_nothing_left_behind("complete native fork");
    assert!(
        !fs::read_to_string(fixture.generated_rollout())
            .unwrap()
            .contains("[OmniSession recovery file]")
    );

    let fixture = Fixture::new();
    let missing = "Read the synthetic migration plan before changing the schema.";
    let credential = "fake-private-value";
    let message = format!("{}{missing} password={credential}", "word ".repeat(26_000));
    let record = json!({
        "type": "assistant",
        "sessionId": SOURCE_ID,
        "cwd": fixture.workspace,
        "message": {"role": "assistant", "content": message}
    });
    let source = fixture
        .root
        .join(format!("claude/projects/synthetic/{SOURCE_ID}.jsonl"));
    let mut file = fs::OpenOptions::new().append(true).open(&source).unwrap();
    writeln!(file, "{record}").unwrap();
    drop(file);

    let mut preview = args.clone();
    preview.push("--dry-run".to_owned());
    let planned = fixture.run(&preview, &faithful);
    assert!(planned.status.success(), "{}", planned.stderr);
    assert!(!fixture.generated_rollout().exists());
    fixture.assert_nothing_left_behind("partial native fork preview");

    let failed = fixture.run(&args, &[]);
    assert!(!failed.status.success(), "divergent read-back must fail");
    fixture.assert_nothing_left_behind("rolled-back partial native fork");

    let assert_recovery_file = || {
        let events = fs::read_to_string(fixture.generated_rollout()).unwrap();
        assert!(!events.contains(missing));
        let path = events
            .lines()
            .find_map(|line| {
                let event: Value = serde_json::from_str(line).unwrap();
                let payload = &event["payload"];
                if payload["role"] != "assistant" {
                    return None;
                }
                let text = payload["content"][0]["text"].as_str()?;
                text.strip_prefix("[OmniSession recovery file]")?
                    .lines()
                    .last()
                    .map(PathBuf::from)
            })
            .expect("the receiving agent needs the recovery path in its history");
        assert!(path.starts_with(fixture.root.join("tmp")));
        let document = fs::read_to_string(&path).unwrap();
        assert!(document.contains(missing));
        assert!(!document.contains(credential));
        #[cfg(unix)]
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o077, 0);
    };
    let run = fixture.run(&args, &faithful);
    assert!(run.status.success(), "{}", run.stderr);
    assert_recovery_file();

    for mode in ["lost-completion", "failed-completion"] {
        let run = fixture.run(
            &Route::Resume.args(),
            &[
                ("FAKE_CODEX_ROLLOUT", "faithful"),
                ("FAKE_CODEX_IMPORT", mode),
            ],
        );
        assert!(
            !run.status.success(),
            "{mode} must stop the transfer: {}",
            run.stderr
        );
        assert!(
            run.stderr.contains("may have created a thread"),
            "{}",
            run.stderr
        );
        assert_eq!(
            fixture.launch_arguments(),
            None,
            "an imported thread still exists"
        );
        assert_recovery_file();
    }
}

#[test]
fn codex_provider_mismatch_rolls_back_before_launch() {
    if !tools_available() {
        return;
    }
    let fixture = Fixture::new();
    fs::write(
        fixture.root.join("codex/work.config.toml"),
        "model_provider = 'openai'\n",
    )
    .unwrap();
    let profile = fixture.root.join("state/profiles.toml");
    fs::write(
        &profile,
        "[profiles.local]\nagent = 'codex'\nargs = ['--config=model_provider=gateway', '-p', 'work']\n",
    )
    .unwrap();
    #[cfg(unix)]
    fs::set_permissions(&profile, fs::Permissions::from_mode(0o600))
        .expect("private launch profile");
    let run = fixture.run(
        &strings(&[
            "fork",
            &format!("claude:{SOURCE_ID}"),
            "--in",
            "local",
            "--materialize-only",
        ]),
        &[("FAKE_CODEX_ROLLOUT", "faithful")],
    );
    assert!(!run.status.success());
    assert!(
        run.stderr
            .contains("does not use the selected model provider"),
        "{}",
        run.stderr
    );
    assert!(
        fixture
            .rpc_log()
            .contains(&format!("thread/delete {THREAD_ID}"))
    );
    assert!(!fixture.generated_rollout().exists());
    assert_eq!(fixture.launch_arguments(), None);
}

#[test]
fn readback_failure_rolls_back_and_launches_semantic_handoff() {
    if !tools_available() {
        return;
    }
    for route in Route::ALL {
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
            Path::new(handoff).starts_with(fixture.root.join("tmp")),
            "{label}: {prompt}"
        );
        assert!(
            !prompt.contains(SOURCE_QUESTION),
            "{label} put history in argv"
        );
        let document = fs::read_to_string(fixture.capture.join("handoff.md"))
            .unwrap_or_else(|error| panic!("{label} launched Codex without a handoff: {error}"));
        assert_eq!(
            fs::read_to_string(handoff).expect("saved handoff after exit"),
            document
        );
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

// When someone can answer, a native import that cannot run asks what to do instead of quietly
// writing a handoff file. The synthetic Codex fails read-back, so every import here fails.
#[test]
fn failed_import_asks_before_falling_back_when_someone_can_answer() {
    if !tools_available() {
        return;
    }
    let args = strings(&["resume", &format!("claude:{SOURCE_ID}"), "--in", "codex"]);
    let imports = |fixture: &Fixture| {
        fixture
            .rpc_log()
            .iter()
            .filter(|request| request.as_str() == "externalAgentConfig/import")
            .count()
    };

    let fixture = Fixture::new();
    let run = fixture.run_answering(&args, &[], "h\n");
    assert!(run.status.success(), "handoff: {}", run.stderr);
    for expected in [
        "Codex native import could not run: Codex import failed read-back verification",
        "[r] retry   [h] continue with a handoff file   [q] cancel",
    ] {
        assert!(run.stderr.contains(expected), "{expected}: {}", run.stderr);
    }
    assert!(
        !run.stderr.contains("using semantic handoff"),
        "the automatic fallback must not run when someone answers: {}",
        run.stderr
    );
    assert!(
        fixture.launch_arguments().is_some(),
        "handoff never launched"
    );

    // An empty line or an answer that is not on the menu asks again, `r` runs the import again,
    // and cancel launches nothing and still reports the failure.
    let fixture = Fixture::new();
    let run = fixture.run_answering(&args, &[], "\nf\nr\nq\n");
    assert!(!run.status.success(), "cancel must fail: {}", run.stderr);
    assert!(
        run.stderr.contains("Codex native import failed"),
        "{}",
        run.stderr
    );
    assert_eq!(imports(&fixture), 2, "only `r` runs the import again");
    assert_eq!(
        run.stderr.matches("Reading source trajectory").count(),
        2,
        "a retry must read the source again: {}",
        run.stderr
    );
    assert_eq!(fixture.launch_arguments(), None, "cancel launched Codex");
    fixture.assert_nothing_left_behind("cancel");
}

// Forking in the source agent instead of importing. The yolo mode was chosen for Codex, so Claude,
// which has that mode too, must still start in its own default.
#[cfg(unix)]
#[test]
fn forking_in_the_source_agent_does_not_inherit_the_target_mode() {
    use std::os::unix::fs::PermissionsExt;

    let args = strings(&["resume", &format!("claude:{SOURCE_ID}"), "--in", "codex"]);
    let fixture = Fixture::new();
    let claude = fixture.root.join("claude-launcher");
    let log = fixture.capture.join("claude-args");
    fs::write(
        &claude,
        "#!/bin/sh\ncase \"$1\" in --version) echo '2.1.300 (Claude Code)';; --help) echo '  --dangerously-skip-permissions  Bypass all permission checks';; *) echo \"$*\" > \"$FAKE_CLAUDE_LOG\";; esac\n",
    )
    .expect("write synthetic Claude");
    fs::set_permissions(&claude, fs::Permissions::from_mode(0o755)).expect("Claude mode");
    let mut yolo = args.clone();
    yolo.extend(strings(&["--mode", "yolo"]));
    let run = fixture.run_answering(
        &yolo,
        &[
            ("OMNI_CLAUDE_BIN", claude.to_str().expect("UTF-8 path")),
            ("FAKE_CLAUDE_LOG", log.to_str().expect("UTF-8 path")),
            (
                "FAKE_CODEX_HELP",
                "  --dangerously-bypass-approvals-and-sandbox  Skip all prompts",
            ),
        ],
        "f\n",
    );
    assert!(run.status.success(), "fork in source: {}", run.stderr);
    assert!(
        run.stderr.contains("[f] fork in Claude instead"),
        "{}",
        run.stderr
    );
    assert!(
        run.stderr.contains("Codex permission mode: yolo"),
        "{}",
        run.stderr
    );
    assert!(
        run.stderr
            .contains("Claude permission mode: default (no flags)"),
        "{}",
        run.stderr
    );
    assert_eq!(
        fs::read_to_string(&log).expect("Claude launch").trim(),
        format!("--resume {SOURCE_ID} --fork-session"),
        "a mode chosen for Codex must not follow the fork into Claude"
    );
    assert_eq!(fixture.launch_arguments(), None, "fork launched Codex too");
}

// Without a choice the agent starts with no extra flags. A mode chosen on the target page or with
// `--mode` reaches it as leading flags, and one its `--help` does not list steps down.
#[test]
fn permission_mode_flags_lead_the_launch_and_missing_modes_step_down() {
    const HELP: (&str, &str) = (
        "FAKE_CODEX_HELP",
        "      --approve-for-me  Route approval requests through automatic review",
    );
    if !tools_available() {
        return;
    }
    let resume = |mode: &[&str]| {
        let mut args = strings(&["resume", &format!("claude:{SOURCE_ID}"), "--in", "codex"]);
        args.extend(strings(mode));
        args
    };
    for (label, args, environment, flags, notes) in [
        (
            "default",
            resume(&[]),
            vec![HELP],
            vec![],
            vec!["Codex permission mode: default (no flags)"],
        ),
        (
            "auto",
            resume(&["--mode", "auto"]),
            vec![HELP],
            vec!["--approve-for-me"],
            vec!["Codex permission mode: auto (--approve-for-me)"],
        ),
        (
            "yolo on an agent without it",
            resume(&["--mode", "yolo"]),
            vec![HELP],
            vec!["--approve-for-me"],
            vec!["installed Codex has no `yolo` mode; using `auto`"],
        ),
        (
            "auto on an agent without any mode flag",
            resume(&["--mode", "auto"]),
            vec![],
            vec![],
            vec!["installed Codex has no `auto` mode; using `default`"],
        ),
    ] {
        let fixture = Fixture::new();
        let run = fixture.run(&args, &environment);
        assert!(run.status.success(), "{label}: {}", run.stderr);
        for note in notes {
            assert!(run.stderr.contains(note), "{label}: {}", run.stderr);
        }
        let arguments = fixture
            .launch_arguments()
            .unwrap_or_else(|| panic!("{label} never launched Codex: {}", run.stderr));
        let (prompt, leading) = arguments.split_last().expect("launch prompt");
        assert_eq!(leading, flags.as_slice(), "{label}");
        let handoff = prompt.split('`').nth(1).expect("handoff path");
        assert!(Path::new(handoff).is_file(), "{label}: {prompt}");
    }
}

#[cfg(unix)]
const SYNTHETIC_OPENCODE: &str = r#"#!/bin/sh
for arg do [ "$arg" = --standalone ] || last=$arg; done
echo " $*" >> "$FAKE_CODEX_CAPTURE/opencode.log"
case "$FAKE_OPENCODE_OUTCOME:$*" in
    pipe-switch:*" export "*|pipe-cancel:*" delete "*)
        touch "$FAKE_CODEX_CAPTURE/close-pipe"
        for i in $(seq 1 120); do
            [ ! -f "$FAKE_CODEX_CAPTURE/pipe-closed" ] || break
            sleep 0.1
        done ;;
esac
case "$*" in
    --version)
        [ ! -f "$FAKE_CODEX_CAPTURE/models-picked" ] || exit 1
        echo "$FAKE_OPENCODE_VERSION" ;;
    "--pure models"|"models --standalone")
        case "$FAKE_OPENCODE_OUTCOME" in pipe-*) touch "$FAKE_CODEX_CAPTURE/models-picked" ;; esac
        echo "synthetic/model" ;;
    *" import "*)
        case "$FAKE_OPENCODE_OUTCOME" in
            delayed-first)
                sed 's/"id"[[:space:]]*:[[:space:]]*"ses_[^"]*"/"id":"ses_unrelated"/g' "$last" > "$FAKE_CODEX_CAPTURE/stale.json"
                (for i in $(seq 1 80); do
                    if [ -f "$FAKE_CODEX_CAPTURE/returned-stale" ]; then
                        /bin/cp "$last" "$FAKE_CODEX_CAPTURE/stranded.json"
                        exit
                    fi
                    sleep 0.25
                 done) </dev/null >/dev/null 2>&1 &
                exit 0 ;;
            queued-move|queued-copy|claimed-missing)
                case "$FAKE_OPENCODE_OUTCOME" in
                    queued-move) /bin/mv "$last" "$FAKE_CODEX_CAPTURE/pending.json" ;;
                    queued-copy) /bin/cp "$last" "$FAKE_CODEX_CAPTURE/pending.json" && /bin/rm "$last" ;;
                    *) /bin/cp "$last" "$FAKE_CODEX_CAPTURE/stranded.json" ;;
                esac
                if [ "$1" = --pure ]; then printf '\033[91m\033[1mError: \033[0mFile not found: %s\n' "$last" >&2
                elif [ "$FAKE_OPENCODE_VERSION" = 2.0.0 ]; then
                    printf "[12:34:56.789] ERROR (#1): Error: Failed to read session data: ENOENT: no such file or directory, open '%s'\n" "$last"
                else printf "Error: Failed to read session data: ENOENT: no such file or directory, open '%s'\n" "$last" >&2; fi
                exit 1 ;;
        esac
        /bin/cp "$last" "$FAKE_CODEX_CAPTURE/stranded.json"
        case "$FAKE_OPENCODE_OUTCOME" in
            delete-retry-*|pipe-cancel) /bin/cp "$last" "$FAKE_CODEX_CAPTURE/pending.json"; exit 0 ;;
            delete-*|pipe-switch) exit 0 ;;
            success-aux)
                (for i in $(seq 1 80); do
                    [ ! -f "$FAKE_CODEX_CAPTURE/exported" ] || exit 0
                    sleep 0.25
                 done
                 touch "$FAKE_CODEX_CAPTURE/blocked") &
                exit 0 ;;
            retry)
                /bin/cp "$last" "$FAKE_CODEX_CAPTURE/pending.json"
                (for i in $(seq 1 80); do
                    [ ! -f "$FAKE_CODEX_CAPTURE/release" ] || break
                    sleep 0.25
                 done
                 /bin/mv "$FAKE_CODEX_CAPTURE/pending.json" "$FAKE_CODEX_CAPTURE/stranded.json"
                 touch "$FAKE_CODEX_CAPTURE/retried") </dev/null >/dev/null 2>&1 &
                exit 124 ;;
            signal) kill -TERM "$$" ;;
            *) exit 1 ;;
        esac ;;
    "session delete "*|*" session delete "*)
        case "$FAKE_OPENCODE_OUTCOME" in
            delete-retry-*|pipe-cancel) /bin/rm "$FAKE_CODEX_CAPTURE/stranded.json" ;;
            delete-lied) exit 0 ;;
            *) exit 1 ;;
        esac ;;
    *" db "*|*" GET /api/session/"*)
        id=${last##*/}
        case "$FAKE_OPENCODE_OUTCOME" in
            queued-*|delete-retry-*|pipe-cancel)
                if [ "$1" = --pure ]; then echo '[]'; exit 0; fi
                echo 'HTTP 404 Not Found' >&2
                printf '{"_tag":"SessionNotFoundError","sessionID":"%s"}\n' "$id" ;;
            delete-wrong-id)
                echo 'HTTP 404 Not Found' >&2
                echo '{"_tag":"SessionNotFoundError","sessionID":"ses_unrelated"}' ;;
            delete-lied)
                printf '{"data":{"id":"%s"}}\n' "$id"; exit 0 ;;
            *) echo 'database is locked' >&2 ;;
        esac
        exit 1 ;;
    *" export "*)
        if [ "$FAKE_OPENCODE_OUTCOME" = delayed-first ] && [ ! -f "$FAKE_CODEX_CAPTURE/stranded.json" ]; then
            if [ -f "$FAKE_CODEX_CAPTURE/exported" ]; then
                /bin/cat "$FAKE_CODEX_CAPTURE/stale.json"
                touch "$FAKE_CODEX_CAPTURE/returned-stale"
                exit 0
            fi
            touch "$FAKE_CODEX_CAPTURE/exported"
            exit 1
        fi
        touch "$FAKE_CODEX_CAPTURE/exported"
        exec /bin/cat "$FAKE_CODEX_CAPTURE/stranded.json" ;;
esac
"#;

#[cfg(unix)]
fn write_opencode_fixture(fixture: &Fixture) -> PathBuf {
    let opencode = fixture.root.join("opencode");
    fs::write(&opencode, SYNTHETIC_OPENCODE).expect("write synthetic OpenCode");
    fs::set_permissions(&opencode, fs::Permissions::from_mode(0o755)).expect("OpenCode mode");
    opencode
}

#[cfg(unix)]
fn run_opencode_import(fixture: &Fixture, version: &str, outcome: &str) -> Run {
    let opencode = write_opencode_fixture(fixture);
    let mut args = strings(&["resume", &format!("claude:{SOURCE_ID}"), "--in", "opencode"]);
    if matches!(
        outcome,
        "success-aux" | "delayed-first" | "published-materialize-only"
    ) {
        args.push("--materialize-only".to_owned());
    }
    fixture.run(
        &args,
        &[
            ("OMNI_OPENCODE_BIN", opencode.to_str().expect("UTF-8 path")),
            ("FAKE_OPENCODE_VERSION", version),
            ("FAKE_OPENCODE_OUTCOME", outcome),
            (
                INTERRUPT,
                if outcome == "delete-retry-recorded" {
                    "recorded"
                } else if outcome.starts_with("delete-") {
                    "materialized"
                } else {
                    ""
                },
            ),
        ],
    )
}

#[cfg(unix)]
#[test]
fn opencode_preparation_failure_falls_back_before_starting_the_importer() {
    let fixture = Fixture::new();
    fs::write(fixture.root.join("state/imports"), "not a directory").unwrap();
    let run = run_opencode_import(&fixture, "2.2.0", "preflight");
    assert!(run.status.success(), "{}", run.stderr);
    assert!(
        run.stderr.contains("using semantic handoff"),
        "{}",
        run.stderr
    );
    let calls = fs::read_to_string(fixture.capture.join("opencode.log")).unwrap();
    assert!(!calls.contains(" import "), "{calls}");
    assert!(
        calls.lines().last().unwrap().contains("--prompt"),
        "{calls}"
    );
}

#[cfg(unix)]
#[test]
fn opencode_switch_commits_or_restores_its_binding_after_stdout_closes() {
    for outcome in ["pipe-switch", "pipe-cancel"] {
        let fixture = Fixture::new();
        let mut source = fs::OpenOptions::new()
            .append(true)
            .open(
                fixture
                    .root
                    .join(format!("claude/projects/synthetic/{SOURCE_ID}.jsonl")),
            )
            .unwrap();
        let record = json!({"type":"assistant", "sessionId":SOURCE_ID, "cwd":fixture.workspace,
            "message":{"role":"assistant", "content":format!("{}saved final detail", "word ".repeat(26_000))}});
        writeln!(source, "{record}").unwrap();
        drop(source);

        let (reader, writer) = std::io::pipe().unwrap();
        let mut command = fixture.command();
        command
            .args(["switch", "opencode"])
            .env("OMNI_OPENCODE_BIN", write_opencode_fixture(&fixture))
            .env("FAKE_OPENCODE_VERSION", "2.2.0")
            .env("FAKE_OPENCODE_OUTCOME", outcome)
            .env(
                INTERRUPT,
                if outcome == "pipe-cancel" {
                    "recorded"
                } else {
                    ""
                },
            )
            .stdin(Stdio::null())
            .stdout(Stdio::from(writer))
            .stderr(Stdio::piped());
        let child = command.spawn().unwrap();
        wait_for_file(&fixture.capture.join("close-pipe"), Duration::from_secs(20)).unwrap();
        drop(reader);
        fs::write(fixture.capture.join("pipe-closed"), "").unwrap();
        let output = child.wait_with_output().unwrap();
        let run = Run {
            status: output.status,
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            elapsed: Duration::ZERO,
        };

        let published = fixture.capture.join("stranded.json");
        if outcome == "pipe-cancel" {
            assert_eq!(run.status.code(), Some(130), "{}", run.stderr);
            assert!(!published.exists());
            fs::rename(fixture.capture.join("pending.json"), &published).unwrap();
        } else {
            assert!(run.status.success(), "{}", run.stderr);
        }
        let contents = fs::read_to_string(published).unwrap();
        let doc: Value = serde_json::from_str(&contents).unwrap();
        let target = format!("opencode:{}", doc["info"]["id"].as_str().unwrap());
        let expected = if outcome == "pipe-cancel" {
            format!("claude:{SOURCE_ID}")
        } else {
            target
        };
        assert_eq!(fixture.bound_session(), expected);
        let path = run
            .stderr
            .lines()
            .find_map(|line| line.strip_prefix("Recovery handoff saved to "))
            .unwrap();
        assert!(contents.contains(path));
        fixture.assert_recovery_saved(outcome, &run, "saved final detail");
        let calls = fs::read_to_string(fixture.capture.join("opencode.log")).unwrap();
        let (_, after_model) = calls.split_once(" models --standalone").unwrap();
        assert!(!after_model.contains("--version"), "{calls}");
        assert!(
            after_model.contains(" session import ") && after_model.contains(" session export "),
            "{calls}"
        );
        assert!(!after_model.contains("--prompt"), "{calls}");
    }
}

#[cfg(unix)]
#[test]
fn opencode_accepts_verified_history_from_a_delayed_worker_or_with_a_live_auxiliary() {
    for outcome in ["delayed-first", "success-aux"] {
        let fixture = Fixture::new();
        let run = run_opencode_import(&fixture, "1.18.18", outcome);
        assert!(run.status.success(), "{}", run.stderr);
        let calls = fs::read_to_string(fixture.capture.join("opencode.log")).unwrap();
        assert!(calls.contains(" export "), "{calls}");
        assert!(!calls.contains(" session delete "), "{calls}");
        if outcome == "delayed-first" {
            assert!(fixture.capture.join("returned-stale").exists());
            assert!(fixture.capture.join("stranded.json").is_file());
        } else {
            assert!(!fixture.capture.join("blocked").exists(), "{calls}");
        }
        fixture.assert_nothing_left_behind(outcome);
    }
}

#[cfg(unix)]
#[test]
fn failed_opencode_import_saves_a_private_handoff_for_a_complete_source() {
    for outcome in ["published", "published-materialize-only"] {
        let fixture = Fixture::new();
        let run = run_opencode_import(&fixture, "1.18.18", outcome);
        assert!(!run.status.success());
        let document: Value =
            serde_json::from_slice(&fs::read(fixture.capture.join("stranded.json")).unwrap())
                .unwrap();
        assert!(
            run.stderr
                .contains(document["info"]["id"].as_str().unwrap())
        );
        fixture.assert_recovery_saved(outcome, &run, SOURCE_QUESTION);
    }
}

#[cfg(unix)]
#[test]
fn uncertain_opencode_rollback_preserves_the_target_and_recovery_reference() {
    for (version, outcome) in [
        ("1.18.18", "published"),
        ("1.18.18", "retry"),
        ("1.18.18", "signal"),
        ("1.18.18", "queued-move"),
        ("2.0.0", "queued-copy"),
        ("2.2.0", "claimed-missing"),
        ("1.18.18", "delete-retry-materialized"),
        ("2.2.0", "delete-retry-recorded"),
        ("1.18.18", "delete-busy"),
        ("2.2.0", "delete-wrong-id"),
        ("2.2.0", "delete-lied"),
    ] {
        let fixture = Fixture::new();
        let source = fixture
            .root
            .join(format!("claude/projects/synthetic/{SOURCE_ID}.jsonl"));
        let mut source = fs::OpenOptions::new().append(true).open(source).unwrap();
        let record = json!({"type":"assistant", "sessionId":SOURCE_ID, "cwd":fixture.workspace,
            "message":{"role":"assistant", "content":format!("{}saved final detail", "word ".repeat(26_000))}});
        writeln!(source, "{record}").unwrap();
        drop(source);
        let run = run_opencode_import(&fixture, version, outcome);
        if outcome == "retry" {
            fs::write(fixture.capture.join("release"), "").unwrap();
            wait_for_file(&fixture.capture.join("retried"), Duration::from_secs(15)).unwrap();
        }
        if outcome.starts_with("queued-") || outcome.starts_with("delete-retry-") {
            // The worker completes its pending write only after OmniSession exits.
            let published = fixture.capture.join("stranded.json");
            assert!(!published.exists());
            fs::rename(fixture.capture.join("pending.json"), published).unwrap();
        }
        assert!(!run.status.success(), "{outcome}: {}", run.stderr);
        let imported = fs::read_to_string(fixture.capture.join("stranded.json")).unwrap();
        let document: Value = serde_json::from_str(&imported).unwrap();
        assert!(
            run.stderr
                .contains(document["info"]["id"].as_str().unwrap()),
            "{}",
            run.stderr
        );
        let calls = fs::read_to_string(fixture.capture.join("opencode.log")).unwrap();
        assert!(!calls.contains("--prompt"), "{calls}");
        if outcome.starts_with("delete-") {
            if outcome.starts_with("delete-retry-") {
                assert_eq!(run.status.code(), Some(130), "{}", run.stderr);
            } else {
                assert!(
                    run.stderr.contains("rollback also failed"),
                    "{}",
                    run.stderr
                );
            }
            let (_, after_delete) = calls.split_once(" session delete ").unwrap();
            assert!(!after_delete.contains(" export "), "{calls}");
        } else {
            assert!(
                run.stderr.contains("may still be writing"),
                "{}",
                run.stderr
            );
            assert!(!calls.contains(" session delete "), "{calls}");
        }
        let file = run
            .stderr
            .lines()
            .find_map(|line| line.strip_prefix("Recovery handoff saved to "))
            .unwrap();
        assert!(Path::new(file).is_file());
        assert!(imported.contains(&file.replace('\\', "\\\\")));
        fixture.assert_recovery_saved(outcome, &run, "saved final detail");
    }
}

#[test]
fn failed_rollback_refuses_to_launch_while_generated_thread_may_remain() {
    if !tools_available() {
        return;
    }
    for route in Route::ALL {
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
            fixture.assert_recovery_saved(&label, &run, SOURCE_QUESTION);
        }
    }
}

#[test]
fn codex_ledger_failure_rolls_back_the_reported_thread_before_fallback() {
    assert_codex_completion_fallback(
        "ledger",
        &[],
        &[&format!("thread/delete {THREAD_ID}"), "launch"],
    );
}

#[test]
fn codex_preparation_failure_falls_back_without_deleting_a_thread() {
    assert_codex_completion_fallback("prepare", &[], &["launch"]);
}

#[test]
fn codex_progress_rolls_back_the_exact_target_when_completion_is_lost() {
    for mode in ["success", "ledger"] {
        for ending in ["disconnect", "timeout"] {
            assert_codex_completion_fallback(
                mode,
                &[
                    ("FAKE_CODEX_COMPLETION", ending),
                    ("OMNI_TEST_CODEX_RPC_TIMEOUT_MS", HANG_RPC_TIMEOUT_MS),
                ],
                &[&format!("thread/delete {THREAD_ID}"), "launch"],
            );
        }
    }
}

#[test]
fn codex_preallocation_failures_fall_back_without_deleting_a_thread() {
    for (stage, subtype) in [
        ("session_persist", "failed_to_load_session_config"),
        (
            "session_persist",
            "failed_to_load_session_config_invalid_data",
        ),
        ("session_permit", "failed_to_acquire_import_permit"),
    ] {
        assert_codex_completion_fallback(
            "prepare",
            &[
                ("FAKE_CODEX_FAILURE_STAGE", stage),
                ("FAKE_CODEX_FAILURE_SUBTYPE", subtype),
            ],
            &["launch"],
        );
    }
}

fn assert_codex_completion_fallback(mode: &str, extra: &[(&str, &str)], expected_actions: &[&str]) {
    if !tools_available() {
        return;
    }
    let fixture = Fixture::new();
    let mut environment = vec![
        ("FAKE_CODEX_IMPORT", mode),
        ("FAKE_CODEX_ROLLOUT", "faithful"),
        ("FAKE_CODEX_LOG_LAUNCH", "1"),
    ];
    environment.extend_from_slice(extra);
    let run = fixture.run(&Route::Resume.args(), &environment);
    assert!(run.status.success(), "{mode} {extra:?}: {}", run.stderr);
    let actions: Vec<_> = fixture
        .rpc_log()
        .into_iter()
        .filter(|call| call == "launch" || call.starts_with("thread/delete"))
        .collect();
    assert_eq!(actions, expected_actions, "{mode}");
    assert!(!fixture.generated_rollout().exists(), "{mode}");
    assert!(fixture.capture.join("handoff.md").exists());
    assert_eq!(fixture.bound_session(), format!("claude:{SOURCE_ID}"));
    fixture.assert_nothing_left_behind(mode);
}

#[test]
fn codex_ambiguous_receipts_never_delete_or_launch() {
    if !tools_available() {
        return;
    }
    for (mode, ending, progress_id) in [
        ("ledger-wrong-source", "", "synthetic-import"),
        ("prepare-wrong-source", "", "synthetic-import"),
        ("success-wrong-source", "", "synthetic-import"),
        ("failed-completion", "", "synthetic-import"),
        ("success-wrong-source", "disconnect", "synthetic-import"),
        ("success", "disconnect", "other-import"),
        ("success", "conflict-progress", "synthetic-import"),
        ("success", "conflict-completed", "synthetic-import"),
    ] {
        let fixture = Fixture::new();
        let run = fixture.run(
            &Route::Resume.args(),
            &[
                ("FAKE_CODEX_IMPORT", mode),
                ("FAKE_CODEX_ROLLOUT", "faithful"),
                ("FAKE_CODEX_COMPLETION", ending),
                ("FAKE_CODEX_PROGRESS_ID", progress_id),
            ],
        );
        assert!(
            !run.status.success(),
            "{mode} {ending} {progress_id}: {}",
            run.stderr
        );
        let calls = fixture.rpc_log();
        assert!(
            !calls.iter().any(|call| call.starts_with("thread/delete")),
            "{mode}: {calls:?}"
        );
        assert_eq!(
            fixture.generated_rollout().exists(),
            mode != "prepare-wrong-source"
        );
        assert_eq!(fixture.launch_arguments(), None, "{mode}");
        assert_eq!(fixture.bound_session(), format!("claude:{SOURCE_ID}"));
        fixture.assert_recovery_saved(mode, &run, SOURCE_QUESTION);
    }
}

#[test]
fn broken_app_server_ends_within_bound_without_partial_state() {
    if !tools_available() {
        return;
    }
    let fixture = Fixture::new();
    let missing = fixture.root.join("missing-native-home");
    let mut args = Route::Resume.args();
    args.push("--materialize-only".to_owned());
    let run = fixture.run(&args, &[("CODEX_HOME", missing.to_str().unwrap())]);
    assert!(!run.status.success(), "a missing Codex home must fail");
    assert!(run.stderr.contains("CODEX_HOME"), "{}", run.stderr);
    assert!(
        run.stderr.contains(missing.to_str().unwrap()),
        "{}",
        run.stderr
    );
    assert!(fixture.rpc_log().is_empty());

    let run = fixture.run(&Route::Resume.args(), &[("FAKE_CODEX_IMPORT", "reject")]);
    assert!(run.status.success(), "{}", run.stderr);
    assert!(run.stderr.contains("using semantic handoff"));
    assert!(fixture.launch_arguments().is_some());
    fixture.assert_import_abandoned("rejected import", &run);
    for (mode, reason) in [
        ("hang", "Codex app-server timed out or disconnected"),
        ("disconnect", "Codex app-server timed out or disconnected"),
        ("malformed", "invalid Codex app-server response"),
    ] {
        let label = format!("resume with {mode} import");
        let fixture = Fixture::new();
        let run = fixture.run(&Route::Resume.args(), &broken("FAKE_CODEX_IMPORT", mode));
        assert!(!run.status.success(), "{label}: {}", run.stderr);
        for expected in [reason, "may have created a thread"] {
            assert!(run.stderr.contains(expected), "{label}: {}", run.stderr);
        }
        assert_eq!(
            fixture.launch_arguments(),
            None,
            "{label} launched a handoff"
        );
        fixture.assert_import_abandoned(&label, &run);

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
fn progress_notifications_cannot_extend_an_import_deadline() {
    if !tools_available() {
        return;
    }
    for mode in ["late-reply", "late-completion"] {
        let fixture = Fixture::new();
        let run = fixture.run(
            &Route::Resume.args(),
            &[
                ("FAKE_CODEX_IMPORT", mode),
                ("FAKE_CODEX_ROLLOUT", "faithful"),
                ("OMNI_TEST_CODEX_RPC_TIMEOUT_MS", HANG_RPC_TIMEOUT_MS),
            ],
        );
        assert!(!run.status.success(), "{mode}: {}", run.stderr);
        assert!(run.stderr.contains("timed out"), "{mode}: {}", run.stderr);
        assert!(fixture.capture.join("progress-started").exists());
        assert!(!fixture.capture.join("progress-finished").exists());
        assert!(fixture.launch_arguments().is_none());
    }
}

#[test]
fn failed_in_server_verification_rollback_refuses_to_launch() {
    if !tools_available() {
        return;
    }
    for (route, read) in [(Route::Resume, "empty"), (Route::Switch, "defer")] {
        let label = format!("{route:?} with {read} thread/read and rejected rollback");
        let fixture = Fixture::new();
        let run = fixture.run(
            &route.args(),
            &[
                ("FAKE_CODEX_READ", read),
                ("FAKE_CODEX_DELETE", "reject"),
                ("OMNI_TEST_CODEX_RPC_TIMEOUT_MS", HANG_RPC_TIMEOUT_MS),
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
        fixture.assert_recovery_saved(&label, &run, SOURCE_QUESTION);
    }
}

#[test]
fn interrupted_codex_import_rolls_back_and_exits_without_launching() {
    if !tools_available() {
        return;
    }
    let faithful = ("FAKE_CODEX_ROLLOUT", "faithful");
    for (route, checkpoint, environment) in [
        (Route::Resume, "materialized", vec![faithful]),
        (Route::Switch, "recorded", vec![faithful]),
        // Read-back fails and rolls back on its own; the interrupt must still block the handoff.
        (Route::Resume, "materialized", vec![]),
    ] {
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

    {
        let route = Route::Resume;
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
        fixture.assert_recovery_saved(&label, &run, SOURCE_QUESTION);
    }
}

#[test]
fn console_ctrl_c_during_codex_import_rolls_back_the_surviving_thread() {
    if !tools_available() {
        return;
    }
    {
        let route = Route::Resume;
        let label = format!("{route:?} interrupted from the console during the import request");
        let fixture = Fixture::new();
        let run = fixture.run_interrupted_import(&route.args());
        assert_eq!(run.status.code(), Some(130), "{label}: {}", run.stderr);
        assert!(
            !fixture.capture.join("server-interrupted").exists(),
            "{label}: the app-server received omni's interrupt: {}",
            run.stderr
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
#[cfg(unix)]
fn omp_routes_native_history_without_handoff_or_implicit_permissions() {
    let source = format!("claude:{SOURCE_ID}");
    for (args, prefix) in [
        (strings(&["resume", &source, "--in", "oh-my-pi"]), vec![]),
        (
            strings(&["switch", "omp", "--mode", "always-ask"]),
            strings(&["--approval-mode", "always-ask"]),
        ),
        (
            strings(&["switch", "omp", "--mode", "accept-edits"]),
            strings(&["--approval-mode", "write"]),
        ),
        (
            strings(&["switch", "omp", "--mode", "yolo"]),
            strings(&["--approval-mode", "yolo"]),
        ),
    ] {
        let fixture = Fixture::new();
        let root = fixture.root.join("omp/sessions");
        let run = fixture.run(
            &args,
            &[
                ("OMNI_OMP_BIN", path_str(synthetic_pi())),
                ("OMP_SESSION_DIR", path_str(&root)),
                ("FAKE_PI_VERSION", "omp/18.2.10"),
            ],
        );
        assert!(run.status.success(), "{args:?}: {}", run.stderr);
        let bytes = fs::read(fixture.capture.join("pi-launch-args")).unwrap();
        let launched = bytes
            .split(|byte| *byte == 0)
            .filter(|argument| !argument.is_empty())
            .map(|argument| String::from_utf8_lossy(argument).into_owned())
            .collect::<Vec<_>>();
        assert_eq!(&launched[..prefix.len()], prefix.as_slice());
        let [flag, path] = &launched[prefix.len()..] else {
            panic!("{args:?}: unexpected launch flags {launched:?}");
        };
        assert_eq!(flag, "--session");
        assert!(Path::new(path).starts_with(&root));
        let history = fs::read_to_string(path).unwrap();
        assert_eq!(history.find('\n'), Some(255));
        assert!(history.contains(SOURCE_QUESTION));
        if args[0] == "resume" {
            assert_eq!(fixture.bound_session(), source);
        } else {
            assert!(fixture.bound_session().starts_with("omp:"), "{args:?}");
        }
        fixture.assert_nothing_left_behind(&format!("{args:?}"));
    }
}

#[test]
fn interrupted_pi_import_rolls_back_and_exits_without_launching() {
    if !tools_available() {
        return;
    }
    assert_interrupted_pi_family_import("pi", "OMNI_PI_BIN", "0.82.0");
    #[cfg(unix)]
    assert_interrupted_pi_family_import("omp", "OMNI_OMP_BIN", "omp/18.2.10");
}

fn assert_interrupted_pi_family_import(provider: &str, binary_override: &str, version: &str) {
    let source = format!("claude:{SOURCE_ID}");
    let cases = vec![
        (
            strings(&["resume", &source, "--in", provider]),
            "materialized",
        ),
        (
            strings(&["resume", &source, "--in", provider, "--materialize-only"]),
            "planned",
        ),
        (strings(&["switch", provider]), "recorded"),
    ];
    for (args, checkpoint) in cases {
        let label = format!("`omni {}` interrupted at {checkpoint}", args.join(" "));
        let fixture = Fixture::new();
        let pi_root = fixture.root.join(provider);
        let sessions = pi_root.join("sessions");
        let run = fixture.run(
            &args,
            &[
                (binary_override, path_str(synthetic_pi())),
                ("PI_CODING_AGENT_DIR", path_str(&pi_root)),
                ("OMP_SESSION_DIR", path_str(&sessions)),
                ("FAKE_PI_VERSION", version),
                (INTERRUPT, checkpoint),
            ],
        );
        assert_eq!(run.status.code(), Some(130), "{label}: {}", run.stderr);
        let target = rolled_back_target(&label, &run);
        assert!(
            target.starts_with(&format!("{provider}:")),
            "{label}: {target}"
        );
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
    /// Prefix of the named pipes this fixture's synthetic app-servers hold while they run.
    #[cfg(windows)]
    liveness: String,
}

impl Fixture {
    fn new() -> Self {
        let temporary = tempfile::tempdir().expect("temporary fixture");
        // Ordinary canonical paths compare equal to the workspace omni resolves on every platform.
        let root = omnis_core::canonicalize_path(temporary.path()).expect("canonical fixture");
        let fixture = Self {
            workspace: root.join("workspace"),
            capture: root.join("capture"),
            #[cfg(windows)]
            liveness: format!(
                "omnisession-test-app-server-{}",
                uuid::Uuid::new_v4().simple()
            ),
            root,
            _temporary: temporary,
        };
        for directory in [
            "home",
            "tmp",
            "capture",
            "codex",
            "workspace",
            "state",
            "claude/projects/synthetic",
        ] {
            fs::create_dir_all(fixture.root.join(directory)).expect("isolated fixture directory");
        }
        #[cfg(unix)]
        fs::set_permissions(
            fixture.root.join("state"),
            fs::Permissions::from_mode(0o700),
        )
        .expect("private OmniSession state directory");

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
            command
                .env("PATH", tool_path().expect("Windows fixture tools"))
                .env("FAKE_CODEX_LIVENESS", &self.liveness);
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
            .env("OMP_SESSION_DIR", self.root.join("omp/sessions"))
            .env("CURSOR_AGENT_HOME", self.root.join("cursor"))
            .env("ANTIGRAVITY_CLI_HOME", self.root.join("antigravity-cli"))
            .env("ANTIGRAVITY_IDE_HOME", self.root.join("antigravity-ide"))
            .env(
                "ANTIGRAVITY_DESKTOP_HOME",
                self.root.join("antigravity-desktop"),
            )
            .env(
                "ANTIGRAVITY_IDE_USER_DATA_DIR",
                self.root.join("antigravity-ide-user-data"),
            )
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
            "OMNI_OMP_BIN",
            "OMNI_CURSOR_AGENT_BIN",
            "OMNI_CURSOR_IDE_BIN",
        ] {
            command.env(variable, self.root.join("missing").join(variable));
        }
        command
    }

    fn run(&self, args: &[String], environment: &[(&str, &str)]) -> Run {
        self.run_during(args, environment, |_| Ok(()))
    }

    /// Runs omni as if a person answered its import questions with `answers`, one per line.
    fn run_answering(&self, args: &[String], environment: &[(&str, &str)], answers: &str) -> Run {
        let path = self.root.join("answers");
        fs::write(&path, answers).expect("write answers");
        let mut environment = environment.to_vec();
        environment.push(("OMNI_TEST_IMPORT_CHOICES", "1"));
        let stdin = fs::File::open(&path).expect("open answers");
        self.run_with(args, &environment, Stdio::from(stdin), |_| Ok(()))
    }

    // Runs omni as a process group leader on a console of its own, so a signal or console event
    // reaches omni like a terminal Ctrl+C. `during` runs while omni does; when a step fails, omni is
    // killed and the test panics with omni's output.
    fn run_during(
        &self,
        args: &[String],
        environment: &[(&str, &str)],
        during: impl FnOnce(&Child) -> Result<(), String>,
    ) -> Run {
        self.run_with(args, environment, Stdio::null(), during)
    }

    fn run_with(
        &self,
        args: &[String],
        environment: &[(&str, &str)],
        stdin: Stdio,
        during: impl FnOnce(&Child) -> Result<(), String>,
    ) -> Run {
        let started = Instant::now();
        let mut command = self.command();
        command
            .args(args)
            .envs(environment.iter().copied())
            .stdin(stdin)
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
        if let Err(error) = during(&child) {
            let _ = child.kill();
            let _ = child.wait();
            let stderr =
                String::from_utf8_lossy(&stderr.join().expect("omni stderr reader")).into_owned();
            panic!("`omni {}`: {error}\n{stderr}", args.join(" "));
        }
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

    // A terminal Ctrl+C signals the whole foreground process group. The synthetic app-server holds
    // the import open until omni acknowledges SIGINT, so the interrupt always lands mid-import.
    #[cfg(unix)]
    fn run_interrupted_import(&self, args: &[String]) -> Run {
        let acknowledged = self.capture.join("interrupt-ack");
        self.run_during(
            args,
            &[
                ("FAKE_CODEX_ROLLOUT", "faithful"),
                ("FAKE_CODEX_IMPORT", "hold"),
                ("OMNI_TEST_INTERRUPT_ACK", path_str(&acknowledged)),
            ],
            |omni| {
                wait_for_file(&self.capture.join("importing"), INTERRUPT_STEP)?;
                let server = self.import_server_pid()?;
                if !self.running_app_servers().contains(&server.to_string()) {
                    return Err(format!(
                        "the liveness probe does not see synthetic app-server {server}"
                    ));
                }
                if process_group(server)? == omni.id() {
                    return Err(format!(
                        "synthetic app-server {server} runs in omni's process group"
                    ));
                }
                rustix::process::kill_process_group(
                    rustix::process::Pid::from_child(omni),
                    rustix::process::Signal::INT,
                )
                .map_err(|error| format!("signal omni's process group: {error}"))?;
                wait_for_file(&acknowledged, INTERRUPT_STEP)?;
                fs::write(self.capture.join("release"), "")
                    .map_err(|error| format!("release synthetic import: {error}"))
            },
        )
    }

    // Types Ctrl+Break at omni's console while the synthetic app-server holds the import open, and
    // releases the import only after omni acknowledged the interrupt.
    #[cfg(windows)]
    fn run_interrupted_import(&self, args: &[String]) -> Run {
        let acknowledged = self.capture.join("interrupt-ack");
        let mut sender = ConsoleBreakSender::start(&self.capture);
        self.run_during(
            args,
            &[
                ("FAKE_CODEX_ROLLOUT", "faithful"),
                ("FAKE_CODEX_IMPORT", "hold"),
                ("OMNI_TEST_INTERRUPT_ACK", path_str(&acknowledged)),
            ],
            |omni| {
                wait_for_file(&self.capture.join("importing"), INTERRUPT_STEP)?;
                // A new process connects to its console while it starts, after spawn returns, so
                // the sender attaches only once omni is mid-import.
                sender.attach(omni.id())?;
                let console = sender.send_break()?;
                let server = self.import_server_pid()?;
                if !self.running_app_servers().contains(&server.to_string()) {
                    return Err(format!(
                        "the liveness probe does not see synthetic app-server {server}"
                    ));
                }
                if !console.contains(&omni.id()) {
                    return Err(format!(
                        "omni {} is missing from its console: {console:?}",
                        omni.id()
                    ));
                }
                if console.contains(&server) {
                    return Err(format!(
                        "synthetic app-server {server} shares omni's console: {console:?}"
                    ));
                }
                wait_for_file(&acknowledged, INTERRUPT_STEP)?;
                fs::write(self.capture.join("release"), "")
                    .map_err(|error| format!("release synthetic import: {error}"))
            },
        )
    }

    /// The synthetic app-server serving the import request.
    fn import_server_pid(&self) -> Result<u32, String> {
        let pids = fs::read_to_string(self.capture.join("server-pids"))
            .map_err(|error| format!("read synthetic app-server PIDs: {error}"))?;
        pids.lines()
            .next()
            .and_then(|pid| pid.trim().parse().ok())
            .ok_or_else(|| format!("no synthetic app-server PID in {pids:?}"))
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
        if run.status.success() {
            self.assert_nothing_left_behind(label);
        } else {
            self.assert_recovery_saved(label, run, SOURCE_QUESTION);
        }
    }

    fn assert_recovery_saved(&self, label: &str, run: &Run, expected: &str) {
        let mut paths = run
            .stderr
            .lines()
            .filter_map(|line| line.strip_prefix("Recovery handoff saved to "));
        let path = Path::new(
            paths
                .next()
                .unwrap_or_else(|| panic!("{label}: {}", run.stderr)),
        );
        assert_eq!(
            paths.next(),
            None,
            "{label} reported multiple recovery files"
        );
        assert_eq!(path.parent(), Some(self.root.join("tmp").as_path()));
        assert!(fs::read_to_string(path).unwrap().contains(expected));
        #[cfg(unix)]
        assert_eq!(fs::metadata(path).unwrap().permissions().mode() & 0o077, 0);
        fs::remove_file(path).expect("remove the verified private recovery file");
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
            .map(|entry| entry.expect("temporary directory entry").path())
            .collect::<Vec<_>>();
        let delivered: Vec<PathBuf> = if self.capture.join("handoff.md").exists() {
            let args = self
                .launch_arguments()
                .expect("successful handoff arguments");
            let path = args
                .last()
                .and_then(|prompt| prompt.split('`').nth(1))
                .expect("successful handoff path");
            vec![PathBuf::from(path)]
        } else {
            Vec::new()
        };
        assert_eq!(
            temporary, delivered,
            "{label} should retain only the handoff received by the launched agent"
        );
        let running = self.running_app_servers();
        assert!(
            running.is_empty(),
            "{label}: synthetic app-servers {running:?} outlived omni"
        );
    }

    /// PIDs of this fixture's synthetic app-servers that are still running.
    #[cfg(unix)]
    fn running_app_servers(&self) -> Vec<String> {
        fs::read_to_string(self.capture.join("server-pids"))
            .unwrap_or_default()
            .lines()
            .filter(|pid| {
                Command::new("kill")
                    .args(["-0", pid])
                    .stderr(Stdio::null())
                    .status()
                    .expect("probe synthetic app-server")
                    .success()
            })
            .map(str::to_owned)
            .collect()
    }

    /// PIDs of this fixture's synthetic app-servers that are still running.
    ///
    /// A PID probe can list a process omni already killed and reaped, while its console host or an
    /// antivirus scanner still references it, or a new process that reused the PID. Each app-server
    /// instead holds a named pipe that Windows closes during termination.
    #[cfg(windows)]
    fn running_app_servers(&self) -> Vec<String> {
        let prefix = format!("{}-", self.liveness);
        fs::read_dir(r"\\.\pipe\")
            .expect("list named pipes")
            .filter_map(|entry| {
                let name = entry.ok()?.file_name().into_string().ok()?;
                name.strip_prefix(&prefix).map(str::to_owned)
            })
            .collect()
    }
}

#[cfg(unix)]
fn process_group(pid: u32) -> Result<u32, String> {
    let process = i32::try_from(pid)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
        .ok_or_else(|| format!("invalid process ID {pid}"))?;
    let group = rustix::process::getpgid(Some(process))
        .map_err(|error| format!("read process group of {pid}: {error}"))?;
    u32::try_from(group.as_raw_pid()).map_err(|error| format!("process group of {pid}: {error}"))
}

/// Drives [`CONSOLE_BREAK_SCRIPT`] through its marker files.
#[cfg(windows)]
struct ConsoleBreakSender {
    child: Child,
    directory: PathBuf,
}

#[cfg(windows)]
impl ConsoleBreakSender {
    /// Starts the sender and waits until its bindings are compiled, before omni starts importing.
    fn start(capture: &Path) -> Self {
        let directory = capture.join("console-break");
        let temporary = directory.join("tmp");
        fs::create_dir_all(&temporary).expect("console break sender directory");
        let script = directory.join("send-break.ps1");
        fs::write(&script, CONSOLE_BREAK_SCRIPT).expect("write console break sender");
        let log = fs::File::create(directory.join("sender.log")).expect("console break sender log");
        let powershell = PathBuf::from(env::var_os("SystemRoot").expect("SystemRoot"))
            .join("System32")
            .join("WindowsPowerShell")
            .join("v1.0")
            .join("powershell.exe");
        let child = Command::new(powershell)
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
            ])
            .arg(&script)
            .arg(&directory)
            // Add-Type compiles in TEMP, which omni's isolated temporary directory must not collect.
            .env("TEMP", &temporary)
            .env("TMP", &temporary)
            .stdin(Stdio::null())
            .stdout(log.try_clone().expect("clone console break sender log"))
            .stderr(log)
            // A hidden console of its own keeps the event away from this test until it attaches.
            .creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP)
            .spawn()
            .expect("start console break sender");
        let mut sender = Self { child, directory };
        sender
            .wait_for("ready", Duration::from_secs(120))
            .unwrap_or_else(|error| panic!("{error}"));
        sender
    }

    /// Attaches the sender to omni's console.
    fn attach(&mut self, omni: u32) -> Result<(), String> {
        let staged = self.directory.join("omni-pid.staged");
        fs::write(&staged, omni.to_string()).map_err(|error| format!("stage omni PID: {error}"))?;
        fs::rename(&staged, self.directory.join("omni-pid"))
            .map_err(|error| format!("publish omni PID: {error}"))?;
        self.wait_for("attached", INTERRUPT_STEP)
    }

    /// Types Ctrl+Break at omni's console and returns the processes that were attached to it.
    fn send_break(&mut self) -> Result<Vec<u32>, String> {
        fs::write(self.directory.join("send"), "")
            .map_err(|error| format!("request console break: {error}"))?;
        self.wait_for("sent", INTERRUPT_STEP)?;
        fs::read_to_string(self.directory.join("sent"))
            .map_err(|error| format!("read console processes: {error}"))?
            .lines()
            .map(|pid| {
                pid.trim()
                    .parse()
                    .map_err(|error| format!("console process {pid:?}: {error}"))
            })
            .collect()
    }

    fn wait_for(&mut self, marker: &str, timeout: Duration) -> Result<(), String> {
        let path = self.directory.join(marker);
        let deadline = Instant::now() + timeout;
        while !path.exists() {
            let exited = self
                .child
                .try_wait()
                .map_err(|error| format!("poll console break sender: {error}"))?;
            if exited.is_some() || Instant::now() >= deadline {
                return Err(format!(
                    "console break sender never reported `{marker}` (exit: {exited:?}):\n{}",
                    fs::read_to_string(self.directory.join("sender.log")).unwrap_or_default()
                ));
            }
            thread::sleep(Duration::from_millis(10));
        }
        Ok(())
    }
}

#[cfg(windows)]
impl Drop for ConsoleBreakSender {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
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
    // Absorb any first-exec scan before omni's timeouts start. After one exec succeeds, no forked
    // child still holds a write descriptor, so omni's own execs cannot find the script busy.
    let output = output_after_write(Command::new(&path).arg("--version"));
    assert!(
        output.status.success(),
        "synthetic {name} version probe failed"
    );
    path
}

/// Runs a synthetic provider written moments ago. A child that a concurrent test forked while the
/// script was open for writing keeps that descriptor until it execs, and Linux refuses to run the
/// script meanwhile, so busy executables are retried briefly.
#[cfg(unix)]
fn output_after_write(command: &mut Command) -> std::process::Output {
    for _ in 0..50 {
        match command.output() {
            Err(error) if error.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                thread::sleep(Duration::from_millis(20));
            }
            result => return result.expect("run synthetic provider"),
        }
    }
    command.output().expect("run synthetic provider")
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

fn wait_for_file(path: &Path, timeout: Duration) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    while !path.exists() {
        if Instant::now() >= deadline {
            return Err(format!("{} never appeared", path.display()));
        }
        thread::sleep(Duration::from_millis(10));
    }
    Ok(())
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
