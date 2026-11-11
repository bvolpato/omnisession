use std::{
    collections::HashMap,
    io::Read,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};

use super::path_uuid;
use crate::support::{provider_file, sqlite_snapshot};
use omnis_ir::SourceRevision;

#[derive(Clone, Debug, Default)]
pub(super) struct GuiNames {
    sqlite_home: Option<PathBuf>,
}

impl GuiNames {
    pub(super) fn from_environment() -> Self {
        Self {
            sqlite_home: std::env::var_os("CODEX_SQLITE_HOME")
                .filter(|path| !path.is_empty())
                .map(PathBuf::from),
        }
    }

    pub(super) fn read<'a>(
        &self,
        home: &Path,
        ids: impl Iterator<Item = &'a str>,
    ) -> HashMap<String, StateName> {
        let mut ids = ids.peekable();
        if ids.peek().is_none() {
            return HashMap::new();
        }
        let Ok(configured) = configured_home(home) else {
            return HashMap::new();
        };
        let root = configured
            .as_deref()
            .or(self.sqlite_home.as_deref())
            .unwrap_or(home);
        let Some(path) = provider_file(root, &root.join("state_5.sqlite")) else {
            return HashMap::new();
        };
        // Older stores can omit the database or its name columns.
        with_database(root, &path, |db| read_names(db, ids)).unwrap_or_default()
    }
}

