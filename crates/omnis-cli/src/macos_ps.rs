//! Current-user process table from macOS `/bin/ps`, for active-writer checks.
//!
//! `command` joins arguments with spaces, so it cannot show where an executable path containing
//! spaces ends. `comm` holds the complete executable path, or a title the process set, in one
//! column. Checks read both.

#[cfg(target_os = "macos")]
use anyhow::{Context, Result, bail};

/// One inspection of current-user processes.
pub(crate) struct ProcessTable {
    /// `/bin/ps -ww -x -o pid=,command=` output: arguments joined by spaces.
    pub(crate) commands: String,
    /// `/bin/ps -ww -x -o pid=,comm=` output: executable path or process title, never split.
    pub(crate) executables: String,
}

#[cfg(test)]
impl ProcessTable {
    pub(crate) fn from_outputs(commands: &str, executables: &str) -> Self {
        Self {
            commands: commands.to_owned(),
            executables: executables.to_owned(),
        }
    }
}

/// Reads both process-table views for `provider`'s active-writer check.
#[cfg(target_os = "macos")]
pub(crate) fn inspect(provider: &str) -> Result<ProcessTable> {
    let read = |columns: &str| -> Result<String> {
        let output = std::process::Command::new("/bin/ps")
            .args(["-ww", "-x", "-o", columns])
            .output()
            .with_context(|| format!("checking active {provider} processes"))?;
        if !output.status.success() {
            bail!("could not inspect {provider} process state");
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    };
    Ok(ProcessTable {
        commands: read("pid=,command=")?,
        executables: read("pid=,comm=")?,
    })
}

/// Parses `pid value` rows, skipping malformed rows and `own_pid`.
pub(crate) fn rows(output: &str, own_pid: u32) -> impl Iterator<Item = (u32, &str)> {
    output.lines().filter_map(move |line| {
        let line = line.trim_start();
        let split = line.find(char::is_whitespace)?;
        let pid = line[..split].parse::<u32>().ok()?;
        (pid != own_pid).then_some((pid, line[split..].trim()))
    })
}

/// Final path component of an executable path or process title.
pub(crate) fn executable_name(executable: &str) -> &str {
    executable
        .rsplit_once('/')
        .map_or(executable, |(_, name)| name)
}
