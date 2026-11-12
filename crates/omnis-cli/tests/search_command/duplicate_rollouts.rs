use std::{fs, path::Path, time::Duration};

use serde_json::{Value, json};

use super::{CODEX_ID, Fixture, write_codex_session};

fn run_json(fixture: &Fixture, args: &[&str]) -> Value {
    let output = fixture
        .command()
        .env("CODEX_SQLITE_HOME", fixture.root.join("codex"))
        .arg("--json")
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn assert_listed(fixture: &Fixture, directory: &Path) {
    let response = run_json(
        fixture,
        &[
            "list",
            "--provider",
            "codex",
            "--project",
            directory.to_str().unwrap(),
        ],
    );
    let rows = response["sessions"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "{response}");
    assert_eq!(rows[0]["session"]["id"], CODEX_ID);
    assert_eq!(rows[0]["project_path"], json!(directory));
}

#[test]
fn listing_a_newer_archived_copy_preserves_the_active_rollouts_search_results() {
    for identical in [true, false] {
        let fixture = Fixture::new();
        let other = fixture.root.join("archive-project");
        fs::create_dir(&other).unwrap();
        let name = format!("rollout-2026-01-02T00-00-00-{CODEX_ID}.jsonl");
        let active = fixture.root.join("codex/sessions/2026/01/02").join(&name);
        let archived_dir = fixture.root.join("codex/archived_sessions");
        fs::create_dir(&archived_dir).unwrap();
        let archive = archived_dir.join(name);
        let reference = format!("codex:{CODEX_ID}");
        let active_reply = "emeraldchiffinch";
        let (archive_project, archive_title, archive_reply) = if identical {
            (&fixture.workspace, "Active request", active_reply)
        } else {
            (&other, "Archived request", "ochrecockatoo")
        };
        write_codex_session(
            &fixture.root,
            CODEX_ID,
            archive_project,
            archive_title,
            archive_reply,
        );
        fs::rename(&active, &archive).unwrap();
        write_codex_session(
            &fixture.root,
            CODEX_ID,
            &fixture.workspace,
            "Active request",
            active_reply,
        );
        fs::File::options()
            .write(true)
            .open(&archive)
            .unwrap()
            .set_modified(active.metadata().unwrap().modified().unwrap() + Duration::from_secs(60))
            .unwrap();
        let active_bytes = fs::read(&active).unwrap();
        let archive_bytes = fs::read(&archive).unwrap();
        assert_eq!(active_bytes == archive_bytes, identical);

        let run = |args: &[&str]| run_json(&fixture, args);
        let cached = |workspace: &Path, term: &str| {
            run(&[
                "search",
                term,
                "--provider",
                "codex",
                "--cached",
                "--project",
                workspace.to_str().unwrap(),
            ])
        };
        assert_eq!(run(&["index", "--provider", "codex"])["failed"], 0);
        assert_eq!(
            cached(&fixture.workspace, active_reply)["results"][0]["session"],
            reference
        );
        assert_listed(&fixture, &fixture.workspace);
        assert_eq!(
            cached(&fixture.workspace, active_reply)["results"][0]["session"],
            reference
        );
        assert!(
            run(&["show", &reference])["events"]
                .to_string()
                .contains(active_reply)
        );
        assert_eq!(run(&["index", "--provider", "codex"])["stale"], 0);
        assert_eq!(fs::read(&active).unwrap(), active_bytes);
        assert_eq!(fs::read(&archive).unwrap(), archive_bytes);

        fs::rename(active, fixture.root.join("detached")).unwrap();
        assert_listed(&fixture, archive_project);
        let restored = run(&["show", &reference]);
        assert!(restored["events"].to_string().contains(archive_reply));
        assert_eq!(restored["workspace"]["current_dir"], json!(archive_project));
        assert_eq!(
            cached(archive_project, archive_reply)["results"][0]["session"],
            reference
        );
        assert_eq!(fs::read(archive).unwrap(), archive_bytes);
    }
}
