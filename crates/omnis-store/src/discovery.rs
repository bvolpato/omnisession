use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

use chrono::{DateTime, Utc};
use omnis_ir::Provider;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    CatalogListing, DiscoveryScope, IndexedSession, Result, SessionDiscoveryAttempt, Store,
    StoreError, database_error, immediate_transaction, indexed_session_from_row, now_timestamp,
};

/// Tells a picker whether to read the catalog, follow another reader, or run discovery itself.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionDiscoveryClaim {
    /// A recent check finished. Its catalog changes, if any, are visible.
    Fresh,
    /// Another reader owns this attempt and has not published a result.
    InProgress(SessionDiscoveryAttempt),
    /// The caller owns this attempt.
    Claimed(SessionDiscoveryAttempt),
}

/// Outcome visible to a reader following one provider-wide discovery attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionDiscoveryState {
    /// The owner is still within its lease.
    InProgress,
    /// The owner committed its listing.
    Published,
    /// The listing failed and the previous catalog is still available.
    Failed,
    /// The owner has not completed the attempt within its lease.
    Expired,
    /// The current attempt belongs to another invocation or home.
    Superseded,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Observation {
    selected_home: bool,
    id: Uuid,
    checked_at: i64,
    succeeded: Option<bool>,
}

impl Observation {
    fn state(&self, fresh_since: DateTime<Utc>) -> SessionDiscoveryState {
        match self.succeeded {
            Some(true) => SessionDiscoveryState::Published,
            Some(false) => SessionDiscoveryState::Failed,
            None if self.checked_at < fresh_since.timestamp_millis() => {
                SessionDiscoveryState::Expired
            }
            None => SessionDiscoveryState::InProgress,
        }
    }
}

fn current_attempt(
    connection: &Connection,
    provider: Provider,
    version: u32,
    home: &Path,
) -> Result<Option<Observation>> {
    let mut statement = connection
        .prepare_cached(
            "SELECT lease.attempt_id, lease.checked_at, result.succeeded, lease.source_home = ?3
             FROM session_index_reader_checks AS lease
             LEFT JOIN session_index_discovery_results AS result USING (provider,reader_version,attempt_id)
             WHERE lease.provider = ?1 AND lease.reader_version = ?2",
        )
        .map_err(database_error)?;
    statement
        .query_row(
            params![
                provider.to_string(),
                version,
                home.as_os_str().as_encoded_bytes()
            ],
            |row| {
                Ok(Observation {
                    id: Uuid::from_bytes(row.get(0)?),
                    checked_at: row.get(1)?,
                    succeeded: row.get(2)?,
                    selected_home: row.get(3)?,
                })
            },
        )
        .optional()
        .map_err(database_error)
}

fn recent_claim(
    connection: &Connection,
    provider: Provider,
    version: u32,
    home: &Path,
    fresh_since: DateTime<Utc>,
) -> Result<Option<SessionDiscoveryClaim>> {
    let Some(check) = current_attempt(connection, provider, version, home)? else {
        return Ok(None);
    };
    if !check.selected_home || check.checked_at < fresh_since.timestamp_millis() {
        return Ok(None);
    }
    if check.succeeded.is_some() {
        return Ok(Some(SessionDiscoveryClaim::Fresh));
    }
    Ok(Some(SessionDiscoveryClaim::InProgress(
        SessionDiscoveryAttempt {
            provider,
            version,
            home: home.to_owned(),
            id: check.id,
        },
    )))
}

/// Identifies the provider-wide check in effect when a command starts a live catalog read.
#[derive(Debug)]
pub struct SessionDiscoveryCheckpoint {
    provider: Provider,
    version: u32,
    home: PathBuf,
    observation: Option<Observation>,
    workspace: Option<PathBuf>,
    workspace_publication: Option<(i64, bool)>,
}

impl SessionDiscoveryCheckpoint {
    /// Whether the recorded provider-wide check selected this command's native home.
    #[must_use]
    pub fn is_for_selected_home(&self) -> bool {
        self.observation
            .as_ref()
            .is_some_and(|check| check.selected_home)
    }

