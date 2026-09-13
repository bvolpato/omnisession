//! Windows provider alias behavior that needs real processes and a console.
#![cfg(windows)]

use std::{
    env, fs,
    os::windows::{fs::OpenOptionsExt, process::CommandExt},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Output, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};

use wait_timeout::ChildExt;

const PROVIDER_CHILD_TEST: &str = "provider_child";
const READY_VARIABLE: &str = "OMNI_TEST_WINDOWS_CHILD_READY";
const RELEASE_VARIABLE: &str = "OMNI_TEST_WINDOWS_CHILD_RELEASE";
const BREAK_PARENT_VARIABLE: &str = "OMNI_TEST_WINDOWS_CHILD_BREAK_PARENT";
const INTERRUPTED_EXIT_CODE: i32 = 42;
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const FILE_SHARE_READ: u32 = 0x0000_0001;
const FILE_SHARE_WRITE: u32 = 0x0000_0002;
const TIMEOUT: Duration = Duration::from_secs(60);
const ALIASES: [&str; 8] = [
    "agy.exe",
    "claude.exe",
    "codex.exe",
    "cursor-agent.exe",
    "grok.exe",
    "hermes.exe",
    "opencode.exe",
    "pi.exe",
];

/// Sends Ctrl+Break to the process group led by the parent of `$env:OMNI_TEST_BREAK_CHILD`.
///
/// Calling `GenerateConsoleCtrlEvent` through PowerShell keeps this crate free of unsafe code.
const SEND_BREAK_SCRIPT: &str = "$ErrorActionPreference = 'Stop'; \
    $group = (Get-CimInstance -ClassName Win32_Process -Filter ('ProcessId = ' + $env:OMNI_TEST_BREAK_CHILD)).ParentProcessId; \
    $quote = [char]34; \
    Add-Type -Namespace OmniTest -Name Console -MemberDefinition ('[System.Runtime.InteropServices.DllImport(' + $quote + 'kernel32.dll' + $quote + ', SetLastError = true)] public static extern bool GenerateConsoleCtrlEvent(uint ctrlEvent, uint processGroupId);'); \
    if (-not [OmniTest.Console]::GenerateConsoleCtrlEvent(1, [uint32]$group)) { throw ('GenerateConsoleCtrlEvent failed: ' + [System.Runtime.InteropServices.Marshal]::GetLastWin32Error()) }";

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

/// Provider stand-in that `omni` launches.
///
/// Reports ready and, when asked, sends Ctrl+Break to its own console process group. Exits 42
/// after a console interrupt, or 0 once the release file exists.
#[test]
fn provider_child() {
    let Some(ready) = env::var_os(READY_VARIABLE) else {
        return;
    };
    ctrlc::set_handler(|| INTERRUPTED.store(true, Ordering::SeqCst))
        .expect("install provider interrupt handler");
    fs::write(ready, b"ready").expect("report provider ready");
    if env::var_os(BREAK_PARENT_VARIABLE).is_some() {
        send_break_to_parent_group();
    }
    let release = env::var_os(RELEASE_VARIABLE).map(PathBuf::from);
    let deadline = Instant::now() + TIMEOUT;
    while Instant::now() < deadline {
        if INTERRUPTED.load(Ordering::SeqCst) {
            // Outlive a parent that the same event would end without a handler of its own.
            thread::sleep(Duration::from_millis(500));
            std::process::exit(INTERRUPTED_EXIT_CODE);
        }
        if release.as_deref().is_some_and(Path::exists) {
            std::process::exit(0);
        }
        thread::sleep(Duration::from_millis(10));
    }
    eprintln!("provider child timed out");
    std::process::exit(3);
}

#[test]
fn provider_wait_survives_console_break_and_forwards_exit_code() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let log = directory.path().join("omni.log");
    let mut command = Command::new(env!("CARGO_BIN_EXE_omni"));
    command
        .args([
            "shim",
            "exec",
            "codex",
            "--",
            "--exact",
            PROVIDER_CHILD_TEST,
        ])
        .env("OMNISESSION_HOME", directory.path().join("state"))
        .env("OMNI_BYPASS", "1")
        .env("OMNI_CODEX_BIN", test_executable())
        .env(READY_VARIABLE, directory.path().join("ready"))
        .env(BREAK_PARENT_VARIABLE, "1")
        .env_remove(RELEASE_VARIABLE)
        // A hidden console of its own keeps the event away from this test, and a new process
        // group lets the provider target only `omni` and itself.
        .creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
    let mut omni = spawn_logged(&mut command, &log);
    let status = wait(&mut omni, TIMEOUT * 2);
    assert_eq!(
        status.code(),
        Some(INTERRUPTED_EXIT_CODE),
        "omni did not wait for the provider exit code:\n{}",
        read_log(&log)
    );
}

