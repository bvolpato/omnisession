use std::path::Path;

use anyhow::{Context, Result, ensure};
use omnis_ir::{Provider, SessionRef};

use super::{
    ANNOTATIONS, AntigravityDesktopAdapter, CONVERSATIONS, SUMMARIES, archived,
    conversation_database, native_directory, native_session, read_summaries,
};
use crate::{
    SearchSourceObservation,
    antigravity::validate_id,
    support::{provider_file, sidecar, store_is_missing, validate_provider},
};

pub(super) fn observe(
    adapter: &AntigravityDesktopAdapter,
    session: &SessionRef,
    listed: Option<&Path>,
) -> Result<Option<SearchSourceObservation>> {
    validate_provider(session, Provider::AntigravityDesktop)?;
    validate_id(&session.id)?;
    let root = adapter.root()?;
    let annotation = root.join(ANNOTATIONS).join(format!("{}.pbtxt", session.id));
    let visible = || -> Result<bool> {
        match native_directory(&root.canonicalize()?, ANNOTATIONS)? {
            Some(directory) => archived(&directory, &session.id).map(|archived| !archived),
            None => Ok(true),
        }
    };
    if listed.is_none() && !visible().unwrap_or(false) {
        return Ok(None);
    }
    let database = root.join(CONVERSATIONS).join(format!("{}.db", session.id));
    let summaries = root.join(SUMMARIES);
    let mut paths = vec![
        sidecar(&database, "-wal"),
        database,
        root.join(ANNOTATIONS),
        annotation,
    ];
    if provider_file(root, &summaries).is_some() || store_is_missing(&summaries) {
        paths.push(summaries);
    } else {
        // Replacing an unusable optional summary file changes its parent directory.
        paths.push(root.to_path_buf());
    }
    let mut summaries_unavailable = false;
    let mut database_generation = None;
    let mut summary_revision = None;
    let mut observation = SearchSourceObservation::capture(root, paths, || {
        ensure!(
            visible()?,
            "Antigravity Desktop archive status changed during search read"
        );
        let database = conversation_database(root, &session.id)
            .context("Antigravity Desktop database changed during search read")?;
        ensure!(
            listed.is_none_or(|path| path == database),
            "Antigravity Desktop database changed during search read"
        );
        let summaries = read_summaries(root).ok().flatten();
        let summary = summaries.as_ref().and_then(|rows| rows.get(&session.id));
        summaries_unavailable = summary.is_none();
        if summary.is_some() {
            summary_revision = provider_file(root, &root.join(SUMMARIES))
                .and_then(|path| crate::source_file_revision(&path)?.contents);
        }
        let (listed, generation) = native_session(&session.id, database, summary);
        database_generation = generation;
        Ok(listed)
    })?;
    observation.desktop_summaries_unavailable = summaries_unavailable;
    observation.database_generation = database_generation;
    observation.database_summary_revision = summary_revision;
    Ok(Some(observation))
}
