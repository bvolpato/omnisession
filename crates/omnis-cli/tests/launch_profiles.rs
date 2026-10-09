#![cfg(unix)]

//! Launch profiles from local configuration, driven end to end through the CLI.
//!
//! The profile file lives in the `OmniSession` state directory, so each test writes its own in a
//! temporary directory. The Claude store is synthetic. Transfers use `--dry-run`, which prints the
//! command that a profile builds, except one test that starts a synthetic program to prove that
//! arguments and environment changes reach a real process.

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::{Command, Output},
};

use serde_json::{Value, json};

const SESSION_ID: &str = "11111111-1111-4111-8111-111111111111";
/// A value that looks like a credential. It must reach the launched program and nothing else.
const CREDENTIAL: &str = "gateway-credential-0123456789abcdef";
const GATEWAY_URL: &str = "http://10.1.2.3:4";

/// Records what the launched program received, one file for each fact. Its `--help` lists one
/// permission flag, as an agent does, and is answered before anything is recorded.
const WRAPPER: &str = r#"#!/bin/sh
if [ "$1" = "--help" ]; then
    echo "  --dangerously-skip-permissions  run every command without asking"
    exit 0
fi
for argument do printf '%s\000' "$argument" >> "$CAPTURE/args"; done
pwd > "$CAPTURE/cwd"
printf '%s' "${ANTHROPIC_BASE_URL-unset}" > "$CAPTURE/base-url"
printf '%s' "${ANTHROPIC_AUTH_TOKEN-unset}" > "$CAPTURE/token"
printf '%s' "${ANTHROPIC_API_KEY-unset}" > "$CAPTURE/api-key"
"#;

const PROFILES: &str = r#"
[profiles.claude-local]
label = "Claude Code (local gateway)"
agent = "claude"
args = ["--settings", "${HOME}/gateway-settings.json"]
unset = ["ANTHROPIC_API_KEY"]

[profiles.claude-local.env]
ANTHROPIC_BASE_URL = "${GATEWAY_URL:-http://127.0.0.1:9999}"
ANTHROPIC_AUTH_TOKEN = "${GATEWAY_TOKEN:-local-token}"

[profiles.wrapped]
agent = "claude"
program = "${ROOT}/bin/wrapper"
args = ["--wrapped"]
unset = ["ANTHROPIC_API_KEY"]

[profiles.wrapped.env]
ANTHROPIC_BASE_URL = "${GATEWAY_URL:-http://127.0.0.1:9999}"
ANTHROPIC_AUTH_TOKEN = "${GATEWAY_TOKEN:-local-token}"

[profiles.ghost]
agent = "codex"

[profiles.lost]
agent = "claude"
program = "${HOME}/no-such-program"
"#;

struct Fixture {
    _temporary: tempfile::TempDir,
    root: PathBuf,
    workspace: PathBuf,
    capture: PathBuf,
}

impl Fixture {
    /// `profiles` may use `${ROOT}`, which becomes the fixture directory. `None` writes no file.
    fn new(profiles: Option<&str>) -> Self {
        let temporary = tempfile::tempdir().expect("temporary fixture");
        let root = temporary.path().canonicalize().expect("canonical fixture");
        let fixture = Self {
            workspace: root.join("workspace"),
            capture: root.join("capture"),
            root,
            _temporary: temporary,
        };
        for directory in [
            fixture.root.join("home"),
            fixture.root.join("state"),
            fixture.root.join("bin"),
            fixture.root.join("claude/projects/synthetic"),
            fixture.workspace.clone(),
            fixture.capture.clone(),
        ] {
            fs::create_dir_all(directory).expect("fixture directory");
        }
        fs::write(
            fixture
                .root
                .join(format!("claude/projects/synthetic/{SESSION_ID}.jsonl")),
            json!({
                "type": "user", "sessionId": SESSION_ID, "uuid": "synthetic-user",
                "cwd": fixture.workspace, "timestamp": "2026-01-01T00:00:00Z",
                "message": {"role": "user", "content": "Synthetic profile request"}
            })
            .to_string(),
        )
        .expect("synthetic session");
        fs::write(
            fixture.root.join("claude/history.jsonl"),
            json!({
                "sessionId": SESSION_ID, "project": fixture.workspace,
                "timestamp": 1_767_225_600_000_i64
            })
            .to_string(),
        )
        .expect("synthetic history");
        fixture.executable("bin/wrapper", WRAPPER);
        fixture.executable("bin/claude", "#!/bin/sh\nexit 0\n");
        if let Some(text) = profiles {
            fixture.write_profiles(&text.replace("${ROOT}", &fixture.root.display().to_string()));
        }
        fixture
    }

