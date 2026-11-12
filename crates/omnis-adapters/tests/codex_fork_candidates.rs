use std::fs;

use omnis_adapters::CodexAdapter;
use omnis_ir::{Provider, SessionRef};
use serde_json::json;

#[test]
fn fork_discovery_excludes_reverts_and_forks_of_other_threads() -> anyhow::Result<()> {
    const PARENT: &str = "10000000-0000-4000-8000-000000000001";
    const EXISTING_CHILD: &str = "20000000-0000-4000-8000-000000000002";
    const REPLACEMENT: &str = "30000000-0000-4000-8000-000000000003";
    const OTHER_CHILD: &str = "40000000-0000-4000-8000-000000000004";
    const NEW_CHILD: &str = "50000000-0000-4000-8000-000000000005";

    let home = tempfile::tempdir()?;
    let project = home.path().join("project");
    let sessions = home.path().join("sessions/2026/01/15");
    fs::create_dir(&project)?;
    fs::create_dir_all(&sessions)?;
    let write = |thread: &str, name: &str, parent: Option<&str>, mode: &str| {
        let header = json!({"type":"session_meta","timestamp":"2026-01-15T12:00:01Z",
            "payload":{"id":thread,"cwd":project,"forked_from_id":parent,"history_mode":mode}});
        fs::write(
            sessions.join(format!("rollout-2026-01-15T12-00-01-{name}.jsonl")),
            header.to_string(),
        )
    };
    let adapter = CodexAdapter::with_root(home.path());
    let source = SessionRef::new(Provider::Codex, PARENT);
    let started = "2026-01-15T12:00:00Z".parse()?;
    let candidates = || adapter.fork_candidates_created_since(&source, &project, started);

    // A revert retains the original fork parent and writes a new physical rollout ID.
    write(
        EXISTING_CHILD,
        &format!("{EXISTING_CHILD}_{REPLACEMENT}"),
        Some(PARENT),
        "paginated",
    )?;
    assert!(candidates()?.is_empty());
    write(OTHER_CHILD, OTHER_CHILD, None, "paginated")?;
    assert!(candidates()?.is_empty());
    write(OTHER_CHILD, OTHER_CHILD, Some(EXISTING_CHILD), "legacy")?;
    assert!(candidates()?.is_empty());
    write(NEW_CHILD, NEW_CHILD, Some(PARENT), "legacy")?;
    let expected = [SessionRef::new(Provider::Codex, NEW_CHILD)];
    let plain = sessions.join(format!("rollout-2026-01-15T12-00-01-{NEW_CHILD}.jsonl"));
    let compressed = plain.with_extension("jsonl.zst");
    let encoded = zstd::stream::encode_all(fs::File::open(&plain)?, 1)?;
    fs::write(&compressed, &encoded[..4])?;
    assert_eq!(candidates()?, expected);
    fs::write(&compressed, encoded)?;
    fs::write(&plain, b"{\n")?;
    assert_eq!(candidates()?, expected);
    write(NEW_CHILD, NEW_CHILD, Some(EXISTING_CHILD), "legacy")?;
    assert!(candidates()?.is_empty());
    Ok(())
}
