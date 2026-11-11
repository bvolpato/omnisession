//! Read-only discovery for the Antigravity desktop app (`antigravity-desktop`).
//!
//! Transcripts come from per-conversation `SQLite` databases that share the Antigravity CLI step
//! schema. Titles, timestamps, and workspaces come from the language server summary cache, which
//! can keep entries for conversations that no longer have a local database. Archived
//! conversations, whose databases the app empties, are not listed.

mod source;

use std::{
    collections::HashMap,
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow, ensure};
use chrono::Utc;
use omnis_ir::{CanonicalSnapshot, Provider, SessionRef};
use prost::Message;
use uuid::Uuid;

use crate::{
    LaunchPlan, LaunchTarget, NativeListing, NativeSession, ProviderAdapter, ProviderInstallation,
    antigravity::{
        ProtoTimestamp, history_file_time, nonempty, proto_timestamp, push_database_steps,
        validate_id, workspace_uri_path,
    },
    support::{
        EventBuilder, MAX_DISCOVERED_FILES, MAX_DISCOVERY_ENTRIES, paths_match, provider_file,
        provider_root, sort_sessions, store_is_missing, validate_provider,
    },
};

const ANNOTATIONS: &str = "annotations";
const CONVERSATIONS: &str = "conversations";
const SUMMARIES: &str = "agyhub_summaries_proto.pb";
const MAX_SUMMARIES_SIZE: u64 = 64 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct AntigravityDesktopAdapter {
    root: Option<PathBuf>,
    cached_step_count: Option<(Uuid, usize)>,
}

impl AntigravityDesktopAdapter {
    /// Discovery can list the databases when their optional title and workspace metadata is unreadable.
    pub const UNREADABLE_SUMMARIES: &str = "Antigravity Desktop summary metadata is unreadable. Conversation databases remain available in all-projects discovery.";

    #[must_use]
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        Self {
            root: Some(root.into()),
            cached_step_count: None,
        }
    }

    /// Uses the last declared count for this database if the Desktop summary is unavailable.
    #[must_use]
    pub fn with_cached_step_count(root: &Path, file: Uuid, count: usize) -> Self {
        Self {
            root: Some(root.to_owned()),
            cached_step_count: Some((file, count)),
        }
    }

    fn root(&self) -> Result<&Path> {
        self.root
            .as_deref()
            .ok_or_else(|| anyhow!("Antigravity Desktop data root was not found"))
    }
}

impl Default for AntigravityDesktopAdapter {
    fn default() -> Self {
        Self {
            root: provider_root("ANTIGRAVITY_DESKTOP_HOME", &[".gemini", "antigravity"]),
            cached_step_count: None,
        }
    }
}

impl ProviderAdapter for AntigravityDesktopAdapter {
    fn provider(&self) -> Provider {
        Provider::AntigravityDesktop
    }

    fn discovery_cache_home(&self) -> std::io::Result<Option<PathBuf>> {
        crate::support::discovery_home(self.root.as_deref())
    }

    fn probe(&self) -> ProviderInstallation {
        let data_root = self
            .root
            .clone()
            .filter(|root| root.join(CONVERSATIONS).is_dir());
        ProviderInstallation {
            provider: Provider::AntigravityDesktop,
            installed: data_root.is_some(),
            executable: None,
            data_root,
        }
    }

    fn list_sessions(&self, project: Option<&Path>) -> Result<Vec<NativeSession>> {
        self.list_catalog(project).map(|listing| listing.sessions)
    }

