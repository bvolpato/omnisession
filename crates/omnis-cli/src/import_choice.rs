//! What to do when a native import cannot run: ask on a terminal instead of silently writing a
//! handoff file the user did not ask for.

use std::io::{self, BufRead, IsTerminal, Write};

use anyhow::{Context, Result};

/// Asked for after a native import could not run; the caller runs the import again.
#[derive(Debug)]
pub(crate) struct RetryNativeImport;

impl std::fmt::Display for RetryNativeImport {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("native import retry requested")
    }
}

impl std::error::Error for RetryNativeImport {}

/// The user's answer when a native import could not run.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ImportChoice {
    /// Try the same import again, after closing the agent that blocked it for example.
    Retry,
    /// Stay in the source agent and fork the session there, which needs no import.
    ForkInSource,
    /// Start the target with a private handoff file.
    Handoff,
    Cancel,
}

/// Which answers make sense for this failure.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ImportChoices<'a> {
    /// Source agent's display name when it can fork the session itself.
    pub(crate) fork_in: Option<&'a str>,
    /// Whether the target can start from a handoff file.
    pub(crate) handoff: bool,
}

impl ImportChoices<'_> {
    fn menu(self) -> String {
        let mut entries = vec!["[r] retry".to_owned()];
        if let Some(source) = self.fork_in {
            entries.push(format!("[f] fork in {source} instead"));
        }
        if self.handoff {
            entries.push("[h] continue with a handoff file".to_owned());
        }
        entries.push("[q] cancel".to_owned());
        entries.join("   ")
    }

    /// Reads one answer. `None` asks again: the answer is not on the menu, or the line is empty,
    /// so an Enter typed ahead of the prompt never starts another import by itself.
    fn parse(self, answer: &str) -> Option<ImportChoice> {
        match answer.trim().to_ascii_lowercase().as_str() {
            "r" | "retry" => Some(ImportChoice::Retry),
            "f" | "fork" if self.fork_in.is_some() => Some(ImportChoice::ForkInSource),
            "h" | "handoff" if self.handoff => Some(ImportChoice::Handoff),
            "q" | "c" | "cancel" | "quit" => Some(ImportChoice::Cancel),
            _ => None,
        }
    }
}

/// Whether a person can answer: scripts and pipes keep the automatic fallback.
pub(crate) fn can_ask() -> bool {
    // Integration tests answer through a pipe.
    std::env::var_os("OMNI_TEST_IMPORT_CHOICES").is_some_and(|value| !value.is_empty())
        || (io::stdin().is_terminal() && io::stderr().is_terminal())
}

/// Explains the failure and asks what to do next.
///
/// # Errors
///
/// Returns terminal read or write failures.
pub(crate) fn ask(target: &str, reason: &str, choices: ImportChoices<'_>) -> Result<ImportChoice> {
    ask_on(
        &mut io::stdin().lock(),
        &mut io::stderr().lock(),
        target,
        reason,
        choices,
    )
}

fn ask_on(
    input: &mut impl BufRead,
    output: &mut impl Write,
    target: &str,
    reason: &str,
    choices: ImportChoices<'_>,
) -> Result<ImportChoice> {
    writeln!(output, "\n{target} native import could not run: {reason}")
        .context("writing import choices")?;
    loop {
        write!(output, "  {}\n> ", choices.menu()).context("writing import choices")?;
        output.flush().context("writing import choices")?;
        let mut answer = Vec::new();
        // End of input, such as Ctrl-D, cancels.
        if input
            .read_until(b'\n', &mut answer)
            .context("reading import choice")?
            == 0
        {
            return Ok(ImportChoice::Cancel);
        }
        // Bytes that are not text are just an answer that is not on the menu.
        if let Some(choice) = choices.parse(&String::from_utf8_lossy(&answer)) {
            return Ok(choice);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: ImportChoices<'static> = ImportChoices {
        fork_in: Some("Claude"),
        handoff: true,
    };
    const RETRY_ONLY: ImportChoices<'static> = ImportChoices {
        fork_in: None,
        handoff: false,
    };

    #[test]
    fn menu_lists_only_the_answers_that_apply() {
        assert_eq!(
            ALL.menu(),
            "[r] retry   [f] fork in Claude instead   [h] continue with a handoff file   [q] cancel"
        );
        assert_eq!(RETRY_ONLY.menu(), "[r] retry   [q] cancel");
    }

    #[test]
    fn unlisted_and_empty_answers_ask_again() {
        for (answer, choice) in [
            // An Enter typed ahead of the prompt must not retry by itself.
            ("\nR\n", ImportChoice::Retry),
            ("f\n", ImportChoice::ForkInSource),
            (" handoff \n", ImportChoice::Handoff),
            ("q\n", ImportChoice::Cancel),
            // Ctrl-D.
            ("", ImportChoice::Cancel),
        ] {
            let mut output = Vec::new();
            let asked = ask_on(&mut answer.as_bytes(), &mut output, "Codex", "too old", ALL)
                .expect("answer");
            assert_eq!(asked, choice, "{answer:?}");
        }

        // A handoff is not offered where the target cannot take one, so `h` asks again.
        let mut output = Vec::new();
        let asked = ask_on(
            &mut b"h\nf\n\xff\xfe\nq\n".as_slice(),
            &mut output,
            "Cursor IDE",
            "Cursor is running",
            RETRY_ONLY,
        )
        .expect("answer");
        assert_eq!(asked, ImportChoice::Cancel);
        let shown = String::from_utf8(output).expect("UTF-8 prompt");
        assert!(shown.contains("Cursor IDE native import could not run: Cursor is running"));
        assert_eq!(shown.matches("[r] retry   [q] cancel").count(), 4);
    }
}
