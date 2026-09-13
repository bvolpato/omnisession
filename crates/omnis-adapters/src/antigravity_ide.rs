//! Read-only discovery for the Antigravity desktop app (`antigravity-ide`).
//!
//! Transcripts come from per-conversation SQLite databases that share the Antigravity CLI step
//! schema. Titles, timestamps, and workspaces come from the language server summary cache, which
//! can keep entries for conversations that no longer have a local database.

use std::{
    collections::HashMap,
    ffi::OsStr,
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use omnis_ir::{CanonicalSnapshot, Provider, SessionRef};
use prost::Message;

use crate::{
    LaunchPlan, LaunchTarget, NativeSession, ProviderAdapter, ProviderInstallation,
    antigravity::{
        ProtoTimestamp, nonempty, proto_timestamp, push_database_steps, validate_id,
        workspace_uri_path,
    },
    support::{
        EventBuilder, nested_files_matching, paths_match, provider_file, provider_root,
        sort_sessions, store_is_missing, validate_provider,
    },
};

const CONVERSATIONS: &str = "conversations";
const SUMMARIES: &str = "agyhub_summaries_proto.pb";
const MAX_SUMMARIES_SIZE: u64 = 64 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct AntigravityIdeAdapter {
    root: Option<PathBuf>,
}

impl AntigravityIdeAdapter {
    #[must_use]
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        Self {
            root: Some(root.into()),
        }
    }

    fn root(&self) -> Result<&Path> {
        self.root
            .as_deref()
            .ok_or_else(|| anyhow!("Antigravity IDE data root was not found"))
    }
}

impl Default for AntigravityIdeAdapter {
    fn default() -> Self {
        Self {
            root: provider_root("ANTIGRAVITY_IDE_HOME", &[".gemini", "antigravity"]),
        }
    }
}

impl ProviderAdapter for AntigravityIdeAdapter {
    fn provider(&self) -> Provider {
        Provider::AntigravityIde
    }

    fn probe(&self) -> ProviderInstallation {
        let data_root = self
            .root
            .clone()
            .filter(|root| root.join(CONVERSATIONS).is_dir());
        ProviderInstallation {
            provider: Provider::AntigravityIde,
            installed: data_root.is_some(),
            executable: None,
            data_root,
        }
    }

    fn list_sessions(&self, project: Option<&Path>) -> Result<Vec<NativeSession>> {
        let Some(root) = self.root.as_deref() else {
            return Ok(Vec::new());
        };
        let conversations = root.join(CONVERSATIONS);
        if store_is_missing(&conversations) {
            return Ok(Vec::new());
        }
        let summaries = read_summaries(root)?;
        let mut sessions = Vec::new();
        for candidate in nested_files_matching(&conversations, 0, &|path, is_dir| {
            !is_dir && path.extension().is_some_and(|extension| extension == "db")
        }) {
            let Some(id) = candidate.file_stem().and_then(OsStr::to_str) else {
                continue;
            };
            let Some(database) = conversation_database(root, id) else {
                continue;
            };
            let session = native_session(id, database, summaries.get(id));
            if project.is_some_and(|requested| {
                session
                    .project_path
                    .as_deref()
                    .is_none_or(|recorded| !paths_match(recorded, requested))
            }) {
                continue;
            }
            sessions.push(session);
        }
        sort_sessions(&mut sessions);
        Ok(sessions)
    }

    fn read_session(&self, session: &SessionRef) -> Result<CanonicalSnapshot> {
        validate_provider(session, Provider::AntigravityIde)?;
        validate_id(&session.id)?;
        let root = self.root()?;
        let database = conversation_database(root, &session.id)
            .ok_or_else(|| anyhow!("Antigravity IDE session `{}` was not found", session.id))?;
        let summaries = read_summaries(root)?;
        let listed = native_session(&session.id, database.clone(), summaries.get(&session.id));
        let mut builder = EventBuilder::new(Provider::AntigravityIde, &session.id);
        let decoded = push_database_steps(root, &database, &mut builder)?;
        if decoded == 0 && listed.event_count > 0 {
            return Err(anyhow!(
                "Antigravity IDE session `{}` declares history but has no readable steps",
                session.id
            ));
        }
        Ok(builder.snapshot(
            session.clone(),
            listed.title,
            listed.project_path,
            listed.git_branch,
            listed.updated_at.unwrap_or_else(Utc::now),
        ))
    }

    fn new_session_plan(&self, _target: &LaunchTarget) -> Result<LaunchPlan> {
        Err(anyhow!("Antigravity IDE has no supported native launcher"))
    }

    fn launch_plan(&self, session: &SessionRef, _target: &LaunchTarget) -> Result<LaunchPlan> {
        validate_provider(session, Provider::AntigravityIde)?;
        Err(anyhow!(
            "Antigravity IDE sessions have no supported native launcher"
        ))
    }
}

