//! Windows process listing for active-writer checks.
//!
//! Windows has no `/proc` or `/bin/ps`. `Win32_Process` reports executable paths and full
//! command lines, which active-writer checks need to recognize script launchers such as
//! `node.exe ...\cli.js`; Toolhelp32 snapshots report only image names.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde_json::Value;

/// One running process as reported by `Win32_Process`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct WindowsProcess {
    pub(crate) pid: u32,
    /// Absent for protected processes and processes the current user cannot inspect.
    pub(crate) executable: Option<PathBuf>,
    /// Absent under the same conditions as [`WindowsProcess::executable`].
    pub(crate) command_line: Option<String>,
}

// Cold Windows PowerShell plus CIM can take tens of seconds on a busy machine.
#[cfg(windows)]
const LIST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
#[cfg(windows)]
const MAX_LIST_OUTPUT: u64 = 16 * 1024 * 1024;

/// Prints process rows as ASCII-only JSON so console code pages cannot change the bytes.
#[cfg(windows)]
const LIST_SCRIPT: &str = r"$ErrorActionPreference = 'Stop'; $rows = @(Get-CimInstance -ClassName Win32_Process -Property ProcessId,ExecutablePath,CommandLine | Select-Object ProcessId,ExecutablePath,CommandLine); $json = ConvertTo-Json -InputObject $rows -Compress -Depth 2; [regex]::Replace($json, '[^\u0000-\u007F]', { param($match) '\u{0:x4}' -f [int][char]$match.Value })";

/// Lists running processes through Windows PowerShell and CIM, failing closed after a timeout.
#[cfg(windows)]
pub(crate) fn list_processes() -> Result<Vec<WindowsProcess>> {
    use std::{
        io::Read,
        os::windows::process::CommandExt,
        process::{Command, Stdio},
        thread,
    };

    use anyhow::anyhow;
    use wait_timeout::ChildExt;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let system_root = std::env::var_os("SystemRoot")
        .filter(|value| !value.is_empty())
        .context("SystemRoot is not set")?;
    let powershell =
        PathBuf::from(system_root).join(r"System32\WindowsPowerShell\v1.0\powershell.exe");
    let mut child = Command::new(&powershell)
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            LIST_SCRIPT,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .with_context(|| format!("starting `{}`", powershell.display()))?;
    let mut stdout = child
        .stdout
        .take()
        .context("capturing Windows process list")?;
    let reader = thread::spawn(move || -> std::io::Result<Vec<u8>> {
        let mut output = Vec::new();
        (&mut stdout)
            .take(MAX_LIST_OUTPUT + 1)
            .read_to_end(&mut output)?;
        std::io::copy(&mut stdout, &mut std::io::sink())?;
        Ok(output)
    });
    let Some(status) = child
        .wait_timeout(LIST_TIMEOUT)
        .context("waiting for Windows process list")?
    else {
        let _ = child.kill();
        let _ = child.wait();
        let _ = reader.join();
        bail!(
            "Windows process list timed out after {} seconds",
            LIST_TIMEOUT.as_secs()
        );
    };
    let output = reader
        .join()
        .map_err(|_| anyhow!("Windows process list reader panicked"))?
        .context("reading Windows process list")?;
    if !status.success() {
        bail!("Windows process list exited with {status}");
    }
    if output.len() > usize::try_from(MAX_LIST_OUTPUT).unwrap_or(usize::MAX) {
        bail!("Windows process list exceeded {MAX_LIST_OUTPUT} bytes");
    }
    let output = String::from_utf8(output).context("Windows process list is not UTF-8")?;
    parse_processes(&output)
}

/// Parses `ConvertTo-Json` output for `Win32_Process` rows.
///
/// Accepts an array, a single object (how `ConvertTo-Json` prints one piped row), or empty
/// output. Rejects rows without a valid process ID.
pub(crate) fn parse_processes(output: &str) -> Result<Vec<WindowsProcess>> {
    let output = output.trim_start_matches('\u{feff}').trim();
    if output.is_empty() {
        return Ok(Vec::new());
    }
    let rows = match serde_json::from_str(output).context("parsing Windows process list")? {
        Value::Array(rows) => rows,
        row @ Value::Object(_) => vec![row],
        Value::Null => Vec::new(),
        _ => bail!("Windows process list is not a JSON array"),
    };
    rows.iter().map(parse_process).collect()
}

