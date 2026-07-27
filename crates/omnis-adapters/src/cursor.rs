use std::{collections::HashMap, env, fs, path::Path, path::PathBuf};

use anyhow::{Result, anyhow};
use chrono::{DateTime, Utc};
use omnis_ir::{EventKind, Provider, ReplayPolicy, SessionRef};
use rusqlite::Connection;
use serde_json::{Value, json};

use crate::{
    LaunchPlan, LaunchTarget, NativeSession, ProviderAdapter, ProviderInstallation,
    support::{
        EventBuilder, executable, nested_files, parse_timestamp, paths_match, provider_file,
        provider_root, read_json, selected_metadata, sort_sessions, sqlite_snapshot, string_at,
        validate_provider, value_at,
    },
};

#[derive(Clone, Debug)]
pub struct CursorCliAdapter {
    chats_root: Option<PathBuf>,
}

impl CursorCliAdapter {
    #[must_use]
    pub fn with_root(chats_root: impl Into<PathBuf>) -> Self {
        Self {
            chats_root: Some(chats_root.into()),
        }
    }

    fn metadata_files(&self) -> Vec<PathBuf> {
        self.chats_root
            .as_deref()
            .map(|root| nested_files(root, 2, Some("meta.json")))
            .unwrap_or_default()
    }

    fn find_metadata(&self, id: &str) -> Result<(Value, PathBuf)> {
        self.metadata_files()
            .into_iter()
            .find_map(|path| {
                let value = read_json(&path).ok()?;
                (session_id(&value, &path).as_deref() == Some(id)).then_some((value, path))
            })
            .ok_or_else(|| anyhow!("Cursor CLI session `{id}` was not found"))
    }
}

impl Default for CursorCliAdapter {
    fn default() -> Self {
        Self {
            chats_root: provider_root("CURSOR_AGENT_HOME", &[".cursor", "chats"]),
        }
    }
}

#[derive(Default)]
struct CursorMetadata {
    title: Option<String>,
    project_path: Option<PathBuf>,
    git_branch: Option<String>,
    created_at: Option<DateTime<Utc>>,
    updated_at: Option<DateTime<Utc>>,
    event_count: usize,
}

fn session_id(metadata: &Value, path: &Path) -> Option<String> {
    string_at(
        metadata,
        &[&["id"], &["sessionId"], &["session_id"], &["session", "id"]],
    )
    .map(str::to_owned)
    .or_else(|| {
        path.parent()?
            .file_name()?
            .to_str()
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
    })
}

fn metadata(value: &Value) -> CursorMetadata {
    CursorMetadata {
        title: string_at(
            value,
            &[
                &["title"],
                &["name"],
                &["session", "title"],
                &["metadata", "title"],
            ],
        )
        .map(str::to_owned),
        project_path: string_at(
            value,
            &[
                &["cwd"],
                &["projectPath"],
                &["project_path"],
                &["workspace", "path"],
            ],
        )
        .map(PathBuf::from),
        git_branch: string_at(
            value,
            &[&["gitBranch"], &["git_branch"], &["git", "branch"]],
        )
        .map(str::to_owned),
        created_at: parse_timestamp(value_at(
            value,
            &[
                &["createdAt"],
                &["created_at"],
                &["created"],
                &["time", "created"],
            ],
        )),
        updated_at: parse_timestamp(value_at(
            value,
            &[
                &["updatedAt"],
                &["updated_at"],
                &["updated"],
                &["time", "updated"],
            ],
        )),
        event_count: value_at(
            value,
            &[
                &["eventCount"],
                &["event_count"],
                &["messageCount"],
                &["message_count"],
            ],
        )
        .and_then(Value::as_u64)
        .and_then(|count| usize::try_from(count).ok())
        .unwrap_or(1),
    }
}

impl ProviderAdapter for CursorCliAdapter {
    fn provider(&self) -> Provider {
        Provider::CursorCli
    }

