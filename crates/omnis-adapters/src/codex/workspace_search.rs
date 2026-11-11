use super::{
    CodexAdapter, CodexHistory, EventBuilder, MAX_PREVIEW_TAIL_SIZE, NativeSession, Provider,
    TOOL_COMPACTION_RECORD_INTERVAL, live_rollout_path, paths_match, records_history_base,
    rollout_file, sort_sessions, string_at,
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
        let cached_header = self.listing.get().is_some();
        let mut sessions = Vec::new();
        let mut unknown = Vec::new();
        let mut matches = HashMap::<OsString, bool>::new();
        let mut in_project = |path: Option<&Path>| {
            project.is_none_or(|project| {
                path.is_some_and(|path| {
                    if let Some(&found) = matches.get(path.as_os_str()) {
                        return found;
                    }
                    let found = paths_match(path, project);
                    matches.insert(path.as_os_str().to_owned(), found);
                    found
                })
            })
        };
        for raw in self.sessions() {
            let mut session = raw.native_session();
            let verified = session.apply_codex_workspace_index(&workspaces, cached_header);
            if in_project(session.project_path.as_deref()) {
                sessions.push(session);
            } else if check_moves && !verified {
                unknown.push(session);
            }
        }
        if unknown.is_empty() {
            sort_sessions(&mut sessions);
            return (sessions, 0);
        }
        sort_sessions(&mut unknown);
        let mut probe = WorkspaceProbe {
            files_left: 64,
            bytes_left: 4 * MAX_PREVIEW_TAIL_SIZE,
        };
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
            sort_sessions(&mut sessions);
            return (sessions, unknown.len());
        };
        let mut unchecked = 0;
        for session in unknown {
            if let Some(revision) = session.source_revision
                && let Some(hint) = revision.file.and_then(|file| hints.get(&file))
                && revision.matches_file_contents(hint.source_revision)
                && in_project(hint.project_path.as_deref())
            {
                // The old full read selects this file for indexing. Only a verified workspace can rank it.
                sessions.push(session);
                continue;
            }
            match probe.read(&session, home, &canonical) {
                Ok(Some(workspace)) if in_project(Some(&workspace)) => {
                    sessions.push(session);
                }
                Ok(_) => {}
                Err(_) => {
                    unchecked += 1;
                }
            }
        }
        sort_sessions(&mut sessions);
        (sessions, unchecked)
    }
}

// A project search checks only a bounded number of unverified foreign files, newest first.
struct WorkspaceProbe {
    files_left: usize,
    bytes_left: u64,
}

impl WorkspaceProbe {
    fn read(
        &mut self,
        session: &NativeSession,
        home: &Path,
        canonical: &Path,
    ) -> Result<Option<PathBuf>> {
        ensure!(
            self.files_left > 0 && self.bytes_left > 0,
            "workspace search budget exhausted"
        );
        self.files_left -= 1;
        let path = session
            .source_path
            .as_deref()
            .context("missing rollout path")?;
        let limit = self.bytes_left.min(MAX_PREVIEW_TAIL_SIZE);
        ensure!(
            live_rollout_path(home, canonical, path) && fs::metadata(path)?.len() <= limit,
            "rollout exceeds workspace search budget or moved"
        );
        let mut bytes = Vec::new();
        let read = rollout_file::open(path)?
            .take(limit + 1)
            .read_to_end(&mut bytes);
        let used = u64::try_from(bytes.len())?;
        self.bytes_left = self.bytes_left.saturating_sub(used);
        read?;
        ensure!(
            used <= limit,
            "decoded rollout exceeds workspace search budget"
        );
        // A foreign header alone cannot establish the requested directory.
        if session.project_path.is_some()
            && memchr::memmem::find(&bytes, b"turn_context").is_none()
            && memchr::memmem::find(&bytes, b"history_base").is_none()
            && memchr::memmem::find(&bytes, b"\\u").is_none()
        {
            return Ok(None);
        }
        let mut history = CodexHistory::new(None);
        let mut builder = EventBuilder::new(Provider::Codex, &session.session.id);
        builder.plan_preview(None);
        let mut inherited = false;
        let skipped = rollout_file::read_records(bytes.as_slice(), |record, skipped| {
            history.observe_workspace_gaps(skipped.workspace_gaps());
            if history.records_seen == 0 {
                ensure!(
                    record["type"] == "session_meta"
                        && string_at(&record["payload"], &[&["id"], &["session_id"]])
                            == Some(session.session.id.as_str()),
                    "Codex rollout header changed"
                );
                inherited = records_history_base(&record);
                if !inherited && skipped.workspace_gaps() == 0 {
                    history.project_path =
                        string_at(&record["payload"], &[&["cwd"]]).map(PathBuf::from);
                }
            }
            history.push(&mut builder, &record)?;
            if history
                .records_seen
                .is_multiple_of(TOOL_COMPACTION_RECORD_INTERVAL)
            {
                history.compact(&mut builder);
            }
            Ok(())
        })?;
        history.observe_workspace_gaps(skipped.workspace_gaps());
        ensure!(
            !inherited
                || history
                    .workspace_record
                    .is_some_and(|position| position > 1),
            "inherited workspace may be unavailable"
        );
        ensure!(
            session
                .source_revision
                .is_some_and(|expected| expected.contents.is_some()
                    && CodexAdapter::index_file_revision(path)
                        .is_some_and(|actual| actual.matches_file_contents(expected))),
            "Codex rollout changed during workspace search"
        );
        Ok(history.project_path)
    }
}