    fn list_catalog(&self, project: Option<&Path>) -> Result<NativeListing> {
        let Some(root) = self.root.as_deref() else {
            return Ok(NativeListing::default());
        };
        if store_is_missing(root) {
            return Ok(NativeListing::default());
        }
        let root = root
            .canonicalize()
            .context("resolving Antigravity Desktop home")?;
        let Some(conversations) = native_directory(&root, CONVERSATIONS)? else {
            return Ok(NativeListing::default());
        };
        let mut annotations = None;
        let mut notes = Vec::new();
        let summaries = read_summaries(&root).unwrap_or_else(|_| {
            notes.push(Self::UNREADABLE_SUMMARIES.to_owned());
            None
        });
        let entries =
            fs::read_dir(&conversations).context("listing Antigravity Desktop conversations")?;
        let mut sessions = Vec::new();
        let mut databases = 0;
        let mut unverified_desktop_ids = Vec::new();
        let mut missing_desktop_summary_ids = Vec::new();
        for (count, entry) in entries.enumerate() {
            ensure!(
                count < MAX_DISCOVERY_ENTRIES,
                "Antigravity Desktop conversation directory exceeds discovery limit"
            );
            let entry = entry.context("reading Antigravity Desktop directory entry")?;
            let candidate = entry.path();
            let Some(id) = database_id(&candidate) else {
                continue;
            };
            if !entry
                .file_type()
                .context("checking Antigravity Desktop directory entry")?
                .is_file()
            {
                continue;
            }
            if databases == 0 {
                annotations = native_directory(&root, ANNOTATIONS)?;
            }
            databases += 1;
            ensure!(
                databases <= MAX_DISCOVERED_FILES,
                "Antigravity Desktop conversation count exceeds discovery limit"
            );
            let database = candidate
                .canonicalize()
                .context("resolving listed Antigravity Desktop database")?;
            ensure!(
                database == candidate,
                "Antigravity Desktop database moved during discovery"
            );
            // The app hides archived conversations and empties their databases while keeping the
            // summary, so listing them would only show sessions that cannot be read.
            if let Some(directory) = &annotations {
                match archived(directory, id) {
                    Ok(true) => continue,
                    Ok(false) => {}
                    Err(error) => {
                        if unverified_desktop_ids.is_empty() {
                            notes.push(format!("Antigravity Desktop omitted conversations whose archive status could not be verified ({error:#})."));
                        }
                        unverified_desktop_ids.push(id.to_owned());
                        continue;
                    }
                }
            }
            let summary = summaries.as_ref().and_then(|rows| rows.get(id));
            let session = native_session(id, database, summary);
            if project.is_some_and(|requested| {
                session
                    .project_path
                    .as_deref()
                    .is_none_or(|recorded| !paths_match(recorded, requested))
            }) {
                continue;
            }
            if summaries.is_some() && summary.is_none() {
                missing_desktop_summary_ids.push(id.to_owned());
            }
            sessions.push(session);
        }
        sort_sessions(&mut sessions);
        Ok(NativeListing {
            sessions,
            notes,
            desktop_summaries_unavailable: summaries.is_none() && databases != 0,
            missing_desktop_summary_ids,
            unverified_desktop_ids,
        })
    }

    fn search_source_files(
        &self,
        session: &SessionRef,
        source_path: Option<&Path>,
    ) -> Result<Vec<PathBuf>> {
        validate_provider(session, Provider::AntigravityDesktop)?;
        validate_id(&session.id)?;
        search_files(self.root()?, &session.id, source_path)
    }

    fn observe_search_source(
        &self,
        session: &SessionRef,
        path: Option<&Path>,
    ) -> Result<Option<crate::SearchSourceObservation>> {
        source::observe(self, session, path)
    }

    fn read_session(&self, session: &SessionRef) -> Result<CanonicalSnapshot> {
        validate_provider(session, Provider::AntigravityDesktop)?;
        validate_id(&session.id)?;
        let root = self.root()?;
        let database = conversation_database(root, &session.id)
            .ok_or_else(|| anyhow!("Antigravity Desktop session `{}` was not found", session.id))?;
        let summaries = read_summaries(root).ok().flatten();
        let summary = summaries.as_ref().and_then(|rows| rows.get(&session.id));
        let listed = native_session(&session.id, database.clone(), summary);
        let mut builder = EventBuilder::new(Provider::AntigravityDesktop, &session.id);
        let declared_steps = if summary.is_none()
            && let Some((file, count)) = self.cached_step_count
            && listed.source_revision.and_then(|revision| revision.file) == Some(file)
        {
            count
        } else {
            listed.event_count
        };
        push_database_steps(root, &database, &mut builder, declared_steps)?;
        Ok(builder.snapshot(
            session.clone(),
            listed.title,
            listed.project_path,
            listed.git_branch,
            listed.updated_at.unwrap_or_else(Utc::now),
        ))
    }

    fn new_session_plan(&self, _target: &LaunchTarget) -> Result<LaunchPlan> {
        Err(anyhow!(
            "Antigravity Desktop has no supported native launcher"
        ))
    }

    fn discovery_notes(&self) -> Vec<String> {
        if self
            .root
            .as_ref()
            .is_some_and(|root| root.join(CONVERSATIONS).is_dir() && read_summaries(root).is_err())
        {
            vec![Self::UNREADABLE_SUMMARIES.to_owned()]
        } else {
            Vec::new()
        }
    }

