use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::Value;

use super::rollout_lines::{Wire, classify_event, valid_envelope};
use super::workspace_damage::{
    DamagedWorkspace, damaged_workspace_record, oversized_workspace_record,
};
use super::{RolloutWorkspace, WorkspaceSettings, oversized_settings_marker};
use crate::support::{JsonLine, JsonLineLocation};

#[derive(Default)]
pub(super) struct WorkspaceRecords {
    workspace: RolloutWorkspace,
    paginated: bool,
    gap: Option<WorkspaceGap>,
    pub(super) malformed: usize,
    settings_gap: Option<SettingsGap>,
    invalid_rollback: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum WorkspaceGap {
    AwaitingContext,
    ContextRestored,
}

enum SettingsGap {
    Unclassified(Vec<JsonLineLocation>),
    PossibleSettings,
}

impl WorkspaceRecords {
    pub(super) fn from_header(directory: Option<PathBuf>, paginated: bool) -> Self {
        Self {
            paginated,
            workspace: RolloutWorkspace {
                settings: WorkspaceSettings::from_header(directory.clone()),
                directory,
                ..RolloutWorkspace::default()
            },
            ..Self::default()
        }
    }

    pub(super) fn had_gap(&self) -> bool {
        self.gap.is_some()
    }

    fn restore_context(&mut self) {
        if let Some(gap) = &mut self.gap {
            *gap = WorkspaceGap::ContextRestored;
        }
    }

    fn accepts(&self, record: &Value) -> bool {
        let payload = &record["payload"];
        match (record["type"].as_str(), payload["type"].as_str()) {
            (Some("turn_context"), _) => !self.paginated || valid_envelope(record),
            (Some("event_msg"), Some("thread_rolled_back" | "thread_settings_applied")) => {
                !self.paginated
                    || (valid_envelope(record) && classify_event(payload) != Wire::Invalid)
            }
            _ => false,
        }
    }

    /// Returns true when the line interrupts workspace evidence.
    pub(super) fn observe(&mut self, session: &str, line: &JsonLine<'_>) -> bool {
        let kind = match line {
            JsonLine::Record(record) | JsonLine::Unterminated(record) => {
                if self.accepts(record) {
                    self.record(session, record);
                }
                return false;
            }
            JsonLine::Malformed(bytes) => {
                self.malformed += 1;
                damaged_workspace_record(bytes)
            }
            JsonLine::Oversized(prefix, _) => oversized_workspace_record(prefix),
        };
        match kind {
            None => return false,
            Some(DamagedWorkspace::TurnOrRollback) => {}
            Some(DamagedWorkspace::PossibleSettings) => {
                self.settings_gap = Some(SettingsGap::PossibleSettings);
                self.workspace.settings = WorkspaceSettings::default();
            }
            Some(DamagedWorkspace::Unclassified) => {
                if self.workspace.settings.has_owned() {
                    self.settings_gap = Some(SettingsGap::PossibleSettings);
                } else if let JsonLine::Oversized(_, location) = line
                    && let SettingsGap::Unclassified(locations) = self
                        .settings_gap
                        .get_or_insert_with(|| SettingsGap::Unclassified(Vec::new()))
                {
                    locations.push(*location);
                }
                self.workspace.settings = WorkspaceSettings::default();
            }
        }
        self.gap = Some(WorkspaceGap::AwaitingContext);
        self.workspace.directory = None;
        self.workspace.rolled_back = false;
        true
    }

    fn record(&mut self, session: &str, record: &Value) {
        let payload = &record["payload"];
        self.workspace.settings.observe(session, record);
        match (record["type"].as_str(), payload["type"].as_str()) {
            (Some("turn_context"), _) => {
                if let Some(cwd) = payload["cwd"].as_str() {
                    self.workspace.directory = Some(PathBuf::from(cwd));
                    self.restore_context();
                }
            }
            (Some("event_msg"), Some("thread_settings_applied"))
                if super::workspace_settings::belongs_to(payload, session) =>
            {
                self.settings_gap = None;
                self.restore_context();
            }
            (Some("event_msg"), Some("thread_rolled_back"))
                if payload["num_turns"].as_u64() != Some(0) =>
            {
                self.workspace.directory = None;
                self.workspace.rolled_back = true;
                self.invalid_rollback |= payload["num_turns"].as_u64().is_none();
            }
            _ => {}
        }
    }

    pub(super) fn use_history(&mut self, directory: Option<PathBuf>, restored: bool) {
        if !self.had_gap() || (restored && self.workspace.rolled_back && !self.invalid_rollback) {
            self.workspace.directory = directory;
        }
    }

    pub(super) fn finish(mut self, path: &Path) -> Result<RolloutWorkspace> {
        if self.had_gap() && self.invalid_rollback {
            self.workspace.rolled_back = false;
        }
        let directory = self
            .workspace
            .settings
            .directory(self.workspace.directory)
            .filter(|path| path.is_absolute());
        let unverified = match self.settings_gap {
            Some(SettingsGap::PossibleSettings) => true,
            Some(SettingsGap::Unclassified(locations))
                if directory.is_some() && self.gap != Some(WorkspaceGap::AwaitingContext) =>
            {
                oversized_settings_marker(path, &locations)?
            }
            _ => false,
        };
        self.workspace.directory = if self.gap == Some(WorkspaceGap::AwaitingContext) || unverified
        {
            None
        } else {
            directory
        };
        Ok(self.workspace)
    }
}