    fn probe(&self) -> ProviderInstallation {
        ProviderInstallation {
            provider: Provider::CursorCli,
            installed: executable("cursor-agent").is_some()
                || self.chats_root.as_deref().is_some_and(Path::is_dir),
            executable: executable("cursor-agent"),
            data_root: self.chats_root.clone(),
        }
    }

    fn list_sessions(&self, project: Option<&Path>) -> Result<Vec<NativeSession>> {
        let mut sessions = Vec::new();
        for path in self.metadata_files() {
            let value: Value = match read_json(&path) {
                Ok(value) => value,
                Err(_) => continue,
            };
            let Some(id) = session_id(&value, &path) else {
                continue;
            };
            let metadata = metadata(&value);
            if project.is_some_and(|project| {
                metadata
                    .project_path
                    .as_deref()
                    .is_none_or(|recorded| !paths_match(recorded, project))
            }) {
                continue;
            }
            sessions.push(NativeSession {
                session: SessionRef::new(Provider::CursorCli, id),
                title: metadata.title,
                project_path: metadata.project_path,
                git_branch: metadata.git_branch,
                created_at: metadata.created_at,
                updated_at: metadata.updated_at,
                event_count: metadata.event_count,
                source_path: Some(path),
            });
        }
        sort_sessions(&mut sessions);
        Ok(sessions)
    }

    fn read_session(&self, session: &SessionRef) -> Result<omnis_ir::CanonicalSnapshot> {
        validate_provider(session, Provider::CursorCli)?;
        let (value, _path) = self.find_metadata(&session.id)?;
        let metadata = metadata(&value);
        let captured_at = metadata.updated_at.unwrap_or_else(Utc::now);
        let mut builder = EventBuilder::new(Provider::CursorCli, &session.id);
        builder.push(
            EventKind::ProviderEvent,
            json!({
                "storage": "opaque",
                "metadata": selected_metadata(&value),
            }),
            metadata.updated_at,
            ReplayPolicy::HistoricalOnly,
            Some("meta".to_owned()),
            None,
        );
        Ok(builder.snapshot(
            session.clone(),
            metadata.title,
            metadata.project_path,
            metadata.git_branch,
            captured_at,
        ))
    }

    fn new_session_plan(&self, target: &LaunchTarget) -> Result<LaunchPlan> {
        Ok(LaunchPlan {
            program: "cursor-agent".to_owned(),
            args: target.prompt.iter().cloned().collect(),
            cwd: target.cwd.clone(),
        })
    }

    fn launch_plan(&self, session: &SessionRef, target: &LaunchTarget) -> Result<LaunchPlan> {
        validate_provider(session, Provider::CursorCli)?;
        if target.fork {
            return Err(anyhow!(
                "Cursor CLI has no verified fork flag; pass `--no-fork` to resume in place"
            ));
        }
        let mut args = vec!["--resume".to_owned(), session.id.clone()];
        if let Some(prompt) = &target.prompt {
            args.push(prompt.clone());
        }
        Ok(LaunchPlan {
            program: "cursor-agent".to_owned(),
            args,
            cwd: target.cwd.clone(),
        })
    }
}

#[derive(Clone, Debug)]
pub struct CursorIdeAdapter {
    metadata_root: Option<PathBuf>,
}

impl CursorIdeAdapter {
    #[must_use]
    pub fn with_root(metadata_root: impl Into<PathBuf>) -> Self {
        Self {
            metadata_root: Some(metadata_root.into()),
        }
    }

    fn database_path(&self) -> Option<PathBuf> {
        let root = self.metadata_root.as_deref()?;
        provider_file(root, &root.join("globalStorage").join("state.vscdb"))
    }

