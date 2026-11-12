use chrono::{DateTime, Utc};
use omnis_ir::{Provider, SessionRef, SourceRevision};
use omnis_store::{
    DiscoveryScope, IndexedSession, LocalOpenerEvidence, SessionTrajectoryOrigin, Store,
    StoreError, TrajectoryDocument, TrajectoryIndexFailure, TrajectorySourceGuard,
};
use uuid::Uuid;

fn revision(file: u128, contents: u128) -> SourceRevision {
    SourceRevision {
        file: Some(Uuid::from_u128(file)),
        contents: Some(Uuid::from_u128(contents)),
        ..SourceRevision::default()
    }
}

fn full(text: &str, rev: SourceRevision) -> TrajectoryDocument<'_> {
    TrajectoryDocument {
        ancestor_revisions: &[],
        title_authoritative: true,
        title_is_verified: false,
        local_opener: LocalOpenerEvidence::Unknown,
        database_step_proof: None,
        database_wal_commits: None,
        database_summary_revision: None,
        desktop_projection: None,
        legacy_utf8_text: None,
        projection: None,
        withdrawn_title: None,
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
        title_authoritative: false,
        source_complete: false,
        truncation_strategy: "event_head_tail",
        source_byte_count: 100,
        ..full("preview", rev)
    }
}

fn failure(doc: &TrajectoryDocument<'_>) -> TrajectoryIndexFailure {
    TrajectoryIndexFailure {
        ancestor_revisions: doc.ancestor_revisions.to_vec(),
        source_file: doc.source_revision.file,
        source_contents: doc.source_revision.contents,
        source_updated_at: Some(doc.source_updated_at),
        source_time_precise: true,
        native_summary: doc.origin == SessionTrajectoryOrigin::NativeSummary,
        document_version: doc.document_version,
    }
}

fn locator(session: &SessionRef) -> IndexedSession {
    IndexedSession {
        source_path: None,
        source_revision: None,
        session: session.clone(),
        title: None,
        project_path: None,
        git_branch: None,
        created_at: None,
        updated_at: None,
        updated_at_approximate: false,
        event_count: 1,
    }
}

