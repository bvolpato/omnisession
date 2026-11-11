use chrono::Utc;
use omnis_ir::{Provider, SessionRef};
use omnis_store::{DiscoveryScope, IndexedSession, Store};

#[test]
fn scoped_rows_cannot_overwrite_a_newer_provider_listing() {
    let root = tempfile::tempdir().unwrap();
    let database = root.path().join("cache.sqlite3");
    let scoped = Store::open(&database).unwrap();
    let wide = Store::open(&database).unwrap();
    let provider = Provider::Antigravity;
    let home = root.path().join("home");
    let other = root.path().join("other");
    let checkpoint = || {
        scoped
            .session_discovery_checkpoint(provider, 7, &home, None)
            .unwrap()
    };
    let before = checkpoint();
    let owner = wide
        .mark_session_discovery_attempted(provider, 7, &home)
        .unwrap();
    let pending = checkpoint();
    assert!(pending.is_for_selected_home());
    let mut row = IndexedSession {
        source_revision: None,
        session: SessionRef::new(provider, "shared"),
        title: Some("Published title".into()),
        project_path: None,
        git_branch: None,
        created_at: None,
        updated_at: None,
        updated_at_approximate: false,
        event_count: 1,
    };
    assert!(
        wide.publish_session_discovery(
            &owner,
            std::slice::from_ref(&row),
            DiscoveryScope::PartialProvider,
            Utc::now()
        )
        .unwrap()
    );
    let current = checkpoint();
    row.title = Some("Earlier workspace listing".into());
    for obsolete in [&before, &pending] {
        assert!(
            !scoped
                .merge_sessions_at_discovery_checkpoint(obsolete, std::slice::from_ref(&row))
                .unwrap()
        );
    }
    assert_eq!(
        scoped.indexed_sessions_for_provider(provider).unwrap()[0]
            .title
            .as_deref(),
        Some("Published title")
    );
    assert!(
        scoped
            .merge_sessions_at_discovery_checkpoint(&current, &[])
            .unwrap()
    );
    row.title = Some("Revalidated native title".into());
    assert!(
        scoped
            .merge_sessions_at_discovery_checkpoint(&current, std::slice::from_ref(&row))
            .unwrap()
    );
    assert_eq!(
        scoped.indexed_sessions_for_provider(provider).unwrap(),
        vec![row]
    );
    wide.mark_session_discovery_attempted(provider, 7, &other)
        .unwrap();
    assert!(
        !scoped
            .merge_sessions_at_discovery_checkpoint(&current, &[])
            .unwrap()
    );
    assert!(!checkpoint().is_for_selected_home());
}

#[test]
fn independent_workspaces_and_unchanged_listings_keep_their_checkpoints() {
    let root = tempfile::tempdir().unwrap();
    let database = root.path().join("cache.sqlite3");
    let store = Store::open(&database).unwrap();
    let home = root.path().join("native");
    let first = root.path().join("first");
    let second = root.path().join("second");
    let provider = Provider::Antigravity;
    let read = |project: &std::path::Path| {
        store
            .session_discovery_checkpoint(provider, 7, &home, Some(project))
            .unwrap()
    };
    let initial = read(&first);
    let separate = read(&second);
    let row = IndexedSession {
        source_revision: None,
        session: SessionRef::new(provider, "new"),
        title: Some("Found".into()),
        project_path: Some(first.clone()),
        git_branch: None,
        created_at: None,
        updated_at: None,
        updated_at_approximate: false,
        event_count: 1,
    };
    let db = rusqlite::Connection::open(database).unwrap();
    let checkpoint_count = || {
        db.query_row("SELECT count(*) FROM session_workspace_catalogs", [], |r| {
            r.get::<_, i64>(0)
        })
        .unwrap()
    };
    assert!(store.discovery_checkpoint_is_current(&initial).unwrap());
    assert_eq!(checkpoint_count(), 0);
    assert!(
        store
            .merge_sessions_at_discovery_checkpoint(&initial, std::slice::from_ref(&row))
            .unwrap()
    );
    assert!(store.discovery_checkpoint_is_current(&separate).unwrap());
    assert_eq!(checkpoint_count(), 1);
    assert!(
        !store
            .merge_sessions_at_discovery_checkpoint(&initial, &[])
            .unwrap()
    );
    let current = read(&first);
    assert!(current.can_follow(&initial));

    db.execute_batch("CREATE TRIGGER reject_unchanged_workspace_write BEFORE UPDATE ON session_workspace_catalogs BEGIN SELECT RAISE(ABORT, 'redundant write'); END").unwrap();
    for _ in 0..8 {
        assert!(
            store
                .merge_sessions_at_discovery_checkpoint(&current, std::slice::from_ref(&row))
                .unwrap()
        );
    }
    db.execute_batch("DROP TRIGGER reject_unchanged_workspace_write")
        .unwrap();
    assert!(store.discovery_checkpoint_is_current(&current).unwrap());
    assert_eq!(checkpoint_count(), 1);
    assert!(
        store
            .merge_sessions_at_discovery_checkpoint(&current, &[])
            .unwrap()
    );
    assert!(!store.discovery_checkpoint_is_current(&current).unwrap());
    assert!(store.discovery_checkpoint_is_current(&separate).unwrap());
    assert_eq!(store.session_index_checked_at(provider).unwrap(), None);
}
