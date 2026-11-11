use std::io::{BufWriter, Write};

use anyhow::{Context, Result, bail};
use omnis_core::{
    ImportTrajectory, import_trajectory, redact_secrets, safe_terminal_line, write_recovery_handoff,
};
use omnis_ir::CanonicalSnapshot;
use tempfile::NamedTempFile;

/// Native writers serialize and verify the same ordered history.
pub(crate) trait ImportSource {
    fn history(&self) -> Result<(&CanonicalSnapshot, ImportTrajectory)>;
}

impl ImportSource for CanonicalSnapshot {
    fn history(&self) -> Result<(&CanonicalSnapshot, ImportTrajectory)> {
        Ok((self, import_trajectory(self)))
    }
}

/// Keep the file for later resumes until the operating system cleans its temporary directory.
pub(crate) fn keep_handoff(mut file: NamedTempFile) {
    file.disable_cleanup(true);
    let _ = writeln!(
        std::io::stderr().lock(),
        "Recovery handoff saved to {}",
        safe_terminal_line(&file.path().display().to_string())
    );
}

pub(crate) fn write_temp_handoff(
    snapshot: &CanonicalSnapshot,
    preamble: Option<&str>,
) -> Result<NamedTempFile> {
    let directory = omnis_core::canonicalize_path(std::env::temp_dir())
        .context("resolving the temporary handoff directory")?;
    let mut file = tempfile::Builder::new()
        .prefix("omnisession-handoff-")
        .suffix(".md")
        .tempfile_in(directory)
        .context("creating a private temporary handoff")?;
    let path = file
        .path()
        .to_str()
        .context("the handoff path must be UTF-8")?;
    if path.chars().any(char::is_control) || redact_secrets(path) != path {
        bail!("the temporary directory path cannot be included safely in the handoff");
    }
    {
        let mut writer = BufWriter::new(file.as_file_mut());
        if let Some(preamble) = preamble {
            writeln!(writer, "{preamble}\n")?;
        }
        write_recovery_handoff(snapshot, &mut writer).context("writing recovery context")?;
        writer.flush().context("flushing recovery context")?;
    }
    file.as_file()
        .sync_all()
        .context("syncing recovery context")?;
    Ok(file)
}
