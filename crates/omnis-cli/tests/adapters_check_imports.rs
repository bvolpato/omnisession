//! Drives `omni adapters --check-imports` against synthetic provider launchers.
#![cfg(unix)]

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
};

use serde_json::Value;

/// Launcher that records every invocation, so a test can prove nothing ran.
fn launcher(directory: &Path, name: &str, version_output: &str) -> PathBuf {
    let path = directory.join(name);
    fs::write(
        &path,
        format!("#!/bin/sh\necho launched >> \"$0.calls\"\necho '{version_output}'\n"),
    )
    .expect("write launcher");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("launcher mode");
    path
}

fn calls(launcher: &Path) -> usize {
    fs::read_to_string(format!("{}.calls", launcher.display())).map_or(0, |log| log.lines().count())
}

#[test]
fn reports_version_gates_and_accepts_prerelease_builds() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let root = temporary.path();
    let bin = root.join("bin");
    fs::create_dir_all(&bin).expect("bin directory");
    let pi = launcher(&bin, "pi", "0.79.2");
    let codex = launcher(&bin, "codex", "codex-cli 0.147.0-alpha.3");

    let run = |arguments: &[&str]| {
        let output = Command::new(env!("CARGO_BIN_EXE_omni"))
            .args(arguments)
            .current_dir(root)
            // Inherited `OMNI_*_BIN` overrides or provider homes would reach real installs.
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", root.join("home"))
            .env("XDG_CONFIG_HOME", root.join("xdg/config"))
            .env("XDG_DATA_HOME", root.join("xdg/data"))
            .env("OMNISESSION_HOME", root.join("state"))
            .env("OMNI_NO_UPDATE_CHECK", "1")
            .env("OMNI_PI_BIN", &pi)
            .env("OMNI_CODEX_BIN", &codex)
            .output()
            .expect("run omni adapters");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<Value>(&output.stdout).expect("adapters JSON")
    };
    let check = |report: &Value, provider: &str| {
        report
            .as_array()
            .expect("provider list")
            .iter()
            .find(|entry| entry["provider"] == provider)
            .unwrap_or_else(|| panic!("{provider} entry"))["native_import_check"]
            .clone()
    };

    // The default listing launches nothing.
    let plain = run(&["--json", "adapters"]);
    assert_eq!(check(&plain, "pi"), Value::Null);
    assert_eq!((calls(&pi), calls(&codex)), (0, 0));

    let checked = run(&["--json", "adapters", "--check-imports"]);
    assert_eq!((calls(&pi), calls(&codex)), (1, 1));
    let pi_check = check(&checked, "pi");
    assert_eq!(pi_check["ready"], false);
    let blocker = pi_check["blocker"].as_str().expect("Pi blocker");
    assert!(
        blocker.contains("Pi 0.79.2 is too old"),
        "unexpected blocker: {blocker}"
    );
    let codex_check = check(&checked, "codex");
    assert_eq!(codex_check["ready"], true);
    assert_eq!(codex_check["version"], "0.147.0-alpha.3");
    // Agents without a launcher are not probed.
    assert_eq!(check(&checked, "grok"), Value::Null);
}