    fn executable(&self, relative: &str, content: &str) {
        let path = self.root.join(relative);
        fs::write(&path, content).expect("write program");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).expect("make executable");
        // Run it once before omni does, so omni never finds the new script busy.
        output_after_write(Command::new(&path).env("CAPTURE", &self.capture));
    }

    fn profiles_path(&self) -> PathBuf {
        self.root.join("state/profiles.toml")
    }

    fn write_profiles(&self, text: &str) {
        fs::write(self.profiles_path(), text).expect("write profile file");
        fs::set_permissions(self.profiles_path(), fs::Permissions::from_mode(0o600))
            .expect("private profile file");
    }

    fn source() -> String {
        format!("claude:{SESSION_ID}")
    }

    fn home(&self) -> String {
        self.root.join("home").display().to_string()
    }

    fn omni(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_omni"));
        command
            .env_clear()
            .current_dir(&self.workspace)
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.root.join("home"))
            .env("OMNISESSION_HOME", self.root.join("state"))
            .env("CLAUDE_CONFIG_DIR", self.root.join("claude"))
            .env("CODEX_HOME", self.root.join("codex"))
            .env("OMNI_CLAUDE_BIN", self.root.join("bin/claude"))
            .env("OMNI_CODEX_BIN", self.root.join("missing-codex"))
            .env("OMNI_NO_UPDATE_CHECK", "1")
            .env("CAPTURE", &self.capture)
            .env("GATEWAY_URL", GATEWAY_URL)
            .env("GATEWAY_TOKEN", CREDENTIAL)
            // Set in the parent, so a profile that unsets it proves that removal works.
            .env("ANTHROPIC_API_KEY", "inherited-key");
        // `env_clear` also drops the coverage setting, so pass it on when the suite runs under
        // `cargo llvm-cov`. Otherwise the code that these tests run in `omni` is not counted.
        if let Some(coverage) = std::env::var_os("LLVM_PROFILE_FILE") {
            command.env("LLVM_PROFILE_FILE", coverage);
        }
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.omni().args(args).output().expect("run omni")
    }

    fn succeeds(&self, args: &[&str]) -> Output {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "omni {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn json(&self, args: &[&str]) -> Value {
        let mut all = vec!["--json"];
        all.extend_from_slice(args);
        let output = self.succeeds(&all);
        assert_no_credential(&output);
        serde_json::from_slice(&output.stdout).expect("JSON output")
    }

    /// The error text of a command that must fail.
    fn fails(&self, args: &[&str]) -> String {
        let output = self.run(args);
        assert!(!output.status.success(), "omni {args:?} should fail");
        assert_no_credential(&output);
        String::from_utf8_lossy(&output.stderr).into_owned()
    }

    fn captured(&self, name: &str) -> String {
        fs::read_to_string(self.capture.join(name)).unwrap_or_else(|_| panic!("capture {name}"))
    }
}

/// Runs a script written moments ago. A child that a concurrent test forked while the script was
/// open for writing keeps that descriptor until it execs, and Linux refuses to run the script
/// meanwhile, so busy executables are retried briefly.
fn output_after_write(command: &mut Command) -> Output {
    for _ in 0..50 {
        match command.output() {
            Err(error) if error.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            result => return result.expect("run synthetic program"),
        }
    }
    command.output().expect("run synthetic program")
}

fn assert_no_credential(output: &Output) {
    for stream in [&output.stdout, &output.stderr] {
        assert!(
            !String::from_utf8_lossy(stream).contains(CREDENTIAL),
            "a credential reached the output"
        );
    }
}

fn env_entry<'a>(launch: &'a Value, name: &str) -> &'a Value {
    launch["env"]
        .as_array()
        .expect("environment changes")
        .iter()
        .find(|entry| entry["name"] == name)
        .unwrap_or_else(|| panic!("no change for {name}: {launch}"))
}

fn strings(value: &Value) -> Vec<&str> {
    value
        .as_array()
        .expect("string list")
        .iter()
        .map(|item| item.as_str().expect("string"))
        .collect()
}