    fn workspace_paths(&self) -> HashMap<String, PathBuf> {
        let Some(root) = self.metadata_root.as_deref() else {
            return HashMap::new();
        };
        let workspace_root = root.join("workspaceStorage");
        if workspace_root
            .symlink_metadata()
            .is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            return HashMap::new();
        }
        let Ok(entries) = fs::read_dir(workspace_root) else {
            return HashMap::new();
        };
        entries
            .flatten()
            .filter_map(|entry| {
                let file_type = entry.file_type().ok()?;
                if file_type.is_symlink() || !file_type.is_dir() {
                    return None;
                }
                let id = entry.file_name().to_str()?.to_owned();
                let candidate = entry.path().join("workspace.json");
                let path = provider_file(root, &candidate)?;
                let value = read_json(&path).ok()?;
                let uri = string_at(&value, &[&["folder"], &["workspace"]])?;
                Some((id, file_uri_path(uri).unwrap_or_else(|| PathBuf::from(uri))))
            })
            .collect()
    }

    fn headers(&self) -> Result<Vec<CursorIdeHeader>> {
        let database = self
            .database_path()
            .filter(|path| path.is_file())
            .ok_or_else(|| anyhow!("Cursor IDE metadata database was not found"))?;
        let root = self
            .metadata_root
            .as_deref()
            .ok_or_else(|| anyhow!("Cursor IDE metadata root was not found"))?;
        let snapshot = sqlite_snapshot(root, &database)?;
        let connection = &snapshot.connection;
        let workspace_paths = self.workspace_paths();
        let mut headers = Vec::new();
        if table_exists(connection, "composerHeaders")? {
            let mut statement = connection.prepare(
                "SELECT composerId, workspaceId, createdAt, lastUpdatedAt, \
                 isArchived, isSubagent, value FROM composerHeaders",
            )?;
            let rows = statement.query_map([], |row| {
                let value = row.get_ref(6)?;
                let bytes = match value {
                    rusqlite::types::ValueRef::Text(value)
                    | rusqlite::types::ValueRef::Blob(value) => value.to_vec(),
                    _ => Vec::new(),
                };
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                    row.get::<_, Option<i64>>(4)?,
                    row.get::<_, Option<i64>>(5)?,
                    bytes,
                ))
            })?;
            for row in rows.flatten() {
                let (id, workspace_id, created, updated, archived, subagent, bytes) = row;
                let mut value =
                    serde_json::from_slice::<Value>(&bytes).unwrap_or_else(|_| json!({}));
                let Some(object) = value.as_object_mut() else {
                    continue;
                };
                object
                    .entry("composerId")
                    .or_insert_with(|| Value::String(id));
                if let Some(workspace_id) = workspace_id {
                    object
                        .entry("workspaceId")
                        .or_insert_with(|| Value::String(workspace_id.clone()));
                    if let Some(path) = workspace_paths.get(&workspace_id) {
                        object
                            .entry("workspacePath")
                            .or_insert_with(|| Value::String(path.to_string_lossy().into_owned()));
                    }
                }
                insert_integer(object, "createdAt", created);
                insert_integer(object, "lastUpdatedAt", updated);
                insert_boolean(object, "isArchived", archived);
                insert_boolean(object, "isSubagent", subagent);
                if let Some(header) = CursorIdeHeader::parse(&value, &database) {
                    headers.push(header);
                }
            }
            return Ok(headers);
        }
        if !table_exists(connection, "ItemTable")? {
            return Ok(headers);
        }
        let mut statement = connection.prepare(
            "SELECT value FROM ItemTable WHERE key IN \
             ('composer.composerData', 'composer.composerHeaders', 'cursor.composer.composerData')",
        )?;
        let values = statement.query_map([], |row| {
            let value = row.get_ref(0)?;
            Ok(match value {
                rusqlite::types::ValueRef::Text(value) | rusqlite::types::ValueRef::Blob(value) => {
                    value.to_vec()
                }
                _ => Vec::new(),
            })
        })?;
        for bytes in values.flatten() {
            let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
                continue;
            };
            for header in composer_headers(&value) {
                if let Some(header) = CursorIdeHeader::parse(header, &database) {
                    headers.push(header);
                }
            }
        }
        Ok(headers)
    }
}

