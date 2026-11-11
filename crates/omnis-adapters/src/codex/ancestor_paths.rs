//! Filenames for exact immutable-ancestor lookups during one indexing pass.

use std::{
    collections::HashMap,
    ffi::OsString,
    path::{Path, PathBuf},
};

use anyhow::Result;
use rusqlite::{Connection, OpenFlags, types::ValueRef};
use tempfile::{NamedTempFile, TempPath};

use super::rollout_identity::{rollout_id, visit_rollouts};

#[derive(Default)]
pub(super) struct AncestorPaths {
    paths: HashMap<String, Vec<PathBuf>>,
    overflow: Option<TempPath>,
}

impl AncestorPaths {
    pub(super) fn scan(home: &Path, limit: usize) -> Result<Self> {
        let mut result = Self::default();
        let mut writer = None;
        let mut retained = 0;
        visit_rollouts(home, |path| {
            let Some(id) = rollout_id(path) else {
                return Ok(());
            };
            if retained < limit {
                result
                    .paths
                    .entry(id.to_owned())
                    .or_default()
                    .push(path.to_owned());
                retained += 1;
            } else {
                // Keep older and nested ancestors addressable without holding every filename in RAM.
                let database = match &mut writer {
                    Some(database) => database,
                    slot => {
                        let file = NamedTempFile::new()?.into_temp_path();
                        let database = Connection::open(&file)?;
                        database.execute_batch(
                            "PRAGMA journal_mode = OFF; BEGIN;
                             CREATE TABLE paths(id TEXT, path NOT NULL);
                             CREATE INDEX paths_by_id ON paths(id)",
                        )?;
                        result.overflow = Some(file);
                        slot.insert(database)
                    }
                };
                let mut insert = database.prepare_cached("INSERT INTO paths VALUES (?1, ?2)")?;
                if let Some(text) = path.to_str() {
                    insert.execute(rusqlite::params![id, text])?;
                } else {
                    insert.execute(rusqlite::params![id, serde_json::to_vec(path.as_os_str())?])?;
                }
            }
            Ok(())
        })?;
        if let Some(database) = writer {
            database.execute_batch("COMMIT")?;
        }
        Ok(result)
    }

    pub(super) fn visit(&self, id: &str, mut each: impl FnMut(&Path) -> Result<()>) -> Result<()> {
        for path in self.paths.get(id).into_iter().flatten() {
            each(path)?;
        }
        if let Some(file) = &self.overflow {
            let db = Connection::open_with_flags(file, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            let mut statement = db.prepare("SELECT path FROM paths WHERE id = ?1")?;
            let mut rows = statement.query([id])?;
            while let Some(row) = rows.next()? {
                match row.get_ref(0)? {
                    ValueRef::Text(text) => each(Path::new(std::str::from_utf8(text)?))?,
                    ValueRef::Blob(bytes) => {
                        each(Path::new(&serde_json::from_slice::<OsString>(bytes)?))?;
                    }
                    _ => anyhow::bail!("invalid path in the temporary Codex filename index"),
                }
            }
        }
        Ok(())
    }
}
