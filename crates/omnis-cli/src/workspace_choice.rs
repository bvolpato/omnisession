//! What to do when a session's workspace differs from the current directory: ask on a terminal
//! whether to switch to the source workspace, continue in the current folder, or cancel.

use std::io::{self, BufRead, IsTerminal, Write};

use anyhow::{Context, Result};

/// The user's answer when the session's workspace does not match the current directory.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WorkspaceChoice {
    /// Switch the launch and import working directory to the session's source workspace.
    Source,
    /// Continue in the caller's current directory (same effect as `--allow-workspace-mismatch`).
    Current,
    /// Stop without importing or launching.
    Cancel,
}

/// Describes the workspace mismatch shown to the user.
#[derive(Clone, Copy, Debug)]
pub(crate) struct WorkspacePrompt<'a> {
    /// Recorded source workspace path, when the session has one.
    pub(crate) source: Option<&'a str>,
    /// Whether switching to `source` is possible (verified, local, available).
    pub(crate) can_use_source: bool,
    /// Caller's current directory path.
    pub(crate) current: &'a str,
}

impl WorkspacePrompt<'_> {
    fn menu(self) -> &'static str {
        if self.can_use_source {
            "[s] switch to source workspace   [c] continue in current folder   [q] cancel"
        } else {
            "[c] continue in current folder   [q] cancel"
        }
    }

    /// Reads one answer. `None` asks again: the answer is not on the menu, or the line is empty,
    /// so an Enter typed ahead of the prompt never starts a transfer in the wrong folder.
    fn parse(self, answer: &str) -> Option<WorkspaceChoice> {
        match answer.trim().to_ascii_lowercase().as_str() {
            "s" | "source" if self.can_use_source => Some(WorkspaceChoice::Source),
            "c" | "current" => Some(WorkspaceChoice::Current),
            "q" | "cancel" | "quit" => Some(WorkspaceChoice::Cancel),
            _ => None,
        }
    }
}

/// Whether a person can answer: scripts and pipes fail closed with `--allow-workspace-mismatch`.
pub(crate) fn can_ask() -> bool {
    // Integration tests answer through a pipe.
    std::env::var_os("OMNI_TEST_WORKSPACE_CHOICES").is_some_and(|value| !value.is_empty())
        || (io::stdin().is_terminal() && io::stderr().is_terminal())
}

/// Explains the workspace mismatch and asks which directory to use.
///
/// # Errors
///
/// Returns terminal read or write failures.
pub(crate) fn ask(prompt: WorkspacePrompt<'_>) -> Result<WorkspaceChoice> {
    ask_on(&mut io::stdin().lock(), &mut io::stderr().lock(), prompt)
}

fn ask_on(
    input: &mut impl BufRead,
    output: &mut impl Write,
    prompt: WorkspacePrompt<'_>,
) -> Result<WorkspaceChoice> {
    match (prompt.source, prompt.can_use_source) {
        (Some(source), true) => {
            writeln!(
                output,
                "\nSource workspace differs from current directory:\n  source:  {source}\n  current: {}",
                prompt.current
            )
            .context("writing workspace choices")?;
        }
        (Some(source), false) => {
            writeln!(
                output,
                "\nSource workspace is unavailable:\n  source:  {source}\n  current: {}",
                prompt.current
            )
            .context("writing workspace choices")?;
        }
        (None, _) => {
            writeln!(
                output,
                "\nSource session has no verified workspace:\n  current: {}",
                prompt.current
            )
            .context("writing workspace choices")?;
        }
    }
    loop {
        write!(output, "  {}\n> ", prompt.menu()).context("writing workspace choices")?;
        output.flush().context("writing workspace choices")?;
        let mut answer = Vec::new();
        // End of input, such as Ctrl-D, cancels.
        if input
            .read_until(b'\n', &mut answer)
            .context("reading workspace choice")?
            == 0
        {
            return Ok(WorkspaceChoice::Cancel);
        }
        // Bytes that are not text are just an answer that is not on the menu.
        if let Some(choice) = prompt.parse(&String::from_utf8_lossy(&answer)) {
            return Ok(choice);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WITH_SOURCE: WorkspacePrompt<'static> = WorkspacePrompt {
        source: Some("/work/source"),
        can_use_source: true,
        current: "/work/current",
    };
    const MISSING_SOURCE: WorkspacePrompt<'static> = WorkspacePrompt {
        source: Some("/work/deleted"),
        can_use_source: false,
        current: "/work/current",
    };
    const UNVERIFIED_SOURCE: WorkspacePrompt<'static> = WorkspacePrompt {
        source: None,
        can_use_source: false,
        current: "/work/current",
    };

    #[test]
    fn menu_lists_switch_only_when_source_is_usable() {
        assert_eq!(
            WITH_SOURCE.menu(),
            "[s] switch to source workspace   [c] continue in current folder   [q] cancel"
        );
        assert_eq!(
            MISSING_SOURCE.menu(),
            "[c] continue in current folder   [q] cancel"
        );
        assert_eq!(
            UNVERIFIED_SOURCE.menu(),
            "[c] continue in current folder   [q] cancel"
        );
    }

    #[test]
    fn answers_and_reprompts_behave_safely() {
        for (answer, expected) in [
            // An Enter typed ahead of the prompt must not pick a folder by itself.
            ("\nS\n", WorkspaceChoice::Source),
            (" source \n", WorkspaceChoice::Source),
            ("c\n", WorkspaceChoice::Current),
            ("current\n", WorkspaceChoice::Current),
            ("q\n", WorkspaceChoice::Cancel),
            // Ctrl-D.
            ("", WorkspaceChoice::Cancel),
        ] {
            let mut output = Vec::new();
            let chosen = ask_on(&mut answer.as_bytes(), &mut output, WITH_SOURCE).expect("answer");
            assert_eq!(chosen, expected, "{answer:?}");
        }

        // Switching to source is not offered when the source workspace is unavailable.
        let mut output = Vec::new();
        let chosen = ask_on(
            &mut b"s\n\xff\xfe\nc\n".as_slice(),
            &mut output,
            MISSING_SOURCE,
        )
        .expect("answer");
        assert_eq!(chosen, WorkspaceChoice::Current);
        let shown = String::from_utf8(output).expect("UTF-8 prompt");
        assert!(shown.contains("Source workspace is unavailable:"));
        assert!(shown.contains("source:  /work/deleted"));
        assert_eq!(
            shown
                .matches("[c] continue in current folder   [q] cancel")
                .count(),
            3
        );
    }

    #[test]
    fn unverified_source_omits_source_line() {
        let mut output = Vec::new();
        let chosen = ask_on(&mut b"q\n".as_slice(), &mut output, UNVERIFIED_SOURCE).expect("q");
        assert_eq!(chosen, WorkspaceChoice::Cancel);
        let shown = String::from_utf8(output).expect("UTF-8 prompt");
        assert!(shown.contains("Source session has no verified workspace:"));
        assert!(!shown.contains("source:"));
    }
}