fn table_exists(connection: &Connection, table: &str) -> rusqlite::Result<bool> {
    connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
        [table],
        |row| row.get(0),
    )
}

fn insert_integer(object: &mut serde_json::Map<String, Value>, key: &str, value: Option<i64>) {
    if let Some(value) = value {
        object
            .entry(key)
            .or_insert_with(|| Value::Number(value.into()));
    }
}

fn insert_boolean(object: &mut serde_json::Map<String, Value>, key: &str, value: Option<i64>) {
    if let Some(value) = value {
        object.entry(key).or_insert_with(|| Value::Bool(value != 0));
    }
}

struct CursorIdeHeader {
    id: String,
    title: Option<String>,
    project_path: Option<PathBuf>,
    created_at: Option<DateTime<Utc>>,
    updated_at: Option<DateTime<Utc>>,
    source_path: PathBuf,
    metadata: Value,
}

impl CursorIdeHeader {
    fn parse(value: &Value, database: &Path) -> Option<Self> {
        let id = string_at(value, &[&["composerId"], &["id"]])?;
        if value.get("isSubagent").and_then(Value::as_bool) == Some(true)
            || value.get("isArchived").and_then(Value::as_bool) == Some(true)
            || value.get("isRoot").and_then(Value::as_bool) == Some(false)
            || string_at(value, &[&["type"]]) == Some("subagent")
            || string_at(value, &[&["parentComposerId"]]).is_some()
            || string_at(value, &[&["rootComposerId"]]).is_some_and(|root| root != id)
        {
            return None;
        }
        let project_path = workspace_path(value);
        Some(Self {
            id: id.to_owned(),
            title: string_at(value, &[&["title"], &["name"]]).map(str::to_owned),
            project_path,
            created_at: parse_timestamp(value_at(
                value,
                &[&["createdAt"], &["created_at"], &["created"]],
            )),
            updated_at: parse_timestamp(value_at(
                value,
                &[
                    &["updatedAt"],
                    &["lastUpdatedAt"],
                    &["updated_at"],
                    &["lastUpdated"],
                ],
            )),
            source_path: database.to_path_buf(),
            metadata: json!({
                "composerId": id,
                "title": string_at(value, &[&["title"], &["name"]]),
                "workspace": workspace_path(value),
            }),
        })
    }

    fn into_native(self) -> NativeSession {
        NativeSession {
            session: SessionRef::new(Provider::CursorIde, self.id),
            title: self.title,
            project_path: self.project_path,
            git_branch: None,
            created_at: self.created_at,
            updated_at: self.updated_at,
            event_count: 1,
            source_path: Some(self.source_path),
        }
    }
}

fn composer_headers(value: &Value) -> &[Value] {
    value
        .get("composerHeaders")
        .and_then(Value::as_array)
        .or_else(|| {
            value
                .get("composerData")
                .and_then(|data| data.get("composerHeaders"))
                .and_then(Value::as_array)
        })
        .or_else(|| value.get("allComposers").and_then(Value::as_array))
        .or_else(|| value.as_array())
        .map_or(&[], Vec::as_slice)
}

fn workspace_path(value: &Value) -> Option<PathBuf> {
    if let Some(path) = string_at(
        value,
        &[
            &["workspacePath"],
            &["projectPath"],
            &["cwd"],
            &["workspace", "path"],
            &["workspace", "folder"],
            &["workspace", "rootPath"],
            &["workspaceUri", "fsPath"],
            &["workspaceUri", "path"],
            &["workspace", "uri", "fsPath"],
            &["workspace", "uri", "path"],
        ],
    ) {
        return Some(PathBuf::from(path));
    }
    let uri = string_at(
        value,
        &[
            &["workspaceUri"],
            &["workspaceURI"],
            &["workspace", "uri"],
            &["workspaceUris", "0"],
        ],
    )
    .or_else(|| {
        let first = value
            .get("workspaceUris")
            .and_then(Value::as_array)
            .and_then(|uris| uris.first())?;
        first
            .as_str()
            .or_else(|| string_at(first, &[&["fsPath"], &["path"]]))
    })?;
    file_uri_path(uri)
}

