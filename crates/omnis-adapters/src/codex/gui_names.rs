use std::{
    collections::HashMap,
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
        let root = self.sqlite_home.as_deref().unwrap_or(home);
        let Some(path) = provider_file(root, &root.join("state_5.sqlite")) else {
            return HashMap::new();
        };
        // Older stores can omit the database or its name columns.
        with_database(root, &path, |db| read_names(db, ids)).unwrap_or_default()
    }
}

pub(super) struct StateName {
    pub(super) path: PathBuf,
    pub(super) name: Option<String>,
    cwd: Option<PathBuf>,
}

impl StateName {
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
        project: Option<&Path>,
        revision: Option<SourceRevision>,
    ) -> bool {
        if path_uuid(&self.path).as_deref() != Some(id)
            || project
                .zip(self.cwd.as_deref())
                .is_some_and(|(project, cwd)| !super::paths_match(project, cwd))
        {
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
    let cwd = if columns.contains_key("cwd") {
        "cwd"
    } else {
        "NULL"
    };
    let mut query = db.prepare(&format!(
        "SELECT rollout_path, {cwd}, name FROM threads WHERE id = ?1 AND history_mode = 'paginated'"
    ))?;
    let mut names = HashMap::new();
    for id in ids {
        if let Ok(row) = query.query_row([id], |row| {
            let name = row
                .get::<_, Option<String>>(2)?
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            Ok(StateName {
                path: PathBuf::from(row.get::<_, String>(0)?),
                cwd: row.get::<_, Option<String>>(1)?.map(PathBuf::from),
                name,
            })
        }) {
            names.insert(id.to_owned(), row);
        }
    }
    Ok(names)
}
