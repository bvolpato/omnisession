use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::{
        fs::PermissionsExt,
        process::{CommandExt, ExitStatusExt},
    },
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use rustix::process::{Pid, Signal, kill_process, kill_process_group};
use wait_timeout::ChildExt;

use super::{SESSION_ID, command, write_synthetic_claude};

const BLOCKING_LAUNCHER: &str = r#"#!/bin/sh
case " $* " in
    *' --version '*)
        if [ "$PHASE" != launch ] || [ ! -e "$MODELS_DONE" ]; then echo '2.0.0'; exit; fi ;;
    *' models '*)
        if [ "$PHASE" = launch ]; then : > "$MODELS_DONE"; echo 'synthetic/model'; exit; fi ;;
    *' --show-toplevel '*) ;;
    *) exit 1 ;;
esac
exec 3<"$FIFO"
(read -r line <&3) &
worker=$!
: > "$READY"
wait "$worker"
"#;

#[test]
fn cancellation_stops_resume_preparation_workers_and_preserves_normal_git_termination() {
    let source = format!("claude:{SESSION_ID}");
    for (helper, phase, args) in [
        ("git", "workspace", vec!["--json", "inspect", &source]),
        (
            "opencode",
            "models",
            vec!["--json", "resume", &source, "--in", "opencode", "--dry-run"],
        ),
        (
            "opencode",
            "launch",
            vec!["resume", &source, "--in", "opencode"],
        ),
    ] {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        let workspace = root.join("workspace");
        write_synthetic_claude(root, &workspace);
        let ready = root.join("ready");
        let fifo = root.join("release");
        assert!(
            Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        let anchor = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&fifo)
            .unwrap();
        let mut release = OpenOptions::new().write(true).open(&fifo).unwrap();
        let launcher = root.join(helper);
        fs::write(&launcher, BLOCKING_LAUNCHER).unwrap();
        fs::set_permissions(&launcher, fs::Permissions::from_mode(0o700)).unwrap();
        if helper == "git" {
            fs::create_dir(workspace.join(".git")).unwrap();
        }
        let mut omni = command(root, &workspace)
            .args(args)
            .env("PATH", format!("{}:/usr/bin:/bin", root.display()))
            .env("OMNI_MODE", "default")
            .env("OMNI_OPENCODE_BIN", &launcher)
            .env("FIFO", &fifo)
            .env("READY", &ready)
            .env("PHASE", phase)
            .env("MODELS_DONE", root.join("models-done"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !ready.exists() && omni.try_wait().unwrap().is_none() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        let started = ready.exists();
        drop(anchor);
        let pid = Pid::from_child(&omni);
        if started {
            if helper == "git" {
                kill_process(pid, Signal::TERM).unwrap();
            } else {
                kill_process_group(pid, Signal::INT).unwrap();
            }
        }
        let status = omni.wait_timeout(Duration::from_secs(3)).unwrap();
        let stopped = helper == "opencode" && status.is_some() && worker_dropped_fifo(&mut release);
        let _ = release.write_all(b"release\n");
        if status.is_none() {
            let _ = omni.kill();
        }
        omni.wait().unwrap();
        assert!(started, "{helper} did not reach {phase}");
        let status = status.expect("omni must exit while the fixture is held");
        if helper == "git" {
            assert_eq!(status.signal(), Some(signal_hook::consts::SIGTERM));
        } else {
            assert_eq!(status.code(), Some(130));
            assert!(stopped, "{phase} worker survived cancellation");
        }
    }
}

fn worker_dropped_fifo(release: &mut fs::File) -> bool {
    // A dying worker can hold its reader briefly after the launcher exits.
    // Omit the newline so a surviving worker waits until the fixture releases it.
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        match release.write_all(b"x") {
            Ok(()) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            Ok(()) => return false,
            Err(error) => return error.kind() == std::io::ErrorKind::BrokenPipe,
        }
    }
}