#[test]
fn profile_adds_arguments_and_environment_changes_to_a_dry_run() {
    let fixture = Fixture::new(Some(PROFILES));
    let plan = fixture.json(&[
        "resume",
        &Fixture::source(),
        "--in",
        "claude-local",
        "--no-fork",
        "--dry-run",
    ]);

    assert_eq!(plan["target"], "claude");
    assert_eq!(plan["profile"], "claude-local");
    let launch = &plan["launch"];
    assert_eq!(launch["program"], "claude");
    assert_eq!(
        strings(&launch["args"]),
        [
            "--settings",
            &format!("{}/gateway-settings.json", fixture.home()),
            "--resume",
            SESSION_ID
        ]
    );
    assert_eq!(
        env_entry(launch, "ANTHROPIC_API_KEY"),
        &json!({"name": "ANTHROPIC_API_KEY", "unset": true})
    );
    assert_eq!(
        env_entry(launch, "ANTHROPIC_BASE_URL")["value"],
        GATEWAY_URL
    );
    // The value is a credential by its name, so output shows a placeholder.
    let token = env_entry(launch, "ANTHROPIC_AUTH_TOKEN")["value"]
        .as_str()
        .expect("token value");
    assert!(token.contains("REDACTED"), "{token}");
}

#[test]
fn text_dry_run_shows_the_environment_as_arguments_of_env() {
    let fixture = Fixture::new(Some(PROFILES));
    let output = fixture.succeeds(&[
        "resume",
        &Fixture::source(),
        "--in",
        "claude-local",
        "--no-fork",
        "--dry-run",
    ]);
    assert_no_credential(&output);
    let text = String::from_utf8_lossy(&output.stdout);
    let launch = text
        .lines()
        .find(|line| line.starts_with("Launch: "))
        .unwrap_or_else(|| panic!("no launch line: {text}"));
    assert!(
        launch.starts_with(r#"Launch: "env" "-u" "ANTHROPIC_API_KEY""#),
        "{launch}"
    );
    assert!(
        launch.contains(&format!(r#""ANTHROPIC_BASE_URL={GATEWAY_URL}""#)),
        "{launch}"
    );
    assert!(
        launch.ends_with(&format!(
            r#""claude" "--settings" "{}/gateway-settings.json" "--resume" "{SESSION_ID}""#,
            fixture.home()
        )),
        "{launch}"
    );
}

#[test]
fn built_in_agent_launches_without_a_profile() {
    let fixture = Fixture::new(Some(PROFILES));
    let plan = fixture.json(&[
        "resume",
        &Fixture::source(),
        "--in",
        "claude",
        "--no-fork",
        "--dry-run",
    ]);
    assert_eq!(plan["profile"], Value::Null);
    assert_eq!(plan["launch"]["env"], json!([]));
    assert_eq!(strings(&plan["launch"]["args"]), ["--resume", SESSION_ID]);
}

#[test]
fn fork_through_a_profile_keeps_the_agent_fork_flag() {
    let fixture = Fixture::new(Some(PROFILES));
    let plan = fixture.json(&[
        "fork",
        &Fixture::source(),
        "--in",
        "claude-local",
        "--dry-run",
    ]);
    assert_eq!(plan["profile"], "claude-local");
    let args = strings(&plan["launch"]["args"]);
    assert_eq!(
        &args[..2],
        [
            "--settings",
            &format!("{}/gateway-settings.json", fixture.home())
        ]
    );
    assert_eq!(&args[2..], ["--resume", SESSION_ID, "--fork-session"]);
}

#[test]
fn switch_to_a_profile_resumes_the_bound_session() {
    let fixture = Fixture::new(Some(PROFILES));
    fixture.json(&["task", "start", "continuity", "--from", &Fixture::source()]);
    let plan = fixture.json(&["switch", "claude-local", "--dry-run"]);
    assert_eq!(plan["profile"], "claude-local");
    let args = strings(&plan["launch"]["args"]);
    assert!(args.contains(&"--settings"), "{args:?}");
    assert!(args.contains(&SESSION_ID), "{args:?}");
    assert!(!args.contains(&"--fork-session"), "{args:?}");
}

#[test]
fn profile_program_replaces_the_agent_command() {
    let fixture = Fixture::new(Some(PROFILES));
    let plan = fixture.json(&[
        "resume",
        &Fixture::source(),
        "--in",
        "wrapped",
        "--no-fork",
        "--dry-run",
    ]);
    assert_eq!(
        plan["launch"]["program"],
        fixture.root.join("bin/wrapper").display().to_string()
    );
    assert_eq!(
        strings(&plan["launch"]["args"]),
        ["--wrapped", "--resume", SESSION_ID]
    );
}

#[test]
fn launched_program_receives_the_arguments_and_the_real_environment() {
    let fixture = Fixture::new(Some(PROFILES));
    let output = fixture.succeeds(&["resume", &Fixture::source(), "--in", "wrapped", "--no-fork"]);
    assert_no_credential(&output);

    assert_eq!(
        fixture.captured("args"),
        format!("--wrapped\0--resume\0{SESSION_ID}\0")
    );
    assert_eq!(fixture.captured("base-url"), GATEWAY_URL);
    // Output hides the value. The program gets the real one.
    assert_eq!(fixture.captured("token"), CREDENTIAL);
    assert_eq!(fixture.captured("api-key"), "unset");
    assert_eq!(
        fixture.captured("cwd").trim(),
        fixture.workspace.display().to_string()
    );
}

#[test]
fn cross_agent_import_reports_the_profile_of_its_target() {
    let fixture = Fixture::new(Some(PROFILES));
    // An agent that passes the version gate, so the import route is the one that a person gets.
    fixture.executable(
        "bin/codex",
        "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo 'codex-cli 0.146.0'; fi\nexit 0\n",
    );
    let dry_run = |target: &str| {
        let mut command = fixture.omni();
        command
            .env("OMNI_CODEX_BIN", fixture.root.join("bin/codex"))
            .args([
                "--json",
                "resume",
                &Fixture::source(),
                "--in",
                target,
                "--dry-run",
            ]);
        let output = command.output().expect("run omni");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<Value>(&output.stdout).expect("JSON output")
    };

    // A dry run imports nothing. The report names the agent and the profile that starts it.
    let through_profile = dry_run("ghost");
    assert_eq!(through_profile["target"], "codex");
    assert_eq!(through_profile["profile"], "ghost");
    assert_eq!(through_profile["dry_run"], true);

    let built_in = dry_run("codex");
    assert_eq!(built_in["target"], "codex");
    assert_eq!(built_in["profile"], Value::Null);
}

#[test]
fn permission_mode_flags_come_from_the_program_that_runs() {
    let fixture = Fixture::new(Some(PROFILES));
    let source = Fixture::source();

    // The wrapper lists the yolo flag, and the plain agent in the fixture lists nothing.
    let wrapped = fixture.json(&[
        "resume",
        &source,
        "--in",
        "wrapped",
        "--no-fork",
        "--mode",
        "yolo",
        "--dry-run",
    ]);
    assert_eq!(
        strings(&wrapped["launch"]["args"]),
        [
            "--dangerously-skip-permissions",
            "--wrapped",
            "--resume",
            SESSION_ID
        ]
    );
    let plain = fixture.json(&[
        "resume",
        &source,
        "--in",
        "claude",
        "--no-fork",
        "--mode",
        "yolo",
        "--dry-run",
    ]);
    assert_eq!(strings(&plain["launch"]["args"]), ["--resume", SESSION_ID]);

    // The agent does not have to be installed for a profile that brings its own program.
    let mut command = fixture.omni();
    command
        .env("OMNI_CLAUDE_BIN", fixture.root.join("missing-claude"))
        .args([
            "--json",
            "resume",
            &source,
            "--in",
            "wrapped",
            "--no-fork",
            "--mode",
            "yolo",
            "--dry-run",
        ]);
    let output = command.output().expect("run omni");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let missing: Value = serde_json::from_slice(&output.stdout).expect("JSON output");
    assert_eq!(
        strings(&missing["launch"]["args"]),
        [
            "--dangerously-skip-permissions",
            "--wrapped",
            "--resume",
            SESSION_ID
        ]
    );
}

#[test]
fn mode_that_the_wrapper_lacks_is_reported_for_the_wrapper() {
    let fixture = Fixture::new(Some(PROFILES));
    // The wrapper lists only the yolo flag, so `auto` steps down to `default`, and the warning
    // names the profile program instead of the installed agent.
    let output = fixture.succeeds(&[
        "resume",
        &Fixture::source(),
        "--in",
        "wrapped",
        "--no-fork",
        "--mode",
        "auto",
        "--dry-run",
    ]);
    let text = String::from_utf8_lossy(&output.stderr);
    assert!(
        text.contains("the program of launch profile `wrapped`") && text.contains("no `auto` mode"),
        "{text}"
    );
}

#[test]
fn unknown_name_lists_the_profiles_that_exist() {
    let fixture = Fixture::new(Some(PROFILES));
    let error = fixture.fails(&["resume", &Fixture::source(), "--in", "nothing", "--dry-run"]);
    assert!(
        error.contains("unknown agent or launch profile `nothing`"),
        "{error}"
    );
    for name in ["claude-local", "wrapped", "ghost", "lost"] {
        assert!(error.contains(name), "{error}");
    }
}

#[test]
fn profile_with_an_unset_variable_stops_before_anything_runs() {
    let fixture = Fixture::new(Some(
        r#"
[profiles.needs-token]
agent = "claude"
[profiles.needs-token.env]
ANTHROPIC_AUTH_TOKEN = "${NO_SUCH_GATEWAY_TOKEN}"
"#,
    ));
    let error = fixture.fails(&[
        "resume",
        &Fixture::source(),
        "--in",
        "needs-token",
        "--no-fork",
    ]);
    assert!(
        error.contains("launch profile `needs-token` cannot start"),
        "{error}"
    );
    assert!(error.contains("omni profiles"), "{error}");
    assert!(error.contains("NO_SUCH_GATEWAY_TOKEN"), "{error}");
    assert!(!fixture.capture.join("args").exists());
}

#[test]
fn profile_with_a_missing_program_stops_before_anything_runs() {
    let fixture = Fixture::new(Some(PROFILES));
    let source = Fixture::source();
    for dry_run in [true, false] {
        let mut args = vec!["resume", source.as_str(), "--in", "lost", "--no-fork"];
        if dry_run {
            args.push("--dry-run");
        }
        let error = fixture.fails(&args);
        assert!(
            error.contains("launch profile `lost` cannot start"),
            "{error}"
        );
        assert!(error.contains("no-such-program"), "{error}");
        assert!(error.contains("omni profiles"), "{error}");
    }
    assert!(!fixture.capture.join("args").exists());
}

#[test]
fn broken_profile_file_does_not_stop_built_in_agents() {
    let fixture = Fixture::new(Some("[profiles.broken\n"));
    let plan = fixture.json(&[
        "resume",
        &Fixture::source(),
        "--in",
        "claude",
        "--no-fork",
        "--dry-run",
    ]);
    assert_eq!(plan["profile"], Value::Null);

    let error = fixture.fails(&[
        "resume",
        &Fixture::source(),
        "--in",
        "claude-local",
        "--dry-run",
    ]);
    assert!(error.contains("could not be read"), "{error}");
    assert!(error.contains("profiles.toml"), "{error}");
}

#[test]
fn no_profile_file_means_only_built_in_agents() {
    let fixture = Fixture::new(None);
    let error = fixture.fails(&[
        "resume",
        &Fixture::source(),
        "--in",
        "claude-local",
        "--dry-run",
    ]);
    assert!(
        error.contains("unknown agent or launch profile `claude-local`"),
        "{error}"
    );
    assert!(!error.contains("profiles:"), "{error}");

    let listed = fixture.json(&["profiles"]);
    assert_eq!(listed["profiles"], json!([]));
}

#[test]
fn profiles_command_reports_readiness_without_values() {
    let fixture = Fixture::new(Some(PROFILES));
    let listed = fixture.json(&["profiles"]);
    assert_eq!(
        listed["file"],
        fixture.profiles_path().display().to_string()
    );
    let by_name = |name: &str| {
        listed["profiles"]
            .as_array()
            .expect("profiles")
            .iter()
            .find(|profile| profile["name"] == name)
            .unwrap_or_else(|| panic!("no profile {name}: {listed}"))
    };
    let local = by_name("claude-local");
    assert_eq!(local["ready"], true);
    assert_eq!(local["label"], "Claude Code (local gateway)");
    assert_eq!(
        local["sets"],
        json!(["ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_BASE_URL"])
    );
    assert_eq!(by_name("wrapped")["ready"], true);
    let ghost = by_name("ghost");
    assert_eq!(ghost["ready"], false);
    assert!(
        ghost["problem"]
            .as_str()
            .expect("problem")
            .contains("`codex` is not installed"),
        "{ghost}"
    );
    let lost = by_name("lost");
    assert_eq!(lost["ready"], false);
    assert!(
        lost["problem"]
            .as_str()
            .expect("problem")
            .contains("no-such-program"),
        "{lost}"
    );
    // Neither a value nor the default of a value is listed.
    let text = listed.to_string();
    assert!(
        !text.contains("local-token") && !text.contains("gateway-settings"),
        "{text}"
    );

    let output = fixture.succeeds(&["profiles"]);
    assert_no_credential(&output);
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        text.contains("claude-local  Claude Code (local gateway)"),
        "{text}"
    );
    assert!(text.contains("    ready"), "{text}");
    assert!(
        text.contains("    not ready: the agent `codex` is not installed"),
        "{text}"
    );
}

#[test]
fn profile_file_that_other_users_can_write_is_refused() {
    let fixture = Fixture::new(Some(PROFILES));
    fs::set_permissions(fixture.profiles_path(), fs::Permissions::from_mode(0o666))
        .expect("loosen permissions");
    let error = fixture.fails(&["profiles"]);
    assert!(error.contains("chmod go-w"), "{error}");
}

#[test]
fn profiles_cannot_change_omni_variables_or_where_sessions_are() {
    for variable in ["OMNI_MODE", "OMNISESSION_HOME", "HOME", "CLAUDE_CONFIG_DIR"] {
        let fixture = Fixture::new(Some(&format!(
            "[profiles.sneaky]\nagent = \"claude\"\n[profiles.sneaky.env]\n{variable} = \"x\"\n"
        )));
        let error = fixture.fails(&["profiles"]);
        assert!(error.contains("reserved"), "{variable}: {error}");
        assert!(error.contains("profile `sneaky`"), "{variable}: {error}");
    }
}

#[test]
fn handoff_to_another_agent_starts_it_through_the_profile() {
    let fixture = Fixture::new(Some(
        r#"
[profiles.codex-gateway]
agent = "codex"
args = ["--profile", "gateway"]
unset = ["OPENAI_API_KEY"]

[profiles.codex-gateway.env]
OPENAI_BASE_URL = "${GATEWAY_URL:-http://127.0.0.1:9999}"
"#,
    ));
    // The fixture has no Codex binary, so the move is a semantic handoff that starts a new session.
    let dry_run =
        |target: &str| fixture.json(&["resume", &Fixture::source(), "--in", target, "--dry-run"]);

    let through_profile = dry_run("codex-gateway");
    assert_eq!(through_profile["profile"], "codex-gateway");
    let launch = &through_profile["launch"];
    assert_eq!(launch["program"], "codex");
    assert_eq!(strings(&launch["args"])[..2], ["--profile", "gateway"]);
    assert_eq!(env_entry(launch, "OPENAI_BASE_URL")["value"], GATEWAY_URL);
    assert_eq!(env_entry(launch, "OPENAI_API_KEY")["unset"], true);

    let built_in = dry_run("codex");
    assert_eq!(built_in["profile"], Value::Null);
    assert_eq!(built_in["launch"]["env"], json!([]));
    assert!(!strings(&built_in["launch"]["args"]).contains(&"--profile"));
}

#[test]
fn credential_in_an_argument_is_hidden_from_output_and_reaches_the_program() {
    // Neither variable name looks like a credential, so only the value decides what is hidden.
    let fixture = Fixture::new(Some(
        r#"
[profiles.keyed]
agent = "claude"
program = "${ROOT}/bin/wrapper"
args = ["--api-key", "${GATEWAY_TOKEN}"]

[profiles.keyed.env]
GATEWAY_HEADER = "x-${GATEWAY_TOKEN}"
"#,
    ));
    let source = Fixture::source();
    let dry_run = [
        "resume",
        source.as_str(),
        "--in",
        "keyed",
        "--no-fork",
        "--dry-run",
    ];

    let plan = fixture.json(&dry_run);
    assert_eq!(
        strings(&plan["launch"]["args"]),
        ["--api-key", "[REDACTED]", "--resume", SESSION_ID]
    );
    assert_eq!(
        env_entry(&plan["launch"], "GATEWAY_HEADER")["value"],
        "x-[REDACTED]"
    );
    let text = fixture.succeeds(&dry_run);
    assert_no_credential(&text);
    assert!(String::from_utf8_lossy(&text.stdout).contains("[REDACTED]"));

    let output = fixture.succeeds(&["resume", &source, "--in", "keyed", "--no-fork"]);
    assert_no_credential(&output);
    assert_eq!(
        fixture.captured("args"),
        format!("--api-key\0{CREDENTIAL}\0--resume\0{SESSION_ID}\0")
    );
}

#[test]
fn syntax_error_never_quotes_a_credential_from_the_file() {
    let fixture = Fixture::new(Some(&format!(
        "[profiles.broken]\nagent = \"claude\"\nargs = [\"--api-key\", {CREDENTIAL}]\n"
    )));
    let error = fixture.fails(&["profiles"]);
    assert!(error.contains("line 3"), "{error}");
    assert!(error.contains("profiles.toml"), "{error}");
}

#[test]
fn launch_runs_the_program_that_omni_checked_not_the_one_the_profile_path_finds() {
    let fixture = Fixture::new(Some(
        r#"
[profiles.twin]
agent = "claude"
program = "twin"

[profiles.twin.env]
PATH = "${ROOT}/b:/usr/bin:/bin"
"#,
    ));
    // Both directories hold a `twin`. Each marks itself when omni starts it, which is when the
    // first argument is `--resume`. The check that ran first found the one in `a`.
    for (directory, mark) in [("a", "A"), ("b", "B")] {
        fs::create_dir_all(fixture.root.join(directory)).expect("directory");
        fixture.executable(
            &format!("{directory}/twin"),
            &format!(
                "#!/bin/sh\nif [ \"$1\" = \"--resume\" ]; then printf {mark} > \"$CAPTURE/which\"; fi\nexit 0\n"
            ),
        );
    }
    let mut command = fixture.omni();
    command
        .env(
            "PATH",
            format!("{}/a:/usr/bin:/bin", fixture.root.display()),
        )
        .args(["resume", &Fixture::source(), "--in", "twin", "--no-fork"]);
    let output = command.output().expect("run omni");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fixture.captured("which"), "A");
}

#[test]
fn program_named_like_an_agent_runs_the_agent_and_not_what_path_finds() {
    let fixture = Fixture::new(Some(
        r#"
[profiles.named]
agent = "claude"
program = "claude"
args = ["--named"]
"#,
    ));
    // `OMNI_CLAUDE_BIN` names the real agent, and `PATH` holds another `claude`, as the provider
    // shim directory does. The profile must start the agent that `omni` itself starts.
    let mark = |name: &str| {
        format!(
            "#!/bin/sh\nif [ \"$1\" = \"--named\" ]; then printf {name} > \"$CAPTURE/which\"; fi\nexit 0\n"
        )
    };
    fixture.executable("bin/claude", &mark("agent"));
    fs::create_dir_all(fixture.root.join("shims")).expect("directory");
    fixture.executable("shims/claude", &mark("path"));

    let mut command = fixture.omni();
    command
        .env(
            "PATH",
            format!("{}/shims:/usr/bin:/bin", fixture.root.display()),
        )
        .args(["resume", &Fixture::source(), "--in", "named", "--no-fork"]);
    let output = command.output().expect("run omni");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fixture.captured("which"), "agent");
}