    /// Whether this attempt still owns provider-wide publication at this checkpoint.
    #[must_use]
    pub fn owns(&self, attempt: &SessionDiscoveryAttempt) -> bool {
        self.observation.as_ref().is_some_and(|check| {
            check.selected_home && check.id == attempt.id && check.succeeded.is_none()
        })
    }

    /// Whether the intervening publications selected this command's home.
    #[must_use]
    pub fn can_follow(&self, prior: &Self) -> bool {
        let wide_changed = self.observation != prior.observation;
        let workspace_changed = self.workspace_publication != prior.workspace_publication;
        let workspace_local = self.workspace_publication.is_some_and(|(_, local)| local);
        let wide_local = self.is_for_selected_home()
            || (self.observation.is_none() && workspace_changed && workspace_local);
        (wide_changed || workspace_changed)
            && (!wide_changed || wide_local)
            && (!workspace_changed || workspace_local)
    }

    fn is_current(&self, connection: &Connection) -> Result<bool> {
        Ok(
            current_attempt(connection, self.provider, self.version, &self.home)?
                == self.observation
                && workspace_publication(
                    connection,
                    self.provider,
                    &self.home,
                    self.workspace.as_deref(),
                )? == self.workspace_publication,
        )
    }

    fn record_workspace(&self, tx: &Transaction<'_>, sessions: &[IndexedSession]) -> Result<()> {
        let Some(workspace) = self.workspace.as_deref() else {
            return Ok(());
        };
        let mut sorted: Vec<_> = sessions.iter().collect();
        sorted.sort_unstable_by(|a, b| a.session.id.cmp(&b.session.id));
        let mut hash = Sha256::new();
        for row in sorted {
            let fields = (
                &row.session.id,
                &row.title,
                &row.project_path,
                &row.git_branch,
                row.created_at.map(|time| time.timestamp_millis()),
                row.updated_at.map(|time| time.timestamp_millis()),
                row.updated_at_approximate,
                row.event_count,
            );
            hash.update(
                serde_json::to_vec(&fields).map_err(|_| StoreError::InvalidSessionReference)?,
            );
        }
        let fingerprint: [u8; 32] = hash.finalize().into();
        tx.execute(
            "INSERT INTO session_workspace_catalogs(provider,workspace,source_home,fingerprint,generation)
             SELECT ?1,?2,?3,?4,coalesce(max(generation),0)+1 FROM session_workspace_catalogs WHERE provider=?1
             ON CONFLICT(provider,workspace) DO UPDATE SET source_home=excluded.source_home,
                 fingerprint=excluded.fingerprint, generation=excluded.generation
             WHERE source_home IS NOT excluded.source_home OR fingerprint IS NOT excluded.fingerprint",
            params![self.provider.to_string(), workspace.as_os_str().as_encoded_bytes(), self.home.as_os_str().as_encoded_bytes(), fingerprint],
        ).map_err(database_error)?;
        Ok(())
    }
}

fn workspace_publication(
    connection: &Connection,
    provider: Provider,
    home: &Path,
    workspace: Option<&Path>,
) -> Result<Option<(i64, bool)>> {
    if provider != Provider::Antigravity {
        return Ok(None);
    }
    let local = home.as_os_str().as_encoded_bytes();
    let provider = provider.to_string();
    let read = |row: &rusqlite::Row<'_>| Ok((row.get(0)?, row.get(1)?));
    let result = if let Some(workspace) = workspace {
        connection.query_row(
            "SELECT generation, source_home = ?2 FROM session_workspace_catalogs WHERE provider = ?1 AND workspace = ?3",
            params![provider, local, workspace.as_os_str().as_encoded_bytes()], read,
        )
    } else {
        connection.query_row(
            "SELECT generation, source_home = ?2 FROM session_workspace_catalogs WHERE provider = ?1 ORDER BY generation DESC LIMIT 1",
            params![provider, local], read,
        )
    };
    result.optional().map_err(database_error)
}

pub(super) enum CatalogWrite<'a> {
    Unchecked,
    Publish(
        &'a SessionDiscoveryAttempt,
        DateTime<Utc>,
        Option<&'a SessionDiscoveryCheckpoint>,
    ),
    Merge(&'a SessionDiscoveryCheckpoint),
}

