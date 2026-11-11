use std::{fs, path::Path};

use serde_json::json;

use super::{SESSION_ID, command, successful_json};

#[test]
fn lists_and_searches_a_saved_cursor_workspace_from_its_member_folders() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let settings = root.join("settings");
    let first = root.join("first");
    let second = root.join("second");
    for folder in [&settings, &first, &second] {
        fs::create_dir(folder).unwrap();
    }
    let workspace = settings.join("team.code-workspace");
    fs::write(
        &workspace,
        r#"{"folders":[{"path":"../first"},{"name":"Other project","path":"../second"}]}"#,
    )
    .unwrap();
    let cursor = root.join("cursor");
    fs::create_dir_all(cursor.join("globalStorage")).unwrap();
    let database = rusqlite::Connection::open(cursor.join("globalStorage/state.vscdb")).unwrap();
    database.execute_batch(
        "CREATE TABLE composerHeaders(composerId, workspaceId, createdAt, lastUpdatedAt, isArchived, isSubagent, value);"
    ).unwrap();
    database
        .execute(
            "INSERT INTO composerHeaders VALUES (?1, NULL, 1, 2, 0, 0, ?2)",
            [
                SESSION_ID,
                &json!({"title":"Synthetic multi-root", "workspacePath":workspace}).to_string(),
            ],
        )
        .unwrap();
    drop(database);
    let run = |cwd: &Path, args: &[&str]| {
        successful_json(
            command(root, cwd)
                .env("CURSOR_IDE_HOME", &cursor)
                .arg("--json")
                .args(args),
        )
    };
    let indexed = run(&first, &["index", "--provider", "cursor-ide"]);
    assert_eq!(indexed["sessions"], 1);
    let source = format!("cursor-ide:{SESSION_ID}");
    for (cwd, expected) in [(&first, true), (&second, true), (&settings, false)] {
        let listing = run(cwd, &["list", "--provider", "cursor-ide"]);
        assert_eq!(
            listing["sessions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|row| row["session"]["id"] == SESSION_ID),
            expected,
        );
        for mode in ["--no-index", "--cached"] {
            let found = run(
                cwd,
                &["search", "synthetic", "--provider", "cursor-ide", mode],
            );
            assert_eq!(
                found["results"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|row| row["session"] == source),
                expected,
                "{mode} search from {}: {found}",
                cwd.display(),
            );
        }
    }
}
