//! Stable canonical types used between `OmniSession` core, adapters, and bundles.

use std::{fmt, path::PathBuf, str::FromStr};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use uuid::Uuid;

pub const SCHEMA_VERSION: &str = "1.0";

/// Tool payloads can contain entire files or logs. Search retains both edges.
pub const SEARCH_DOCUMENT_TOOL_EDGE_BYTE_LIMIT: usize = 2 * 1024;
pub const SEARCH_DOCUMENT_EVENT_EDGE_BYTE_LIMIT: usize = 32 * 1024;
pub const SEARCH_DOCUMENT_EDGE_BYTE_LIMIT: usize = 1024 * 1024;
pub const SEARCH_TRUNCATION_NOTICE: &str = "\n[truncated by OmniSession]\n";

/// Measurements of eligible, redacted text after event limits and before document limits.
#[derive(Clone, Debug)]
pub struct SearchProjection {
    pub byte_count: usize,
    /// SHA-256 of the eligible text with its event separators.
    pub digest: [u8; 32],
    /// SHA-256 before the last native record that contributed eligible text.
    pub before_last_record: [u8; 32],
    /// Searchable byte boundaries at damaged records in a full scan with interior damage.
    pub interior_damage: Option<Vec<usize>>,
    pub last_record_time: Option<i64>,
    pub dated_suffix_records: Vec<(usize, usize, i64)>,
    /// Physical length at which the JSONL head and tail windows were sampled.
    pub sampled_file_bytes: Option<u64>,
    /// Digest and length of the confirmed text in the legacy sampling windows after damage.
    pub legacy_damage_sample: Option<([u8; 32], usize)>,
    pub torn_final: bool,
    pub verified_append: bool,
    /// Physical length scanned from start to end, including unreadable native records.
    pub fully_scanned_bytes: Option<u64>,
    state: sha2::Sha256,
}

impl SearchProjection {
    #[must_use]
    pub fn new(byte_count: usize, state: sha2::Sha256, before_last_record: sha2::Sha256) -> Self {
        use sha2::Digest;
        Self {
            byte_count,
            interior_damage: None,
            last_record_time: None,
            dated_suffix_records: Vec::new(),
            sampled_file_bytes: None,
            legacy_damage_sample: None,
            torn_final: false,
            verified_append: false,
            fully_scanned_bytes: None,
            digest: state.clone().finalize().into(),
            before_last_record: before_last_record.finalize().into(),
            state,
        }
    }

    /// Checks an older index digest that included the leading events of the missing record.
    #[must_use]
    pub fn matches_with_suffix(&self, suffix: &[u8], expected: &[u8]) -> bool {
        use sha2::Digest;
        self.state
            .clone()
            .chain_update(suffix)
            .finalize()
            .as_slice()
            == expected
    }
}

impl PartialEq for SearchProjection {
    fn eq(&self, other: &Self) -> bool {
        self.byte_count == other.byte_count
            && self.digest == other.digest
            && self.before_last_record == other.before_last_record
            && self.last_record_time == other.last_record_time
            && self.dated_suffix_records == other.dated_suffix_records
            && self.interior_damage == other.interior_damage
            && self.sampled_file_bytes == other.sampled_file_bytes
            && self.legacy_damage_sample == other.legacy_damage_sample
            && self.torn_final == other.torn_final
            && self.verified_append == other.verified_append
            && self.fully_scanned_bytes == other.fully_scanned_bytes
    }
}

impl Eq for SearchProjection {}

/// Splits a capped search document at the marker between its two retained windows.
#[must_use]
pub fn search_document_windows(text: &str) -> Option<(&str, &str)> {
    search_windows(text, SEARCH_DOCUMENT_EDGE_BYTE_LIMIT)
}

/// Returns the bounded event length, including its separator, when its complete head and tail
/// survive at the outside edge of an older document window.
#[must_use]
pub fn capped_search_event_at_edge(window: &str, tail: bool) -> Option<usize> {
    for edge in [
        SEARCH_DOCUMENT_EVENT_EDGE_BYTE_LIMIT,
        SEARCH_DOCUMENT_TOOL_EDGE_BYTE_LIMIT,
    ] {
        // Each UTF-8 boundary can move inward by at most three bytes.
        for len in (2 * edge - 6..=2 * edge).rev() {
            let event = if tail {
                let Some(start) = window.len().checked_sub(len) else {
                    continue;
                };
                window
                    .split_at_checked(start)
                    .filter(|(prefix, _)| prefix.ends_with("\n\n"))
                    .map(|(_, event)| event)
            } else {
                window
                    .split_at_checked(len)
                    .filter(|(_, suffix)| suffix.starts_with("\n\n"))
                    .map(|(event, _)| event)
            };
            if event.is_some_and(|text| search_windows(text, edge).is_some()) {
                return Some(len + 2);
            }
        }
    }
    None
}

fn search_windows(text: &str, edge_limit: usize) -> Option<(&str, &str)> {
    let head_budget = (2 * edge_limit - SEARCH_TRUNCATION_NOTICE.len()).div_ceil(2);
    (head_budget.saturating_sub(3)..=head_budget).find_map(|i| {
        let (head, rest) = text.split_at_checked(i)?;
        Some((head, rest.strip_prefix(SEARCH_TRUNCATION_NOTICE)?))
    })
}

