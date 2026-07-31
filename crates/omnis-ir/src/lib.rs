//! Stable canonical types used between `OmniSession` core, adapters, and bundles.

use std::{fmt, path::PathBuf, str::FromStr};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use uuid::Uuid;

pub const SCHEMA_VERSION: &str = "1.0";

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Provider {
    #[serde(alias = "claude-code")]
    Claude,
    Codex,
    #[serde(rename = "opencode", alias = "open-code")]
    OpenCode,
    Grok,
    Hermes,
    Antigravity,
    Pi,
    CursorCli,
    CursorIde,
    GenericAcp,
    Imported,
}

impl Provider {
    #[must_use]
    pub const fn command(self) -> Option<&'static str> {
        match self {
            Self::Claude => Some("claude"),
            Self::Codex => Some("codex"),
            Self::OpenCode => Some("opencode"),
            Self::Grok => Some("grok"),
            Self::Hermes => Some("hermes"),
            Self::Antigravity => Some("agy"),
            Self::Pi => Some("pi"),
            Self::CursorCli => Some("cursor-agent"),
            Self::CursorIde | Self::GenericAcp | Self::Imported => None,
        }
    }
}

impl fmt::Display for Provider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::OpenCode => "opencode",
            Self::Grok => "grok",
            Self::Hermes => "hermes",
            Self::Antigravity => "antigravity",
            Self::Pi => "pi",
            Self::CursorCli => "cursor-cli",
            Self::CursorIde => "cursor-ide",
            Self::GenericAcp => "acp",
            Self::Imported => "imported",
        };
        formatter.write_str(value)
    }
}

#[derive(Debug, Error)]
#[error("unknown provider `{0}`")]
pub struct ParseProviderError(String);

impl FromStr for Provider {
    type Err = ParseProviderError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "claude" | "claude-code" => Ok(Self::Claude),
            "codex" => Ok(Self::Codex),
            "opencode" | "open-code" => Ok(Self::OpenCode),
            "grok" => Ok(Self::Grok),
            "hermes" | "hermes-agent" => Ok(Self::Hermes),
            "agy" | "antigravity" | "google-antigravity" => Ok(Self::Antigravity),
            "pi" | "pi-coding-agent" => Ok(Self::Pi),
            "cursor" | "cursor-cli" | "cursor-agent" => Ok(Self::CursorCli),
            "cursor-ide" => Ok(Self::CursorIde),
            "acp" | "generic-acp" => Ok(Self::GenericAcp),
            "imported" => Ok(Self::Imported),
            _ => Err(ParseProviderError(value.to_owned())),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionRef {
    pub provider: Provider,
    pub id: String,
}

impl SessionRef {
    #[must_use]
    pub fn new(provider: Provider, id: impl Into<String>) -> Self {
        Self {
            provider,
            id: id.into(),
        }
    }
}

impl fmt::Display for SessionRef {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}:{}", self.provider, self.id)
    }
}

