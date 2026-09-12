use std::{
    fs,
    path::PathBuf,
    time::{Duration, Instant},
};

use anyhow::Result;
use chrono::{DateTime, Utc};
use omnis_adapters::{AdapterRegistry, NativeSession};
use omnis_core::{SEARCH_DOCUMENT_VERSION, trajectory_search_document};
use omnis_ir::{Provider, SessionRef};
use omnis_store::{SessionTrajectoryOrigin, Store, TrajectoryIndexState};

use crate::{read_session, store_search_document};

// Larger sources index only their sampled head and tail.
const FULL_READ_SOURCE_BYTES: u64 = 16 * 1024 * 1024;
const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct IndexCandidate {
    pub(crate) session: SessionRef,
    pub(crate) updated_at: Option<DateTime<Utc>>,
    pub(crate) source_path: Option<PathBuf>,
}

impl IndexCandidate {
    // Sessions without a discovered source file are skipped so oversized files are never read
    // whole. Reading an OpenCode session spawns its CLI, so those index when opened instead.
    pub(crate) fn from_session(session: &NativeSession) -> Option<Self> {
        let provider = session.session.provider;
        let readable = provider == Provider::Imported
            || (provider != Provider::OpenCode && session.source_path.is_some());
        readable.then(|| Self {
            session: session.session.clone(),
            updated_at: session.updated_at,
            source_path: session.source_path.clone(),
        })
    }