fn cached_document_is_eligible(store: &Store, session: &SessionRef) -> bool {
    store
        .read_trajectory_search(|read| -> omnis_store::Result<_> {
            Ok(read
                .conflicting_cached_documents(session.provider, &[&session.id])?
                .is_empty())
        })
        .unwrap()
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
    fn write(&self, doc: &TrajectoryDocument<'_>, guard: &TrajectorySourceGuard) -> usize {
        self.index
            .upsert_observed_trajectory_documents_checked(
                &[(&self.session, *doc, *guard)],
                |_, _| true,
            )
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
    let current = failure(&second);
    f.write(&first, &f.observe());
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
    let old = failure(&first);
    let rejected = index.record_observed_trajectory_index_failure_checked(
        session,
        &old,
        f.observe(),
        |_, rev| rev == second.source_revision,
    );
    assert!(matches!(rejected, Err(StoreError::TrajectorySourceChanged)));
    for obsolete in [
        &old,
        &TrajectoryIndexFailure {
            native_summary: false,
            ..current.clone()
        },
        &TrajectoryIndexFailure {
            source_time_precise: false,
            ..current.clone()
        },
    ] {
        index
            .clear_observed_trajectory_index_failure(session, obsolete)
            .unwrap();
    }
    f.write(&first, &prior);
    assert_eq!(f.saved().as_ref(), Some(&current));
    let reading = f.observe();
    let concurrent = failure(&summary(revision(1, 3)));
    f.record(&concurrent);
    let rejected = index.record_observed_trajectory_index_failure(session, &current, reading);
    assert!(matches!(rejected, Err(StoreError::TrajectorySourceChanged)));
    f.write(&first, &reading);
    assert_eq!(f.saved(), Some(concurrent));
    f.write(&second, &f.observe());
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
    assert_eq!(f.write(&first, &f.observe()), 1);
    let old = reader.observe_trajectory_document(session).unwrap();
    assert_eq!(f.write(&preview(first.source_revision), &f.observe()), 0);
    raw.execute_batch(
        "CREATE TRIGGER no_chunk_delete BEFORE DELETE ON session_trajectory_chunk_rows
        BEGIN SELECT RAISE(ABORT, 'unchanged search chunks were rewritten'); END;",
    )
    .unwrap();
    let revised = TrajectoryDocument {
        source_revision: revision(1, 2),
        ..first
    };
    assert_eq!(f.write(&revised, &f.observe()), 0);
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
    assert_eq!(f.write(&replaced, &fresh), 1);
    assert_eq!(f.index.trajectory_title(session).unwrap(), None);
    let weak = TrajectoryDocument {
        source_revision: SourceRevision::default(),
        document_version: 7,
        ..first
    };
    assert_eq!(f.write(&weak, &f.observe()), 0);
    let state = reader.trajectory_index_state(session).unwrap().unwrap();
    assert_eq!(state.source_revision, replaced.source_revision);
    assert!(!state.source_complete);
}

#[test]
fn a_legacy_full_document_waits_for_a_complete_verified_upgrade() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("synthetic.sqlite3");
    let session = SessionRef::new(Provider::Antigravity, "sample");
    let legacy = TrajectoryDocument {
        document_version: 4,
        source_updated_at: DateTime::from_timestamp_millis(1_000_000).unwrap(),
        ..full("historical result", SourceRevision::default())
    };
    let raw = rusqlite::Connection::open(&path).unwrap();
    raw.execute_batch(
        "CREATE TABLE session_trajectories (
            id INTEGER PRIMARY KEY, provider TEXT NOT NULL, session_id TEXT NOT NULL,
            redacted_text TEXT NOT NULL, source_updated_at INTEGER NOT NULL,
            source_complete INTEGER NOT NULL, complete INTEGER NOT NULL, indexed_at INTEGER NOT NULL,
            source_byte_count INTEGER NOT NULL, indexed_byte_count INTEGER NOT NULL,
            truncation_strategy TEXT NOT NULL, document_version INTEGER NOT NULL DEFAULT 0,
            UNIQUE(provider, session_id)
        );",
    )
    .unwrap();
    raw.execute(
        "INSERT INTO session_trajectories (
            provider, session_id, redacted_text, source_updated_at, source_complete, complete,
            indexed_at, source_byte_count, indexed_byte_count, truncation_strategy, document_version
         ) VALUES (?1, ?2, ?3, ?4, 1, 1, ?4, ?5, ?5, 'none', ?6)",
        rusqlite::params![
            session.provider.to_string(),
            session.id,
            legacy.redacted_text,
            legacy.source_updated_at.timestamp_millis(),
            i64::try_from(legacy.source_byte_count).unwrap(),
            legacy.document_version,
        ],
    )
    .unwrap();
    drop(raw);
    let f = Fixture {
        index: Store::open(path).unwrap(),
        root,
        session,
    };
    let read = || f.index.trajectory_index_state(&f.session).unwrap().unwrap();
    assert_eq!(read().source_updated_at, legacy.source_updated_at);
    assert!(!read().source_time_precise);
    let rev = revision(1, 1);
    let partial = TrajectoryDocument {
        document_version: 7,
        ..preview(rev)
    };
    assert_eq!(f.write(&partial, &f.observe()), 0);
    let state = read();
    assert!(state.source_complete);
    assert_eq!(state.source_revision, legacy.source_revision);
    assert_eq!(state.document_version, legacy.document_version);
    let upgraded = TrajectoryDocument {
        source_revision: rev,
        document_version: 7,
        ..legacy
    };
    assert_eq!(f.write(&upgraded, &f.observe()), 0);
    assert_eq!(read().document_version, upgraded.document_version);
    assert_eq!(read().source_revision, rev);
    let limited = TrajectoryDocument {
        provider_limited: true,
        document_version: 8,
        ..partial
    };
    assert_eq!(f.write(&limited, &f.observe()), 1);
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
    let other = failure(&full("", revision(1, 2)));
    assert_eq!(f.write(&current, &f.observe()), 1);
    let same = failure(&current);
    let future = TrajectoryIndexFailure {
        document_version: 7,
        ..other.clone()
    };
    for (failed, retain) in [(other, false), (same, true), (future, true)] {
        f.record(&failed);
        assert_eq!(f.write(&observed, &f.observe()), 0);
        assert_eq!(f.saved().as_ref(), retain.then_some(&failed));
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

#[test]
fn a_verified_native_read_publishes_its_locator_with_the_indexed_source() {
    let f = Fixture::new();
    let current = full("middlequartz", revision(1, 1));
    let mut locator = IndexedSession {
        source_path: None,
        source_revision: None,
        session: f.session.clone(),
        title: Some("First".into()),
        project_path: Some("first".into()),
        git_branch: None,
        created_at: Some(current.source_updated_at),
        updated_at: Some(current.source_updated_at),
        updated_at_approximate: false,
        event_count: 1,
    };
    let write = |locator: &IndexedSession, document, verified| {
        f.index
            .upsert_verified_session_document_checked(locator, document, f.observe(), |_, _| {
                verified
            })
    };
    assert_eq!(write(&locator, current, true).unwrap(), 1);
    let read_locator = || {
        f.index
            .indexed_sessions_for_provider(Provider::Antigravity)
            .unwrap()
            .pop()
            .unwrap()
    };
    locator.title = Some("New title".into());
    locator.project_path = Some("second".into());
    assert_eq!(
        write(&locator, preview(current.source_revision), true).unwrap(),
        0
    );
    assert_eq!(read_locator().project_path, locator.project_path);
    assert_eq!(
        f.index
            .search_session_trajectories("middlequartz", 1)
            .unwrap(),
        vec![f.session.clone()]
    );

    let raw = rusqlite::Connection::open(f.root.path().join("synthetic.sqlite3")).unwrap();
    raw.execute_batch(
        "CREATE TRIGGER unchanged_locator BEFORE UPDATE ON session_index
        BEGIN SELECT RAISE(ABORT, 'identical locator was rewritten'); END;",
    )
    .unwrap();
    locator.updated_at = Some(current.source_updated_at + chrono::Duration::nanoseconds(1));
    assert_eq!(write(&locator, current, true).unwrap(), 0);
    raw.execute_batch("DROP TRIGGER unchanged_locator").unwrap();
    let previous = read_locator();
    locator.project_path = Some("elsewhere".into());
    let summary = TrajectoryDocument {
        origin: SessionTrajectoryOrigin::NativeSummary,
        ..full("", revision(2, 3))
    };
    assert!(matches!(
        write(&locator, summary, false),
        Err(StoreError::TrajectorySourceChanged)
    ));
    assert_eq!(read_locator(), previous);
    assert_eq!(
        f.index
            .trajectory_index_state(&f.session)
            .unwrap()
            .unwrap()
            .source_revision,
        current.source_revision
    );
    let obsolete = TrajectoryDocument {
        document_version: current.document_version - 1,
        ..summary
    };
    assert!(matches!(
        write(&locator, obsolete, true),
        Err(StoreError::TrajectorySourceChanged)
    ));
    assert_eq!(read_locator(), previous);
    locator.event_count = 0;
    assert_eq!(write(&locator, summary, true).unwrap(), 1);
    assert_eq!(read_locator().event_count, 0);
}

#[test]
fn a_picker_preview_waits_for_the_new_reader_to_confirm_coverage() {
    for limited in [false, true] {
        let f = Fixture::new();
        let mut locator = IndexedSession {
            source_path: None,
            source_revision: None,
            session: f.session.clone(),
            title: Some("Previous name".into()),
            project_path: None,
            git_branch: None,
            created_at: None,
            updated_at: None,
            updated_at_approximate: false,
            event_count: 1,
        };
        let cached = TrajectoryDocument {
            source_complete: !limited,
            provider_limited: limited,
            ..full("retainedmiddle", revision(1, 6))
        };
        let home = f.root.path().join("native");
        let other = f.root.path().join("restored");
        let publish = |metadata: &IndexedSession, home: &std::path::Path, document| {
            f.index
                .upsert_verified_session_document_in_home(
                    metadata,
                    Some(home),
                    document,
                    f.observe(),
                    |_, _| true,
                )
                .unwrap()
        };
        let version = || {
            f.index
                .trajectory_index_state(&f.session)
                .unwrap()
                .unwrap()
                .document_version
        };
        assert_eq!(publish(&locator, &home, cached), 1);
        assert_eq!(version(), cached.document_version);
        locator.title = Some("Current name".into());
        let partial = TrajectoryDocument {
            document_version: 7,
            ..preview(revision(1, 7))
        };
        assert_eq!(publish(&locator, &other, partial), 0);
        assert_eq!(f.write(&partial, &f.observe()), 0);
        assert!(
            f.index
                .indexed_sessions_in_homes(Some(&other), None)
                .unwrap()
                .is_empty()
        );
        assert_eq!(version(), cached.document_version);
        assert_eq!(
            f.index.indexed_sessions().unwrap()[0].title.as_deref(),
            Some("Previous name")
        );
        assert_eq!(
            f.index
                .search_session_trajectories("retainedmiddle", 1)
                .unwrap(),
            vec![f.session.clone()]
        );
        let confirmed = TrajectoryDocument {
            document_version: 7,
            source_revision: partial.source_revision,
            ..cached
        };
        assert_eq!(publish(&locator, &other, confirmed), 0);
        assert_eq!(version(), confirmed.document_version);
        assert_eq!(
            f.index
                .indexed_sessions_in_homes(Some(&other), None)
                .unwrap()[0]
                .title,
            locator.title
        );
        assert!(
            f.index
                .indexed_sessions_in_homes(Some(&home), None)
                .unwrap()
                .is_empty()
        );
    }
}

#[test]
fn a_background_read_cannot_write_after_its_catalog_row_moves_to_another_home() {
    let f = Fixture::new();
    let writer = Store::open(f.root.path().join("synthetic.sqlite3")).unwrap();
    let a = f.root.path().join("a");
    let b = f.root.path().join("b");
    let mut locator = IndexedSession {
        source_path: None,
        source_revision: None,
        session: f.session.clone(),
        title: Some("A".into()),
        project_path: None,
        git_branch: None,
        created_at: None,
        updated_at: None,
        updated_at_approximate: false,
        event_count: 1,
    };
    let observe = |home: &std::path::Path| {
        f.index
            .observe_discovered_trajectory_document(&f.session)
            .unwrap()
            .for_published_catalog_home(home)
    };
    let list = |locator: &IndexedSession, home: &std::path::Path| {
        writer
            .cache_discovered_sessions(
                Provider::Antigravity,
                std::slice::from_ref(locator),
                omnis_store::DiscoveryScope::PartialProvider,
                Some(home),
            )
            .unwrap();
    };
    let old = full("alpha", revision(1, 1));
    let rejected = |guard| {
        assert!(matches!(
            f.index.upsert_observed_trajectory_documents_checked(
                &[(&f.session, old, guard)],
                |_, _| true
            ),
            Err(StoreError::TrajectorySourceChanged)
        ));
        assert!(matches!(
            f.index.record_observed_trajectory_index_failure_checked(
                &f.session,
                &failure(&old),
                guard,
                |_, _| true
            ),
            Err(StoreError::TrajectorySourceChanged)
        ));
    };
    let missing = observe(&a);
    list(&locator, &a);
    rejected(missing);

    let before_rebind = observe(&a);
    locator.title = Some("B".into());
    list(&locator, &b);
    rejected(before_rebind);
    rejected(observe(&a));
    assert!(
        f.index
            .trajectory_index_state(&f.session)
            .unwrap()
            .is_none()
    );
    assert!(f.saved().is_none());

    let before_rename = observe(&b);
    locator.title = Some("B renamed".into());
    list(&locator, &b);
    rejected(before_rename);
    assert_eq!(f.write(&full("beta", revision(2, 2)), &observe(&b)), 1);

    // Direct reads can publish a verified locator and document together when the native home changes.
    assert_eq!(
        writer
            .upsert_verified_session_document_in_home(
                &locator,
                Some(&a),
                old,
                f.observe(),
                |_, _| true,
            )
            .unwrap(),
        1
    );
    assert_eq!(
        f.index
            .indexed_sessions_for_provider_in_home(Provider::Antigravity, &a)
            .unwrap(),
        vec![locator]
    );
}

#[test]
fn live_search_reads_the_source_title_and_body_from_the_same_home() {
    let f = Fixture::new();
    let writer = Store::open(f.root.path().join("synthetic.sqlite3")).unwrap();
    let a = f.root.path().join("a");
    let b = f.root.path().join("b");
    let write = |home: &std::path::Path, file, text| {
        let locator = IndexedSession {
            project_path: Some(home.to_owned()),
            ..locator(&f.session)
        };
        let doc = TrajectoryDocument {
            derived_title: Some(text),
            ..full(text, revision(file, file))
        };
        writer
            .upsert_verified_session_document_in_home(
                &locator,
                Some(home),
                doc,
                writer.observe_trajectory_document(&f.session).unwrap(),
                |_, _| true,
            )
            .unwrap();
    };
    write(&a, 1, "redwood");
    let candidates = [(f.session.id.as_str(), revision(1, 1).file)];
    let cached_workspace = || -> omnis_store::Result<_> {
        let rows = f.index.indexed_sessions_for_provider(f.session.provider)?;
        Ok(rows[0].project_path.clone().unwrap())
    };
    f.index
        .read_trajectory_search(|read| -> omnis_store::Result<()> {
            assert_eq!(cached_workspace()?, a);
            write(&b, 2, "birch");
            assert_eq!(cached_workspace()?, a);
            assert!(
                read.conflicting_cached_documents(f.session.provider, &[&f.session.id])?
                    .is_empty()
            );
            assert!(
                read.conflicting_native_documents(f.session.provider, &a, &candidates)?
                    .is_empty()
            );
            assert_eq!(read.trajectory_titles()?[&f.session], "redwood");
            let page = read.search_session_trajectory_page_for_sessions(
                "redwood",
                1,
                std::slice::from_ref(&f.session),
            )?;
            assert_eq!(page.matches[0].session, f.session);
            Ok(())
        })
        .unwrap();
    f.index
        .read_trajectory_search(|read| -> omnis_store::Result<()> {
            assert_eq!(cached_workspace()?, b);
            assert!(
                read.conflicting_cached_documents(f.session.provider, &[&f.session.id])?
                    .is_empty()
            );
            assert!(
                read.search_session_trajectory_page_for_sessions(
                    "redwood",
                    1,
                    std::slice::from_ref(&f.session)
                )?
                .matches
                .is_empty()
            );
            assert_eq!(
                read.search_session_trajectory_page_for_sessions(
                    "birch",
                    1,
                    std::slice::from_ref(&f.session)
                )?
                .matches[0]
                    .session,
                f.session
            );
            assert!(
                read.conflicting_native_documents(f.session.provider, &a, &candidates)?
                    .contains(&f.session)
            );
            assert_eq!(read.trajectory_titles()?[&f.session], "birch");
            assert_eq!(
                read.native_source_title(&f.session, &a, revision(1, 1).file)?
                    .as_deref(),
                Some("redwood")
            );
            assert_eq!(
                read.native_source_title(&f.session, &a, revision(2, 2).file)?,
                None
            );
            // A different selected file cannot borrow a verified document from the same home.
            assert!(
                read.conflicting_native_documents(f.session.provider, &b, &candidates)?
                    .contains(&f.session)
            );
            Ok(())
        })
        .unwrap();
}

#[test]
fn an_unverified_native_document_stays_with_its_historical_locator() {
    let f = Fixture::new();
    let store = &f.index;
    let provider = Provider::AntigravityDesktop;
    let session = SessionRef::new(provider, "sample");
    let a = f.root.path().join("a");
    let b = f.root.path().join("b");
    let mut locator = IndexedSession {
        project_path: Some(a.clone()),
        ..locator(&session)
    };
    let historical = TrajectoryDocument {
        derived_title: Some("Old title"),
        document_version: 4,
        ..full("historical body", SourceRevision::default())
    };
    store.upsert_indexed_session(&locator).unwrap();
    store
        .upsert_trajectory_document(&session, &historical)
        .unwrap();
    let cached = || cached_document_is_eligible(store, &session);
    let title = |home: &std::path::Path, file| {
        store
            .trajectory_title_for_native_source(&session, home, file)
            .unwrap()
    };
    assert!(cached());
    assert_eq!(title(&a, None), None);
    assert_eq!(title(&b, revision(2, 2).file), None);
    store.upsert_indexed_session(&locator).unwrap();
    assert!(cached());
    locator.project_path = Some(b.clone());
    store.upsert_indexed_session(&locator).unwrap();
    assert!(!cached());
    let publish = |home: &std::path::Path| {
        store
            .cache_discovered_sessions(
                provider,
                std::slice::from_ref(&locator),
                DiscoveryScope::Workspace,
                Some(home),
            )
            .unwrap();
    };
    publish(&b);
    assert!(!cached());
    let doc = TrajectoryDocument {
        derived_title: Some("New title"),
        ..full("current body", revision(2, 2))
    };
    let guard = store
        .observe_discovered_trajectory_document(&session)
        .unwrap()
        .with_catalog_step_count(provider, 3, false);
    store
        .upsert_observed_trajectory_documents_checked(&[(&session, doc, guard)], |_, _| true)
        .unwrap();
    assert!(cached());
    assert_eq!(title(&b, None).as_deref(), Some("New title"));
    publish(&a);
    assert!(!cached());
    assert_eq!(title(&a, revision(2, 2).file), None);
    assert_eq!(title(&b, None), None);
    assert_eq!(title(&b, revision(3, 3).file), None);
    let count = |home, file| {
        store
            .verified_desktop_step_count(&session, home, Uuid::from_u128(file))
            .unwrap()
    };
    assert_eq!(count(&b, 2), Some(3));
    assert_eq!(count(&a, 2), None);
    assert_eq!(count(&b, 3), None);
    assert_eq!(
        title(&b, doc.source_revision.file).as_deref(),
        Some("New title")
    );
    publish(&b);
    assert!(cached());

    let partial = TrajectoryDocument {
        derived_title: Some("Partial title"),
        ..preview(revision(4, 4))
    };
    store
        .upsert_verified_session_document_in_home(
            &locator,
            Some(&b),
            partial,
            store.observe_trajectory_document(&session).unwrap(),
            |_, _| true,
        )
        .unwrap();
    assert_eq!(
        title(&b, partial.source_revision.file).as_deref(),
        Some("Partial title")
    );
    publish(&a);
    assert_eq!(
        title(&b, partial.source_revision.file).as_deref(),
        Some("Partial title")
    );
}

#[test]
fn a_new_desktop_declaration_survives_loss_of_the_catalog_locator() {
    let fixture = Fixture::new();
    let store = &fixture.index;
    let provider = Provider::AntigravityDesktop;
    let session = SessionRef::new(provider, "restored");
    let home = fixture.root.path().join("home");
    let other = fixture.root.path().join("other");
    let file = Uuid::from_u128(2);
    let known = IndexedSession {
        event_count: 3,
        source_revision: Some(revision(2, 2)),
        ..locator(&session)
    };
    store
        .upsert_verified_session_document_with_catalog(
            &known,
            Some(&home),
            false,
            full("verified history", revision(2, 2)),
            store.observe_trajectory_document(&session).unwrap(),
            |_, _| true,
        )
        .unwrap();
    let count = || {
        store
            .verified_desktop_step_count(&session, &home, file)
            .unwrap()
    };
    assert_eq!(count(), Some(3));

    let empty = IndexedSession {
        source_revision: Some(revision(2, 3)),
        event_count: 0,
        ..known
    };
    let discover = |home, scope| {
        store
            .cache_discovered_sessions(provider, std::slice::from_ref(&empty), scope, Some(home))
            .unwrap();
    };
    discover(
        &home,
        omnis_store::DiscoveryScope::DesktopPartialSummaries {
            missing_summary_ids: vec![session.id.clone()],
            unverified_archive_ids: Vec::new(),
        },
    );
    assert_eq!(count(), Some(3));
    discover(&home, omnis_store::DiscoveryScope::Workspace);
    discover(&other, omnis_store::DiscoveryScope::Workspace);
    assert_eq!(count(), Some(0));
}

#[test]
fn a_direct_desktop_declaration_rejects_older_reads_and_catalogs() {
    let fixture = Fixture::new();
    let store = &fixture.index;
    let provider = Provider::AntigravityDesktop;
    let session = SessionRef::new(provider, "restored");
    let home = fixture.root.path();
    let file = Uuid::from_u128(2);
    let row = |count| IndexedSession {
        event_count: count,
        source_revision: Some(revision(2, 2)),
        ..locator(&session)
    };
    let observed = || store.observe_trajectory_document(&session).unwrap();
    store
        .upsert_verified_session_document_in_home(
            &row(3),
            Some(home),
            full("verified text", revision(2, 2)),
            observed(),
            |_, _| true,
        )
        .unwrap();
    let publish = |steps, guard: TrajectorySourceGuard, current| {
        store.upsert_verified_session_document_with_catalog(
            &row(steps),
            Some(home),
            false,
            preview(revision(2, 2)),
            guard.with_incomplete_native_history(true),
            |_, _| current,
        )
    };
    let old_read = observed();
    let old_scan = store
        .mark_session_discovery_attempted(provider, 7, home)
        .unwrap();
    let old_workspace = store
        .session_discovery_checkpoint(provider, 7, home, Some(home))
        .unwrap();
    let count = || {
        store
            .verified_desktop_step_count(&session, home, file)
            .unwrap()
    };

    publish(3, observed(), true).unwrap();
    assert!(matches!(
        publish(1, old_read, true),
        Err(StoreError::TrajectorySourceChanged)
    ));
    assert!(matches!(
        store.publish_session_discovery(
            &old_scan,
            &[row(1)],
            DiscoveryScope::CompleteProvider,
            chrono::Utc::now()
        ),
        Err(StoreError::TrajectorySourceChanged)
    ));
    assert!(matches!(
        store.merge_sessions_at_discovery_checkpoint(&old_workspace, &[row(1)]),
        Err(StoreError::TrajectorySourceChanged)
    ));
    assert!(matches!(
        publish(0, observed(), false),
        Err(StoreError::TrajectorySourceChanged)
    ));
    assert_eq!(count(), Some(3));
    assert_eq!(store.indexed_session(&session).unwrap(), Some(row(3)));
    publish(0, observed(), true).unwrap();
    assert_eq!(count(), Some(0));
    assert_eq!(store.indexed_session(&session).unwrap(), Some(row(0)));
    store.forget_session(&session).unwrap();
    assert_eq!(count(), None);
}

#[test]
fn a_legacy_document_home_needs_a_selected_file_before_it_can_survive_a_restore() {
    let f = Fixture::new();
    let session = &f.session;
    let rows = [locator(session)];
    let a = f.root.path().join("a");
    let b = f.root.path().join("b");
    let file = revision(1, 1);
    let doc = TrajectoryDocument {
        derived_title: Some("Old title"),
        ..full("old body", file)
    };
    f.index
        .upsert_verified_session_document_in_home(&rows[0], Some(&b), doc, f.observe(), |_, _| true)
        .unwrap();
    let path = f.root.path().join("synthetic.sqlite3");
    let raw = rusqlite::Connection::open(&path).unwrap();
    raw.execute_batch(
        "DROP INDEX unverified_document_homes;
         ALTER TABLE native_search_document_homes DROP COLUMN verification;
         CREATE TRIGGER remember_rebound_document_home BEFORE UPDATE OF home ON native_catalog_homes
         BEGIN SELECT 1; END;",
    )
    .unwrap();
    let upgraded = Store::open(path).unwrap();
    let historical = || cached_document_is_eligible(&upgraded, session);
    assert!(historical());
    assert_eq!(
        upgraded
            .trajectory_title_for_native_source(session, &b, None)
            .unwrap(),
        None
    );
    assert_eq!(
        upgraded
            .trajectory_title_for_native_source(session, &b, revision(2, 2).file)
            .unwrap(),
        None
    );
    upgraded
        .cache_discovered_sessions(session.provider, &rows, DiscoveryScope::Workspace, Some(&b))
        .unwrap();
    assert!(historical());
    upgraded
        .cache_discovered_sessions(
            session.provider,
            &rows,
            DiscoveryScope::Workspace.with_source_files(&|_| None),
            Some(&b),
        )
        .unwrap();
    assert!(!historical());
    upgraded
        .cache_discovered_sessions(
            session.provider,
            &rows,
            DiscoveryScope::Workspace.with_source_files(&|_| file.file),
            Some(&a),
        )
        .unwrap();
    assert!(historical());
    assert_eq!(
        upgraded
            .trajectory_title_for_native_source(session, &a, None)
            .unwrap()
            .as_deref(),
        Some("Old title")
    );
}

#[test]
fn a_partial_read_from_another_home_owns_its_preview_and_retires_its_failure() {
    let f = Fixture::new();
    let a = f.root.path().join("a");
    let b = f.root.path().join("b");
    let publish = |home: &std::path::Path, doc: TrajectoryDocument<'_>, verified| {
        let metadata = IndexedSession {
            source_revision: Some(doc.source_revision),
            ..locator(&f.session)
        };
        f.index.upsert_verified_session_document_in_home(
            &metadata,
            Some(home),
            doc,
            f.observe()
                .with_incomplete_native_history(!doc.source_complete),
            |_, _| verified,
        )
    };
    let original_text = format!("{}firstsuffix", "first home ".repeat(12_000));
    let primary = full(&original_text, revision(1, 1));
    publish(&a, primary, true).unwrap();
    let partial = TrajectoryDocument {
        source_complete: false,
        truncation_strategy: "source_incomplete",
        ..full("second home", revision(2, 2))
    };
    let matches = |home| {
        f.index
            .read_trajectory_search(|snapshot| {
                snapshot.search_scoped_native_documents(
                    "second",
                    1,
                    &[(f.session.clone(), home, partial.source_revision.file)],
                )
            })
            .unwrap()
            .matches
    };
    let failed = failure(&partial);
    f.record(&failed);
    assert!(matches!(
        publish(&b, partial, false),
        Err(StoreError::TrajectorySourceChanged)
    ));
    assert_eq!(f.saved(), Some(failed));
    assert!(matches(Some(b.clone())).is_empty());

    assert_eq!(publish(&b, partial, true).unwrap(), 1);
    assert!(f.saved().is_none());
    assert_eq!(matches(None)[0].session, f.session);
    assert!(matches(Some(a.clone())).is_empty());
    assert_eq!(
        f.index
            .trajectory_index_state(&f.session)
            .unwrap()
            .unwrap()
            .source_revision,
        primary.source_revision
    );
    publish(&b, full("restored", revision(2, 3)), true).unwrap();
    assert!(matches(Some(b)).is_empty());

    let restore = TrajectoryDocument {
        source_complete: false,
        truncation_strategy: "source_incomplete",
        ..full("first home", revision(1, 4))
    };
    publish(&a, restore, true).unwrap();
    for home in [Some(&a), None] {
        let hit = f
            .index
            .read_trajectory_search(|snapshot| {
                let file = primary.source_revision.file;
                let conflicts = if let Some(home) = home {
                    snapshot.conflicting_native_documents(
                        f.session.provider,
                        home,
                        &[(&f.session.id, file)],
                    )?
                } else {
                    snapshot.conflicting_cached_documents(f.session.provider, &[&f.session.id])?
                };
                assert!(conflicts.contains(&f.session));
                snapshot.search_scoped_native_documents(
                    "firstsuffix",
                    1,
                    &[(f.session.clone(), home.cloned(), file)],
                )
            })
            .unwrap();
        assert_eq!(
            hit.matches.len(),
            1,
            "lost original suffix for home {home:?}"
        );
        assert!(hit.matches[0].source_complete);
        assert_eq!(hit.matches[0].indexed_byte_count, original_text.len());
    }
}
