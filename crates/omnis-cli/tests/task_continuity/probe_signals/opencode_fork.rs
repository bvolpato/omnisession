use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::{fs::PermissionsExt, process::CommandExt},
    path::Path,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use rustix::process::{Pid, Signal, kill_process_group};
use wait_timeout::ChildExt;

use super::command;

const PROVIDER: &str = r#"#!/bin/sh
case "$1:$2:$3:$4" in
    --version:*) echo '2.2.0' ;;
    api:--standalone:GET:*)
        id=${4##*/}
        if [ -f "$ROOT/$id" ]; then
            printf '{"data":{"id":"%s","location":{"directory":"%s"}}}\n' "$id" "$PWD"
        else
            echo 'HTTP 404 Not Found' >&2
            printf '{"_tag":"SessionNotFoundError","sessionID":"%s"}\n' "$id"
            exit 1
        fi ;;
    api:--standalone:POST:/api/session/ses_parent/fork)
        : > "$ROOT/ses_child"; echo '{"id":"ses_child"}' ;;
    session:export:ses_child:*)
        : > "$ROOT/reading"
        read -r release < "$ROOT/release"
        echo '{"info":{"id":"ses_child"}}' ;;
    session:delete:*)
        printf '%s\n' "$3" >> "$ROOT/deleted"
        if [ "$REMOVE" = yes ]; then rm "$ROOT/$3"; fi ;;
    --session:*) : > "$ROOT/launched" ;;
    *) exit 1 ;;
esac
"#;

#[test]
fn interrupted_native_fork_verifies_removal_of_the_returned_child() {
    for (phase, remove) in [
        ("readback", true),
        ("readback", false),
        ("recorded", true),
        ("launch", true),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let workspace = root.join("workspace");
        fs::create_dir(&workspace).unwrap();
        fs::write(root.join("ses_parent"), "source").unwrap();
        fs::write(root.join("ses_other"), "unrelated").unwrap();
        let provider = root.join("opencode");
        fs::write(&provider, PROVIDER).unwrap();
        fs::set_permissions(&provider, fs::Permissions::from_mode(0o700)).unwrap();
        let fifo = root.join("release");
        assert!(
            Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        let mut release = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&fifo)
            .unwrap();
        let mut cli = command(root, &workspace);
        if phase == "recorded" {
            cli.env("OMNI_TEST_IMPORT_INTERRUPT", phase);
        }
        let mut omni = cli
            .args(["fork", "opencode:ses_parent", "--in", "opencode"])
            .env("PATH", "/usr/bin:/bin")
            .env("OMNI_OPENCODE_BIN", &provider)
            .env("OMNI_MODE", "default")
            .env("OMNI_TEST_INTERRUPT_ACK", root.join("ack"))
            .env("ROOT", root)
            .env("REMOVE", if remove { "yes" } else { "no" })
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn()
            .unwrap();
        let held = wait_for_marker(&mut omni, &root.join("reading"));
        drop(omni.stdout.take());
        let acknowledged = if held && phase == "readback" {
            kill_process_group(Pid::from_child(&omni), Signal::INT).unwrap();
            wait_for_marker(&mut omni, &root.join("ack"))
        } else {
            held
        };
        release.write_all(b"continue\n").unwrap();
        if omni.wait_timeout(Duration::from_secs(5)).unwrap().is_none() {
            omni.kill().unwrap();
        }
        let output = omni.wait_with_output().unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            held && acknowledged,
            "Ctrl+C did not reach the fork guard: {stderr}"
        );
        assert!(root.join("ses_parent").exists() && root.join("ses_other").exists());
        if phase == "launch" {
            assert_eq!(output.status.code(), Some(0), "{stderr}");
            assert!(root.join("ses_child").exists() && root.join("launched").exists());
            assert!(!root.join("deleted").exists());
            continue;
        }
        assert_eq!(
            output.status.code(),
            Some(if remove { 130 } else { 1 }),
            "{stderr}"
        );
        assert_eq!(
            fs::read_to_string(root.join("deleted")).unwrap(),
            "ses_child\n"
        );
        assert_eq!(root.join("ses_child").exists(), !remove);
        assert!(!root.join("launched").exists());
        assert!(stderr.contains("ses_child"), "{stderr}");
        if !remove {
            assert!(stderr.contains("rollback also failed"), "{stderr}");
        }
    }
}

fn wait_for_marker(child: &mut Child, marker: &Path) -> bool {
    let deadline = Instant::now() + Duration::from_secs(8);
    while !marker.exists() && child.try_wait().unwrap().is_none() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    marker.exists()
}
