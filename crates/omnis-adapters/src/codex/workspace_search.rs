use super::{
    CodexAdapter, CodexHistory, CodexSession, EventBuilder, HistoryScope, MAX_PREVIEW_TAIL_SIZE,
    NativeSession, Provider, RolloutCatalog, TOOL_COMPACTION_RECORD_INTERVAL, live_rollout_path,
    paths_match, rollout_file, sort_sessions, string_at,
};
use anyhow::{Context, Result, ensure};
use std::{
    collections::HashMap,
    ffi::OsString,
    fs,
    io::Read,
    path::{Path, PathBuf},
};

impl CodexAdapter {
    pub(super) fn list_project_candidates(
        &self,
        project: Option<&Path>,
        check_moves: bool,
    ) -> (Vec<NativeSession>, usize) {
        let workspaces = self
            .use_workspace_index
            .then(|| omnis_store::CodexWorkspace::discovery_index(None).ok())
            .flatten()
            .unwrap_or_default();
        let (listing, cached_header) = self.listing(true);
        if listing.sessions.is_empty() {
            return (Vec::new(), 0);
        }
        let catalog = self.workspace_catalog.as_deref();
        let observations = catalog
            .map(super::WorkspaceCatalog::list)
            .unwrap_or_default();
        let indexed = catalog
            .map(super::WorkspaceCatalog::indexed_sessions)
            .unwrap_or_default();
        let mut sessions = Vec::new();
        let mut unknown = Vec::new();
        let mut projected = HashMap::new();
        let mut matches = HashMap::<OsString, bool>::new();
        let mut in_project = |path: Option<&Path>| {
            project.is_none_or(|project| {
                path.filter(|path| path.is_absolute()).is_some_and(|path| {
                    if let Some(&found) = matches.get(path.as_os_str()) {
                        return found;
                    }
                    let found = paths_match(path, project);
                    matches.insert(path.as_os_str().to_owned(), found);
                    found
                })
            })
        };
        let rollouts = self.codex_home.as_deref().map(RolloutCatalog::new);
        let mut revisions = HashMap::new();
        for raw in &listing.sessions {
            let (session, verified) = if raw.needs_visible_projection() {
                raw.apply_indexed_workspace(&workspaces, cached_header, |id, revision| {
                    !revision.is_nil()
                        && rollouts.as_ref().is_some_and(|catalog| {
                            *revisions
                                .entry(id)
                                .or_insert_with(|| catalog.inventory_revision(id))
                                == revision
                        })
                })
            } else {
                let mut session = raw.native_session();
                if let Some((catalog, observation)) = catalog.zip(observations.get(&raw.id)) {
                    catalog.apply(raw, &mut session, observation, project);
                }
                let verified = session.apply_codex_workspace_index(&workspaces, cached_header);
                if let Some((catalog, previous)) = catalog.zip(indexed.get(&raw.id)) {
                    catalog.refresh_replaced(self, raw, &session, previous);
                }
                (session, verified)
            };
            if project.is_some() && !verified && raw.needs_visible_projection() {
                projected.insert(raw.id.as_str(), raw);
                unknown.push(session);
            } else if in_project(session.project_path.as_deref()) {
                sessions.push(session);
            } else if check_moves && !verified {
                unknown.push(session);
            }
        }
        let unchecked = self.probe_project_candidates(
            &mut sessions,
            unknown,
            &projected,
            &workspaces,
            &mut in_project,
        );
        sort_sessions(&mut sessions);
        (sessions, unchecked)
    }

    fn probe_project_candidates(
        &self,
        sessions: &mut Vec<NativeSession>,
        mut unknown: Vec<NativeSession>,
        projected: &HashMap<&str, &CodexSession>,
        workspaces: &omnis_store::CodexWorkspaceIndex,
        in_project: &mut impl FnMut(Option<&Path>) -> bool,
    ) -> usize {
        if unknown.is_empty() {
            return 0;
        }
        sort_sessions(&mut unknown);
        let hints = self
            .use_workspace_index
            .then(|| omnis_store::CodexWorkspace::refresh_hints().ok())
            .flatten()
            .unwrap_or_default();
        let Some((home, canonical)) = self
            .codex_home
            .as_deref()
            .and_then(|home| Some((home, fs::canonicalize(home).ok()?)))
        else {
            return unknown.len();
        };
        let mut probe = WorkspaceProbe {
            adapter: self,
            home,
            canonical: &canonical,
            catalog: RolloutCatalog::new(home),
            files_left: 64,
            bytes_left: 4 * MAX_PREVIEW_TAIL_SIZE,
        };
        let mut unchecked = 0;
        for mut session in unknown {
            let raw = projected.get(session.session.id.as_str()).copied();
            if raw.is_none()
                && let Some(revision) = session.source_revision
                && !workspaces.needs_recheck(revision)
                && let Some(hint) = revision.file.and_then(|file| hints.get(&file))
                && revision.matches_file_contents(hint.source_revision)
                && in_project(hint.project_path.as_deref())
            {
                // The old full read selects this file for indexing. Only a verified workspace can rank it.
                sessions.push(session);
                continue;
            }
            match probe.read(&session, raw) {
                Ok(Some(workspace)) if in_project(Some(&workspace)) => {
                    if let Some(raw) = raw {
                        session.git_branch = raw.git_at(Some(&workspace)).branch;
                        session.project_path = Some(workspace);
                    }
                    sessions.push(session);
                }
                Ok(_) => {}
                Err(_) => {
                    unchecked += 1;
                }
            }
        }
        unchecked
    }
}