macro_rules! define_providers {
    ($( $(#[$variant_attribute:meta])* $variant:ident ),+ $(,)?) => {
        #[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
        #[serde(rename_all = "kebab-case")]
        pub enum Provider {
            $(
                $(#[$variant_attribute])*
                $variant,
            )+
        }

        impl Provider {
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];
        }
    };
}

define_providers! {
    #[serde(alias = "claude-code")]
    Claude,
    Codex,
    #[serde(rename = "opencode", alias = "open-code")]
    OpenCode,
    Grok,
    Hermes,
    #[serde(alias = "antigravity-cli", alias = "agy")]
    Antigravity,
    AntigravityIde,
    AntigravityDesktop,
    Pi,
    #[serde(rename = "omp", alias = "oh-my-pi", alias = "ohmypi")]
    OhMyPi,
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
            Self::OhMyPi => Some("omp"),
            Self::CursorCli => Some("cursor-agent"),
            Self::AntigravityIde
            | Self::AntigravityDesktop
            | Self::CursorIde
            | Self::GenericAcp
            | Self::Imported => None,
        }
    }

    /// Every name [`FromStr`] accepts, canonical name first.
    #[must_use]
    pub const fn names(self) -> &'static [&'static str] {
        match self {
            Self::Claude => &["claude", "claude-code"],
            Self::Codex => &["codex"],
            Self::OpenCode => &["opencode", "open-code"],
            Self::Grok => &["grok"],
            Self::Hermes => &["hermes", "hermes-agent"],
            Self::Antigravity => &[
                "antigravity",
                "agy",
                "antigravity-cli",
                "google-antigravity",
            ],
            Self::AntigravityIde => &["antigravity-ide"],
            Self::AntigravityDesktop => &["antigravity-desktop"],
            Self::Pi => &["pi", "pi-coding-agent"],
            Self::OhMyPi => &["omp", "oh-my-pi", "ohmypi"],
            Self::CursorCli => &["cursor-cli", "cursor", "cursor-agent"],
            Self::CursorIde => &["cursor-ide"],
            Self::GenericAcp => &["acp", "generic-acp"],
            Self::Imported => &["imported"],
        }
    }
}

impl fmt::Display for Provider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.names()[0])
    }
}

#[derive(Debug, Error)]
#[error("unknown provider `{0}`")]
pub struct ParseProviderError(String);

impl FromStr for Provider {
    type Err = ParseProviderError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let name = value.to_ascii_lowercase();
        Self::ALL
            .iter()
            .copied()
            .find(|provider| provider.names().contains(&name.as_str()))
            .ok_or_else(|| ParseProviderError(value.to_owned()))
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

/// The selected provider file, its contents revision, and any missing source needed for indexing.
#[derive(Clone, Copy, Debug, Default, Eq)]
pub struct SourceRevision {
    /// A historical file ID derived from the same open file for cache migration.
    pub legacy_file: Option<Uuid>,
    pub file: Option<Uuid>,
    pub contents: Option<Uuid>,
    pub missing_ancestor: Option<Uuid>,
}

impl PartialEq for SourceRevision {
    fn eq(&self, other: &Self) -> bool {
        self.identity() == other.identity()
    }
}

impl std::hash::Hash for SourceRevision {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.identity().hash(state);
    }
}

impl SourceRevision {
    // Stored revisions keep their identity when a live reader supplies a historical file ID.
    fn identity(self) -> (Option<Uuid>, Option<Uuid>, Option<Uuid>) {
        (self.file, self.contents, self.missing_ancestor)
    }

    /// Checks whether both reads observed the same provider file and contents revision.
    #[must_use]
    pub fn matches_file_contents(self, other: Self) -> bool {
        self.file.is_some() && self.file == other.file && self.contents == other.contents
    }
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
    OpenWorkspace,
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
            Self::OpenWorkspace => "open workspace",
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
        let antigravity_cli: SessionRef = "antigravity-cli:abc"
            .parse()
            .expect("valid Antigravity CLI alias");
        let antigravity_ide: SessionRef = "antigravity-ide:xyz"
            .parse()
            .expect("valid Antigravity IDE reference");
        let hermes: SessionRef = "hermes-agent:ghi".parse().expect("valid Hermes alias");
        let pi: SessionRef = "pi-coding-agent:def".parse().expect("valid Pi alias");
        assert_eq!(antigravity.provider, Provider::Antigravity);
        assert_eq!(antigravity.to_string(), "antigravity:abc");
        assert_eq!(antigravity_cli.provider, Provider::Antigravity);
        assert_eq!(antigravity_cli.to_string(), "antigravity:abc");
        assert_eq!(antigravity_ide.provider, Provider::AntigravityIde);
        assert_eq!(antigravity_ide.to_string(), "antigravity-ide:xyz");
        assert_eq!(hermes.provider, Provider::Hermes);
        assert_eq!(hermes.to_string(), "hermes:ghi");
        assert_eq!(pi.provider, Provider::Pi);
        assert_eq!(pi.to_string(), "pi:def");
        assert_eq!(
            serde_json::from_str::<Provider>(r#""antigravity-cli""#).expect("cli alias"),
            Provider::Antigravity
        );
        assert_eq!(
            serde_json::from_str::<Provider>(r#""antigravity-ide""#).expect("ide provider"),
            Provider::AntigravityIde
        );
    }

    #[test]
    fn every_provider_name_parses_to_its_own_provider() {
        for &provider in Provider::ALL {
            for name in provider.names() {
                assert_eq!(name.parse::<Provider>().ok(), Some(provider), "{name}");
            }
        }
        assert_eq!("AGY".parse::<Provider>().ok(), Some(Provider::Antigravity));
        assert!("unknown-agent".parse::<Provider>().is_err());
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