#[test]
fn shim_install_relinks_aliases_from_older_omni_builds() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let root = directory.path();
    let OlderBuildFixture {
        omni,
        older_omni,
        state,
        shims,
    } = OlderBuildFixture::new(root);

    // A provider running through an alias keeps the older image mapped during refresh.
    let ready = root.join("ready");
    let release = root.join("release");
    let log = root.join("running.log");
    let mut command = Command::new(shims.join("codex.exe"));
    command
        .args(["--exact", PROVIDER_CHILD_TEST])
        .env("OMNISESSION_HOME", &state)
        .env("OMNI_BYPASS", "1")
        .env("OMNI_CODEX_BIN", test_executable())
        .env(READY_VARIABLE, &ready)
        .env(RELEASE_VARIABLE, &release)
        .env_remove(BREAK_PARENT_VARIABLE);
    let mut running = spawn_logged(&mut command, &log);
    wait_for_file(&ready, &mut running, &log);

    let installed = run_shim(&omni, "install", &state);
    assert_success(&installed, "shim install over older aliases");
    assert!(
        String::from_utf8_lossy(&installed.stdout).contains("Relinked 8 provider aliases"),
        "{}",
        String::from_utf8_lossy(&installed.stdout)
    );
    assert_aliases_point_to(&shims, &omni);

    fs::write(&release, b"release").expect("release running provider");
    let status = wait(&mut running, TIMEOUT);
    assert!(
        status.success(),
        "running alias failed:\n{}",
        read_log(&log)
    );

    assert_success(
        &run_shim(&omni, "install", &state),
        "shim install after running alias exits",
    );
    assert_eq!(
        shim_names(&shims),
        ALIASES,
        "retired aliases were left behind"
    );

    for alias in ALIASES {
        fs::remove_file(shims.join(alias)).expect("remove current alias");
    }
    link_aliases(&older_omni, &shims);
    assert_success(
        &run_shim(&omni, "uninstall", &state),
        "shim uninstall of older aliases",
    );
    assert!(!shims.exists(), "shim directory remained after uninstall");

    fs::create_dir_all(&shims).expect("recreate shim directory");
    let foreign = shims.join("pi.exe");
    fs::copy(test_executable(), &foreign).expect("write foreign executable");
    let refused = run_shim(&omni, "install", &state);
    assert!(!refused.status.success(), "replaced a foreign executable");
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("unowned shim path"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
    assert!(!same_file::is_same_file(&foreign, &omni).expect("compare foreign alias"));
}

#[test]
fn shim_install_restores_earlier_aliases_when_a_later_relink_fails() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let fixture = OlderBuildFixture::new(directory.path());
    // `hermes.exe` is relinked last. A separate copy of the older build can be locked without
    // locking the hard links that every earlier alias shares.
    let last = fixture.shims.join("hermes.exe");
    fs::remove_file(&last).expect("unlink last alias");
    fs::copy(&fixture.older_omni, &last).expect("copy older build to last alias");
    let lock = fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .open(&last)
        .expect("open last alias without delete sharing");

    let failed = run_shim(&fixture.omni, "install", &fixture.state);
    assert!(
        !failed.status.success(),
        "shim install succeeded over a locked alias"
    );
    let stderr = String::from_utf8_lossy(&failed.stderr);
    assert!(
        stderr.contains("earlier alias changes were rolled back") && stderr.contains("hermes.exe"),
        "{stderr}"
    );
    for alias in ALIASES {
        let path = fixture.shims.join(alias);
        assert!(
            !same_file::is_same_file(&path, &fixture.omni).expect("compare alias"),
            "`{alias}` still links to the installed build after rollback"
        );
        if alias != "hermes.exe" {
            assert!(
                same_file::is_same_file(&path, &fixture.older_omni).expect("compare alias"),
                "`{alias}` was not restored to the older build"
            );
        }
    }
    assert_eq!(
        shim_names(&fixture.shims),
        ALIASES,
        "staged or retired links were left behind"
    );

    drop(lock);
    assert_success(
        &run_shim(&fixture.omni, "install", &fixture.state),
        "shim install after the lock is released",
    );
    assert_aliases_point_to(&fixture.shims, &fixture.omni);
    assert_eq!(shim_names(&fixture.shims), ALIASES);
}