impl CatalogWrite<'_> {
    pub(super) fn permits(
        self,
        transaction: &Transaction<'_>,
        sessions: &[IndexedSession],
        unverified_desktop_ids: &[String],
    ) -> Result<bool> {
        match self {
            Self::Unchecked => Ok(true),
            Self::Publish(attempt, finished_at, checkpoint) => {
                if let Some(checkpoint) = checkpoint
                    && !checkpoint.is_current(transaction)?
                {
                    return Ok(false);
                }
                publish_attempt(
                    transaction,
                    attempt,
                    finished_at,
                    sessions,
                    unverified_desktop_ids,
                )
            }
            Self::Merge(checkpoint) => {
                if !checkpoint.is_current(transaction)? {
                    return Ok(false);
                }
                checkpoint.record_workspace(transaction, sessions)?;
                Ok(true)
            }
        }
    }
}

impl Store {
    /// Observes the reader and home without claiming discovery.
    ///
    /// # Errors
    ///
    /// Returns an error if the store cannot read the discovery state.
    pub fn session_discovery_checkpoint(
        &self,
        provider: Provider,
        version: u32,
        home: &Path,
        project: Option<&Path>,
    ) -> Result<SessionDiscoveryCheckpoint> {
        let workspace = project.filter(|_| provider == Provider::Antigravity);
        let connection = self.connection.borrow();
        let tx = connection.unchecked_transaction().map_err(database_error)?;
        let checkpoint = SessionDiscoveryCheckpoint {
            provider,
            version,
            home: home.to_owned(),
            observation: current_attempt(&tx, provider, version, home)?,
            workspace: workspace.map(Path::to_owned),
            workspace_publication: workspace_publication(&tx, provider, home, workspace)?,
        };
        tx.commit().map_err(database_error)?;
        Ok(checkpoint)
    }

    /// Checks a live read without publishing any catalog data.
    ///
    /// # Errors
    ///
    /// Returns an error if the store cannot read its discovery state.
    pub fn discovery_checkpoint_is_current(
        &self,
        checkpoint: &SessionDiscoveryCheckpoint,
    ) -> Result<bool> {
        let connection = self.connection.borrow();
        let tx = connection.unchecked_transaction().map_err(database_error)?;
        let current = checkpoint.is_current(&tx)?;
        tx.commit().map_err(database_error)?;
        Ok(current)
    }

    /// Merges a workspace listing while its discovery checkpoint is current, including a successful empty listing.
    /// This operation preserves provider-wide discovery ownership and completion.
    ///
    /// # Errors
    ///
    /// Returns an error if the store cannot read the checkpoint or save the rows.
    pub fn merge_sessions_at_discovery_checkpoint(
        &self,
        checkpoint: &SessionDiscoveryCheckpoint,
        sessions: &[IndexedSession],
    ) -> Result<bool> {
        self.merge_catalog_at_discovery_checkpoint(checkpoint, sessions, DiscoveryScope::Workspace)
    }

    /// Merges a Desktop database listing while keeping summary metadata from the same native home.
    ///
    /// # Errors
    ///
    /// Returns an error when the catalog or checkpoint cannot be read or written.
    pub fn merge_desktop_databases_at_checkpoint(
        &self,
        checkpoint: &SessionDiscoveryCheckpoint,
        sessions: &[IndexedSession],
    ) -> Result<bool> {
        self.merge_catalog_at_discovery_checkpoint(
            checkpoint,
            sessions,
            DiscoveryScope::DesktopDatabasesWithoutSummaries,
        )
    }

