#![cfg(unix)]

//! Drives native Codex import failures end to end through a synthetic `codex` binary.

use std::{
    fs,
    io::Read,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
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

// Synthetic Codex CLI. It passes the version gate, answers just enough app-server JSON-RPC to import
// a thread whose visible turns echo the imported history, and records any provider launch. The
// rollout it writes diverges from that history, so OmniSession's store read-back always fails.
// `FAKE_CODEX_IMPORT` breaks the import request, `FAKE_CODEX_READ=empty` breaks in-server turn
// verification, and `FAKE_CODEX_DELETE` breaks rollback.
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
        while IFS= read -r record; do
            text=${record#*'"message":{"content":'}
            text=${text%%',"role":'*}
            case "$record" in
            *'"type":"assistant"}') item='{"type":"agentMessage","text":'"$text"'}' ;;
            *) item='{"type":"userMessage","content":[{"type":"text","text":'"$text"'}]}' ;;
            esac
            items=${items:+$items,}$item
        done < "$source"
        printf '%s' "$items" > "$capture/turn-items"
        /bin/mkdir -p "${rollout%/*}"
        printf '{"type":"session_meta","timestamp":"2026-01-01T00:00:00Z","payload":{"id":"%s"}}\n{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Synthetic divergent history"}]}}\n' "$FAKE_CODEX_THREAD" > "$rollout"
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

#[test]
fn readback_failure_rolls_back_and_launches_semantic_handoff() {
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
            PathBuf::from(handoff).starts_with(fixture.root.join("state/handoffs")),
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
        assert_eq!(
            cwd.trim(),
            fixture.workspace.to_str().expect("UTF-8 workspace")
        );
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
            fixture.assert_nothing_left_behind(&label);
        }
    }
}

#[test]
fn broken_app_server_ends_within_bound_without_partial_state() {
    for (mode, reason) in [
        ("hang", "Codex app-server timed out or disconnected"),
        ("disconnect", "Codex app-server timed out or disconnected"),
        ("malformed", "invalid Codex app-server response"),
    ] {
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
    for route in Route::ALL {
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
        let root = temporary.path().canonicalize().expect("canonical fixture");
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
        command
            .env_clear()
            .current_dir(&self.workspace)
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.root.join("home"))
            .env("USERPROFILE", self.root.join("home"))
            .env("TMPDIR", self.root.join("tmp"))
            .env("XDG_CONFIG_HOME", self.root.join("xdg/config"))
            .env("XDG_DATA_HOME", self.root.join("xdg/data"))
            .env("XDG_STATE_HOME", self.root.join("xdg/state"))
            .env("XDG_CACHE_HOME", self.root.join("xdg/cache"))
            .env("OMNISESSION_HOME", self.root.join("state"))
            .env("CLAUDE_CONFIG_DIR", self.root.join("claude"))
            .env("CODEX_HOME", self.root.join("codex"))
            .env("OMNI_NO_UPDATE_CHECK", "1")
            .env("OMNI_CODEX_BIN", synthetic_codex())
            .env("FAKE_CODEX_CAPTURE", &self.capture)
            .env("FAKE_CODEX_THREAD", THREAD_ID);
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
        let started = Instant::now();
        let mut child = self
            .command()
            .args(args)
            .envs(environment.iter().copied())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("launch omni");
        let stdout = drain(child.stdout.take().expect("omni stdout"));
        let stderr = drain(child.stderr.take().expect("omni stderr"));
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
            .expect("isolated TMPDIR")
            .map(|entry| entry.expect("TMPDIR entry").file_name())
            .collect::<Vec<_>>();
        assert!(
            temporary.is_empty(),
            "{label} left isolated import sources behind: {temporary:?}"
        );
        let pids = fs::read_to_string(self.capture.join("server-pids")).unwrap_or_default();
        for pid in pids.lines() {
            let alive = Command::new("kill")
                .args(["-0", pid])
                .stderr(Stdio::null())
                .status()
                .expect("probe synthetic app-server")
                .success();
            assert!(!alive, "{label}: synthetic app-server {pid} outlived omni");
        }
    }
}

// Reuses one unchanged script. Endpoint security can hold a new executable's first exec for
// seconds, which would trip omni's 5-second version probe.
fn synthetic_codex() -> &'static Path {
    static CODEX: OnceLock<PathBuf> = OnceLock::new();
    CODEX.get_or_init(|| {
        let directory = Path::new(env!("CARGO_TARGET_TMPDIR")).join("native_import_fallback");
        fs::create_dir_all(&directory).expect("synthetic Codex directory");
        let path = directory.join("codex");
        if fs::read_to_string(&path).ok().as_deref() != Some(FAKE_CODEX) {
            let staged =
                tempfile::NamedTempFile::new_in(&directory).expect("stage synthetic Codex");
            fs::write(staged.path(), FAKE_CODEX).expect("write synthetic Codex");
            fs::set_permissions(staged.path(), fs::Permissions::from_mode(0o700))
                .expect("make synthetic Codex executable");
            staged.persist(&path).expect("install synthetic Codex");
        }
        // Absorb any first-exec scan before omni's timeouts start.
        let output = Command::new(&path)
            .arg("--version")
            .output()
            .expect("prime synthetic Codex");
        assert!(
            output.status.success(),
            "synthetic Codex version probe failed"
        );
        path
    })
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
