use std::{fs, path::Path};

use serde_json::{Value, json};

use super::{CODEX_ID, Fixture, write_codex_session};

fn run_in(fixture: &Fixture, home: &Path, cwd: &Path, args: &[&str]) -> Value {
    let out = fixture
        .command()
        .current_dir(cwd)
        .env("CODEX_HOME", home)
        .env("CODEX_SQLITE_HOME", home)
        .arg("--json")
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice::<Value>(&out.stdout).unwrap()
}

#[test]
fn cached_search_retains_both_codex_homes_with_the_same_session_id() {
    let fixture = Fixture::new();
    let roots = [fixture.root.join("first"), fixture.root.join("second")];
    let workspaces = [
        fixture.workspace.clone(),
        fixture.root.join("other-workspace"),
    ];
    let titles = ["Check queue dispatch", "Locate worker status"];
    let markers = ["palegrapefruit", "saffronquince"];
    let session = format!("codex:{CODEX_ID}");
    for (i, root) in roots.iter().enumerate() {
        fs::create_dir_all(root.join("codex/sessions/2026/01/02")).unwrap();
        write_codex_session(
            root,
            CODEX_ID,
            &workspaces[i],
            titles[i],
            &format!("opalorchid {}", markers[i]),
        );
    }
    let run =
        |i: usize, args: &[&str]| run_in(&fixture, &roots[i].join("codex"), &workspaces[i], args);
    let check = |i: usize, query: &str, options: &[&str]| {
        let args = ["search", query, "--provider", "codex", "--show-text"];
        let result = run(i, &[args.as_slice(), options].concat());
        let rows = result["results"].as_array().unwrap();
        assert_eq!(rows.len(), 1, "home {i}, {query}: {result}");
        let hit = &rows[0];
        assert_eq!(hit["session"], session);
        assert_eq!(hit["title"], titles[i]);
        assert_eq!(hit["project_path"], json!(workspaces[i]));
        assert_eq!(hit["match"], "conversation");
        assert!(hit["snippet"].as_str().unwrap().contains(markers[i]));
    };
    for (i, &marker) in markers.iter().enumerate() {
        assert_eq!(run(i, &["index", "--provider", "codex"])["indexed"], 1);
        check(i, marker, &["--cached", "--all-projects"]);
    }
    for (i, &marker) in markers.iter().enumerate() {
        check(i, marker, &["--cached", "--all-projects"]);
        check(i, "opalorchid", &["--cached"]);
        let foreign = run(
            i,
            &[
                "search",
                markers[1 - i],
                "--provider",
                "codex",
                "--cached",
                "--all-projects",
            ],
        );
        assert!(
            foreign["results"].as_array().unwrap().is_empty(),
            "{foreign}"
        );
    }
    for root in &roots {
        fs::rename(root.join("codex/sessions"), root.join("offline")).unwrap();
    }
    for i in 0..2 {
        check(i, "opalorchid", &["--cached"]);
    }
    for root in &roots {
        fs::rename(root.join("offline"), root.join("codex/sessions")).unwrap();
    }
    assert!(
        run(0, &["show", &session])["events"]
            .to_string()
            .contains(markers[0])
    );
    check(1, markers[1], &["--cached", "--all-projects"]);
    check(0, "opalorchid", &[]);
    check(1, "opalorchid", &["--cached"]);
}

#[test]
fn metadata_only_codex_sessions_cannot_borrow_a_legacy_imported_document() {
    let portable = Fixture::new();
    let fixture = Fixture::new();
    let source = format!("codex:{CODEX_ID}");
    let bundle = fixture.root.join("portable.json");
    let exported = run_in(
        &portable,
        &portable.root.join("codex"),
        &portable.workspace,
        &["export", &source, "--output", bundle.to_str().unwrap()],
    );
    let id = exported["bundle_id"].as_str().unwrap();
    let imported = format!("imported:{id}");
    let second = fixture.root.join("second");
    let project = second.join("bravosorrel");
    fs::create_dir_all(second.join("codex/sessions/2026/01/02")).unwrap();
    fs::create_dir(&project).unwrap();
    write_codex_session(
        &second,
        CODEX_ID,
        &project,
        "Second native request",
        "Second native answer",
    );
    write_codex_session(
        &fixture.root,
        CODEX_ID,
        &fixture.workspace,
        "First native request",
        "First native answer",
    );
    let run = |args: &[&str]| run_in(&fixture, &second.join("codex"), &project, args);
    run(&["import", bundle.to_str().unwrap()]);
    let db = rusqlite::Connection::open(fixture.root.join("state/store.sqlite3")).unwrap();
    // Older imports indexed the bundle under its source session ID.
    assert_eq!(db.execute(
        "UPDATE session_trajectories SET provider='codex', session_id=?1
         WHERE provider='imported' AND session_id=?2 AND origin='imported_bundle' AND codex_home IS NULL",
        [CODEX_ID, id],
    ).unwrap(), 1);
    // Interrupt indexing after native discovery persists the session metadata.
    db.execute_batch(
        "CREATE TRIGGER hold_document BEFORE INSERT ON session_trajectories
         WHEN new.provider='codex' AND new.origin='native'
         BEGIN SELECT RAISE(FAIL, 'synthetic write interruption'); END;",
    )
    .unwrap();
    assert_eq!(run(&["index", "--provider", "codex"])["failed"], 1);
    db.execute_batch("DROP TRIGGER hold_document;").unwrap();
    let query = |term: &str, provider: &str| {
        run(&[
            "search",
            term,
            "--cached",
            "--all-projects",
            "--show-text",
            "--provider",
            provider,
        ])
    };
    let check = |native_title: Option<&str>| {
        for term in ["Pagination", "backoff"] {
            let hits = query(term, "codex");
            assert!(hits["results"].as_array().unwrap().is_empty(), "{hits}");
        }
        let native = query("bravosorrel", "codex");
        let row = &native["results"][0];
        assert_eq!(row["session"], source, "{native}");
        assert_eq!(row["title"], json!(native_title));
        assert_eq!(row["project_path"], json!(project));
        let alias = query("Pagination", "imported");
        let row = &alias["results"][0];
        assert_eq!(row["session"], imported, "{alias}");
        assert_eq!(row["title"], "Tune the rate limiter backoff");
        assert!(row["snippet"].as_str().unwrap().contains("Pagination"));
    };
    check(None);
    run_in(
        &fixture,
        &fixture.root.join("codex"),
        &fixture.workspace,
        &["show", &source],
    );
    check(None);
    run(&["show", &source]);
    check(Some("Second native request"));
}