    /// Merges the observed workspace rows and source files while their discovery checkpoint is current.
    ///
    /// # Errors
    ///
    /// Returns an error if the scope is invalid or the store cannot save the catalog.
    pub fn merge_catalog_at_discovery_checkpoint<'a>(
        &self,
        checkpoint: &SessionDiscoveryCheckpoint,
        sessions: &[IndexedSession],
        listing: impl Into<CatalogListing<'a>>,
    ) -> Result<bool> {
        let listing = listing.into();
        if !matches!(
            listing.scope,
            DiscoveryScope::Workspace | DiscoveryScope::DesktopDatabasesWithoutSummaries
        ) {
            return Err(StoreError::InvalidSessionReference);
        }
        if sessions.is_empty()
            && (listing.scope != DiscoveryScope::Workspace || checkpoint.workspace.is_none())
        {
            return self.discovery_checkpoint_is_current(checkpoint);
        }
        self.write_discovered_sessions(
            checkpoint.provider,
            sessions,
            listing,
            Some(&checkpoint.home),
            CatalogWrite::Merge(checkpoint),
        )
    }

    /// Claims a provider-wide discovery if this reader has no recent check for the selected home.
    ///
    /// # Errors
    ///
    /// Returns an error if the store cannot read or record the attempt.
    pub fn try_begin_session_discovery(
        &self,
        provider: Provider,
        version: u32,
        home: &Path,
        fresh_since: DateTime<Utc>,
    ) -> Result<SessionDiscoveryClaim> {
        if let Some(claim) = recent_claim(
            &self.connection.borrow(),
            provider,
            version,
            home,
            fresh_since,
        )? {
            return Ok(claim);
        }
        self.record_session_discovery_attempt(provider, version, home, Some(fresh_since))
    }

    /// Records an unconditional provider-wide discovery of the selected native home.
    ///
    /// # Errors
    ///
    /// Returns an error if the store cannot record the attempt.
    pub fn mark_session_discovery_attempted(
        &self,
        provider: Provider,
        version: u32,
        home: &Path,
    ) -> Result<SessionDiscoveryAttempt> {
        match self.record_session_discovery_attempt(provider, version, home, None)? {
            SessionDiscoveryClaim::Claimed(attempt) => Ok(attempt),
            _ => Err(StoreError::CorruptStore),
        }
    }

    fn record_session_discovery_attempt(
        &self,
        provider: Provider,
        version: u32,
        home: &Path,
        fresh_since: Option<DateTime<Utc>>,
    ) -> Result<SessionDiscoveryClaim> {
        let mut connection = self.connection.borrow_mut();
        let transaction = immediate_transaction(&mut connection)?;
        if let Some(since) = fresh_since
            && let Some(claim) = recent_claim(&transaction, provider, version, home, since)?
        {
            return Ok(claim);
        }
        let name = provider.to_string();
        let id = Uuid::new_v4();
        let now = now_timestamp();
        transaction.execute(
            "INSERT INTO session_index_reader_checks (provider, reader_version, source_home, checked_at, attempt_id)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (provider, reader_version) DO UPDATE SET source_home = excluded.source_home,
                 checked_at = excluded.checked_at, attempt_id = excluded.attempt_id",
            params![name, version, home.as_os_str().as_encoded_bytes(), now, id.into_bytes()],
        ).map_err(database_error)?;
        transaction
            .execute(
                "INSERT INTO session_index_checks (provider, checked_at) VALUES (?1, ?2)
             ON CONFLICT (provider) DO UPDATE SET checked_at = excluded.checked_at",
                params![name, now],
            )
            .map_err(database_error)?;
        transaction.commit().map_err(database_error)?;
        Ok(SessionDiscoveryClaim::Claimed(SessionDiscoveryAttempt {
            provider,
            version,
            home: home.to_owned(),
            id,
        }))
    }

    /// Reads the state of one discovery without taking a write lock.
    ///
    /// # Errors
    ///
    /// Returns an error if the store cannot read the attempt.
    pub fn observe_session_discovery(
        &self,
        attempt: &SessionDiscoveryAttempt,
        fresh_since: DateTime<Utc>,
    ) -> Result<SessionDiscoveryState> {
        let observation = current_attempt(
            &self.connection.borrow(),
            attempt.provider,
            attempt.version,
            &attempt.home,
        )?;
        Ok(match observation {
            Some(check) if check.selected_home && check.id == attempt.id => {
                check.state(fresh_since)
            }
            _ => SessionDiscoveryState::Superseded,
        })
    }

    /// Publishes this attempt's provider-wide listing and completion in one transaction.
    ///
    /// Returns false when the listing is workspace-scoped or the attempt no longer owns the check.
    ///
    /// # Errors
    ///
    /// Returns an error if the metadata is invalid or the transaction fails.
    pub fn publish_session_discovery<'a>(
        &self,
        attempt: &SessionDiscoveryAttempt,
        sessions: &[IndexedSession],
        scope: impl Into<CatalogListing<'a>>,
        finished_at: DateTime<Utc>,
    ) -> Result<bool> {
        self.write_discovered_sessions(
            attempt.provider,
            sessions,
            scope.into(),
            Some(&attempt.home),
            CatalogWrite::Publish(attempt, finished_at, None),
        )
    }

    /// Publishes a provider-wide catalog only while its scoped discovery checkpoint is current.
    ///
    /// # Errors
    ///
    /// Returns an error if the listing or write fails.
    pub fn publish_at_discovery_checkpoint<'a>(
        &self,
        attempt: &SessionDiscoveryAttempt,
        checkpoint: &SessionDiscoveryCheckpoint,
        sessions: &[IndexedSession],
        scope: impl Into<CatalogListing<'a>>,
        finished_at: DateTime<Utc>,
    ) -> Result<bool> {
        if !checkpoint.owns(attempt) {
            return Ok(false);
        }
        self.write_discovered_sessions(
            attempt.provider,
            sessions,
            scope.into(),
            Some(&attempt.home),
            CatalogWrite::Publish(attempt, finished_at, Some(checkpoint)),
        )
    }

    /// Returns the IDs in the last successful Desktop listing for this reader and home.
    /// A missing snapshot is distinct from a listing that returned no sessions.
    ///
    /// # Errors
    ///
    /// Returns an error if the store cannot read the snapshot.
    pub fn published_desktop_session_ids(
        &self,
        version: u32,
        home: &Path,
    ) -> Result<Option<HashSet<String>>> {
        let ids: Option<String> = self.connection.borrow().query_row(
            "SELECT session_ids FROM desktop_discovery_snapshots WHERE reader_version = ?1 AND source_home = ?2",
            params![version, home.as_os_str().as_encoded_bytes()],
            |row| row.get(0),
        ).optional().map_err(database_error)?;
        ids.map(|json| serde_json::from_str(&json).map_err(|_| StoreError::CorruptStore))
            .transpose()
    }

    /// Reads the catalog rows in the last successful Desktop listing for this reader and home.
    ///
    /// # Errors
    ///
    /// Returns an error if the snapshot or its catalog rows cannot be read.
    pub fn published_desktop_sessions(
        &self,
        version: u32,
        home: &Path,
    ) -> Result<Option<Vec<IndexedSession>>> {
        let connection = self.connection.borrow();
        let mut statement = connection.prepare(
            "SELECT catalog.provider, catalog.session_id, title, project_path, git_branch,
                    created_at, updated_at, updated_at_approximate, event_count,
                    catalog.source_file_id, catalog.source_contents_id
             FROM desktop_discovery_snapshots AS snapshot
             LEFT JOIN json_each(snapshot.session_ids) AS listed ON true
             LEFT JOIN native_catalog_homes AS native ON native.provider = 'antigravity-desktop'
                 AND native.session_id = listed.value
             LEFT JOIN session_index AS catalog ON catalog.provider = 'antigravity-desktop' AND catalog.session_id = listed.value
                 AND (native.home IS NULL OR NOT native.verified OR native.home = snapshot.source_home)
             WHERE snapshot.reader_version = ?1 AND snapshot.source_home = ?2
             ORDER BY updated_at IS NULL, updated_at DESC, catalog.session_id",
        ).map_err(database_error)?;
        // The left join returns one empty row for a known empty snapshot.
        let rows = statement
            .query_map(
                params![version, home.as_os_str().as_encoded_bytes()],
                |row| {
                    if matches!(row.get_ref(0)?, rusqlite::types::ValueRef::Null) {
                        Ok(None)
                    } else {
                        indexed_session_from_row(row).map(Some)
                    }
                },
            )
            .map_err(database_error)?;
        let mut sessions = None;
        for row in rows {
            sessions
                .get_or_insert_with(Vec::new)
                .extend(row.map_err(|_| StoreError::CorruptStore)?);
        }
        Ok(sessions)
    }

    /// Records a failed discovery and preserves the last catalog. Returns false for an obsolete or finished attempt.
    ///
    /// # Errors
    ///
    /// Returns an error if the transaction fails.
    pub fn finish_session_discovery(
        &self,
        attempt: &SessionDiscoveryAttempt,
        finished_at: DateTime<Utc>,
    ) -> Result<bool> {
        let mut connection = self.connection.borrow_mut();
        let transaction = immediate_transaction(&mut connection)?;
        let finished = complete_attempt(&transaction, attempt, finished_at, false)?;
        transaction.commit().map_err(database_error)?;
        Ok(finished)
    }
}

