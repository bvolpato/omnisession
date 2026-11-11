use chrono::{DateTime, Utc};
use omnis_ir::{Provider, SessionRef, SourceRevision};
use omnis_store::{
    SessionTrajectoryOrigin, Store, StoreError, TrajectoryDocument, TrajectoryIndexFailure,
    TrajectorySourceGuard,
};
use uuid::Uuid;

fn revision(file: u128, contents: u128) -> SourceRevision {
    SourceRevision {
        file: Some(Uuid::from_u128(file)),
        contents: Some(Uuid::from_u128(contents)),
        missing_ancestor: None,
    }
}

fn full(text: &str, rev: SourceRevision) -> TrajectoryDocument<'_> {
    TrajectoryDocument {
        source_revision: rev,
        redacted_text: text,
        source_updated_at: DateTime::<Utc>::from_timestamp(1_000, 123_456).unwrap(),
        source_byte_count: text.len(),
        indexed_byte_count: text.len(),
        truncation_strategy: "none",
        source_complete: true,
        provider_limited: false,
        origin: SessionTrajectoryOrigin::Native,
        document_version: 6,
        derived_title: None,
    }
}

fn preview(rev: SourceRevision) -> TrajectoryDocument<'static> {
    TrajectoryDocument {
        source_complete: false,
        truncation_strategy: "event_head_tail",
        source_byte_count: 100,
        ..full("preview", rev)
    }
}

fn failure(doc: TrajectoryDocument<'_>) -> TrajectoryIndexFailure {
    TrajectoryIndexFailure {
        source_file: doc.source_revision.file,
        source_contents: doc.source_revision.contents,
        source_updated_at: Some(doc.source_updated_at),
        source_time_precise: true,
        native_summary: doc.origin == SessionTrajectoryOrigin::NativeSummary,
        document_version: doc.document_version,
    }
}

struct Fixture {
    root: tempfile::TempDir,
    index: Store,
    session: SessionRef,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let index = Store::open(root.path().join("synthetic.sqlite3")).unwrap();
        Self {
            root,
            index,
            session: SessionRef::new(Provider::Antigravity, "sample"),
        }
    }
    fn observe(&self) -> TrajectorySourceGuard {
        self.index
            .observe_trajectory_document(&self.session)
            .unwrap()
    }
    fn write(&self, doc: TrajectoryDocument<'_>, guard: TrajectorySourceGuard) -> usize {
        self.index
            .upsert_observed_trajectory_documents_checked(&[(&self.session, doc, guard)], |_, _| {
                true
            })
            .unwrap()
    }
    fn record(&self, failed: &TrajectoryIndexFailure) {
        self.index
            .record_trajectory_index_failure(&self.session, failed)
            .unwrap();
    }
    fn saved(&self) -> Option<TrajectoryIndexFailure> {
        self.index.trajectory_index_failure(&self.session).unwrap()
    }
}

#[test]
fn a_late_failure_or_clear_cannot_replace_the_current_receipt() {
    let f = Fixture::new();
    let session = &f.session;
    let index = Store::open(f.root.path().join("synthetic.sqlite3")).unwrap();
    let summary = |rev| TrajectoryDocument {
        origin: SessionTrajectoryOrigin::NativeSummary,
        ..full("", rev)
    };
    let first = summary(revision(1, 1));
    let second = summary(revision(1, 2));
    let current = failure(second);
    f.write(first, f.observe());
    let prior = index.observe_trajectory_document(session).unwrap();
    index
        .record_observed_trajectory_index_failure_checked(
            session,
            &current,
            prior,
            |selected, rev| selected == session && rev == second.source_revision,
        )
        .unwrap();
    assert_eq!(index.trajectory_index_failures().unwrap()[session], current);
    let old = failure(first);
    let rejected = index.record_observed_trajectory_index_failure_checked(
        session,
        &old,
        f.observe(),
        |_, rev| rev == second.source_revision,
    );
    assert!(matches!(rejected, Err(StoreError::TrajectorySourceChanged)));
    for obsolete in [
        old,
        TrajectoryIndexFailure {
            native_summary: false,
            ..current
        },
        TrajectoryIndexFailure {
            source_time_precise: false,
            ..current
        },
    ] {
        index
            .clear_observed_trajectory_index_failure(session, &obsolete)
            .unwrap();
    }
    f.write(first, prior);
    assert_eq!(f.saved(), Some(current));
    let reading = f.observe();
    let concurrent = failure(summary(revision(1, 3)));
    f.record(&concurrent);
    let rejected = index.record_observed_trajectory_index_failure(session, &current, reading);
    assert!(matches!(rejected, Err(StoreError::TrajectorySourceChanged)));
    f.write(first, reading);
    assert_eq!(f.saved(), Some(concurrent));
    f.write(second, f.observe());
    assert_eq!(f.saved(), None);
    let rejected = index.record_observed_trajectory_index_failure(session, &old, prior);
    assert!(matches!(rejected, Err(StoreError::TrajectorySourceChanged)));
}