    fn oversized(&self) -> bool {
        self.source_path
            .as_deref()
            .and_then(|path| fs::metadata(path).ok())
            .is_some_and(|metadata| metadata.is_file() && metadata.len() > FULL_READ_SOURCE_BYTES)
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct IndexProgress {
    pub(crate) indexed: usize,
    pub(crate) total: usize,
    pub(crate) failed: usize,
    pub(crate) titles: Vec<(SessionRef, String)>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct IndexSummary {
    pub(crate) candidates: usize,
    pub(crate) stale: usize,
    pub(crate) indexed: usize,
    pub(crate) failed: usize,
    pub(crate) stopped: bool,
}

pub(crate) fn needs_index(
    candidate: &IndexCandidate,
    state: Option<&TrajectoryIndexState>,
) -> bool {
    let Some(state) = state else {
        return true;
    };
    // The index stores millisecond timestamps, so compare at that precision.
    state.document_version < SEARCH_DOCUMENT_VERSION
        || candidate.updated_at.is_some_and(|updated_at| {
            updated_at.timestamp_millis() > state.source_updated_at.timestamp_millis()
        })
        || (!state.source_complete && !candidate.oversized())
}

/// Indexes stale sessions in candidate order, reporting progress and derived titles in batches.
///
/// # Errors
///
/// Returns an error only when the index state cannot be read. Unreadable sessions are counted.
pub(crate) fn index_candidates(
    registry: &AdapterRegistry,
    store: &Store,
    candidates: Vec<IndexCandidate>,
    stop: &dyn Fn() -> bool,
    report: &mut dyn FnMut(IndexProgress),
) -> Result<IndexSummary> {
    let states = store.trajectory_index_states()?;
    let candidate_count = candidates.len();
    let stale = candidates
        .into_iter()
        .filter(|candidate| needs_index(candidate, states.get(&candidate.session)))
        .collect::<Vec<_>>();
    let mut summary = IndexSummary {
        candidates: candidate_count,
        stale: stale.len(),
        ..IndexSummary::default()
    };
    report(IndexProgress {
        total: stale.len(),
        ..IndexProgress::default()
    });
    let mut titles = Vec::new();
    let mut last_report = Instant::now();
    for candidate in stale {
        if stop() {
            summary.stopped = true;
            break;
        }
        match index_session(registry, store, &candidate) {
            Ok(title) => {
                summary.indexed += 1;
                if let Some(title) = title {
                    titles.push((candidate.session, title));
                }
            }
            Err(_) => summary.failed += 1,
        }
        if last_report.elapsed() >= PROGRESS_INTERVAL {
            report(progress(&summary, &mut titles));
            last_report = Instant::now();
        }
    }
    report(progress(&summary, &mut titles));
    Ok(summary)
}

fn progress(summary: &IndexSummary, titles: &mut Vec<(SessionRef, String)>) -> IndexProgress {
    IndexProgress {
        indexed: summary.indexed + summary.failed,
        total: summary.stale,
        failed: summary.failed,
        titles: std::mem::take(titles),
    }
}

fn index_session(
    registry: &AdapterRegistry,
    store: &Store,
    candidate: &IndexCandidate,
) -> Result<Option<String>> {
    let imported = candidate.session.provider == Provider::Imported;
    let full_read = imported || !candidate.oversized();
    let snapshot = if imported {
        read_session(registry, &candidate.session)?
    } else if full_read {
        registry.read_session(&candidate.session)?
    } else {
        registry.preview_session(&candidate.session)?
    };
    let document = trajectory_search_document(&snapshot);
    let source_updated_at = candidate
        .updated_at
        .map_or(snapshot.captured_at, |updated_at| {
            updated_at.max(snapshot.captured_at)
        });
    let origin = if imported {
        SessionTrajectoryOrigin::ImportedBundle
    } else {
        SessionTrajectoryOrigin::Native
    };
    Ok(store_search_document(
        store,
        &candidate.session,
        &snapshot,
        &document,
        full_read,
        origin,
        source_updated_at,
    )?)
}

#[cfg(test)]
mod tests {
    use std::fs::File;

    use omnis_adapters::CodexAdapter;
    use serde_json::json;

    use super::*;

    fn candidate(
        updated_at: Option<DateTime<Utc>>,
        source_path: Option<PathBuf>,
    ) -> IndexCandidate {
        IndexCandidate {
            session: SessionRef::new(Provider::Codex, "synthetic"),
            updated_at,
            source_path,
        }
    }

    #[test]
    fn stale_versions_changed_sources_and_previews_need_indexing() {
        let now = Utc::now();
        let current = TrajectoryIndexState {
            source_updated_at: now,
            source_complete: true,
            document_version: SEARCH_DOCUMENT_VERSION,
        };
        let small = candidate(Some(now), None);

        assert!(needs_index(&small, None));
        assert!(!needs_index(&small, Some(&current)));
        assert!(needs_index(
            &small,
            Some(&TrajectoryIndexState {
                document_version: 0,
                ..current
            })
        ));
        assert!(needs_index(
            &candidate(Some(now + chrono::Duration::seconds(1)), None),
            Some(&current)
        ));
        assert!(needs_index(
            &small,
            Some(&TrajectoryIndexState {
                source_complete: false,
                ..current
            })
        ));
        let stored = TrajectoryIndexState {
            source_updated_at: DateTime::from_timestamp_millis(now.timestamp_millis())
                .expect("millisecond timestamp"),
            ..current
        };
        assert!(!needs_index(&small, Some(&stored)));
    }

    #[test]
    fn sampled_documents_of_oversized_sources_stay_current() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let path = temporary.path().join("rollout.jsonl");
        File::create(&path)
            .and_then(|file| file.set_len(FULL_READ_SOURCE_BYTES + 1))
            .expect("sparse oversized source");
        let now = Utc::now();
        let sampled = TrajectoryIndexState {
            source_updated_at: now,
            source_complete: false,
            document_version: SEARCH_DOCUMENT_VERSION,
        };

        assert!(!needs_index(
            &candidate(Some(now), Some(path)),
            Some(&sampled)
        ));
    }

    #[test]
    fn only_sessions_with_known_sources_are_background_candidates() {
        let session = |provider, source_path: Option<&str>| NativeSession {
            session: SessionRef::new(provider, "synthetic"),
            title: None,
            project_path: None,
            git_branch: None,
            created_at: None,
            updated_at: None,
            updated_at_approximate: false,
            event_count: 0,
            source_path: source_path.map(PathBuf::from),
        };

        assert!(
            IndexCandidate::from_session(&session(Provider::Codex, Some("/synthetic"))).is_some()
        );
        assert!(IndexCandidate::from_session(&session(Provider::Codex, None)).is_none());
        assert!(
            IndexCandidate::from_session(&session(Provider::OpenCode, Some("/synthetic")))
                .is_none()
        );
        assert!(IndexCandidate::from_session(&session(Provider::Imported, None)).is_some());
    }

    #[test]
    fn background_index_makes_unopened_conversation_text_searchable() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let workspace = temporary.path().join("workspace");
        let sessions = temporary.path().join("codex/sessions/2026/01/01");
        fs::create_dir_all(&workspace).expect("workspace");
        fs::create_dir_all(&sessions).expect("codex sessions");
        let id = "019f0000-0000-7000-8000-000000000001";
        fs::write(
            sessions.join(format!("rollout-2026-01-01T00-00-00-{id}.jsonl")),
            format!(
                "{}\n{}\n{}\n",
                json!({"type":"session_meta","timestamp":"2026-01-01T00:00:00Z","payload":{"id":id,"cwd":workspace,"git":{"branch":"main"}}}),
                json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Synthetic rate limiter request"}]}}),
                json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"The zebracorn marker lives only in this answer"}]}}),
            ),
        )
        .expect("synthetic rollout");
        let mut registry = AdapterRegistry::new();
        registry.register(CodexAdapter::with_root(temporary.path().join("codex")));
        let store = Store::open(temporary.path().join("store.sqlite3")).expect("synthetic store");
        let (listed, _) = registry
            .list_sessions_with_notes(Provider::Codex, None)
            .expect("list synthetic sessions");
        let candidates = listed
            .iter()
            .filter_map(IndexCandidate::from_session)
            .collect::<Vec<_>>();
        let mut reports = Vec::new();

        let summary = index_candidates(
            &registry,
            &store,
            candidates.clone(),
            &|| false,
            &mut |progress| {
                reports.push(progress);
            },
        )
        .expect("index synthetic sessions");

        assert_eq!(summary.indexed, 1);
        assert_eq!(
            store
                .search_session_trajectories("zebracorn", 10)
                .expect("search index"),
            vec![SessionRef::new(Provider::Codex, id)]
        );
        let final_report = reports.last().expect("final progress");
        assert_eq!((final_report.indexed, final_report.total), (1, 1));
        assert_eq!(
            final_report.titles,
            vec![(
                SessionRef::new(Provider::Codex, id),
                "Synthetic rate limiter request".to_owned()
            )]
        );
        let again = index_candidates(&registry, &store, candidates, &|| false, &mut |_| {})
            .expect("reindex synthetic sessions");
        assert_eq!(again.stale, 0);
    }
}