    fn launch_plan(&self, session: &SessionRef, _target: &LaunchTarget) -> Result<LaunchPlan> {
        validate_provider(session, Provider::AntigravityDesktop)?;
        Err(anyhow!(
            "Antigravity Desktop sessions have no supported native launcher"
        ))
    }
}

pub(super) fn search_files(root: &Path, id: &str, listed: Option<&Path>) -> Result<Vec<PathBuf>> {
    crate::antigravity::selected_search_files(
        conversation_database(root, id),
        provider_file(root, &root.join(SUMMARIES)),
        listed,
    )
}

fn database_id(path: &Path) -> Option<&str> {
    if path.extension()? != "db" {
        return None;
    }
    let id = path.file_stem()?.to_str()?;
    validate_id(id).ok()?;
    Some(id)
}

fn conversation_database(root: &Path, id: &str) -> Option<PathBuf> {
    validate_id(id).ok()?;
    provider_file(root, &root.join(CONVERSATIONS).join(format!("{id}.db")))
}

fn native_directory(root: &Path, name: &str) -> Result<Option<PathBuf>> {
    let directory = root.join(name);
    if store_is_missing(&directory) {
        return Ok(None);
    }
    let directory = directory
        .canonicalize()
        .with_context(|| format!("resolving Antigravity Desktop {name} directory"))?;
    ensure!(
        directory.starts_with(root) && fs::metadata(&directory)?.is_dir(),
        "invalid Antigravity Desktop {name} directory"
    );
    Ok(Some(directory))
}

/// Whether the conversation's text-format annotation marks it archived.
fn archived(directory: &Path, id: &str) -> Result<bool> {
    const MAX_ANNOTATION_SIZE: u64 = 64 * 1024;
    let path = directory.join(format!("{id}.pbtxt"));
    if store_is_missing(&path) {
        return Ok(false);
    }
    ensure!(
        path.symlink_metadata()?.is_file() && path.canonicalize()? == path,
        "invalid Antigravity Desktop archive annotation"
    );
    let bytes = read_stable(&path, MAX_ANNOTATION_SIZE)
        .context("reading Antigravity Desktop archive annotation")?;
    annotation_archived(std::str::from_utf8(&bytes)?)
}

/// Finds a top-level `archived: true` field in a protobuf text-format message, ignoring nested
/// messages and quoted strings.
fn annotation_archived(text: &str) -> Result<bool> {
    let mut spaced = String::with_capacity(text.len());
    let mut quote = None;
    let mut escaped = false;
    let mut comment = false;
    for character in text.chars() {
        if comment {
            if character == '\n' {
                comment = false;
                spaced.push(' ');
            }
        } else if let Some(delimiter) = quote {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == delimiter {
                quote = None;
                spaced.push_str(" \"\" ");
            }
        } else if matches!(character, '"' | '\'') {
            quote = Some(character);
        } else if character == '#' {
            comment = true;
        } else if matches!(
            character,
            ':' | '{' | '}' | '<' | '>' | '[' | ']' | ',' | ';'
        ) {
            spaced.extend([' ', character, ' ']);
        } else {
            spaced.push(character);
        }
    }
    ensure!(
        quote.is_none(),
        "unterminated string in Antigravity Desktop archive annotation"
    );
    let mut closing = Vec::new();
    let mut archived = None;
    let mut previous = None;
    let mut tokens = spaced.split_whitespace();
    while let Some(token) = tokens.next() {
        match token {
            "{" => closing.push("}"),
            "<" => closing.push(">"),
            "[" => closing.push("]"),
            "}" | ">" | "]" => ensure!(
                closing.pop() == Some(token),
                "invalid Antigravity Desktop archive annotation"
            ),
            "archived" if closing.is_empty() && previous != Some(":") => {
                ensure!(
                    tokens.next() == Some(":"),
                    "invalid Antigravity Desktop archive flag"
                );
                ensure!(
                    archived.is_none(),
                    "duplicate Antigravity Desktop archive flag"
                );
                archived = Some(match tokens.next() {
                    Some("true" | "True" | "t" | "1") => true,
                    Some("false" | "False" | "f" | "0") => false,
                    _ => return Err(anyhow!("invalid Antigravity Desktop archive flag")),
                });
            }
            _ => {}
        }
        previous = Some(token);
    }
    ensure!(
        closing.is_empty(),
        "unfinished Antigravity Desktop archive annotation"
    );
    Ok(archived.unwrap_or(false))
}

