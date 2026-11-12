use std::{
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use omnis_ir::{GitState, Provider, SessionRef, SourceRevision};
use serde_json::Value;
use uuid::Uuid;

use super::history_base::{HistoryBase, HistoryScope, Lineage};
use super::{
    CodexAdapter, CodexSession, ObservedRollout, UNAVAILABLE_ANCESTRY, records_history_base,
    rollout_file,
};
use crate::support::{
    BoundedLine, JsonLineLocation, JsonLineSkips, read_bounded_line_with_budget, validate_provider,
};
use crate::{CodexWorkspaceProjection, IndexRead};

pub(super) fn same_thread(selected: &str, owner: &str) -> bool {
    selected == owner
        || Uuid::parse_str(selected)
            .ok()
            .zip(Uuid::parse_str(owner).ok())
            .is_some_and(|(left, right)| left == right)
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
enum OwnedWorkspace {
    #[default]
    Absent,
    Known(PathBuf),
    Unknown,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) struct WorkspaceSettings {
    owned: OwnedWorkspace,
    pub(super) startup: Option<PathBuf>,
    gaps: JsonLineSkips,
}

impl WorkspaceSettings {
    pub(super) fn from_header(startup: Option<PathBuf>) -> Self {
        Self {
            startup,
            ..Self::default()
        }
    }

    pub(super) fn observe_gaps(&mut self, skipped: JsonLineSkips) {
        if skipped.possible_settings > self.gaps.possible_settings
            || (self.has_owned() && skipped.unclassified > self.gaps.unclassified)
        {
            self.owned = OwnedWorkspace::Unknown;
            self.startup = None;
        }
        if skipped.foreign_settings > self.gaps.foreign_settings {
            self.startup = None;
        }
        self.gaps = skipped;
    }

    pub(super) fn observe(&mut self, session: &str, record: &Value) {
        let payload = &record["payload"];
        if record["type"] != "event_msg" || payload["type"] != "thread_settings_applied" {
            return;
        }
        let settings = &payload["thread_settings"];
        // Native startup can offer the directory in a copied settings record.
        if self.startup.as_deref() != settings["cwd"].as_str().map(Path::new) {
            self.startup = None;
        }
        if payload["thread_id"]
            .as_str()
            .is_some_and(|owner| same_thread(session, owner))
        {
            self.owned = super::workspace_settings::saved_directory(settings)
                .map_or(OwnedWorkspace::Unknown, OwnedWorkspace::Known);
        }
    }

    pub(super) fn has_owned(&self) -> bool {
        !matches!(self.owned, OwnedWorkspace::Absent)
    }

    pub(super) fn directory(&self, fallback: Option<PathBuf>) -> Option<PathBuf> {
        match &self.owned {
            OwnedWorkspace::Absent => fallback,
            OwnedWorkspace::Known(path) => Some(path.clone()),
            OwnedWorkspace::Unknown => None,
        }
    }
}

pub(super) struct WorkspaceScan {
    pub(super) settings: WorkspaceSettings,
    pub(super) directory: Option<PathBuf>,
    pub(super) reconstruct_history: bool,
    pub(super) saved_messages: usize,
    rolled_back: bool,
    inherited: bool,
    display_replay: super::DisplayReplay,
}

impl WorkspaceScan {
    pub(super) fn new(directory: Option<&Path>) -> Self {
        Self {
            settings: WorkspaceSettings::from_header(directory.map(Path::to_path_buf)),
            directory: directory.map(Path::to_path_buf),
            reconstruct_history: false,
            saved_messages: 0,
            rolled_back: false,
            inherited: false,
            display_replay: super::DisplayReplay::default(),
        }
    }

    fn omit(&mut self) {
        self.directory = None;
        self.rolled_back = false;
        // A full reconstruction locates the unreadable row within the retained conversation.
        self.reconstruct_history = true;
    }

    pub(super) fn observe(&mut self, session: &str, record: &Value) {
        self.reconstruct_history |= self.display_replay.observe(record);
        self.saved_messages += usize::from(super::saved_message(record));
        self.settings.observe(session, record);
        if records_history_base(record) {
            self.inherited = true;
            self.directory = None;
            self.rolled_back = false;
            self.reconstruct_history = true;
        } else if record["type"] == "turn_context" {
            self.directory = super::history_base::replay_messages::turn_context_cwd(record)
                .map(PathBuf::from)
                .filter(|path| path.is_absolute());
            self.rolled_back = false;
        } else if super::encoded_rollback(record) {
            let turns = super::rollout_lines::rollback_turns(&record["payload"]);
            if self.settings.gaps.workspace_gaps() > 0 && turns.is_none() {
                self.directory = None;
                self.rolled_back = false;
            } else {
                self.rolled_back |= turns != Some(0);
            }
            self.reconstruct_history = true;
        } else if record["type"] == "compacted" {
            self.reconstruct_history = true;
        }
    }
}

impl IndexRead {
    pub(super) fn observe_inherited_workspace(&mut self, selected_path: &Path) -> Result<()> {
        if matches!(
            self.workspace_projection,
            CodexWorkspaceProjection::Observed(_)
        ) {
            return Ok(());
        }
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
        let native = Self::verified_workspace_header(session, path, revision)?;
        native.read_unchanged(|native, _| {
            let workspace = native.live_workspace()?;
            Ok((workspace.directory, workspace.startup_directory))
        })
    }

    /// Returns the Git evidence recorded for the selected rollout's creation directory.
    ///
    /// # Errors
    ///
    /// Returns an error if the revision or session ID differs, or the header cannot be read.
    pub fn rollout_git(
        session: &SessionRef,
        path: &Path,
        revision: SourceRevision,
        directory: Option<&Path>,
    ) -> Result<GitState> {
        let native = Self::verified_workspace_header(session, path, revision)?;
        Ok(native.git_at(directory))
    }

    fn verified_workspace_header(
        session: &SessionRef,
        path: &Path,
        revision: SourceRevision,
    ) -> Result<CodexSession> {
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
        CodexSession::parse_with_observation(path, observed)?
            .filter(|native| native.id == session.id)
            .context("selected Codex rollout has a different session header")
    }
}

impl CodexSession {
    pub(super) fn visible_leaf(&self) -> Result<Lineage> {
        let mut lineage = self.history_base.leaf_only(
            &self.path,
            HistoryScope::Visible,
            self.subagent_history_start,
        )?;
        if matches!(
            self.history_base,
            HistoryBase::Prefix(_) | HistoryBase::Invalid | HistoryBase::InvalidAttachment
        ) {
            lineage.unavailable = Some("The rollout references earlier history that was not read.");
        }
        Ok(lineage)
    }

    pub(super) fn effective_and_startup_directories(
        &self,
    ) -> Result<(Option<PathBuf>, Option<PathBuf>)> {
        let workspace = self.scan_workspace(false)?;
        let effective = if workspace.rolled_back && !workspace.settings.has_owned() {
            let source;
            let session = if workspace.inherited {
                source = Self {
                    project_path: None,
                    ..self.clone()
                };
                &source
            } else {
                self
            };
            if session.needs_visible_projection() {
                let (_, project, verified) =
                    session.canonical_events_with(&session.visible_leaf()?, |visit| {
                        rollout_file::visit_with_limits(&session.path, rollout_file::LIMITS, visit)
                    })?;
                project.filter(|_| verified)
            } else {
                let (_, project, verified) = session.reconstructed_preview(rollout_file::LIMITS)?;
                project.filter(|_| verified)
            }
        } else {
            workspace.directory
        };
        Ok((
            effective.filter(|path| path.is_absolute()),
            workspace.settings.startup,
        ))
    }

    pub(super) fn scan_workspace(&self, inspect_displays: bool) -> Result<WorkspaceScan> {
        let limits = rollout_file::LIMITS;
        let lineage = self.visible_leaf()?;
        let mut remaining = limits.file_bytes;
        let plain = (!rollout_file::compressed(&self.path)).then_some(self.path.as_path());
        let mut reader = BufReader::new(rollout_file::open(&self.path)?);
        let mut line = Vec::new();
        let mut workspace = WorkspaceScan::new(self.project_path.as_deref());
        let mut skipped = JsonLineSkips::default();
        for row in 1..=limits.records {
            let Some((kind, bytes)) = read_bounded_line_with_budget(
                &mut reader,
                limits.line_bytes,
                remaining,
                &mut line,
            )?
            else {
                break;
            };
            let location = JsonLineLocation {
                offset: limits.file_bytes - remaining,
                length: bytes,
                terminated: line.ends_with(b"\n"),
            };
            remaining -= bytes;
            let oversized = matches!(kind, BoundedLine::Oversized);
            if !oversized {
                match rollout_file::workspace_record_kind(
                    &line,
                    inspect_displays && !workspace.reconstruct_history,
                ) {
                    rollout_file::WorkspaceRecord::Other => continue,
                    rollout_file::WorkspaceRecord::SavedMessage => {
                        workspace.saved_messages += 1;
                        continue;
                    }
                    rollout_file::WorkspaceRecord::Compacted => {
                        workspace.reconstruct_history = true;
                        continue;
                    }
                    rollout_file::WorkspaceRecord::Candidate => {
                        if let Ok(record) = serde_json::from_slice::<Value>(&line)
                            .map(super::saved_message::project)
                            && let Some(visible) = lineage.leaf_row_visibility(&record, row)
                        {
                            if !visible {
                                if record["type"] == "event_msg"
                                    && record["payload"]["type"] == "thread_settings_applied"
                                {
                                    workspace.settings.startup = None;
                                }
                            } else if (record["type"] != "session_meta"
                                || lineage.unavailable.is_some())
                                && (self.history_mode != super::CodexHistoryMode::Paginated
                                    || super::records_history_base(&record)
                                    || super::WorkspaceRecords::accepts(true, &record))
                            {
                                workspace.observe(&self.id, &record);
                            }
                            continue;
                        }
                    }
                }
            }
            let previous = skipped.workspace_gaps();
            skipped.observe(
                rollout_file::workspace_gap_at(plain, location, &line, oversized, Some(&self.id))?,
                oversized,
            );
            if skipped.workspace_gaps() > previous {
                workspace.omit();
            }
            workspace.settings.observe_gaps(skipped);
        }
        ensure!(
            reader.fill_buf()?.is_empty(),
            "provider file exceeds safe record limit"
        );
        workspace.directory = workspace.settings.directory(workspace.directory);
        Ok(workspace)
    }
}