#[test]
fn native_import_into_another_agent_launches_through_the_profile() {
    let fixture = Fixture::new(Some(
        r#"
[profiles.pi-wrapped]
agent = "pi"
program = "${ROOT}/bin/wrapper"
args = ["--wrapped"]
"#,
    ));
    // The installed Pi writes the imported session. The profile only decides how Pi starts, so
    // the wrapper runs after the import and records what it was given.
    fixture.executable(
        "bin/pi",
        "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo 0.99.0; fi\nexit 0\n",
    );
    let store = fixture.root.join("pi-store");
    let mut command = fixture.omni();
    command
        .env("OMNI_PI_BIN", fixture.root.join("bin/pi"))
        .env("PI_CODING_AGENT_DIR", &store)
        .args(["resume", &Fixture::source(), "--in", "pi-wrapped"]);
    let output = command.output().expect("run omni");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_no_credential(&output);

    let imported = walkdir::WalkDir::new(&store)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .count();
    assert!(imported > 0, "the import wrote no session");
    let args = fixture.captured("args");
    assert!(args.starts_with("--wrapped\0"), "{args:?}");
}

#[test]
fn forking_in_the_source_agent_does_not_carry_the_profile_of_the_target() {
    let fixture = Fixture::new(Some(
        r#"
[profiles.pi-wrapped]
agent = "pi"
program = "${ROOT}/bin/wrapper"
args = ["--wrapped"]
"#,
    ));
    // A Pi that is too old, so the native import fails and omni asks what to do instead.
    fixture.executable(
        "bin/pi",
        "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo 0.0.1; fi\nexit 0\n",
    );
    fixture.executable(
        "bin/claude",
        "#!/bin/sh\nfor argument do printf '%s\\000' \"$argument\" >> \"$CAPTURE/claude-args\"; done\nexit 0\n",
    );
    let answers = fixture.root.join("answers");
    fs::write(&answers, "f\n").expect("write answers");
    let mut command = fixture.omni();
    command
        .env("OMNI_PI_BIN", fixture.root.join("bin/pi"))
        .env("PI_CODING_AGENT_DIR", fixture.root.join("pi-store"))
        .env("OMNI_TEST_IMPORT_CHOICES", "1")
        .stdin(std::fs::File::open(&answers).expect("open answers"))
        .args(["resume", &Fixture::source(), "--in", "pi-wrapped"]);
    let output = command.output().expect("run omni");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    // The fork starts Claude itself. The wrapper that was chosen for Pi never runs.
    let claude = fixture.captured("claude-args");
    assert!(claude.contains("--fork-session"), "{claude:?}");
    assert!(!fixture.capture.join("args").exists(), "the wrapper ran");
}