fn native_session(
    id: &str,
    database: PathBuf,
    summary: Option<&ProtoTrajectorySummary>,
) -> NativeSession {
    let recorded_update = summary
        .and_then(|summary| summary.last_modified_time.as_ref())
        .and_then(proto_timestamp);
    // Databases without a summary entry fall back to file time, which non-step writes also move.
    let updated_at = recorded_update.or_else(|| history_file_time(&database));
    let project_path = summary.and_then(ProtoTrajectorySummary::project_path);
    let git_branch = project_path
        .as_ref()
        .and_then(|_| summary.and_then(ProtoTrajectorySummary::git_branch));
    let declared_steps = summary.map(|row| usize::try_from(row.step_count).unwrap_or(usize::MAX));
    NativeSession {
        source_revision: crate::antigravity::database_history_revision(&database, declared_steps),
        session: SessionRef::new(Provider::AntigravityDesktop, id),
        title: summary.and_then(|summary| nonempty(summary.summary.clone())),
        project_path,
        git_branch,
        created_at: summary
            .and_then(|summary| summary.created_time.as_ref())
            .and_then(proto_timestamp),
        updated_at,
        updated_at_approximate: recorded_update.is_none(),
        event_count: declared_steps.unwrap_or_default(),
        source_path: Some(database),
    }
}

fn read_summaries(root: &Path) -> Result<Option<HashMap<String, ProtoTrajectorySummary>>> {
    let path = root.join(SUMMARIES);
    if store_is_missing(&path) {
        return Ok(None);
    }
    let path = provider_file(root, &path)
        .ok_or_else(|| anyhow!("Antigravity Desktop summaries are outside the data root"))?;
    let bytes = read_stable(&path, MAX_SUMMARIES_SIZE)
        .context("failed to read Antigravity Desktop summaries")?;
    ProtoSummariesState::decode(bytes.as_slice())
        .map(|state| Some(state.summaries))
        .context("failed to decode Antigravity Desktop summaries")
}

/// Reads a bounded provider file and retries when a concurrent writer replaces it mid-read.
fn read_stable(path: &Path, limit: u64) -> Result<Vec<u8>> {
    for _ in 0..3 {
        let before = fs::metadata(path)?;
        ensure!(
            before.len() <= limit,
            "Antigravity Desktop metadata exceeds its read limit"
        );
        let mut bytes = Vec::new();
        File::open(path)?.take(limit + 1).read_to_end(&mut bytes)?;
        let after = fs::metadata(path)?;
        if before.len() == after.len()
            && before.modified().ok() == after.modified().ok()
            && u64::try_from(bytes.len()).is_ok_and(|read| read == after.len())
        {
            return Ok(bytes);
        }
    }
    Err(anyhow!("Antigravity Desktop metadata changed during read"))
}

/// `jetbox_summaries_pb.SummariesState`.
#[derive(Clone, PartialEq, Message)]
struct ProtoSummariesState {
    #[prost(map = "string, message", tag = "1")]
    summaries: HashMap<String, ProtoTrajectorySummary>,
}

/// `exa.jetski_cortex_pb.CascadeTrajectorySummary`, visible metadata only.
#[derive(Clone, PartialEq, Message)]
pub(crate) struct ProtoTrajectorySummary {
    #[prost(string, tag = "1")]
    pub(crate) summary: String,
    #[prost(uint32, tag = "2")]
    pub(crate) step_count: u32,
    #[prost(message, optional, tag = "3")]
    pub(crate) last_modified_time: Option<ProtoTimestamp>,
    #[prost(message, optional, tag = "7")]
    pub(crate) created_time: Option<ProtoTimestamp>,
    #[prost(message, repeated, tag = "9")]
    workspaces: Vec<ProtoWorkspaceMetadata>,
    #[prost(message, optional, tag = "17")]
    trajectory_metadata: Option<ProtoTrajectoryMetadata>,
}

impl ProtoTrajectorySummary {
    pub(crate) fn has_workspaces(&self) -> bool {
        !self.workspaces.is_empty()
            || self
                .trajectory_metadata
                .as_ref()
                .is_some_and(|metadata| !metadata.workspace_uris.is_empty())
    }