#[test]
fn changing_only_the_receipt_reuses_chunks_and_rejects_an_old_batch() {
    let f = Fixture::new();
    let session = &f.session;
    let path = f.root.path().join("synthetic.sqlite3");
    let reader = Store::open(&path).unwrap();
    let raw = rusqlite::Connection::open(path).unwrap();
    let first = TrajectoryDocument {
        derived_title: Some("Original"),
        ..full("original indexed words", revision(1, 1))
    };
    assert_eq!(f.write(first, f.observe()), 1);
    let old = reader.observe_trajectory_document(session).unwrap();
    assert_eq!(f.write(preview(first.source_revision), f.observe()), 0);
    raw.execute_batch(
        "CREATE TRIGGER no_chunk_delete BEFORE DELETE ON session_trajectory_chunk_rows
        BEGIN SELECT RAISE(ABORT, 'unchanged search chunks were rewritten'); END;",
    )
    .unwrap();
    let revised = TrajectoryDocument {
        source_revision: revision(1, 2),
        ..first
    };
    assert_eq!(f.write(revised, f.observe()), 0);
    let state = f.index.trajectory_index_states().unwrap()[session];
    assert_eq!(state.source_revision, revised.source_revision);
    assert!(state.source_complete);
    let fresh = f.observe();
    assert_ne!(fresh, old);
    let rejected = reader
        .upsert_observed_trajectory_documents_checked(&[(session, revised, fresh)], |_, _| false);
    assert!(matches!(rejected, Err(StoreError::TrajectorySourceChanged)));
    let other = SessionRef::new(Provider::Antigravity, "other");
    let absent = reader.observe_trajectory_document(&other).unwrap();
    let batch = [(&other, first, absent), (session, first, old)];
    let rejected = reader.upsert_observed_trajectory_documents_checked(&batch, |_, _| true);
    assert!(matches!(rejected, Err(StoreError::TrajectorySourceChanged)));
    assert_eq!(reader.trajectory_index_state(&other).unwrap(), None);
    raw.execute_batch("DROP TRIGGER no_chunk_delete").unwrap();
    let replaced = preview(revision(2, 1));
    assert_eq!(f.write(replaced, fresh), 1);
    assert_eq!(f.index.trajectory_title(session).unwrap(), None);
    let weak = TrajectoryDocument {
        source_revision: SourceRevision::default(),
        document_version: 7,
        ..first
    };
    assert_eq!(f.write(weak, f.observe()), 0);
    let state = reader.trajectory_index_state(session).unwrap().unwrap();
    assert_eq!(state.source_revision, replaced.source_revision);
    assert!(!state.source_complete);
}

#[test]
fn a_legacy_full_document_waits_for_a_complete_verified_upgrade() {
    let f = Fixture::new();
    let path = f.root.path().join("synthetic.sqlite3");
    let legacy = full("historical result", SourceRevision::default());
    f.index
        .upsert_trajectory_document(&f.session, &legacy)
        .unwrap();
    let raw = rusqlite::Connection::open(&path).unwrap();
    for column in [
        "source_file_id",
        "missing_ancestor_id",
        "source_contents_id",
    ] {
        raw.execute_batch(&format!(
            "ALTER TABLE session_trajectories DROP COLUMN {column}"
        ))
        .unwrap();
    }
    let restored = Store::open(path).unwrap();
    let read = || {
        restored
            .trajectory_index_state(&f.session)
            .unwrap()
            .unwrap()
    };
    let rev = revision(1, 1);
    let partial = TrajectoryDocument {
        document_version: 7,
        ..preview(rev)
    };
    assert_eq!(f.write(partial, f.observe()), 0);
    let state = read();
    assert!(state.source_complete);
    assert_eq!(state.source_revision, legacy.source_revision);
    assert_eq!(state.document_version, legacy.document_version);
    let upgraded = TrajectoryDocument {
        source_revision: rev,
        document_version: 7,
        ..legacy
    };
    assert_eq!(f.write(upgraded, f.observe()), 1);
    let limited = TrajectoryDocument {
        provider_limited: true,
        document_version: 8,
        ..partial
    };
    assert_eq!(f.write(limited, f.observe()), 1);
    let state = read();
    assert_eq!(state.source_revision, rev);
    assert!(state.provider_limited);
    assert_eq!(state.document_version, limited.document_version);
}

#[test]
fn a_verified_preview_retires_an_obsolete_failure_and_keeps_full_text() {
    let f = Fixture::new();
    let current = full("preface middlequartz epilogue", revision(1, 1));
    let observed = preview(current.source_revision);
    let other = failure(full("", revision(1, 2)));
    assert_eq!(f.write(current, f.observe()), 1);
    let same = failure(current);
    let future = TrajectoryIndexFailure {
        document_version: 7,
        ..other
    };
    for (failed, retain) in [(other, false), (same, true), (future, true)] {
        f.record(&failed);
        assert_eq!(f.write(observed, f.observe()), 0);
        assert_eq!(f.saved(), retain.then_some(failed));
        f.index
            .clear_observed_trajectory_index_failure(&f.session, &failed)
            .unwrap();
    }
    assert_eq!(
        f.index
            .search_session_trajectories("middlequartz", 10)
            .unwrap(),
        vec![f.session]
    );
}