// A project search checks only a bounded number of unverified foreign files, newest first.
struct WorkspaceProbe<'a> {
    adapter: &'a CodexAdapter,
    home: &'a Path,
    canonical: &'a Path,
    catalog: RolloutCatalog<'a>,
    files_left: usize,
    bytes_left: u64,
}

impl WorkspaceProbe<'_> {
    fn read(
        &mut self,
        session: &NativeSession,
        projected: Option<&CodexSession>,
    ) -> Result<Option<PathBuf>> {
        ensure!(
            self.files_left > 0 && self.bytes_left > 0,
            "workspace search budget exhausted"
        );
        self.files_left -= 1;
        let limit = self.bytes_left.min(MAX_PREVIEW_TAIL_SIZE);
        let mut remaining = limit;
        let result = rollout_file::within_search_budget(&mut remaining, || {
            self.read_workspace(session, projected, limit)
        });
        self.bytes_left -= limit - remaining;
        result
    }

    fn read_workspace(
        &self,
        session: &NativeSession,
        projected: Option<&CodexSession>,
        limit: u64,
    ) -> Result<Option<PathBuf>> {
        let path = session
            .source_path
            .as_deref()
            .context("missing rollout path")?;
        ensure!(
            live_rollout_path(self.home, self.canonical, path)
                && fs::metadata(path)?.len() <= limit,
            "rollout exceeds workspace search budget or moved"
        );
        let mut bytes = Vec::new();
        rollout_file::open(path)?.read_to_end(&mut bytes)?;
        // A foreign header alone cannot establish the requested directory.
        if session.project_path.is_some()
            && memchr::memmem::find(&bytes, b"turn_context").is_none()
            && memchr::memmem::find(&bytes, b"history_base").is_none()
            && memchr::memmem::find(&bytes, b"thread_settings_applied").is_none()
            && memchr::memmem::find(&bytes, b"\\u").is_none()
        {
            return Ok(None);
        }
        let workspace = if let Some(raw) = projected {
            self.referenced_workspace(raw, &bytes)?
        } else {
            Self::leaf_workspace(session, &bytes)?
        };
        ensure!(
            session
                .source_revision
                .is_some_and(|expected| expected.contents.is_some()
                    && CodexAdapter::index_file_revision(path)
                        .is_some_and(|actual| actual.matches_file_contents(expected))),
            "Codex rollout changed during workspace search"
        );
        Ok(workspace)
    }

    fn referenced_workspace(&self, raw: &CodexSession, bytes: &[u8]) -> Result<Option<PathBuf>> {
        // The latest settings for this thread survive conversation rollback and take precedence
        // over every ancestor. Decode the visible leaf first, including gaps after the settings.
        let mut settings = super::workspace_check::WorkspaceSettings::default();
        let skipped = raw.visible_leaf()?.visit_confirmed_leaf_lines(
            &raw.id,
            |visit| rollout_file::read_records(bytes, visit),
            |record, skipped, _, _| {
                settings.observe_gaps(skipped);
                settings.observe(&raw.id, &record);
                Ok(())
            },
        )?;
        settings.observe_gaps(skipped);
        if settings.has_owned() {
            return Ok(settings.directory(None));
        }

        let mut lineage = raw.lineage(self.adapter, HistoryScope::Visible, Some(&self.catalog))?;
        lineage.project(self.adapter, Some(&self.catalog), |lineage| {
            let (_, workspace, verified) = raw
                .canonical_events_with(lineage, |visit| rollout_file::read_records(bytes, visit))?;
            Ok(workspace.filter(|_| verified))
        })
    }

    fn leaf_workspace(session: &NativeSession, bytes: &[u8]) -> Result<Option<PathBuf>> {
        let mut history = CodexHistory::new(None);
        let mut builder = EventBuilder::new(Provider::Codex, &session.session.id);
        let mut inherited = false;
        let mut malformed = 0;
        let skipped = rollout_file::read_records(bytes, |mut record, skipped, terminated| {
            history.observe_malformed(&mut malformed, skipped);
            history.settings.observe_gaps(skipped);
            if !history.paginated || super::WorkspaceRecords::accepts(true, &record) {
                history.settings.observe(&session.session.id, &record);
            }
            history.observe_workspace_gaps(skipped.workspace_gaps(), builder.checkpoint());
            if history.records_seen == 0 {
                ensure!(
                    record["type"] == "session_meta"
                        && string_at(&record["payload"], &[&["id"], &["session_id"]])
                            == Some(session.session.id.as_str()),
                    "Codex rollout header changed"
                );
                inherited = super::records_history_base(&record);
                if !inherited && skipped.workspace_gaps() == 0 {
                    history.workspace.path =
                        string_at(&record["payload"], &[&["cwd"]]).map(PathBuf::from);
                }
            }
            history.push(&mut builder, &mut record, terminated)?;
            if history
                .records_seen
                .is_multiple_of(TOOL_COMPACTION_RECORD_INTERVAL)
            {
                history.compact(&mut builder);
            }
            Ok(())
        })?;
        history.settings.observe_gaps(skipped);
        history.observe_workspace_gaps(skipped.workspace_gaps(), builder.checkpoint());
        ensure!(
            !inherited
                || history.settings.has_owned()
                || history
                    .workspace
                    .record
                    .is_some_and(|position| position > 1),
            "inherited workspace may be unavailable"
        );
        Ok(history.settings.directory(history.workspace.path))
    }
}