pub(super) fn publish_attempt(
    transaction: &Transaction<'_>,
    attempt: &SessionDiscoveryAttempt,
    finished_at: DateTime<Utc>,
    sessions: &[IndexedSession],
    unverified_desktop_ids: &[String],
) -> Result<bool> {
    if !complete_attempt(transaction, attempt, finished_at, true)? {
        return Ok(false);
    }
    if attempt.provider == Provider::AntigravityDesktop {
        let mut ids: Vec<_> = sessions.iter().map(|row| row.session.id.as_str()).collect();
        if !unverified_desktop_ids.is_empty() {
            let previous: Option<String> = transaction.query_row(
                "SELECT session_ids FROM desktop_discovery_snapshots WHERE source_home = ?2 AND reader_version <= ?1
                 ORDER BY reader_version DESC LIMIT 1",
                params![attempt.version, attempt.home.as_os_str().as_encoded_bytes()],
                |row| row.get(0),
            ).optional().map_err(database_error)?;
            if let Some(previous) = previous {
                let known: HashSet<String> =
                    serde_json::from_str(&previous).map_err(|_| StoreError::CorruptStore)?;
                ids.extend(
                    unverified_desktop_ids
                        .iter()
                        .map(String::as_str)
                        .filter(|id| known.contains(*id)),
                );
            }
        }
        ids.sort_unstable();
        ids.dedup();
        let ids = serde_json::to_string(&ids).map_err(|_| StoreError::InvalidSessionReference)?;
        transaction.execute(
            "INSERT INTO desktop_discovery_snapshots(reader_version,source_home,session_ids)
             VALUES (?1,?2,?3)
             ON CONFLICT (reader_version,source_home) DO UPDATE SET session_ids = excluded.session_ids
             WHERE session_ids <> excluded.session_ids",
            params![attempt.version, attempt.home.as_os_str().as_encoded_bytes(), ids],
        ).map_err(database_error)?;
    }
    Ok(true)
}