#[derive(Debug, Error)]
pub enum ParseSessionRefError {
    #[error("session reference must use provider:id syntax")]
    MissingSeparator,
    #[error("session ID cannot be empty")]
    EmptyId,
    #[error(transparent)]
    Provider(#[from] ParseProviderError),
}

impl FromStr for SessionRef {
    type Err = ParseSessionRefError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (provider, id) = value
            .split_once(':')
            .ok_or(ParseSessionRefError::MissingSeparator)?;
        if id.is_empty() {
            return Err(ParseSessionRefError::EmptyId);
        }
        Ok(Self::new(provider.parse()?, id))
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EventSource {
    pub provider: Provider,
    pub native_session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_record_type: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplayPolicy {
    Contextual,
    HistoricalOnly,
    Replayable,
    Secret,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Sensitivity {
    #[default]
    Normal,
    PotentialSecret,
    Secret,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    SessionStarted,
    MessageUser,
    MessageAssistant,
    ReasoningSummary,
    ToolCalled,
    ToolCompleted,
    ToolFailed,
    ApprovalRequested,
    ApprovalDecided,
    CommandExecuted,
    FileRead,
    FilePatch,
    FileSnapshot,
    PlanUpdated,
    TodoUpdated,
    CheckpointCreated,
    CompactionCreated,
    SubagentStarted,
    SubagentMessage,
    SubagentCompleted,
    ArtifactCreated,
    HandoffCreated,
    SessionCompleted,
    ProviderEvent,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OmniEvent {
    pub schema_version: String,
    pub event_id: Uuid,
    pub thread_id: Uuid,
    pub branch_id: Uuid,
    pub sequence: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<DateTime<Utc>>,
    pub source: EventSource,
    pub kind: EventKind,
    pub payload: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_blob_hash: Option<String>,
    pub sensitivity: Sensitivity,
    pub replay_policy: ReplayPolicy,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GitState {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_fingerprint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dirty_tree_digest: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub staged_diff_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unstaged_diff_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub untracked_files: Vec<PathBuf>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceSnapshot {
    pub schema_version: String,
    pub captured_at: DateTime<Utc>,
    pub root: PathBuf,
    pub current_dir: PathBuf,
    pub git: GitState,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub instruction_files: Vec<PathBuf>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub environment_names: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub available_tools: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CanonicalSnapshot {
    pub schema_version: String,
    pub session: SessionRef,
    pub thread_id: Uuid,
    pub branch_id: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub captured_at: DateTime<Utc>,
    pub workspace: WorkspaceSnapshot,
    pub events: Vec<OmniEvent>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferMode {
    NativeResume,
    NativeFork,
    OfficialImport,
    NativeMaterialization,
    SemanticHandoff,
    PortableExport,
}

impl fmt::Display for TransferMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::NativeResume => "native resume",
            Self::NativeFork => "native fork",
            Self::OfficialImport => "official import",
            Self::NativeMaterialization => "native materialization",
            Self::SemanticHandoff => "semantic handoff",
            Self::PortableExport => "portable export",
        };
        formatter.write_str(value)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FidelityStatus {
    Preserved,
    Summarized,
    HistoricalOnly,
    Redacted,
    Omitted,
    Unsupported,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FidelityEntry {
    pub feature: String,
    pub status: FidelityStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FidelityReport {
    pub source: Provider,
    pub target: Provider,
    pub mode: TransferMode,
    pub repository_matches: bool,
    pub entries: Vec<FidelityEntry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BundleManifest {
    pub schema_version: String,
    pub bundle_id: Uuid,
    pub created_at: DateTime<Utc>,
    pub source: SessionRef,
    pub event_count: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub redactions: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PortableBundle {
    pub manifest: BundleManifest,
    pub snapshot: CanonicalSnapshot,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fidelity: Option<FidelityReport>,
}

#[cfg(test)]
mod tests {
    use super::{Provider, SessionRef};

    #[test]
    fn session_reference_round_trips() {
        let reference: SessionRef = "claude:58fb".parse().expect("valid reference");
        assert_eq!(reference.provider, Provider::Claude);
        assert_eq!(reference.id, "58fb");
        assert_eq!(reference.to_string(), "claude:58fb");
    }

    #[test]
    fn cursor_alias_normalizes_to_cli_provider() {
        let reference: SessionRef = "cursor:abc".parse().expect("valid alias");
        assert_eq!(reference.provider, Provider::CursorCli);
        assert_eq!(reference.to_string(), "cursor-cli:abc");
    }

    #[test]
    fn claude_code_alias_normalizes() {
        let reference: SessionRef = "claude-code:abc".parse().expect("valid Claude alias");
        assert_eq!(reference.provider, Provider::Claude);
        assert_eq!(reference.to_string(), "claude:abc");
        assert_eq!(
            serde_json::from_str::<Provider>(r#""claude-code""#).expect("legacy provider"),
            Provider::Claude
        );
    }

    #[test]
    fn new_provider_aliases_normalize() {
        let antigravity: SessionRef = "agy:abc".parse().expect("valid Antigravity alias");
        let hermes: SessionRef = "hermes-agent:ghi".parse().expect("valid Hermes alias");
        let pi: SessionRef = "pi-coding-agent:def".parse().expect("valid Pi alias");
        assert_eq!(antigravity.provider, Provider::Antigravity);
        assert_eq!(antigravity.to_string(), "antigravity:abc");
        assert_eq!(hermes.provider, Provider::Hermes);
        assert_eq!(hermes.to_string(), "hermes:ghi");
        assert_eq!(pi.provider, Provider::Pi);
        assert_eq!(pi.to_string(), "pi:def");
    }

    #[test]
    fn opencode_uses_canonical_product_name_in_json() {
        assert_eq!(
            serde_json::to_string(&Provider::OpenCode).expect("serialize provider"),
            r#""opencode""#
        );
        assert_eq!(
            serde_json::from_str::<Provider>(r#""opencode""#).expect("canonical provider"),
            Provider::OpenCode
        );
        assert_eq!(
            serde_json::from_str::<Provider>(r#""open-code""#).expect("legacy provider"),
            Provider::OpenCode
        );
    }
}
