//! Recovers the original Codex home from a previously saved rollout header.

use std::{collections::HashMap, path::Path};

use rusqlite::{Connection, OptionalExtension, Row, Transaction, params};

use super::{
    IndexedSession, Result, database_error, immediate_transaction, indexed_session_from_row,
};

mod cache;

const MIGRATION: &str = "codex_home_from_saved_rollout_headers";
const CANDIDATES: &str = "SELECT c.provider,c.session_id,c.title,c.project_path,c.git_branch,
    c.created_at,c.updated_at,c.updated_at_approximate,c.event_count,c.source_file_id,c.source_contents_id,
    c.indexed_at,CASE WHEN d.id IS NOT NULL THEN json_array(d.id,d.source_updated_at,d.indexed_at,
        hex(d.content_hash),hex(d.source_file_id),hex(d.source_contents_id),d.document_version) END
    FROM session_index c LEFT JOIN session_trajectories d ON d.provider=c.provider AND d.session_id=c.session_id
    WHERE c.provider='codex' AND c.codex_home IS NULL AND c.updated_at IS NOT NULL AND NOT c.updated_at_approximate
      AND (c.source_file_id IS NULL)=(c.source_contents_id IS NULL)
      AND (d.id IS NULL OR (d.origin='native' AND d.source_file_id IS c.source_file_id))";

#[derive(PartialEq)]
struct Candidate {
    metadata: IndexedSession,
    indexed_at: i64,
    document: Option<String>,
}

impl Candidate {
    fn read(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            metadata: indexed_session_from_row(row)?,
            indexed_at: row.get(11)?,
            document: row.get(12)?,
        })
    }
}

pub(super) fn migrate(connection: &mut Connection, state_root: &Path) -> Result<()> {
    let applied: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM session_trajectory_migrations WHERE name=?1)",
            [MIGRATION],
            |row| row.get(0),
        )
        .map_err(database_error)?;
    if applied {
        return Ok(());
    }
    let observations = connection
        .prepare(CANDIDATES)
        .map_err(database_error)?
        .query_map([], Candidate::read)
        .map_err(database_error)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(database_error)?;
    let homes = if observations.is_empty() {
        HashMap::new()
    } else {
        cache::homes(state_root, observations.iter().map(|item| &item.metadata))
    };

    // Read the optional cache files before taking the writer lock. Check each row again before assigning its home.
    let transaction = immediate_transaction(connection)?;
    let started = transaction
        .execute(
            "INSERT OR IGNORE INTO session_trajectory_migrations(name) VALUES (?1)",
            [MIGRATION],
        )
        .map_err(database_error)?;
    if started != 0 && !homes.is_empty() {
        assign_confirmed_homes(&transaction, &observations, &homes)?;
    }
    transaction.commit().map_err(database_error)
}

fn assign_confirmed_homes(
    transaction: &Transaction<'_>,
    observations: &[Candidate],
    homes: &HashMap<String, String>,
) -> Result<()> {
    let mut current = transaction
        .prepare(&format!("{CANDIDATES} AND c.session_id=?1"))
        .map_err(database_error)?;
    let mut assign = transaction
        .prepare(
            "UPDATE session_index SET codex_home=?2, codex_legacy_name=?3
         WHERE provider='codex' AND session_id=?1 AND codex_home IS NULL",
        )
        .map_err(database_error)?;
    for prior in observations {
        let id = &prior.metadata.session.id;
        let Some(home) = homes.get(id) else { continue };
        let row = current
            .query_row([id], Candidate::read)
            .optional()
            .map_err(database_error)?;
        if row.as_ref() == Some(prior) {
            let unverified_name = prior.document.is_none() && prior.metadata.title.is_some();
            assign
                .execute(params![id, home, unverified_name])
                .map_err(database_error)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{CANDIDATES, Candidate, HashMap, assign_confirmed_homes};
    use crate::Store;

    #[test]
    fn a_saved_home_cannot_claim_a_catalog_entry_changed_during_upgrade() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(root.path().join("store.sqlite3")).unwrap();
        let mut connection = store.connection.borrow_mut();
        connection.execute_batch(
            "INSERT INTO session_index(provider,session_id,title,updated_at,indexed_at,event_count) VALUES
             ('codex','unchanged','old name',1,2,0),('codex','changed','old name',1,2,0);",
        ).unwrap();
        let previous = connection
            .prepare(CANDIDATES)
            .unwrap()
            .query_map([], Candidate::read)
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        let homes = ["unchanged", "changed"].map(|id| (id.to_owned(), "original-home".to_owned()));
        connection
            .execute(
                "UPDATE session_index SET title='new name' WHERE session_id='changed'",
                [],
            )
            .unwrap();
        let transaction = connection.transaction().unwrap();
        assign_confirmed_homes(&transaction, &previous, &HashMap::from(homes)).unwrap();
        transaction.commit().unwrap();
        let rows = connection
            .prepare("SELECT codex_home,codex_legacy_name FROM session_index ORDER BY session_id")
            .unwrap()
            .query_map([], |r| {
                Ok((r.get::<_, Option<String>>(0)?, r.get::<_, bool>(1)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            rows,
            [(None, false), (Some("original-home".to_owned()), true)]
        );
    }
}