/// Current `omni` in `bin`, with every alias hard-linked to an older build in `older`.
struct OlderBuildFixture {
    omni: PathBuf,
    older_omni: PathBuf,
    state: PathBuf,
    shims: PathBuf,
}

impl OlderBuildFixture {
    fn new(root: &Path) -> Self {
        let bin = root.join("bin");
        let older = root.join("older");
        let state = root.join("state");
        let shims = state.join("shims");
        for path in [&bin, &older, &shims] {
            fs::create_dir_all(path).expect("create fixture directory");
        }
        let omni = bin.join("omni.exe");
        fs::copy(env!("CARGO_BIN_EXE_omni"), &omni).expect("install current omni");
        let older_omni = older.join("omni.exe");
        let mut older_image = fs::read(env!("CARGO_BIN_EXE_omni")).expect("read omni image");
        older_image.extend_from_slice(b"older omni build fixture");
        fs::write(&older_omni, older_image).expect("write older omni build");
        link_aliases(&older_omni, &shims);
        Self {
            omni,
            older_omni,
            state,
            shims,
        }
    }
}

fn shim_names(shims: &Path) -> Vec<String> {
    let mut names = fs::read_dir(shims)
        .expect("list shims")
        .map(|entry| {
            entry
                .expect("shim entry")
                .file_name()
                .into_string()
                .expect("UTF-8 shim name")
        })
        .collect::<Vec<_>>();
    names.sort();
    names
}

fn send_break_to_parent_group() {
    let powershell = PathBuf::from(env::var_os("SystemRoot").expect("SystemRoot"))
        .join(r"System32\WindowsPowerShell\v1.0\powershell.exe");
    let status = Command::new(powershell)
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            SEND_BREAK_SCRIPT,
        ])
        .env("OMNI_TEST_BREAK_CHILD", std::process::id().to_string())
        .stdin(Stdio::null())
        // Its own group keeps the sender out of the event it sends.
        .creation_flags(CREATE_NEW_PROCESS_GROUP)
        .status()
        .expect("run console break sender");
    if !status.success() {
        eprintln!("console break sender failed: {status}");
        std::process::exit(4);
    }
}

fn test_executable() -> PathBuf {
    env::current_exe().expect("current test executable")
}

fn link_aliases(target: &Path, shims: &Path) {
    for alias in ALIASES {
        fs::hard_link(target, shims.join(alias)).expect("link alias");
    }
}

fn assert_aliases_point_to(shims: &Path, omni: &Path) {
    for alias in ALIASES {
        assert!(
            same_file::is_same_file(shims.join(alias), omni).expect("compare alias"),
            "`{alias}` still points at an older build"
        );
    }
}

fn run_shim(omni: &Path, action: &str, state: &Path) -> Output {
    Command::new(omni)
        .args(["shim", action, "--bin-dir"])
        .arg(omni.parent().expect("omni directory"))
        .env("OMNISESSION_HOME", state)
        .stdin(Stdio::null())
        .output()
        .expect("run omni shim")
}

fn assert_success(output: &Output, action: &str) {
    assert!(
        output.status.success(),
        "{action} failed: {}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
}

fn spawn_logged(command: &mut Command, log: &Path) -> Child {
    let file = fs::File::create(log).expect("create process log");
    command
        .stdin(Stdio::null())
        .stdout(file.try_clone().expect("clone process log"))
        .stderr(file)
        .spawn()
        .expect("spawn process")
}

fn wait_for_file(path: &Path, child: &mut Child, log: &Path) {
    let deadline = Instant::now() + TIMEOUT;
    while !path.exists() {
        if let Some(status) = child.try_wait().expect("poll process") {
            panic!(
                "process exited with {status} before ready:\n{}",
                read_log(log)
            );
        }
        assert!(
            Instant::now() < deadline,
            "process did not report ready:\n{}",
            read_log(log)
        );
        thread::sleep(Duration::from_millis(20));
    }
}

fn wait(child: &mut Child, timeout: Duration) -> ExitStatus {
    if let Some(status) = child.wait_timeout(timeout).expect("wait for process") {
        return status;
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!("process did not exit within {} seconds", timeout.as_secs());
}

fn read_log(log: &Path) -> String {
    fs::read_to_string(log).unwrap_or_default()
}
