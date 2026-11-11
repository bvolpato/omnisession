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
        "CREATE TABLE composerHeaders(composerId, workspaceId, createdAt, lastUpdatedAt, isArchived, isSubagent, value); CREATE TABLE cursorDiskKV(key TEXT PRIMARY KEY, value TEXT);"
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
    database
        .execute(
            "INSERT INTO cursorDiskKV VALUES (?1, ?2)",
            [
                format!("composerData:{SESSION_ID}"),
                json!({"composerId": SESSION_ID, "fullConversationHeadersOnly": []}).to_string(),
            ],
        )
        .unwrap();
    drop(database);
    let run = |home: &str, cwd: &Path, args: &[&str]| {
        successful_json(
            command(root, cwd)
                .env("OMNISESSION_HOME", root.join(home))
                .env("CURSOR_IDE_HOME", &cursor)
                .arg("--json")
                .args(args),
        )
    };
    let source = format!("cursor-ide:{SESSION_ID}");
    let bundle = root.join("portable.json");
    let bundle = bundle.to_str().unwrap();
    run("native", &first, &["export", &source, "--output", bundle]);
    let imported = run("fresh", &first, &["import", bundle]);
    let imported_id = imported["session"]["id"].as_str().unwrap();
    let indexed = run("native", &first, &["index", "--provider", "cursor-ide"]);
    assert_eq!(indexed["sessions"], 1);
    for (home, provider, id) in [
        ("native", "cursor-ide", SESSION_ID),
        ("fresh", "imported", imported_id),
    ] {
        for (cwd, expected) in [(&first, true), (&second, true), (&settings, false)] {
            let list = run(home, cwd, &["list", "--provider", provider]);
            assert_eq!(
                list["sessions"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|row| row["session"] == json!({"provider": provider, "id": id})),
                expected,
            );
            for mode in ["--no-index", "--cached"] {
                let found = run(
                    home,
                    cwd,
                    &["search", "synthetic", "--provider", provider, mode],
                );
                let locator = format!("{provider}:{id}");
                assert_eq!(
                    found["results"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|row| row["session"].as_str() == Some(&locator)),
                    expected,
                    "{mode} search from {}: {found}",
                    cwd.display(),
                );
            }
        }
    }
}