    pub(crate) fn project_path(&self) -> Option<PathBuf> {
        let uris = self
            .workspaces
            .iter()
            .map(|workspace| workspace.workspace_folder_absolute_uri.as_str())
            .chain(
                self.trajectory_metadata
                    .iter()
                    .flat_map(|metadata| metadata.workspace_uris.iter().map(String::as_str)),
            );
        let mut project: Option<PathBuf> = None;
        for uri in uris {
            let canonical = fs::canonicalize(workspace_uri_path(uri)?)
                .ok()
                .filter(|path| path.is_dir())?;
            if project
                .as_ref()
                .is_some_and(|project| project != &canonical)
            {
                return None;
            }
            project = Some(canonical);
        }
        project
    }

    pub(crate) fn git_branch(&self) -> Option<String> {
        let mut branch: Option<&str> = None;
        for workspace in &self.workspaces {
            if workspace.branch_name.is_empty() {
                continue;
            }
            if branch.is_some_and(|branch| branch != workspace.branch_name) {
                return None;
            }
            branch = Some(&workspace.branch_name);
        }
        branch.map(str::to_owned)
    }
}

/// `exa.cortex_pb.CortexWorkspaceMetadata`.
#[derive(Clone, PartialEq, Message)]
struct ProtoWorkspaceMetadata {
    #[prost(string, tag = "1")]
    workspace_folder_absolute_uri: String,
    #[prost(string, tag = "4")]
    branch_name: String,
}

