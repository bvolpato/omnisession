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

pub(super) struct WorkspaceScan {
    pub(super) directory: Option<PathBuf>,
    pub(super) reconstruct_history: bool,
    rolled_back: bool,
}

impl WorkspaceScan {
    pub(super) fn new(directory: Option<&Path>) -> Self {
        Self {
            directory: directory.map(Path::to_path_buf),
            reconstruct_history: false,
            rolled_back: false,
        }
    }

    fn omit(&mut self) {
        self.directory = None;
        self.rolled_back = false;
        // A full reconstruction locates the unreadable row within the retained conversation.
        self.reconstruct_history = true;
    }

    pub(super) fn observe(&mut self, record: &Value) -> Result<()> {
        if records_history_base(record) {
            self.directory = None;
            self.rolled_back = false;
            self.reconstruct_history = true;
        } else if record["type"] == "turn_context" {
            if let Some(cwd) = string_at(record, &[&["payload", "cwd"]]) {
                self.directory = Some(PathBuf::from(cwd));
                self.rolled_back = false;
            }
        } else if record["type"] == "event_msg" && record["payload"]["type"] == "thread_rolled_back"
        {
            let count = record["payload"]["num_turns"]
                .as_u64()
                .context("Codex rollback marker has no valid num_turns")?;
            self.rolled_back |= count > 0;
            self.reconstruct_history = true;
        } else if record["type"] == "compacted" {
            self.reconstruct_history = true;
        }
        Ok(())
    }
}

impl IndexRead {
    pub(super) fn observe_inherited_workspace(&mut self, selected_path: &Path) -> Result<()> {
        let session = &self.snapshot.session;
        if self.snapshot.events.iter().rev().any(|event| {
            event.source.raw_record_type.as_deref() == Some(UNAVAILABLE_ANCESTRY)
                && event.source.provider == Provider::Codex
                && event.source.native_session_id == session.id
        }) {
            let (directory, _) =
                CodexAdapter::rollout_workspaces(session, selected_path, self.source_revision)?;
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
        let workspace = self.scan_workspace()?;
        if workspace.rolled_back {
            let (_, project, verified) = self.reconstructed_preview(rollout_file::LIMITS)?;
            Ok(project.filter(|_| verified))
        } else {
            Ok(workspace.directory)
        }
    }

    pub(super) fn scan_workspace(&self) -> Result<WorkspaceScan> {
        let limits = rollout_file::LIMITS;
        let mut remaining = limits.file_bytes;
        let mut reader = BufReader::new(rollout_file::open(&self.path)?);
        let mut line = Vec::new();
        let mut workspace = WorkspaceScan::new(self.project_path.as_deref());
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
                workspace.omit();
                continue;
            }
            match rollout_file::workspace_record_kind(&line) {
                rollout_file::WorkspaceRecord::Other => {}
                rollout_file::WorkspaceRecord::Compacted => workspace.reconstruct_history = true,
                rollout_file::WorkspaceRecord::Candidate => {
                    if let Ok(record) = serde_json::from_slice::<Value>(&line) {
                        workspace.observe(&record)?;
                    } else {
                        workspace.omit();
                    }
                }
            }
        }
        ensure!(
            reader.fill_buf()?.is_empty(),
            "provider file exceeds safe record limit"
        );
        Ok(workspace)
    }
}