fn file_uri_path(uri: &str) -> Option<PathBuf> {
    let encoded = uri.strip_prefix("file://")?;
    let decoded = percent_decode(encoded)?;
    #[cfg(target_os = "windows")]
    let decoded = decoded
        .strip_prefix('/')
        .filter(|path| path.as_bytes().get(1) == Some(&b':'))
        .unwrap_or(&decoded)
        .to_owned();
    Some(PathBuf::from(decoded))
}

fn percent_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let high = *bytes.get(index + 1)?;
            let low = *bytes.get(index + 2)?;
            decoded.push((hex_value(high)? << 4) | hex_value(low)?);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

const fn hex_value(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

impl Default for CursorIdeAdapter {
    fn default() -> Self {
        let metadata_root = env::var_os("CURSOR_IDE_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(default_cursor_ide_root);
        Self { metadata_root }
    }
}

#[cfg(target_os = "windows")]
fn default_cursor_ide_root() -> Option<PathBuf> {
    env::var_os("APPDATA")
        .map(PathBuf::from)
        .map(|root| root.join("Cursor").join("User"))
}

#[cfg(target_os = "macos")]
fn default_cursor_ide_root() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|directories| {
        directories
            .home_dir()
            .join("Library")
            .join("Application Support")
            .join("Cursor")
            .join("User")
    })
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn default_cursor_ide_root() -> Option<PathBuf> {
    directories::BaseDirs::new()
        .map(|directories| directories.config_dir().join("Cursor").join("User"))
}

impl ProviderAdapter for CursorIdeAdapter {
    fn provider(&self) -> Provider {
        Provider::CursorIde
    }

    fn probe(&self) -> ProviderInstallation {
        let database = self.database_path();
        ProviderInstallation {
            provider: Provider::CursorIde,
            installed: database.as_deref().is_some_and(Path::is_file),
            executable: None,
            data_root: self.metadata_root.clone(),
        }
    }

    fn list_sessions(&self, project: Option<&Path>) -> Result<Vec<NativeSession>> {
        let mut sessions = self
            .headers()?
            .into_iter()
            .filter(|header| {
                project.is_none_or(|project| {
                    header
                        .project_path
                        .as_deref()
                        .is_some_and(|recorded| paths_match(recorded, project))
                })
            })
            .map(CursorIdeHeader::into_native)
            .collect::<Vec<_>>();
        sort_sessions(&mut sessions);
        Ok(sessions)
    }

    fn read_session(&self, session: &SessionRef) -> Result<omnis_ir::CanonicalSnapshot> {
        validate_provider(session, Provider::CursorIde)?;
        let header = self
            .headers()?
            .into_iter()
            .find(|header| header.id == session.id)
            .ok_or_else(|| anyhow!("Cursor IDE session `{}` was not found", session.id))?;
        let captured_at = header.updated_at.unwrap_or_else(Utc::now);
        let mut builder = EventBuilder::new(Provider::CursorIde, &session.id);
        builder.push(
            EventKind::ProviderEvent,
            json!({ "storage": "opaque", "metadata": header.metadata }),
            header.updated_at,
            ReplayPolicy::HistoricalOnly,
            Some("composerHeader".to_owned()),
            None,
        );
        Ok(builder.snapshot(
            session.clone(),
            header.title,
            header.project_path,
            None,
            captured_at,
        ))
    }

    fn new_session_plan(&self, _target: &LaunchTarget) -> Result<LaunchPlan> {
        Err(anyhow!("Cursor IDE has no supported native launcher"))
    }

    fn launch_plan(&self, session: &SessionRef, _target: &LaunchTarget) -> Result<LaunchPlan> {
        validate_provider(session, Provider::CursorIde)?;
        Err(anyhow!(
            "Cursor IDE sessions have no supported native launcher"
        ))
    }
}