/// `exa.cortex_pb.CortexTrajectoryMetadata`.
#[derive(Clone, PartialEq, Message)]
struct ProtoTrajectoryMetadata {
    #[prost(string, repeated, tag = "7")]
    workspace_uris: Vec<String>,
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, fs, path::Path};

    use chrono::DateTime;
    use omnis_ir::{EventKind, Provider, SessionRef};
    use prost::Message;

    use super::{
        AntigravityDesktopAdapter, ProtoSummariesState, ProtoTrajectorySummary,
        ProtoWorkspaceMetadata, SUMMARIES,
    };
    use crate::{
        ProviderAdapter,
        antigravity::{
            ProtoPlannerResponse, ProtoTextOrScopeItem, ProtoTimestamp, ProtoUserInput, proto_step,
            synthetic_step, write_steps_database,
        },
    };

    const SESSION_ID: &str = "11111111-1111-4111-8111-111111111111";
    const ORPHAN_ID: &str = "22222222-2222-4222-8222-222222222222";
    const STALE_ID: &str = "33333333-3333-4333-8333-333333333333";

    /// Desktop app user steps leave `query` empty.
    fn write_conversation(root: &Path, id: &str) {
        let conversations = root.join("conversations");
        fs::create_dir_all(&conversations).expect("conversations directory");
        let request = "synthetic request";
        write_steps_database(
            &conversations.join(format!("{id}.db")),
            &[
                synthetic_step(
                    14,
                    Some(proto_step::Step::UserInput(ProtoUserInput {
                        user_response: request.to_owned(),
                        items: vec![ProtoTextOrScopeItem {
                            text: request.to_owned(),
                        }],
                        ..ProtoUserInput::default()
                    })),
                ),
                synthetic_step(
                    15,
                    Some(proto_step::Step::PlannerResponse(ProtoPlannerResponse {
                        response: "synthetic answer".to_owned(),
                        modified_response: String::new(),
                    })),
                ),
                synthetic_step(21, None),
            ],
        );
    }

    fn write_summaries(root: &Path, summaries: Vec<(&str, ProtoTrajectorySummary)>) {
        let state = ProtoSummariesState {
            summaries: summaries
                .into_iter()
                .map(|(id, summary)| (id.to_owned(), summary))
                .collect::<HashMap<_, _>>(),
        };
        fs::write(root.join(SUMMARIES), state.encode_to_vec()).expect("summaries fixture");
    }

    fn file_uri(path: &Path) -> String {
        let mut uri_path = path.to_string_lossy().replace('\\', "/");
        if cfg!(windows) {
            uri_path.insert(0, '/');
        }
        format!("file://{uri_path}").replace(' ', "%20")
    }

    #[test]
    fn lists_and_reads_synthetic_desktop_conversations_without_mutation() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let root = temporary.path();
        let project = root.join("project space");
        fs::create_dir(&project).expect("project directory");
        let canonical_project = fs::canonicalize(&project).expect("canonical project directory");
        write_conversation(root, SESSION_ID);
        write_conversation(root, ORPHAN_ID);
        write_summaries(
            root,
            vec![
                (
                    SESSION_ID,
                    ProtoTrajectorySummary {
                        summary: "Synthetic desktop session".to_owned(),
                        step_count: 3,
                        last_modified_time: Some(ProtoTimestamp {
                            seconds: 1_785_240_060,
                            nanos: 0,
                        }),
                        created_time: Some(ProtoTimestamp {
                            seconds: 1_785_240_000,
                            nanos: 0,
                        }),
                        workspaces: vec![ProtoWorkspaceMetadata {
                            workspace_folder_absolute_uri: file_uri(&project),
                            branch_name: "main".to_owned(),
                        }],
                        trajectory_metadata: None,
                    },
                ),
                (
                    STALE_ID,
                    ProtoTrajectorySummary {
                        summary: "Summary without a database".to_owned(),
                        step_count: 1,
                        ..ProtoTrajectorySummary::default()
                    },
                ),
            ],
        );
        let read_store = || {
            [
                root.join(SUMMARIES),
                root.join(format!("conversations/{SESSION_ID}.db")),
                root.join(format!("conversations/{ORPHAN_ID}.db")),
            ]
            .map(|path| fs::read(path).expect("store file"))
        };
        let before = read_store();
        let adapter = AntigravityDesktopAdapter::with_root(root);

        let sessions = adapter.list_sessions(None).expect("session list");
        let mut ids = sessions
            .iter()
            .map(|session| session.session.id.as_str())
            .collect::<Vec<_>>();
        ids.sort_unstable();
        assert_eq!(ids, [SESSION_ID, ORPHAN_ID]);
        let listed = |id: &str| {
            sessions
                .iter()
                .find(|session| session.session.id == id)
                .expect("listed session")
        };
        let session = listed(SESSION_ID);
        assert_eq!(session.title.as_deref(), Some("Synthetic desktop session"));
        assert_eq!(session.project_path.as_ref(), Some(&canonical_project));
        assert_eq!(session.git_branch.as_deref(), Some("main"));
        let expected_timestamp = DateTime::from_timestamp(1_785_240_060, 0);
        assert_eq!(session.updated_at, expected_timestamp);
        assert!(!session.updated_at_approximate);
        assert_eq!(session.event_count, 3);
        let orphan = listed(ORPHAN_ID);
        assert!(orphan.title.is_none() && orphan.project_path.is_none());
        assert!(orphan.updated_at.is_some() && orphan.updated_at_approximate);

        let filtered = adapter.list_sessions(Some(&project)).expect("project list");
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].session.id, SESSION_ID);

        let snapshot = adapter
            .read_session(&SessionRef::new(Provider::AntigravityDesktop, SESSION_ID))
            .expect("session read");
        assert_eq!(
            snapshot
                .events
                .iter()
                .map(|event| &event.kind)
                .collect::<Vec<_>>(),
            [
                &EventKind::MessageUser,
                &EventKind::MessageAssistant,
                &EventKind::ProviderEvent,
                &EventKind::ProviderEvent,
            ]
        );
        assert_eq!(snapshot.events[0].payload["text"], "synthetic request");
        assert_eq!(snapshot.events[3].payload["unsupported_text_records"], 1);
        assert!(omnis_core::import_conversation(&snapshot).truncated);
        assert_eq!(snapshot.title.as_deref(), Some("Synthetic desktop session"));
        assert_eq!(snapshot.workspace.current_dir, canonical_project);
        assert_eq!(read_store(), before);
    }

    #[test]
    fn declared_empty_history_fails_but_corrupt_metadata_does_not_hide_databases() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let root = temporary.path();
        fs::create_dir(root.join("conversations")).expect("conversations directory");
        write_steps_database(&root.join(format!("conversations/{SESSION_ID}.db")), &[]);
        write_summaries(
            root,
            vec![(
                SESSION_ID,
                ProtoTrajectorySummary {
                    step_count: 2,
                    ..ProtoTrajectorySummary::default()
                },
            )],
        );
        let adapter = AntigravityDesktopAdapter::with_root(root);
        let error = adapter
            .read_session(&SessionRef::new(Provider::AntigravityDesktop, SESSION_ID))
            .expect_err("declared history must not read as empty");
        assert!(error.to_string().contains("no readable steps"), "{error:#}");

        fs::write(root.join(SUMMARIES), b"\x0a\xff").expect("corrupt summaries");
        write_conversation(root, ORPHAN_ID);
        let before = fs::read(root.join(SUMMARIES)).unwrap();
        let sessions = adapter.list_sessions(None).unwrap();
        assert_eq!(sessions.len(), 2);
        assert!(
            sessions
                .iter()
                .all(|session| session.title.is_none() && session.project_path.is_none())
        );
        let snapshot = adapter
            .read_session(&SessionRef::new(Provider::AntigravityDesktop, ORPHAN_ID))
            .unwrap();
        assert_eq!(snapshot.events[0].payload["text"], "synthetic request");
        assert!(
            adapter
                .discovery_notes()
                .iter()
                .any(|note| note.contains("unreadable"))
        );
        assert_eq!(fs::read(root.join(SUMMARIES)).unwrap(), before);
    }

    #[test]
    fn a_missing_optional_summary_lists_databases_without_claiming_a_metadata_clear() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        write_conversation(root, SESSION_ID);
        let adapter = AntigravityDesktopAdapter::with_root(root);
        let absent = adapter.list_catalog(None).unwrap();
        assert_eq!(absent.sessions.len(), 1);
        assert!(absent.notes.is_empty() && absent.desktop_summaries_unavailable);
        let session = &absent.sessions[0];
        let file = session.source_revision.unwrap().file.unwrap();
        let reader = AntigravityDesktopAdapter::with_cached_step_count(root, file, 4);
        let has_gap = || {
            let snapshot = reader.read_session(&session.session).unwrap();
            crate::AntigravityAdapter::has_missing_database_steps(&snapshot)
        };
        assert!(has_gap());

        write_summaries(root, vec![(ORPHAN_ID, ProtoTrajectorySummary::default())]);
        let empty = adapter.list_catalog(None).unwrap();
        assert_eq!(empty.sessions.len(), 1);
        assert!(empty.notes.is_empty() && !empty.desktop_summaries_unavailable);
        assert_eq!(empty.missing_desktop_summary_ids, [SESSION_ID]);
        assert!(has_gap());

        for step_count in [3, 0] {
            write_summaries(
                root,
                vec![(
                    SESSION_ID,
                    ProtoTrajectorySummary {
                        step_count,
                        ..Default::default()
                    },
                )],
            );
            assert!(
                adapter
                    .list_catalog(None)
                    .unwrap()
                    .missing_desktop_summary_ids
                    .is_empty()
            );
            assert!(!has_gap());
        }

        fs::remove_file(root.join(SUMMARIES)).unwrap();
        fs::rename(
            session.source_path.as_ref().unwrap(),
            root.join("previous.db"),
        )
        .unwrap();
        write_conversation(root, SESSION_ID);
        assert!(!has_gap());
    }

    #[test]
    fn invalid_archive_annotation_omits_only_its_session_from_live_discovery() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        write_conversation(root, SESSION_ID);
        write_conversation(root, ORPHAN_ID);
        let annotations = root.join("annotations");
        fs::create_dir(&annotations).unwrap();
        let path = annotations.join(format!("{SESSION_ID}.pbtxt"));
        let adapter = AntigravityDesktopAdapter::with_root(root);
        fs::write(&path, "# archived:true\n").unwrap();
        assert_eq!(adapter.list_sessions(None).unwrap().len(), 2);
        for invalid in [
            "archived:true archival_status_timestamp:{",
            "archived:true archived:false",
            "archived perhaps",
        ] {
            fs::write(&path, invalid).unwrap();
            let listing = adapter.list_catalog(None).unwrap();
            assert_eq!(listing.sessions.len(), 1);
            assert_eq!(listing.sessions[0].session.id, ORPHAN_ID);
            assert_eq!(listing.unverified_desktop_ids, [SESSION_ID]);
            assert!(
                listing
                    .notes
                    .iter()
                    .any(|note| note.contains("archive status"))
            );
        }
        let source = SessionRef::new(Provider::AntigravityDesktop, SESSION_ID);
        assert!(!adapter.read_session(&source).unwrap().events.is_empty());
        fs::write(&path, "archived: true").unwrap();
        let resolved = adapter.list_catalog(None).unwrap();
        assert_eq!(resolved.sessions.len(), 1);
        assert_eq!(resolved.sessions[0].session.id, ORPHAN_ID);
        assert!(resolved.unverified_desktop_ids.is_empty() && resolved.notes.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn directory_read_failure_is_distinct_from_an_empty_desktop_catalog() {
        use std::os::unix::fs::PermissionsExt;

        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        let adapter = AntigravityDesktopAdapter::with_root(root);
        assert!(adapter.list_sessions(None).unwrap().is_empty());
        write_conversation(root, SESSION_ID);
        let directory = root.join("conversations");
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o100)).unwrap();
        let cannot_list = fs::read_dir(&directory).is_err();
        let listing = adapter.list_sessions(None);
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        if cannot_list {
            assert!(listing.is_err());
        }
        assert_eq!(adapter.list_sessions(None).unwrap().len(), 1);
    }

    #[test]
    fn search_observes_the_selected_database_wal_count_and_archive() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        write_conversation(root, SESSION_ID);
        let adapter = AntigravityDesktopAdapter::with_root(root);
        let session = SessionRef::new(Provider::AntigravityDesktop, SESSION_ID);
        let path = root
            .join(format!("conversations/{SESSION_ID}.db"))
            .canonicalize()
            .unwrap();
        let writer = rusqlite::Connection::open(&path).unwrap();
        writer
            .execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0")
            .unwrap();
        let observe = || {
            adapter
                .observe_search_source(&session, Some(&path))
                .unwrap()
                .unwrap()
        };
        let missing = observe();
        assert!(missing.desktop_summaries_unavailable);
        fs::write(root.join(SUMMARIES), [0xff]).unwrap();
        assert!(observe().desktop_summaries_unavailable);
        write_summaries(root, vec![(ORPHAN_ID, ProtoTrajectorySummary::default())]);
        assert!(observe().desktop_summaries_unavailable);
        write_summaries(root, vec![(SESSION_ID, ProtoTrajectorySummary::default())]);
        let healthy = observe();
        assert!(!healthy.desktop_summaries_unavailable);
        assert_ne!(healthy.revision, missing.revision);
        writer
            .execute(
                "INSERT INTO steps SELECT idx + 10, step_type, step_payload FROM steps",
                [],
            )
            .unwrap();
        assert!(!healthy.is_current());
        let with_wal = observe();
        assert_ne!(with_wal.revision.contents, healthy.revision.contents);
        write_summaries(
            root,
            vec![(
                SESSION_ID,
                ProtoTrajectorySummary {
                    step_count: 6,
                    ..Default::default()
                },
            )],
        );
        let with_count = observe();
        assert_eq!(
            adapter.list_sessions(None).unwrap()[0].source_revision,
            Some(with_count.revision)
        );
        assert_ne!(with_count.revision.contents, with_wal.revision.contents);
        let annotations = root.join("annotations");
        fs::create_dir(&annotations).unwrap();
        fs::write(
            annotations.join(format!("{SESSION_ID}.pbtxt")),
            "archived: true",
        )
        .unwrap();
        assert!(!with_count.is_current());
        assert!(
            adapter
                .observe_search_source(&session, Some(&path))
                .is_err()
        );
        assert!(
            adapter
                .observe_search_source(&session, None)
                .unwrap()
                .is_none()
        );
        assert!(!adapter.read_session(&session).unwrap().events.is_empty());
    }

    #[test]
    fn archived_conversations_are_not_listed() {
        const ARCHIVED_ID: &str = "44444444-4444-4444-8444-444444444444";
        let temporary = tempfile::tempdir().expect("temporary root");
        let root = temporary.path();
        write_conversation(root, SESSION_ID);
        write_conversation(root, ORPHAN_ID);
        // Archiving empties the database but keeps a summary that still claims steps.
        write_steps_database(&root.join(format!("conversations/{ARCHIVED_ID}.db")), &[]);
        write_summaries(
            root,
            vec![(
                ARCHIVED_ID,
                ProtoTrajectorySummary {
                    step_count: 2,
                    ..ProtoTrajectorySummary::default()
                },
            )],
        );
        let annotations = root.join("annotations");
        fs::create_dir(&annotations).expect("annotations directory");
        fs::write(
            annotations.join(format!("{ARCHIVED_ID}.pbtxt")),
            "archived:true  archival_status_timestamp:{seconds:1782708366  nanos:907334000}",
        )
        .expect("archived annotation");
        // Quoted text and nested messages do not mark the conversation itself archived.
        fs::write(
            annotations.join(format!("{ORPHAN_ID}.pbtxt")),
            r#"title:"archived: true"  nested:{archived:true}"#,
        )
        .expect("unarchived annotation");

        let mut ids = AntigravityDesktopAdapter::with_root(root)
            .list_sessions(None)
            .expect("session list")
            .into_iter()
            .map(|session| session.session.id)
            .collect::<Vec<_>>();
        ids.sort_unstable();

        assert_eq!(ids, [SESSION_ID, ORPHAN_ID]);
    }
}