fn configured_home(home: &Path) -> Result<Option<PathBuf>> {
    const LIMIT: u64 = 4 * 1024 * 1024;
    #[derive(serde::Deserialize)]
    struct Config {
        sqlite_home: Option<PathBuf>,
    }
    let path = home.join("config.toml");
    let metadata = match path.symlink_metadata() {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let path = if metadata.is_symlink() {
        let target = path.canonicalize()?;
        anyhow::ensure!(
            !target.components().any(|part| {
                let name = part.as_os_str().to_string_lossy();
                [
                    "auth.json",
                    "oauth_creds.json",
                    ".credentials.json",
                    "credentials.json",
                    ".credentials",
                    ".creds",
                    ".ssh",
                    ".gnupg",
                ]
                .iter()
                .any(|protected| name.eq_ignore_ascii_case(protected))
            }),
            "Codex config link points to a credential location"
        );
        target
    } else {
        path
    };
    let directory = path.parent().context("invalid Codex config file")?;
    let path = provider_file(directory, &path).context("invalid Codex config file")?;
    let file = std::fs::File::open(path)?;
    let mut text = String::new();
    anyhow::ensure!(file.metadata()?.len() <= LIMIT);
    file.take(LIMIT + 1).read_to_string(&mut text)?;
    anyhow::ensure!(text.len() as u64 <= LIMIT);
    let config: Config = toml::from_str(&text).ok().context("invalid Codex config")?;
    config
        .sqlite_home
        .map(|path| {
            if let Ok(relative) = path.strip_prefix("~") {
                let dirs = directories::BaseDirs::new().context("missing home directory")?;
                Ok(dirs.home_dir().join(relative))
            } else {
                Ok(home.join(path))
            }
        })
        .transpose()
}

pub(super) struct StateName {
    pub(super) path: PathBuf,
    name: Option<String>,
    use_index: bool,
}

impl StateName {
    pub(super) fn title(&self, indexed: impl FnOnce() -> Option<String>) -> Option<String> {
        if self.use_index {
            indexed().or_else(|| self.name.clone())
        } else {
            self.name.clone()
        }
    }

    pub(super) fn matches_path(&self, path: &Path) -> bool {
        self.path.is_absolute()
            && (self.path == path
                || self.path.canonicalize().is_ok_and(|recorded| {
                    path.canonicalize()
                        .is_ok_and(|selected| selected == recorded)
                }))
    }

    pub(super) fn matches_cached(
        &self,
        home: &Path,
        id: &str,
        revision: Option<SourceRevision>,
    ) -> bool {
        if path_uuid(&self.path).as_deref() != Some(id) {
            return false;
        }
        let Some(path) = provider_file(home, &self.path) else {
            return false;
        };
        let Some(relative) = home
            .canonicalize()
            .ok()
            .and_then(|root| path.strip_prefix(root).ok().map(Path::to_owned))
        else {
            return false;
        };
        (relative.starts_with("sessions") || relative.starts_with("archived_sessions"))
            && revision.and_then(|rev| rev.file).is_some_and(|file| {
                crate::source_file_revision(&path).is_some_and(|rev| rev.file == Some(file))
            })
    }
}

fn with_database<T>(
    root: &Path,
    path: &Path,
    read: impl FnOnce(&Connection) -> Result<T>,
) -> Result<T> {
    let wal = path.with_extension("sqlite-wal");
    let shm = path.with_extension("sqlite-shm");
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    if path
        .with_extension("sqlite-journal")
        .symlink_metadata()
        .is_ok()
    {
        anyhow::bail!("Codex database has a rollback journal");
    }
    let option = if wal
        .symlink_metadata()
        .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
    {
        // An immutable read avoids creating WAL sidecars for a checkpointed database.
        "immutable=1"
    } else if provider_file(root, &wal).is_some() && provider_file(root, &shm).is_some() {
        "readonly_shm=1"
    } else {
        // A restored WAL without a shared-memory index needs private sidecars.
        return read(&sqlite_snapshot(root, path)?.connection);
    };
    let uri = database_uri(path, option).context("unsupported Codex database path")?;
    let connection = Connection::open_with_flags(uri, flags | OpenFlags::SQLITE_OPEN_URI)?;
    connection.busy_timeout(Duration::ZERO)?;
    connection.pragma_update(None, "query_only", true)?;
    connection.pragma_update(None, "trusted_schema", false)?;
    read(&connection)
}

fn database_uri(path: &Path, option: &str) -> Option<String> {
    let path = path.to_str()?;
    #[cfg(windows)]
    let path = path
        .strip_prefix(r"\\?\")
        .unwrap_or(path)
        .replace('\\', "/");
    Some(format!(
        "file:{}?{option}",
        path.replace('%', "%25")
            .replace('?', "%3f")
            .replace('#', "%23")
    ))
}

fn read_names<'a>(
    db: &Connection,
    ids: impl Iterator<Item = &'a str>,
) -> Result<HashMap<String, StateName>> {
    // Codex trims Unicode whitespace when comparing the legacy title with the first message.
    const WHITESPACE: &str = " \t\n\r\u{b}\u{c}\u{85}\u{a0}\u{1680}\u{2000}\u{2001}\u{2002}\u{2003}\u{2004}\u{2005}\u{2006}\u{2007}\u{2008}\u{2009}\u{200a}\u{2028}\u{2029}\u{202f}\u{205f}\u{3000}";
    let columns = db
        .prepare(
            "SELECT name, pk FROM pragma_table_info('threads') WHERE EXISTS (
            SELECT 1 FROM sqlite_schema WHERE name = 'threads' AND type = 'table')",
        )?
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?
        .collect::<rusqlite::Result<HashMap<_, _>>>()?;
    if columns.get("id") != Some(&1) {
        return Ok(HashMap::new());
    }
    let legacy = if columns.contains_key("title") && columns.contains_key("first_user_message") {
        "CASE WHEN trim(title, ?2) IS NOT trim(first_user_message, ?2) THEN title END"
    } else {
        "NULL"
    };
    let guardian = if columns.contains_key("source") {
        "CASE WHEN json_valid(source) THEN json_extract(source, '$.subagent.other') IS 'guardian' ELSE 0 END"
    } else {
        "0"
    };
    let mut query = db.prepare(&format!(
        "SELECT rollout_path, history_mode, trim(CASE history_mode
         WHEN 'paginated' THEN name WHEN 'legacy' THEN {legacy} END, ?2), {guardian}
         FROM threads WHERE id = ?1 AND history_mode IN ('legacy', 'paginated')"
    ))?;
    let mut names = HashMap::new();
    for id in ids {
        if let Ok(row) = query.query_row([id, WHITESPACE], |row| {
            let name = row
                .get::<_, Option<String>>(2)?
                .filter(|value| !value.is_empty());
            Ok(StateName {
                path: PathBuf::from(row.get::<_, String>(0)?),
                use_index: row.get::<_, String>(1)? == "legacy"
                    && (name.is_none()
                        || (name.as_deref() == Some("Guardian review")
                            && row.get::<_, bool>(3)?)),
                name,
            })
        }) {
            names.insert(id.to_owned(), row);
        }
    }
    Ok(names)
}