fn parse_process(row: &Value) -> Result<WindowsProcess> {
    let pid = row
        .get("ProcessId")
        .and_then(Value::as_u64)
        .and_then(|pid| u32::try_from(pid).ok())
        .context("Windows process row has no valid ProcessId")?;
    Ok(WindowsProcess {
        pid,
        executable: optional_text(row, "ExecutablePath")?.map(PathBuf::from),
        command_line: optional_text(row, "CommandLine")?,
    })
}

fn optional_text(row: &Value, field: &str) -> Result<Option<String>> {
    match row.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) if text.is_empty() => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => bail!("Windows process field `{field}` is not text"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rows_with_missing_fields_and_escaped_unicode() {
        let output = "\u{feff}[{\"ProcessId\":0,\"ExecutablePath\":null,\"CommandLine\":null},\
            {\"ProcessId\":4812,\"ExecutablePath\":\"C:\\\\Program Files\\\\nodejs\\\\node.exe\",\
            \"CommandLine\":\"\\\"C:\\\\Program Files\\\\nodejs\\\\node.exe\\\" \
            C:\\\\Users\\\\Zo\\u00eb\\\\AppData\\\\Roaming\\\\npm\\\\node_modules\\\\cli.js \\ud83d\\ude80\"},\
            {\"ProcessId\":77,\"ExecutablePath\":\"\",\"CommandLine\":\"\"}]\r\n";
        assert_eq!(
            parse_processes(output).expect("parse process list"),
            [
                WindowsProcess {
                    pid: 0,
                    executable: None,
                    command_line: None,
                },
                WindowsProcess {
                    pid: 4812,
                    executable: Some(PathBuf::from(r"C:\Program Files\nodejs\node.exe")),
                    command_line: Some(
                        "\"C:\\Program Files\\nodejs\\node.exe\" \
                         C:\\Users\\Zoë\\AppData\\Roaming\\npm\\node_modules\\cli.js 🚀"
                            .to_owned()
                    ),
                },
                WindowsProcess {
                    pid: 77,
                    executable: None,
                    command_line: None,
                },
            ]
        );
    }

    #[test]
    fn accepts_single_object_and_empty_output() {
        let single = parse_processes(r#"{"ProcessId":9,"ExecutablePath":"C:\\a.exe"}"#)
            .expect("parse single row");
        assert_eq!(single.len(), 1);
        assert_eq!(single[0].pid, 9);
        assert_eq!(single[0].command_line, None);
        assert!(parse_processes(" \r\n").expect("parse empty").is_empty());
        assert!(parse_processes("[]").expect("parse empty array").is_empty());
    }

    #[test]
    fn rejects_rows_without_valid_identity_or_text() {
        for output in [
            "not json",
            "42",
            r#"[{"ExecutablePath":"C:\\a.exe"}]"#,
            r#"[{"ProcessId":-1}]"#,
            r#"[{"ProcessId":4294967296}]"#,
            r#"[{"ProcessId":"12"}]"#,
            r#"[{"ProcessId":12,"CommandLine":["a"]}]"#,
        ] {
            assert!(parse_processes(output).is_err(), "accepted {output}");
        }
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "spawns PowerShell and CIM, which time out under parallel test load; CI runs it alone"]
    fn lists_current_process_with_executable_and_command_line() {
        let processes = list_processes().expect("list Windows processes");
        let current = processes
            .iter()
            .find(|process| process.pid == std::process::id())
            .expect("current process listed");
        let current_exe = std::env::current_exe().expect("current executable");
        let executable = current
            .executable
            .as_ref()
            .expect("current executable path");
        assert!(
            same_file::is_same_file(executable, &current_exe).expect("compare executable"),
            "listed `{}` for `{}`",
            executable.display(),
            current_exe.display()
        );
        let stem = current_exe
            .file_stem()
            .and_then(std::ffi::OsStr::to_str)
            .expect("UTF-8 executable name");
        assert!(
            current
                .command_line
                .as_deref()
                .is_some_and(|command_line| command_line.contains(stem)),
            "command line {:?} lacks `{stem}`",
            current.command_line
        );
    }
}