#[test]
fn opencode_import_keeps_the_program_of_the_profile() {
    let fixture = Fixture::new(Some(
        r#"
[profiles.opencode-wrapped]
agent = "opencode"
program = "${ROOT}/bin/wrapper"
args = ["--wrapped"]
"#,
    ));
    // An OpenCode that lists one model, which is all that the import plan needs.
    fixture.executable(
        "bin/opencode",
        "#!/bin/sh\nif [ \"$2\" = \"models\" ]; then echo provider/model; fi\nexit 0\n",
    );
    let mut command = fixture.omni();
    command
        .env("OMNI_OPENCODE_BIN", fixture.root.join("bin/opencode"))
        .args([
            "--json",
            "resume",
            &Fixture::source(),
            "--in",
            "opencode-wrapped",
            "--dry-run",
        ]);
    let output = command.output().expect("run omni");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let plan: Value = serde_json::from_slice(&output.stdout).expect("JSON output");
    assert_eq!(plan["profile"], "opencode-wrapped");
    assert_eq!(
        plan["launch"]["program"],
        fixture.root.join("bin/wrapper").display().to_string()
    );
    assert_eq!(strings(&plan["launch"]["args"])[0], "--wrapped");
}

#[test]
fn cross_agent_profile_that_cannot_start_stops_before_the_import_writes() {
    let fixture = Fixture::new(Some(
        r#"
[profiles.pi-lost]
agent = "pi"
program = "${ROOT}/missing/pi-wrapper"

[profiles.pi-var]
agent = "pi"

[profiles.pi-var.env]
GATEWAY_REGION = "${NO_SUCH_REGION}"
"#,
    ));
    // A Pi that passes the version gate, so the import would write a session without the check.
    fixture.executable(
        "bin/pi",
        "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo 0.99.0; fi\nexit 0\n",
    );
    let store = fixture.root.join("pi-store");
    let resume_in = |target: &str| {
        let mut command = fixture.omni();
        command
            .env("OMNI_PI_BIN", fixture.root.join("bin/pi"))
            .env("PI_CODING_AGENT_DIR", &store)
            .args(["resume", &Fixture::source(), "--in", target]);
        command.output().expect("run omni")
    };
    let files = || {
        walkdir::WalkDir::new(&store)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_file())
            .count()
    };

    for profile in ["pi-lost", "pi-var"] {
        let output = resume_in(profile);
        assert!(!output.status.success(), "{profile}");
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(
            error.contains(&format!("launch profile `{profile}` cannot start")),
            "{profile}: {error}"
        );
        assert_eq!(files(), 0, "{profile} wrote a session");
    }

    // The same setup imports through the built-in agent, so the empty store above is the check.
    let output = resume_in("pi");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(files() > 0, "the control import wrote nothing");
}
