use std::{
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use omnis_ir::{Provider, SessionRef, SourceRevision};
use serde_json::Value;

use super::{
    CodexAdapter, CodexSession, ObservedRollout, UNAVAILABLE_ANCESTRY, records_history_base,
    rollout_file,
};
use crate::support::{BoundedLine, read_bounded_line_with_budget, string_at, validate_provider};
use crate::{CodexWorkspaceProjection, IndexRead};

enum Workspace {
    Observed(Option<PathBuf>),
    RolledBack,
}

impl IndexRead {
    pub(super) fn observe_inherited_workspace(&mut self) -> Result<()> {
        let Some(native) = &self.native_session else {
            return Ok(());
        };
        if self.snapshot.events.iter().rev().any(|event| {
            event.source.raw_record_type.as_deref() == Some(UNAVAILABLE_ANCESTRY)
                && event.source.provider == Provider::Codex
                && event.source.native_session_id == native.session.id
        }) {
            let path = native
                .source_path
                .as_deref()
                .context("selected Codex rollout has no source path")?;
            let (directory, _) =
                CodexAdapter::rollout_workspaces(&native.session, path, self.source_revision)?;
            self.workspace_projection = CodexWorkspaceProjection::Observed(directory);
        }
        Ok(())
    }
}

impl CodexAdapter {
    /// Reads the effective and creation directories from the selected Codex rollout revision.
    /// An unreadable workspace change can leave the effective directory unknown.
    ///
    /// # Errors
    ///
    /// Returns an error if the revision or session ID differs, or the file cannot be read.
    pub fn rollout_workspaces(
        session: &SessionRef,
        path: &Path,
        revision: SourceRevision,
    ) -> Result<(Option<PathBuf>, Option<PathBuf>)> {
        validate_provider(session, Provider::Codex)?;
        ensure!(
            revision.file.is_some() && revision.contents.is_some(),
            "selected Codex rollout has no verified file and contents revision"
        );
        ensure!(
            path.symlink_metadata()?.is_file(),
            "Codex rollout must be a regular file"
        );
        let observed = ObservedRollout::read(path)
            .filter(|observed| observed.revision.matches_file_contents(revision))
            .context("selected Codex rollout changed before checking its workspace")?;
        let native = CodexSession::parse_with_observation(path, observed)?
            .filter(|native| native.id == session.id)
            .context("selected Codex rollout has a different session header")?;
        native.read_unchanged(|native, _| Ok((native.effective_directory()?, native.project_path)))
    }
}

impl CodexSession {
    fn effective_directory(&self) -> Result<Option<PathBuf>> {
        let limits = rollout_file::LIMITS;
        let mut remaining = limits.file_bytes;
        let mut reader = BufReader::new(rollout_file::open(&self.path)?);
        let mut line = Vec::new();
        let mut workspace = Workspace::Observed(self.project_path.clone());
        for _ in 0..limits.records {
            let Some((kind, bytes)) = read_bounded_line_with_budget(
                &mut reader,
                limits.line_bytes,
                remaining,
                &mut line,
            )?
            else {
                break;
            };
            remaining -= bytes;
            if matches!(kind, BoundedLine::Oversized) {
                workspace = Workspace::Observed(None);
                continue;
            }
            if !rollout_file::workspace_record_candidate(&line) {
                continue;
            }
            let Ok(record) = serde_json::from_slice::<Value>(&line) else {
                workspace = Workspace::Observed(None);
                continue;
            };
            if records_history_base(&record) {
                workspace = Workspace::Observed(None);
            } else if record["type"] == "turn_context" {
                if let Some(cwd) = string_at(&record, &[&["payload", "cwd"]]) {
                    workspace = Workspace::Observed(Some(PathBuf::from(cwd)));
                }
            } else if record["type"] == "event_msg"
                && record["payload"]["type"] == "thread_rolled_back"
                && record["payload"]["num_turns"]
                    .as_u64()
                    .context("Codex rollback marker has no valid num_turns")?
                    > 0
            {
                workspace = Workspace::RolledBack;
            }
        }
        ensure!(
            reader.fill_buf()?.is_empty(),
            "provider file exceeds safe record limit"
        );
        match workspace {
            Workspace::Observed(directory) => Ok(directory),
            Workspace::RolledBack => {
                // Without a later workspace record, resolve the directory from the surviving turns.
                let (_, project, workspace_reconstructed) = self.reconstructed_preview(limits)?;
                Ok(project.filter(|_| workspace_reconstructed))
            }
        }
    }
}