fn conversation_database(root: &Path, id: &str) -> Option<PathBuf> {
    validate_id(id).ok()?;
    provider_file(root, &root.join(CONVERSATIONS).join(format!("{id}.db")))
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
    let updated_at = recorded_update.or_else(|| {
        fs::metadata(&database)
            .and_then(|metadata| metadata.modified())
            .ok()
            .map(DateTime::<Utc>::from)
    });
    NativeSession {
        session: SessionRef::new(Provider::AntigravityIde, id),
        title: summary.and_then(|summary| nonempty(summary.summary.clone())),
        project_path: summary.and_then(ProtoTrajectorySummary::project_path),
        git_branch: summary
            .and_then(|summary| summary.workspaces.first())
            .and_then(|workspace| nonempty(workspace.branch_name.clone())),
        created_at: summary
            .and_then(|summary| summary.created_time.as_ref())
            .and_then(proto_timestamp),
        updated_at,
        updated_at_approximate: recorded_update.is_none(),
        event_count: summary.map_or(0, |summary| {
            usize::try_from(summary.step_count).unwrap_or(usize::MAX)
        }),
        source_path: Some(database),
    }
}

fn read_summaries(root: &Path) -> Result<HashMap<String, ProtoTrajectorySummary>> {
    let path = root.join(SUMMARIES);
    if store_is_missing(&path) {
        return Ok(HashMap::new());
    }
    let path = provider_file(root, &path)
        .ok_or_else(|| anyhow!("Antigravity IDE summaries are outside the data root"))?;
    let bytes = read_stable(&path).context("failed to read Antigravity IDE summaries")?;
    ProtoSummariesState::decode(bytes.as_slice())
        .map(|state| state.summaries)
        .context("failed to decode Antigravity IDE summaries")
}

/// Reads a bounded provider file and retries when a concurrent writer replaces it mid-read.
fn read_stable(path: &Path) -> Result<Vec<u8>> {
    for _ in 0..3 {
        let before = fs::metadata(path)?;
        if before.len() > MAX_SUMMARIES_SIZE {
            return Err(anyhow!("Antigravity IDE summaries exceed safe read limit"));
        }
        let mut bytes = Vec::new();
        File::open(path)?
            .take(MAX_SUMMARIES_SIZE + 1)
            .read_to_end(&mut bytes)?;
        let after = fs::metadata(path)?;
        if before.len() == after.len()
            && before.modified().ok() == after.modified().ok()
            && u64::try_from(bytes.len()).is_ok_and(|read| read == after.len())
        {
            return Ok(bytes);
        }
    }
    Err(anyhow!("Antigravity IDE summaries changed during read"))
}

/// `jetbox_summaries_pb.SummariesState`.
#[derive(Clone, PartialEq, Message)]
struct ProtoSummariesState {
    #[prost(map = "string, message", tag = "1")]
    summaries: HashMap<String, ProtoTrajectorySummary>,
}

/// `exa.jetski_cortex_pb.CascadeTrajectorySummary`, visible metadata only.
#[derive(Clone, PartialEq, Message)]
struct ProtoTrajectorySummary {
    #[prost(string, tag = "1")]
    summary: String,
    #[prost(uint32, tag = "2")]
    step_count: u32,
    #[prost(message, optional, tag = "3")]
    last_modified_time: Option<ProtoTimestamp>,
    #[prost(message, optional, tag = "7")]
    created_time: Option<ProtoTimestamp>,
    #[prost(message, repeated, tag = "9")]
    workspaces: Vec<ProtoWorkspaceMetadata>,
    #[prost(message, optional, tag = "17")]
    trajectory_metadata: Option<ProtoTrajectoryMetadata>,
}

impl ProtoTrajectorySummary {
    fn project_path(&self) -> Option<PathBuf> {
        self.workspaces
            .first()
            .map(|workspace| workspace.workspace_folder_absolute_uri.as_str())
            .filter(|uri| !uri.is_empty())
            .or_else(|| {
                self.trajectory_metadata
                    .as_ref()?
                    .workspace_uris
                    .first()
                    .map(String::as_str)
            })
            .and_then(workspace_uri_path)
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
        AntigravityIdeAdapter, ProtoSummariesState, ProtoTrajectorySummary, ProtoWorkspaceMetadata,
        SUMMARIES,
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
        let adapter = AntigravityIdeAdapter::with_root(root);

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
        assert_eq!(session.project_path.as_deref(), Some(project.as_path()));
        assert_eq!(session.git_branch.as_deref(), Some("main"));
        assert_eq!(
            session.updated_at,
            DateTime::from_timestamp(1_785_240_060, 0)
        );
        assert!(!session.updated_at_approximate);
        assert_eq!(session.event_count, 3);
        let orphan = listed(ORPHAN_ID);
        assert!(orphan.title.is_none() && orphan.project_path.is_none());
        assert!(orphan.updated_at.is_some() && orphan.updated_at_approximate);

        let filtered = adapter.list_sessions(Some(&project)).expect("project list");
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].session.id, SESSION_ID);

        let snapshot = adapter
            .read_session(&SessionRef::new(Provider::AntigravityIde, SESSION_ID))
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
            ]
        );
        assert_eq!(snapshot.events[0].payload["text"], "synthetic request");
        assert_eq!(snapshot.title.as_deref(), Some("Synthetic desktop session"));
        assert_eq!(snapshot.workspace.current_dir, project);
        assert_eq!(read_store(), before);
    }

    #[test]
    fn fails_on_declared_history_without_steps_and_corrupt_summaries() {
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
        let adapter = AntigravityIdeAdapter::with_root(root);
        let error = adapter
            .read_session(&SessionRef::new(Provider::AntigravityIde, SESSION_ID))
            .expect_err("declared history must not read as empty");
        assert!(error.to_string().contains("no readable steps"), "{error:#}");

        fs::write(root.join(SUMMARIES), b"\x0a\xff").expect("corrupt summaries");
        assert!(adapter.list_sessions(None).is_err());
    }
}
