//! Copies a saved handoff file to the operating system clipboard.

use std::{
    fs::File,
    io,
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use anyhow::{Context, anyhow, bail};
use wait_timeout::ChildExt;

const CLIPBOARD_TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
#[cfg(windows)]
const SET_CLIPBOARD_SCRIPT: &str = concat!(
    "$ErrorActionPreference = 'Stop'; ",
    "$reader = [System.IO.StreamReader]::new(",
    "[Console]::OpenStandardInput(), [System.Text.UTF8Encoding]::new($false)); ",
    "$text = $reader.ReadToEnd(); ",
    "Set-Clipboard -Value $text"
);

struct Helper {
    program: &'static str,
    args: &'static [&'static str],
}

/// Copies the UTF-8 handoff file to the system clipboard.
///
/// The file is passed directly as standard input. Its contents never enter an argument, a shell,
/// or this process's error messages.
pub(super) fn copy_from_file(path: &Path) -> anyhow::Result<()> {
    let helpers = clipboard_helpers();
    if helpers.is_empty() {
        bail!("clipboard copy is not supported on this platform");
    }

    copy_from_file_with_helpers(path, &helpers, CLIPBOARD_TIMEOUT)
}

fn copy_from_file_with_helpers(
    path: &Path,
    helpers: &[Helper],
    timeout: Duration,
) -> anyhow::Result<()> {
    let deadline = Instant::now() + timeout;
    let mut failures = Vec::new();

    for helper in helpers {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            failures.push("the clipboard deadline expired".to_owned());
            break;
        }

        let input = File::open(path).with_context(|| {
            format!("open saved handoff file for clipboard: {}", path.display())
        })?;
        let mut command = Command::new(helper.program);
        command
            .args(helper.args)
            .stdin(Stdio::from(input))
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        #[cfg(target_os = "macos")]
        command
            .env("LANG", "en_US.UTF-8")
            .env("LC_ALL", "en_US.UTF-8");

        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(CREATE_NO_WINDOW);
        }

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                failures.push(format!("{} was not found", helper.program));
                continue;
            }
            Err(error) => {
                failures.push(format!(
                    "{} could not start ({:?})",
                    helper.program,
                    error.kind()
                ));
                continue;
            }
        };

        match child.wait_timeout(remaining) {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => {
                let code = status.code().map_or_else(
                    || "terminated by a signal".to_owned(),
                    |code| format!("exited with status {code}"),
                );
                failures.push(format!("{} {code}", helper.program));
            }
            Ok(None) => {
                let kill_result = child.kill();
                let reap_result = child.wait();
                if let Err(error) = reap_result {
                    return Err(anyhow!(
                        "{} timed out and could not be reaped ({:?})",
                        helper.program,
                        error.kind()
                    ));
                }
                if let Err(error) = kill_result
                    && error.kind() != io::ErrorKind::InvalidInput
                {
                    return Err(anyhow!(
                        "{} timed out and could not be stopped ({:?})",
                        helper.program,
                        error.kind()
                    ));
                }
                failures.push(format!("{} timed out", helper.program));
                break;
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(anyhow!(
                    "could not wait for clipboard helper {} ({:?})",
                    helper.program,
                    error.kind()
                ));
            }
        }
    }

    Err(anyhow!(
        "could not copy saved handoff to clipboard: {}",
        failures.join("; ")
    ))
}

#[cfg(target_os = "macos")]
fn clipboard_helpers() -> Vec<Helper> {
    vec![Helper {
        program: "pbcopy",
        args: &[],
    }]
}

#[cfg(target_os = "linux")]
fn clipboard_helpers() -> Vec<Helper> {
    let mut helpers = Vec::with_capacity(3);
    if std::env::var_os("WAYLAND_DISPLAY").is_some_and(|display| !display.is_empty()) {
        helpers.push(Helper {
            program: "wl-copy",
            args: &["--type", "text/plain;charset=utf-8"],
        });
    }
    helpers.extend([
        Helper {
            program: "xclip",
            args: &["-selection", "clipboard", "-target", "UTF8_STRING"],
        },
        Helper {
            program: "xsel",
            args: &["--clipboard", "--input"],
        },
    ]);
    helpers
}

#[cfg(windows)]
fn clipboard_helpers() -> Vec<Helper> {
    vec![
        Helper {
            program: "pwsh.exe",
            args: &[
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                SET_CLIPBOARD_SCRIPT,
            ],
        },
        Helper {
            program: "powershell.exe",
            args: &[
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                SET_CLIPBOARD_SCRIPT,
            ],
        },
    ]
}

#[cfg(all(test, windows))]
mod windows_tests {
    use std::{fs::File, io::Write, os::windows::process::CommandExt, process::Stdio};

    use super::{CLIPBOARD_TIMEOUT, CREATE_NO_WINDOW, SET_CLIPBOARD_SCRIPT, clipboard_helpers};
    use wait_timeout::ChildExt;

    #[test]
    fn powershell_decodes_utf8_file_stdin_without_a_console() {
        let text = "Synthetic café context 🦊\r\nSecond line\n";
        let mut handoff = tempfile::NamedTempFile::new().expect("create synthetic handoff");
        handoff.write_all(text.as_bytes()).expect("write context");
        let script = format!(
            "function Set-Clipboard {{ param([string]$Value) \
             if ($Value -cne $env:OMNI_EXPECTED_CLIPBOARD) {{ throw 'UTF-8 text mismatch' }}; \
             $script:copied = $true; }}; \
             {SET_CLIPBOARD_SCRIPT}; \
             if (-not $script:copied) {{ throw 'Clipboard helper was not called' }}"
        );
        let mut checked = 0;
        for helper in clipboard_helpers() {
            let result = std::process::Command::new(helper.program)
                .args([
                    "-NoLogo",
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    &script,
                ])
                .env("OMNI_EXPECTED_CLIPBOARD", text)
                .stdin(Stdio::from(File::open(handoff.path()).unwrap()))
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .creation_flags(CREATE_NO_WINDOW)
                .spawn();
            let mut child = match result {
                Ok(child) => child,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => panic!("start {}: {error}", helper.program),
            };
            let Some(status) = child.wait_timeout(CLIPBOARD_TIMEOUT).unwrap() else {
                let _ = child.kill();
                let _ = child.wait();
                panic!("{} decoding timed out", helper.program);
            };
            let output = child.wait_with_output().expect("read PowerShell status");
            assert!(
                status.success(),
                "{} failed: {}",
                helper.program,
                String::from_utf8_lossy(&output.stderr)
            );
            checked += 1;
        }
        assert!(
            checked > 0,
            "no PowerShell host available for decoding check"
        );
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn clipboard_helpers() -> Vec<Helper> {
    Vec::new()
}

#[cfg(all(test, unix))]
mod tests {
    use std::{
        io::Write,
        time::{Duration, Instant},
    };

    use super::{Helper, copy_from_file_with_helpers};

    #[test]
    fn kills_and_reaps_helper_that_never_reads_file_stdin() {
        let mut handoff = tempfile::NamedTempFile::new().expect("create synthetic handoff");
        let payload = [b'x'; 4096];
        for _ in 0..256 {
            handoff
                .write_all(&payload)
                .expect("write synthetic handoff");
        }

        let helpers = [Helper {
            program: "/bin/sh",
            args: &["-c", "exec sleep 30"],
        }];
        let started = Instant::now();
        let error =
            copy_from_file_with_helpers(handoff.path(), &helpers, Duration::from_millis(100))
                .expect_err("non-reading helper should time out");

        assert!(error.to_string().contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