fn complete_attempt(
    transaction: &Transaction<'_>,
    attempt: &SessionDiscoveryAttempt,
    finished_at: DateTime<Utc>,
    succeeded: bool,
) -> Result<bool> {
    let provider = attempt.provider.to_string();
    let id = attempt.id.into_bytes();
    // An older binary can replace the lease. Only the result for the exact attempt is visible.
    let claimed = transaction
        .execute(
            "UPDATE session_index_reader_checks SET checked_at = ?1
         WHERE provider = ?2 AND reader_version = ?3 AND attempt_id = ?4 AND source_home = ?5
           AND NOT EXISTS (SELECT 1 FROM session_index_discovery_results
               WHERE provider = ?2 AND reader_version = ?3 AND attempt_id = ?4)",
            params![
                finished_at.timestamp_millis(),
                provider,
                attempt.version,
                id,
                attempt.home.as_os_str().as_encoded_bytes()
            ],
        )
        .map_err(database_error)?;
    if claimed == 0 {
        return Ok(false);
    }
    transaction.execute(
        "INSERT INTO session_index_discovery_results(provider,reader_version,attempt_id,succeeded)
         VALUES (?1,?2,?3,?4)
         ON CONFLICT (provider,reader_version) DO UPDATE SET attempt_id=excluded.attempt_id, succeeded=excluded.succeeded",
        params![provider, attempt.version, id, succeeded],
    ).map_err(database_error)?;
    Ok(true)
}
